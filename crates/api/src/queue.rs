//! The job queue (DESIGN.md §6): every write to the `jobs` table lives in
//! this module.
//!
//! - [`enqueue_run`]: insert a run and its jobs (called by the launch
//!   endpoint, P0c PR 4).
//! - [`acquire`]: an agent claims the oldest queued job it may run.
//! - [`heartbeat`]: the agent renews its lease; the reply may say "cancel".
//! - [`complete`]: the agent's result moves the job to its next state and
//!   queues the jobs that were waiting for it.
//! - [`reap`]: expired leases are retried, failed or cancelled.
//! - [`cancel_run`]: the scientist cancels a run.
//!
//! Two rules keep this correct with many agents, several API replicas and
//! the reaper all acting at once:
//!
//! 1. **Every transition is a compare-and-set**: `UPDATE … WHERE id = $1
//!    AND state = 'leased' AND attempt = $2`. If another actor moved the
//!    job first, zero rows change and we know.
//! 2. **Changes to a run's jobs are serialized by locking the run's row**
//!    first (`SELECT … FOR UPDATE`). Without it, the last two jobs of a run
//!    finishing at the same moment could each see the other as unfinished,
//!    and neither would mark the run succeeded or queue the job that needs
//!    both. Postgres's default isolation (READ COMMITTED) gives each
//!    *statement* a fresh snapshot, so once the lock is ours, the next
//!    statement sees everything the previous holder committed.
//!
//! Acquire and heartbeat touch one job and don't take the run lock, so the
//! hot path (thousands of heartbeats) never queues behind it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, anyhow, bail};
use protocol::blob::BlobRef;
use protocol::digest::{Digest, HashAlgorithm};
use protocol::job::{InputSource, JobInterface, data_name};
use protocol::lease::{
    HeartbeatAction, InputFile, Lease, LeaseResponse, ResolvedInput, RunRef, Settings,
};
use protocol::result::{JobResult, JobStatus, TaskRecord, TaskStatus};
use protocol::version::FormatVersion;
use sqlx::types::Json;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::agents::Agent;

/// How long an agent may go silent before its lease expires (DESIGN.md
/// §4.1: lease 60 s, heartbeat every 20 s, so two heartbeats can fail and
/// the lease still holds).
pub const LEASE_SECS: i32 = 60;
pub const HEARTBEAT_EVERY_SECS: u64 = 20;
/// After this many attempts a job fails instead of being retried, so a job
/// that crashes agents can't take them all down in turn.
pub const MAX_ATTEMPTS: i32 = 3;

/// The delay before retrying a job whose lease expired: 30 s after the
/// first loss, 2 minutes after later ones.
fn retry_backoff_secs(attempt: i32) -> i32 {
    if attempt <= 1 { 30 } else { 120 }
}

/// A job's state (DESIGN.md §6, "Job states"). Stored as text; this enum
/// is how Rust code names the states, so a typo is a compile error rather
/// than a query that silently matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Blocked,
    Queued,
    Leased,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Blocked => "blocked",
            JobState::Queued => "queued",
            JobState::Leased => "leased",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Skipped => "skipped",
        }
    }

    pub fn is_final(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled | JobState::Skipped
        )
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobState {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<JobState> {
        Ok(match text {
            "blocked" => JobState::Blocked,
            "queued" => JobState::Queued,
            "leased" => JobState::Leased,
            "succeeded" => JobState::Succeeded,
            "failed" => JobState::Failed,
            "cancelled" => JobState::Cancelled,
            "skipped" => JobState::Skipped,
            other => bail!("unknown job state `{other}`"),
        })
    }
}

// ---------------------------------------------------------------------------
// Enqueue

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Batch,
    Analysis,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            JobKind::Batch => "batch",
            JobKind::Analysis => "analysis",
        }
    }
}

/// A run ready to be queued: its plan's jobs, compiled and with their run
/// inputs resolved.
#[derive(Debug, Clone)]
pub struct NewRun {
    pub compute_env: String,
    /// In plan order: a job's index is its position here.
    pub jobs: Vec<NewJob>,
}

#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: JobKind,
    /// The project's tree, already stored (`blobs::store_tree`).
    pub project_tree: BlobRef,
    pub interface: JobInterface,
    /// The job's `from: run` inputs, resolved from the dataset. Its
    /// `from: job` inputs are resolved when the jobs it depends on succeed.
    pub run_inputs: BTreeMap<String, ResolvedInput>,
    /// Indexes of the jobs whose outputs it reads.
    pub depends_on: Vec<usize>,
    /// The agent whose disk holds the run inputs, if they are on one.
    pub pinned_agent: Option<Uuid>,
}

/// Insert a run and all its jobs in the caller's transaction, so launching
/// is all or nothing. Jobs without dependencies start `queued`, the rest
/// `blocked`.
pub async fn enqueue_run(tx: &mut Transaction<'_, Postgres>, run: &NewRun) -> anyhow::Result<Uuid> {
    let run_id: Uuid = sqlx::query_scalar("INSERT INTO runs DEFAULT VALUES RETURNING id")
        .fetch_one(&mut **tx)
        .await?;

    let mut job_ids = Vec::new();
    for (index, job) in run.jobs.iter().enumerate() {
        if let Some(&bad) = job.depends_on.iter().find(|&&dep| dep >= index) {
            // Plans are topologically ordered, so a job can only depend on
            // jobs before it. Anything else is a bug in the caller.
            bail!("job {index} depends on job {bad}, which doesn't come before it");
        }
        let state = if job.depends_on.is_empty() {
            JobState::Queued
        } else {
            JobState::Blocked
        };
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO jobs (run_id, job_index, kind, state, max_attempts, compute_env,
                               pinned_agent, project_tree, interface, resolved_inputs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING id",
        )
        .bind(run_id)
        .bind(i32::try_from(index)?)
        .bind(job.kind.as_str())
        .bind(state.as_str())
        .bind(MAX_ATTEMPTS)
        .bind(&run.compute_env)
        .bind(job.pinned_agent)
        .bind(job.project_tree.digest.to_string())
        .bind(Json(&job.interface))
        .bind(Json(&job.run_inputs))
        .fetch_one(&mut **tx)
        .await
        .with_context(|| format!("inserting job {index}"))?;

        for &dep in &job.depends_on {
            sqlx::query("INSERT INTO job_dependencies (job_id, depends_on_job_id) VALUES ($1, $2)")
                .bind(id)
                .bind(job_ids[dep])
                .execute(&mut **tx)
                .await?;
        }
        job_ids.push(id);
    }
    Ok(run_id)
}

// ---------------------------------------------------------------------------
// Acquire and heartbeat: the agent protocol (DESIGN.md §4.1)

/// Claim the oldest queued job this agent may run, or `None` if there is
/// none (the agent polls again later).
pub async fn acquire(
    pool: &PgPool,
    agent: &Agent,
    agent_version: &str,
) -> anyhow::Result<Option<LeaseResponse>> {
    sqlx::query("UPDATE agents SET agent_version = $2, last_seen_at = now() WHERE id = $1")
        .bind(agent.id)
        .bind(agent_version)
        .execute(pool)
        .await?;

    let mut tx = pool.begin().await?;

    // The heart of the queue. The inner SELECT finds a candidate and locks
    // it; `SKIP LOCKED` makes concurrent agents pass over rows another
    // transaction has locked instead of waiting for them, so two agents
    // never get the same job and never queue behind each other. The
    // expiry comes from the database's clock, never the agent's.
    //
    // `sqlx::types::Json<T>` decodes a JSONB column straight into a Rust
    // type; if the stored JSON doesn't fit, the query fails and the
    // transaction rolls back, leaving the job queued.
    type Row = (
        Uuid,
        Uuid,
        i32,
        i32,
        String,
        String,
        Json<JobInterface>,
        Json<BTreeMap<String, ResolvedInput>>,
    );
    let row: Option<Row> = sqlx::query_as(
        "UPDATE jobs SET state = 'leased', agent_id = $1, attempt = attempt + 1,
                         lease_expires_at = now() + $3 * interval '1 second', progress = NULL
         WHERE id = (SELECT id FROM jobs
                     WHERE state = 'queued' AND compute_env = $2 AND kind = 'batch'
                       AND run_at <= now()
                       AND (pinned_agent IS NULL OR pinned_agent = $1)
                     ORDER BY created_at, job_index
                     FOR UPDATE SKIP LOCKED
                     LIMIT 1)
         RETURNING id, run_id, job_index, attempt,
                   to_char(lease_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
                   project_tree, interface, resolved_inputs",
    )
    .bind(agent.id)
    .bind(&agent.compute_env)
    .bind(LEASE_SECS)
    .fetch_optional(&mut *tx)
    .await?;

    // `let … else`: destructure, or leave the function if it doesn't match.
    let Some((job_id, run_id, job_index, attempt, expires_at, tree, interface, inputs)) = row
    else {
        return Ok(None);
    };

    let (tree_size, nextflow_version, hash_algorithm, time_limit): (
        i64,
        String,
        String,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT b.size, c.nextflow_version, c.hash_algorithm, c.time_limit_secs
             FROM blobs b, compute_envs c WHERE b.digest = $1 AND c.id = $2",
    )
    .bind(&tree)
    .bind(&agent.compute_env)
    .fetch_one(&mut *tx)
    .await?;

    let response = LeaseResponse {
        format_version: FormatVersion::current(),
        lease: Lease {
            job_id: job_id.to_string(),
            attempt: u32::try_from(attempt)?,
            expires_at,
            heartbeat_every_secs: HEARTBEAT_EVERY_SECS,
        },
        run: RunRef {
            run_id: run_id.to_string(),
            job_index: usize::try_from(job_index)?,
        },
        settings: Settings {
            nextflow_version,
            hash_algorithm: hash_algorithm
                .parse::<HashAlgorithm>()
                .map_err(|e| anyhow!(e))?,
            time_limit_secs: time_limit.map(u64::try_from).transpose()?,
        },
        project: BlobRef {
            digest: tree.parse::<Digest>().map_err(|e| anyhow!(e))?,
            size: u64::try_from(tree_size)?,
        },
        // `.0` takes the value out of the `Json` wrapper.
        interface: interface.0,
        inputs: inputs.0,
    };
    // The claim only takes effect at commit: until then, other agents
    // skip this row, and a crash here leaves the job queued.
    tx.commit().await?;
    Ok(Some(response))
}

/// Renew a lease. `None` means the lease is lost: the job isn't leased to
/// this agent with this attempt any more (the reaper took it back, or a
/// newer attempt holds it). The agent must stop and discard its work.
///
/// A lease that has expired but not yet been reaped is still renewed: the
/// attempt is the fence, not the clock. Nobody else can hold the job
/// until the reaper has moved it, and then the attempt check fails.
pub async fn heartbeat(
    pool: &PgPool,
    agent: &Agent,
    job_id: Uuid,
    attempt: u32,
    progress: Option<&str>,
) -> anyhow::Result<Option<HeartbeatAction>> {
    let cancel_requested: Option<bool> = sqlx::query_scalar(
        "UPDATE jobs SET lease_expires_at = now() + $4 * interval '1 second', progress = $5
         WHERE id = $1 AND attempt = $2 AND agent_id = $3 AND state = 'leased'
         RETURNING cancel_requested",
    )
    .bind(job_id)
    .bind(i32::try_from(attempt)?)
    .bind(agent.id)
    .bind(LEASE_SECS)
    .bind(progress)
    .fetch_optional(pool)
    .await?;

    Ok(cancel_requested.map(|cancel| {
        if cancel {
            HeartbeatAction::Cancel
        } else {
            HeartbeatAction::Continue
        }
    }))
}

// ---------------------------------------------------------------------------
// Results, the reaper and cancel

/// What a job's attempt led to. Deciding is kept apart from writing, so
/// the retry policy is a plain function that tests can call directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed(String),
    Cancelled,
    /// Back to the queue, available again after this many seconds.
    Retry {
        after_secs: i32,
    },
}

/// The retry policy for a reported result (DESIGN.md §6): retry what the
/// infrastructure broke; never retry what will fail the same way again.
pub fn decide_result(
    status: &JobStatus,
    tasks: &[TaskRecord],
    attempt: i32,
    max_attempts: i32,
    cancel_requested: bool,
) -> Outcome {
    // `match` must cover every variant: add a JobStatus and this stops
    // compiling until the new status has a policy.
    match status {
        JobStatus::Succeeded => Outcome::Succeeded,
        JobStatus::Failed { exit_code } => {
            let failed: Vec<&str> = tasks
                .iter()
                .filter(|task| task.status == TaskStatus::Failed)
                .map(|task| task.name.as_str())
                .collect();
            if failed.is_empty() {
                let exit = match exit_code {
                    Some(code) => format!("with code {code}"),
                    None => "abnormally".to_string(),
                };
                Outcome::Failed(format!(
                    "Nextflow exited {exit} and no task failed: a problem in Nextflow or the pipeline"
                ))
            } else {
                Outcome::Failed(format!("task failed: {}", failed.join(", ")))
            }
        }
        JobStatus::TimedOut => Outcome::Failed("the job reached its time limit".to_string()),
        JobStatus::Cancelled | JobStatus::Interrupted if cancel_requested => Outcome::Cancelled,
        JobStatus::Cancelled => Outcome::Failed(
            "the agent reported the job cancelled, but no cancel was requested".to_string(),
        ),
        JobStatus::Interrupted if attempt < max_attempts => Outcome::Retry { after_secs: 0 },
        JobStatus::Interrupted => Outcome::Failed(format!(
            "the agent was shut down during all {max_attempts} attempts"
        )),
    }
}

/// The policy for a lease that expired: the agent crashed, lost power or
/// was cut off.
pub fn decide_expired(attempt: i32, max_attempts: i32, cancel_requested: bool) -> Outcome {
    if cancel_requested {
        Outcome::Cancelled
    } else if attempt < max_attempts {
        Outcome::Retry {
            after_secs: retry_backoff_secs(attempt),
        }
    } else {
        Outcome::Failed(format!(
            "the agent running it was lost on all {max_attempts} attempts"
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// The result moved the job to this state.
    Recorded(JobState),
    /// This agent already reported this attempt (its first report got
    /// through but the reply didn't): accepted again, changing nothing.
    AlreadyRecorded(JobState),
    /// The job isn't this agent's current attempt: discard the work.
    Stale,
}

/// Record an agent's result for one attempt.
pub async fn complete(
    pool: &PgPool,
    agent: &Agent,
    job_id: Uuid,
    attempt: u32,
    result: &JobResult,
) -> anyhow::Result<Completion> {
    let attempt = i32::try_from(attempt)?;
    let mut tx = pool.begin().await?;

    let run_id: Option<Uuid> = sqlx::query_scalar("SELECT run_id FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(run_id) = run_id else {
        return Ok(Completion::Stale);
    };
    lock_run(&mut tx, run_id).await?;

    let (state, current_attempt, holder, max_attempts, cancel_requested): (
        String,
        i32,
        Option<Uuid>,
        i32,
        bool,
    ) = sqlx::query_as(
        "SELECT state, attempt, agent_id, max_attempts, cancel_requested
         FROM jobs WHERE id = $1 FOR UPDATE",
    )
    .bind(job_id)
    .fetch_one(&mut *tx)
    .await?;
    let state: JobState = state.parse()?;

    let ours = current_attempt == attempt && holder == Some(agent.id);
    if !ours {
        return Ok(Completion::Stale);
    }
    if state != JobState::Leased {
        // Requeued after an `interrupted` report, the job is no longer
        // held by anyone; a repeat of that report is stale like any other.
        return Ok(if state.is_final() {
            Completion::AlreadyRecorded(state)
        } else {
            Completion::Stale
        });
    }

    let outcome = decide_result(
        &result.status,
        &result.tasks,
        attempt,
        max_attempts,
        cancel_requested,
    );
    let new_state = apply(
        &mut tx,
        job_id,
        run_id,
        &outcome,
        Some(result),
        Some(agent.id),
    )
    .await?;
    tx.commit().await?;
    Ok(Completion::Recorded(new_state))
}

/// Take back every expired lease: retry, fail or cancel each job. Runs
/// every few seconds in the API process; safe to run in several replicas
/// at once. Returns how many jobs it moved.
pub async fn reap(pool: &PgPool) -> anyhow::Result<usize> {
    // Find candidates without locking, then handle each in its own
    // transaction, taking the run lock before the job lock, the same order
    // as `complete`. Locking the job first here and the run first there
    // could deadlock.
    let expired: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, run_id FROM jobs
         WHERE state = 'leased' AND lease_expires_at < now()
         ORDER BY lease_expires_at LIMIT 100",
    )
    .fetch_all(pool)
    .await?;

    let mut reaped = 0;
    for (job_id, run_id) in expired {
        let mut tx = pool.begin().await?;
        lock_run(&mut tx, run_id).await?;
        // Check again under the lock: a heartbeat may have renewed the
        // lease, or another reaper handled it, since we looked.
        let job: Option<(i32, i32, bool)> = sqlx::query_as(
            "SELECT attempt, max_attempts, cancel_requested FROM jobs
             WHERE id = $1 AND state = 'leased' AND lease_expires_at < now()
             FOR UPDATE SKIP LOCKED",
        )
        .bind(job_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((attempt, max_attempts, cancel_requested)) = job else {
            continue; // dropping `tx` rolls it back
        };
        let outcome = decide_expired(attempt, max_attempts, cancel_requested);
        apply(&mut tx, job_id, run_id, &outcome, None, None).await?;
        tx.commit().await?;
        reaped += 1;
    }
    Ok(reaped)
}

/// Cancel a run (DESIGN.md §6): jobs that haven't started are cancelled at
/// once; running jobs are asked to stop through their next heartbeat, and
/// their result (or, if the agent is dead, the reaper) cancels them.
/// Returns false if the run doesn't exist or has already finished.
pub async fn cancel_run(pool: &PgPool, run_id: Uuid) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;
    let cancelled: Option<Uuid> = sqlx::query_scalar(
        "UPDATE runs SET state = 'cancelled' WHERE id = $1 AND state = 'running' RETURNING id",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    if cancelled.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE jobs SET state = 'cancelled', finished_at = now()
         WHERE run_id = $1 AND state IN ('blocked', 'queued')",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE jobs SET cancel_requested = true WHERE run_id = $1 AND state = 'leased'")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    refresh_run(&mut tx, run_id).await?;
    tx.commit().await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Transitions, all inside the caller's transaction with the run locked

async fn lock_run(tx: &mut Transaction<'_, Postgres>, run_id: Uuid) -> anyhow::Result<()> {
    sqlx::query("SELECT id FROM runs WHERE id = $1 FOR UPDATE")
        .bind(run_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(())
}

/// Carry out an outcome for a leased job and everything that follows from
/// it: outputs, the jobs downstream, the run's state.
async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    run_id: Uuid,
    outcome: &Outcome,
    result: Option<&JobResult>,
    agent_id: Option<Uuid>,
) -> anyhow::Result<JobState> {
    let state = match outcome {
        Outcome::Succeeded => {
            finish(tx, job_id, JobState::Succeeded, None, result).await?;
            if let (Some(result), Some(agent_id)) = (result, agent_id) {
                record_outputs(tx, job_id, result, agent_id).await?;
            }
            queue_ready_dependents(tx, job_id).await?;
            JobState::Succeeded
        }
        Outcome::Failed(reason) => {
            finish(tx, job_id, JobState::Failed, Some(reason), result).await?;
            skip_downstream(tx, job_id).await?;
            JobState::Failed
        }
        Outcome::Cancelled => {
            finish(tx, job_id, JobState::Cancelled, None, result).await?;
            skip_downstream(tx, job_id).await?;
            JobState::Cancelled
        }
        Outcome::Retry { after_secs } => {
            let moved = sqlx::query(
                "UPDATE jobs SET state = 'queued', run_at = now() + $2 * interval '1 second',
                                 agent_id = NULL, lease_expires_at = NULL, progress = NULL,
                                 result = COALESCE($3, result)
                 WHERE id = $1 AND state = 'leased'",
            )
            .bind(job_id)
            .bind(after_secs)
            .bind(result.map(Json))
            .execute(&mut **tx)
            .await?;
            ensure_one(moved.rows_affected(), job_id)?;
            JobState::Queued
        }
    };
    refresh_run(tx, run_id).await?;
    Ok(state)
}

async fn finish(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    state: JobState,
    reason: Option<&String>,
    result: Option<&JobResult>,
) -> anyhow::Result<()> {
    let moved = sqlx::query(
        "UPDATE jobs SET state = $2, failure_reason = $3, result = COALESCE($4, result),
                         lease_expires_at = NULL, finished_at = now()
         WHERE id = $1 AND state = 'leased'",
    )
    .bind(job_id)
    .bind(state.as_str())
    .bind(reason)
    .bind(result.map(Json))
    .execute(&mut **tx)
    .await?;
    ensure_one(moved.rows_affected(), job_id)
}

/// The callers hold the run and job locks and checked the state, so a
/// compare-and-set that changes nothing is a bug, not a race.
fn ensure_one(rows: u64, job_id: Uuid) -> anyhow::Result<()> {
    if rows != 1 {
        bail!("job {job_id} changed state under its lock ({rows} rows updated)");
    }
    Ok(())
}

async fn record_outputs(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
    result: &JobResult,
    agent_id: Uuid,
) -> anyhow::Result<()> {
    for output in &result.outputs {
        for (position, file) in output.files.iter().enumerate() {
            sqlx::query(
                "INSERT INTO job_outputs (job_id, name, position, sample, uri, digest, size, agent_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(job_id)
            .bind(&output.name)
            .bind(i32::try_from(position)?)
            .bind(&file.sample)
            .bind(&file.path)
            .bind(file.digest.to_string())
            .bind(i64::try_from(file.size)?)
            .bind(agent_id)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// Queue each job downstream of `job_id` whose dependencies have now all
/// succeeded. Same transaction as the result, so "job done but nothing
/// downstream started" can't happen.
async fn queue_ready_dependents(
    tx: &mut Transaction<'_, Postgres>,
    job_id: Uuid,
) -> anyhow::Result<()> {
    let ready: Vec<Uuid> = sqlx::query_scalar(
        "SELECT j.id FROM jobs j JOIN job_dependencies d ON d.job_id = j.id
         WHERE d.depends_on_job_id = $1 AND j.state = 'blocked'
           AND NOT EXISTS (SELECT 1 FROM job_dependencies d2
                           JOIN jobs up ON up.id = d2.depends_on_job_id
                           WHERE d2.job_id = j.id AND up.state <> 'succeeded')
         ORDER BY j.job_index",
    )
    .bind(job_id)
    .fetch_all(&mut **tx)
    .await?;
    for id in ready {
        queue_blocked_job(tx, id).await?;
    }
    Ok(())
}

/// Resolve a blocked job's `from: job` inputs from the outputs recorded
/// for those jobs, decide where it may run, and queue it.
///
/// Inputs are resolved here, when the job becomes `queued`, not when it is
/// leased: the lease response is then a plain read, the same on every
/// retry.
async fn queue_blocked_job(tx: &mut Transaction<'_, Postgres>, job_id: Uuid) -> anyhow::Result<()> {
    // A type alias names a long tuple type once (clippy's
    // `type_complexity` asks for this).
    type Row = (
        Uuid,
        Json<JobInterface>,
        Json<BTreeMap<String, ResolvedInput>>,
        Option<Uuid>,
    );
    let (run_id, interface, inputs, pinned): Row = sqlx::query_as(
        "SELECT run_id, interface, resolved_inputs, pinned_agent FROM jobs WHERE id = $1",
    )
    .bind(job_id)
    .fetch_one(&mut **tx)
    .await?;
    let mut inputs = inputs.0;

    // Outputs stay on the disk of the agent that made them (P0c), so the
    // job must run where all of its inputs are.
    let mut holders: BTreeSet<Uuid> = pinned.into_iter().collect();

    for input in &interface.inputs {
        let InputSource::Job { job, node, port } = &input.source else {
            continue; // a run input, resolved at launch
        };
        let rows: Vec<(Option<String>, String, String, i64, Uuid)> = sqlx::query_as(
            "SELECT o.sample, o.uri, o.digest, o.size, o.agent_id
             FROM job_outputs o JOIN jobs up ON up.id = o.job_id
             WHERE up.run_id = $1 AND up.job_index = $2 AND o.name = $3
             ORDER BY o.position",
        )
        .bind(run_id)
        .bind(i32::try_from(job.0)?)
        .bind(data_name(node, port))
        .fetch_all(&mut **tx)
        .await?;

        let mut files = Vec::new();
        for (sample, uri, digest, size, agent_id) in rows {
            holders.insert(agent_id);
            files.push(InputFile {
                sample,
                uri,
                digest: digest.parse().map_err(|e: String| anyhow!(e))?,
                size: u64::try_from(size)?,
            });
        }
        inputs.insert(
            input.name.clone(),
            ResolvedInput {
                cardinality: input.cardinality,
                files,
            },
        );
    }

    if holders.len() > 1 {
        // Can't happen with one agent per environment; with several, the
        // fix is uploading outputs to shared storage (DESIGN.md §4.1).
        sqlx::query(
            "UPDATE jobs SET state = 'failed', finished_at = now(),
                             failure_reason = 'its inputs are on different agents, and no agent can reach them all'
             WHERE id = $1 AND state = 'blocked'",
        )
        .bind(job_id)
        .execute(&mut **tx)
        .await?;
        skip_downstream(tx, job_id).await?;
        return Ok(());
    }

    sqlx::query(
        "UPDATE jobs SET state = 'queued', resolved_inputs = $2, pinned_agent = $3, run_at = now()
         WHERE id = $1 AND state = 'blocked'",
    )
    .bind(job_id)
    .bind(Json(&inputs))
    .bind(holders.into_iter().next())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A job that didn't succeed takes every job downstream of it, directly or
/// not, with it. Unrelated branches of the run keep going.
async fn skip_downstream(tx: &mut Transaction<'_, Postgres>, job_id: Uuid) -> anyhow::Result<()> {
    // A recursive query walks the dependency graph inside Postgres.
    sqlx::query(
        "WITH RECURSIVE downstream (id) AS (
             SELECT job_id FROM job_dependencies WHERE depends_on_job_id = $1
             UNION
             SELECT d.job_id FROM job_dependencies d JOIN downstream ON d.depends_on_job_id = downstream.id
         )
         UPDATE jobs SET state = 'skipped', finished_at = now()
         WHERE id IN (SELECT id FROM downstream) AND state = 'blocked'",
    )
    .bind(job_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Bring the run's stored state up to date with its jobs (DESIGN.md §6,
/// "Run state"): `failed` as soon as one job fails, `succeeded` when all
/// have; `finished_at` once no job is left to run. A cancelled run stays
/// cancelled.
async fn refresh_run(tx: &mut Transaction<'_, Postgres>, run_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE runs r SET
             state = CASE
                 WHEN r.state <> 'running' THEN r.state
                 WHEN EXISTS (SELECT 1 FROM jobs WHERE run_id = r.id AND state = 'failed') THEN 'failed'
                 WHEN NOT EXISTS (SELECT 1 FROM jobs WHERE run_id = r.id AND state <> 'succeeded') THEN 'succeeded'
                 ELSE 'running'
             END,
             finished_at = CASE
                 WHEN r.finished_at IS NULL
                      AND NOT EXISTS (SELECT 1 FROM jobs WHERE run_id = r.id
                                      AND state IN ('blocked', 'queued', 'leased'))
                 THEN now()
                 ELSE r.finished_at
             END
         WHERE r.id = $1",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use protocol::types::Cardinality;

    use super::*;
    use crate::testing::*;

    /// `acquire` as an agent would call it, unwrapping the lease.
    async fn lease(pool: &PgPool, agent: &TestAgent) -> LeaseResponse {
        acquire(pool, &agent.agent, "test")
            .await
            .unwrap()
            .expect("a job to lease")
    }

    fn id_of(lease: &LeaseResponse) -> Uuid {
        lease.lease.job_id.parse().unwrap()
    }

    // --- The retry policy, as plain functions ---------------------------

    fn task(name: &str, status: TaskStatus) -> TaskRecord {
        TaskRecord {
            task_id: 1,
            hash: "ab/cdef12".to_string(),
            name: name.to_string(),
            process: "FASTQC".to_string(),
            tag: None,
            status,
            exit_code: None,
            duration_ms: None,
            realtime_ms: None,
        }
    }

    #[test]
    fn failures_are_not_retried() {
        let failed = JobStatus::Failed { exit_code: Some(1) };
        let tasks = [
            task("FASTQC (gut)", TaskStatus::Failed),
            task("FASTQC (liver)", TaskStatus::Completed),
        ];
        assert_eq!(
            decide_result(&failed, &tasks, 1, 3, false),
            Outcome::Failed("task failed: FASTQC (gut)".to_string())
        );
        let Outcome::Failed(reason) = decide_result(&failed, &[], 1, 3, false) else {
            panic!("a Nextflow failure is a failure");
        };
        assert!(reason.contains("no task failed"), "{reason}");
        assert!(matches!(
            decide_result(&JobStatus::TimedOut, &[], 1, 3, false),
            Outcome::Failed(_)
        ));
    }

    #[test]
    fn interruptions_are_retried_until_the_last_attempt() {
        let interrupted = JobStatus::Interrupted;
        assert_eq!(
            decide_result(&interrupted, &[], 2, 3, false),
            Outcome::Retry { after_secs: 0 }
        );
        assert!(matches!(
            decide_result(&interrupted, &[], 3, 3, false),
            Outcome::Failed(_)
        ));
        // Stopping because of a cancel is a cancel, however the agent
        // reports it.
        assert_eq!(
            decide_result(&interrupted, &[], 1, 3, true),
            Outcome::Cancelled
        );
        assert_eq!(
            decide_result(&JobStatus::Cancelled, &[], 1, 3, true),
            Outcome::Cancelled
        );
        assert!(matches!(
            decide_result(&JobStatus::Cancelled, &[], 1, 3, false),
            Outcome::Failed(_)
        ));
    }

    #[test]
    fn lost_leases_back_off() {
        assert_eq!(
            decide_expired(1, 3, false),
            Outcome::Retry { after_secs: 30 }
        );
        assert_eq!(
            decide_expired(2, 3, false),
            Outcome::Retry { after_secs: 120 }
        );
        assert!(matches!(decide_expired(3, 3, false), Outcome::Failed(_)));
        assert_eq!(decide_expired(1, 3, true), Outcome::Cancelled);
    }

    // --- Against Postgres ------------------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    async fn a_lease_describes_the_whole_job(pool: PgPool) {
        let agent = setup(&pool).await;
        let run_id = launch_fastqc_multiqc(&pool).await;

        let lease = lease(&pool, &agent).await;

        assert_eq!(lease.run.run_id, run_id.to_string());
        assert_eq!(lease.run.job_index, 0);
        assert_eq!(lease.lease.attempt, 1);
        assert_eq!(lease.lease.heartbeat_every_secs, 20);
        assert!(
            lease.lease.expires_at.ends_with('Z'),
            "{}",
            lease.lease.expires_at
        );
        assert_eq!(lease.settings.nextflow_version, "26.10.5");
        assert_eq!(lease.settings.time_limit_secs, Some(3600));
        assert_eq!(lease.inputs, resolved_reads());
        assert_eq!(lease.interface.inputs[0].name, "fastqc_1__reads");
        let stored: i64 = sqlx::query_scalar("SELECT size FROM blobs WHERE digest = $1")
            .bind(lease.project.digest.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(lease.project.size, stored as u64);

        // MultiQC waits for FastQC, so there is nothing else to lease.
        assert_eq!(job_state(&pool, run_id, 1).await, "blocked");
        assert!(
            acquire(&pool, &agent.agent, "test")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_agents_never_get_the_same_job(pool: PgPool) {
        setup(&pool).await;
        let tree = store_project(&pool, "many").await;
        launch(&pool, (0..20).map(|_| fastqc_job(&tree)).collect()).await;

        // 30 agents poll at once for 20 jobs. `tokio::spawn` runs each on
        // the runtime's thread pool, so they really race in Postgres.
        let mut handles = Vec::new();
        for n in 0..30 {
            let pool = pool.clone();
            let agent = create_agent(&pool, ENV, &format!("agent-{n}")).await;
            handles.push(tokio::spawn(async move {
                acquire(&pool, &agent.agent, "test").await.unwrap()
            }));
        }
        let mut leased = BTreeSet::new();
        for handle in handles {
            if let Some(lease) = handle.await.unwrap() {
                assert!(leased.insert(lease.lease.job_id.clone()), "leased twice");
            }
        }
        assert_eq!(leased.len(), 20);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn heartbeats_are_fenced_by_agent_and_attempt(pool: PgPool) {
        let agent = setup(&pool).await;
        let other = create_agent(&pool, ENV, "other").await;
        launch_fastqc_multiqc(&pool).await;
        let lease = lease(&pool, &agent).await;
        let job = id_of(&lease);

        assert_eq!(
            heartbeat(&pool, &agent.agent, job, 1, Some("1 of 2 tasks"))
                .await
                .unwrap(),
            Some(HeartbeatAction::Continue)
        );
        assert_eq!(
            heartbeat(&pool, &agent.agent, job, 2, None).await.unwrap(),
            None
        );
        assert_eq!(
            heartbeat(&pool, &other.agent, job, 1, None).await.unwrap(),
            None
        );

        let progress: Option<String> =
            sqlx::query_scalar("SELECT progress FROM jobs WHERE id = $1")
                .bind(job)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(progress.as_deref(), Some("1 of 2 tasks"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn success_queues_the_next_job_with_its_inputs(pool: PgPool) {
        let agent = setup(&pool).await;
        let run_id = launch_fastqc_multiqc(&pool).await;
        let fastqc = lease(&pool, &agent).await;

        let done = complete(
            &pool,
            &agent.agent,
            id_of(&fastqc),
            1,
            &fastqc_succeeded("fastqc_1__zip"),
        )
        .await
        .unwrap();
        assert_eq!(done, Completion::Recorded(JobState::Succeeded));
        assert_eq!(job_state(&pool, run_id, 1).await, "queued");
        assert_eq!(
            run_state(&pool, run_id).await,
            ("running".to_string(), false)
        );

        // MultiQC's input is FastQC's output, and it must run where that is.
        let multiqc = lease(&pool, &agent).await;
        assert_eq!(multiqc.run.job_index, 1);
        let zip = &multiqc.inputs["fastqc_1__zip"];
        assert_eq!(zip.cardinality, Cardinality::PerSample);
        assert_eq!(zip.files.len(), 1);
        assert_eq!(
            zip.files[0].uri,
            "/runs/r1/results/fastqc_1__zip/gut/gut_fastqc.zip"
        );
        let pinned: Option<Uuid> =
            sqlx::query_scalar("SELECT pinned_agent FROM jobs WHERE id = $1")
                .bind(id_of(&multiqc))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pinned, Some(agent.agent.id));

        complete(
            &pool,
            &agent.agent,
            id_of(&multiqc),
            1,
            &result(JobStatus::Succeeded),
        )
        .await
        .unwrap();
        assert_eq!(
            run_state(&pool, run_id).await,
            ("succeeded".to_string(), true)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_pinned_job_waits_for_its_agent(pool: PgPool) {
        let agent = setup(&pool).await;
        let other = create_agent(&pool, ENV, "other").await;
        launch_fastqc_multiqc(&pool).await;
        let fastqc = lease(&pool, &agent).await;
        complete(
            &pool,
            &agent.agent,
            id_of(&fastqc),
            1,
            &fastqc_succeeded("fastqc_1__zip"),
        )
        .await
        .unwrap();

        assert!(
            acquire(&pool, &other.agent, "test")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(lease(&pool, &agent).await.run.job_index, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_failure_skips_everything_downstream(pool: PgPool) {
        let agent = setup(&pool).await;
        let tree = store_project(&pool, "branches").await;
        // Two branches: 0 → 2, and 1 on its own.
        let run_id = launch(
            &pool,
            vec![
                fastqc_job(&tree),
                fastqc_job(&tree),
                multiqc_job(&tree, &[0]),
            ],
        )
        .await;
        let first = lease(&pool, &agent).await;
        let second = lease(&pool, &agent).await;

        let mut failed = result(JobStatus::Failed { exit_code: Some(1) });
        failed.tasks = vec![task("FASTQC (gut)", TaskStatus::Failed)];
        complete(&pool, &agent.agent, id_of(&first), 1, &failed)
            .await
            .unwrap();

        assert_eq!(job_state(&pool, run_id, 0).await, "failed");
        assert_eq!(job_state(&pool, run_id, 2).await, "skipped");
        // The run has failed, but the unrelated branch keeps running.
        assert_eq!(job_state(&pool, run_id, 1).await, "leased");
        assert_eq!(
            run_state(&pool, run_id).await,
            ("failed".to_string(), false)
        );

        complete(
            &pool,
            &agent.agent,
            id_of(&second),
            1,
            &fastqc_succeeded("fastqc_2__zip"),
        )
        .await
        .unwrap();
        assert_eq!(run_state(&pool, run_id).await, ("failed".to_string(), true));

        let reason: String = sqlx::query_scalar("SELECT failure_reason FROM jobs WHERE id = $1")
            .bind(id_of(&first))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(reason, "task failed: FASTQC (gut)");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_interrupted_job_is_retried_then_fails(pool: PgPool) {
        let agent = setup(&pool).await;
        let run_id = launch_fastqc_multiqc(&pool).await;

        for attempt in 1..=3 {
            let lease = lease(&pool, &agent).await;
            assert_eq!(lease.lease.attempt, attempt);
            complete(
                &pool,
                &agent.agent,
                id_of(&lease),
                attempt,
                &result(JobStatus::Interrupted),
            )
            .await
            .unwrap();
        }
        assert_eq!(job_state(&pool, run_id, 0).await, "failed");
        assert_eq!(job_state(&pool, run_id, 1).await, "skipped");
        assert_eq!(run_state(&pool, run_id).await, ("failed".to_string(), true));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_result_can_be_sent_twice_but_not_for_an_old_attempt(pool: PgPool) {
        let agent = setup(&pool).await;
        launch_fastqc_multiqc(&pool).await;
        let lease = lease(&pool, &agent).await;
        let job = id_of(&lease);
        let report = fastqc_succeeded("fastqc_1__zip");

        complete(&pool, &agent.agent, job, 1, &report)
            .await
            .unwrap();
        // The reply was lost and the agent sends again: accepted, and the
        // outputs aren't recorded twice.
        assert_eq!(
            complete(&pool, &agent.agent, job, 1, &report)
                .await
                .unwrap(),
            Completion::AlreadyRecorded(JobState::Succeeded)
        );
        let outputs: i64 = sqlx::query_scalar("SELECT count(*) FROM job_outputs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(outputs, 1);

        assert_eq!(
            complete(&pool, &agent.agent, job, 2, &report)
                .await
                .unwrap(),
            Completion::Stale
        );
        assert_eq!(
            complete(&pool, &agent.agent, Uuid::new_v4(), 1, &report)
                .await
                .unwrap(),
            Completion::Stale
        );
    }

    /// The database itself refuses duplicate outputs, not only
    /// `complete`'s attempt check.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_output_file_is_recorded_once(pool: PgPool) {
        let agent = setup(&pool).await;
        launch_fastqc_multiqc(&pool).await;
        let job = id_of(&lease(&pool, &agent).await);
        complete(
            &pool,
            &agent.agent,
            job,
            1,
            &fastqc_succeeded("fastqc_1__zip"),
        )
        .await
        .unwrap();

        let duplicate = sqlx::query(
            "INSERT INTO job_outputs (job_id, name, position, sample, uri, digest, size, agent_id)
             SELECT job_id, name, position, sample, uri, digest, size, agent_id FROM job_outputs",
        )
        .execute(&pool)
        .await;
        assert!(
            duplicate.is_err(),
            "a second copy of an output row was accepted"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_reaper_takes_back_expired_leases(pool: PgPool) {
        let agent = setup(&pool).await;
        let other = create_agent(&pool, ENV, "other").await;
        let run_id = launch_fastqc_multiqc(&pool).await;
        let first = lease(&pool, &agent).await;
        let job = id_of(&first);

        // A live lease is left alone.
        assert_eq!(reap(&pool).await.unwrap(), 0);

        // The agent goes silent (crash, partition).
        expire_lease(&pool, job).await;
        assert_eq!(reap(&pool).await.unwrap(), 1);
        assert_eq!(job_state(&pool, run_id, 0).await, "queued");
        // Backoff: not available again straight away.
        assert!(
            acquire(&pool, &other.agent, "test")
                .await
                .unwrap()
                .is_none()
        );

        skip_backoff(&pool, job).await;
        let second = lease(&pool, &other).await;
        assert_eq!(second.lease.attempt, 2);

        // The first agent comes back from its partition: fenced off.
        assert_eq!(
            heartbeat(&pool, &agent.agent, job, 1, None).await.unwrap(),
            None
        );
        assert_eq!(
            complete(
                &pool,
                &agent.agent,
                job,
                1,
                &fastqc_succeeded("fastqc_1__zip")
            )
            .await
            .unwrap(),
            Completion::Stale
        );

        // Lost twice more: the job fails rather than taking more agents down.
        expire_lease(&pool, job).await;
        reap(&pool).await.unwrap();
        skip_backoff(&pool, job).await;
        lease(&pool, &other).await;
        expire_lease(&pool, job).await;
        reap(&pool).await.unwrap();

        assert_eq!(job_state(&pool, run_id, 0).await, "failed");
        assert_eq!(job_state(&pool, run_id, 1).await, "skipped");
        assert_eq!(run_state(&pool, run_id).await, ("failed".to_string(), true));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn cancel_stops_waiting_jobs_and_asks_running_ones(pool: PgPool) {
        let agent = setup(&pool).await;
        let run_id = launch_fastqc_multiqc(&pool).await;
        let lease = lease(&pool, &agent).await;
        let job = id_of(&lease);

        assert!(cancel_run(&pool, run_id).await.unwrap());
        assert_eq!(job_state(&pool, run_id, 1).await, "cancelled");
        assert_eq!(job_state(&pool, run_id, 0).await, "leased");
        // Not finished yet: a job is still running.
        assert_eq!(
            run_state(&pool, run_id).await,
            ("cancelled".to_string(), false)
        );

        assert_eq!(
            heartbeat(&pool, &agent.agent, job, 1, None).await.unwrap(),
            Some(HeartbeatAction::Cancel)
        );
        complete(&pool, &agent.agent, job, 1, &result(JobStatus::Cancelled))
            .await
            .unwrap();
        assert_eq!(job_state(&pool, run_id, 0).await, "cancelled");
        assert_eq!(
            run_state(&pool, run_id).await,
            ("cancelled".to_string(), true)
        );

        // Cancelling a finished run does nothing.
        assert!(!cancel_run(&pool, run_id).await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_cancelled_job_whose_agent_died_is_cancelled_by_the_reaper(pool: PgPool) {
        let agent = setup(&pool).await;
        let run_id = launch_fastqc_multiqc(&pool).await;
        let job = id_of(&lease(&pool, &agent).await);

        cancel_run(&pool, run_id).await.unwrap();
        expire_lease(&pool, job).await;
        reap(&pool).await.unwrap();

        assert_eq!(job_state(&pool, run_id, 0).await, "cancelled");
        assert_eq!(
            run_state(&pool, run_id).await,
            ("cancelled".to_string(), true)
        );
    }

    /// Two jobs that a third needs both finish at the same moment. Each
    /// transaction must see the other's result, or neither queues the
    /// third and the run hangs. The run lock makes them take turns.
    #[sqlx::test(migrations = "./migrations")]
    async fn simultaneous_results_still_queue_the_job_that_needs_both(pool: PgPool) {
        let agent = setup(&pool).await;
        let tree = store_project(&pool, "join").await;
        for _ in 0..10 {
            let run_id = launch(
                &pool,
                vec![
                    fastqc_job(&tree),
                    fastqc_job(&tree),
                    multiqc_job(&tree, &[0, 1]),
                ],
            )
            .await;
            let first = lease(&pool, &agent).await;
            let second = lease(&pool, &agent).await;

            // `let` keeps each result alive across the `.await`; a
            // temporary would be dropped at the end of its statement while
            // the future still borrows it.
            let (report_1, report_2) = (
                fastqc_succeeded("fastqc_1__zip"),
                fastqc_succeeded("fastqc_2__zip"),
            );
            let (a, b) = tokio::join!(
                complete(&pool, &agent.agent, id_of(&first), 1, &report_1),
                complete(&pool, &agent.agent, id_of(&second), 1, &report_2),
            );
            a.unwrap();
            b.unwrap();
            assert_eq!(job_state(&pool, run_id, 2).await, "queued");

            let join = lease(&pool, &agent).await;
            assert_eq!(join.inputs.len(), 2, "both upstream outputs resolved");
            complete(
                &pool,
                &agent.agent,
                id_of(&join),
                1,
                &result(JobStatus::Succeeded),
            )
            .await
            .unwrap();
            assert_eq!(
                run_state(&pool, run_id).await,
                ("succeeded".to_string(), true)
            );
        }
    }
}
