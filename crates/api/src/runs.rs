//! Launching, watching and cancelling runs (DESIGN.md §4.3, §6):
//!
//! ```text
//! POST /api/runs             { pipeline_version_id, compute_env, inputs }  → 201 { run_id }
//! GET  /api/runs/{id}        → the run, its jobs, their tasks and outputs
//! POST /api/runs/{id}/cancel → 200, or 409 if the run already finished
//! ```
//!
//! Launching is where everything meets: the compiler (validate, plan,
//! generate each job's Nextflow project), blob storage (each project as a
//! tree), and the queue (`queue::enqueue_run`). All of it happens in one
//! database transaction, so a launch either queues the whole run or leaves
//! no trace. Compiling takes milliseconds and does no I/O, so it runs
//! inside the request.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use compiler::canvas::Canvas;
use compiler::execution_plan::{ExecutionPlan, JobId, JobKind as PlanJobKind};
use protocol::blob::BlobRef;
use protocol::digest::{Digest, HashAlgorithm};
use protocol::job::{InputSource, JobInput};
use protocol::lease::{InputFile, ResolvedInput};
use protocol::result::{JobResult, TaskStatus};
use protocol::types::{Cardinality, SemanticType};
use serde::{Deserialize, Serialize};
use sqlx::types::Json as JsonColumn;
use uuid::Uuid;

use crate::AppState;
use crate::blobs::store_tree;
use crate::error::ApiError;
use crate::pipelines;
use crate::queue::{self, JobKind, NewJob, NewRun};
use crate::users::User;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/runs", post(launch))
        .route("/api/runs/{id}", get(status))
        .route("/api/runs/{id}/cancel", post(cancel))
}

// ---------------------------------------------------------------------------
// Launch

#[derive(Debug, Deserialize)]
struct LaunchRequest {
    pipeline_version_id: Uuid,
    compute_env: String,
    /// Each run input (`fastqc_1__reads`) → the dataset that feeds it.
    #[serde(default)]
    inputs: BTreeMap<String, Uuid>,
}

#[derive(Serialize)]
struct Launched {
    run_id: Uuid,
}

/// Something wrong with a launch request, as the caller sees it. Typed
/// like `ValidationError`, so the launch form can point at the input
/// concerned. Internally tagged (`code` beside the fields), with a
/// `message` added when sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
enum LaunchProblem {
    #[error("compute environment `{compute_env}` does not exist")]
    UnknownComputeEnv { compute_env: String },

    #[error("node `{node}` is an analysis node, and analysis jobs aren't supported yet")]
    AnalysisNotSupported { node: String },

    #[error("input `{input}` needs a dataset")]
    MissingInput { input: String },

    #[error("`{input}` is not an input of this pipeline")]
    UnknownInput { input: String },

    #[error("dataset {dataset} (for input `{input}`) does not exist")]
    UnknownDataset { input: String, dataset: Uuid },

    #[error(
        "dataset {dataset} (for input `{input}`) is on an agent of compute environment \
         `{dataset_env}`, not `{compute_env}`"
    )]
    DatasetElsewhere {
        input: String,
        dataset: Uuid,
        dataset_env: String,
        compute_env: String,
    },

    #[error(
        "input `{input}` takes {expected} file(s) per sample ({ty}), but sample `{sample}` \
         has {found}"
    )]
    WrongFileCount {
        input: String,
        sample: String,
        ty: String,
        expected: usize,
        found: usize,
    },

    #[error(
        "job {job}'s datasets are on different agents, and no agent can reach them all; \
         register them on one agent"
    )]
    InputsOnDifferentAgents { job: usize },
}

fn invalid_launch(problems: &[LaunchProblem]) -> ApiError {
    let details: Vec<serde_json::Value> = problems
        .iter()
        .map(|problem| {
            let mut detail = serde_json::to_value(problem).expect("launch problems serialize");
            detail["message"] = problem.to_string().into();
            detail
        })
        .collect();
    let count = problems.len();
    ApiError::unprocessable(
        "invalid_launch",
        format!(
            "the run can't be launched: {count} problem{}",
            if count == 1 { "" } else { "s" }
        ),
        details.into(),
    )
}

/// A dataset as launch needs it.
struct Dataset {
    agent_id: Uuid,
    compute_env: String,
    /// In sample order, then each sample's files in order.
    files: Vec<(String, InputFile)>,
}

async fn launch(
    State(state): State<AppState>,
    _user: User,
    body: Result<Json<LaunchRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Launched>), ApiError> {
    let Json(request) = body?;

    // 1. The pipeline version's canvas. Immutable, so whatever validated
    //    when it was saved is what runs.
    let canvas: Option<JsonColumn<Canvas>> =
        sqlx::query_scalar("SELECT canvas FROM pipeline_versions WHERE id = $1")
            .bind(request.pipeline_version_id)
            .fetch_optional(&state.pool)
            .await?;
    let JsonColumn(canvas) = canvas.ok_or_else(|| ApiError::not_found("pipeline version"))?;

    // 2. Validate again: cheap, and the stored canvas stays honest even if
    //    the rules tighten later.
    let pipeline = pipelines::check(&canvas, &state.registry)?;

    // 3. Plan. Analysis jobs have no agent able to run them yet, so they
    //    are refused here rather than queued forever.
    let plan = ExecutionPlan::build(pipeline);
    let graph = plan.pipeline().graph();
    let mut problems = Vec::new();
    for job in plan.jobs() {
        if let PlanJobKind::Analysis { node } = &job.kind {
            problems.push(LaunchProblem::AnalysisNotSupported {
                node: graph[*node].id.clone(),
            });
        }
    }

    let algorithm: Option<String> =
        sqlx::query_scalar("SELECT hash_algorithm FROM compute_envs WHERE id = $1")
            .bind(&request.compute_env)
            .fetch_optional(&state.pool)
            .await?;
    let algorithm = match algorithm {
        Some(name) => Some(name.parse::<HashAlgorithm>().map_err(anyhow::Error::msg)?),
        None => {
            problems.push(LaunchProblem::UnknownComputeEnv {
                compute_env: request.compute_env.clone(),
            });
            None
        }
    };

    // 4. Match the request's inputs to the plan's run inputs: the input
    //    ports nothing on the canvas feeds.
    let interfaces: Vec<_> = (0..plan.jobs().len())
        .map(|index| plan.interface(JobId(index)))
        .collect();
    let run_inputs: BTreeMap<&str, &JobInput> = interfaces
        .iter()
        .flat_map(|interface| &interface.inputs)
        .filter(|input| matches!(input.source, InputSource::Run { .. }))
        .map(|input| (input.name.as_str(), input))
        .collect();
    for input in run_inputs.keys() {
        if !request.inputs.contains_key(*input) {
            problems.push(LaunchProblem::MissingInput {
                input: input.to_string(),
            });
        }
    }
    for input in request.inputs.keys() {
        if !run_inputs.contains_key(input.as_str()) {
            problems.push(LaunchProblem::UnknownInput {
                input: input.clone(),
            });
        }
    }

    // 5. Resolve each input's dataset to files.
    let mut resolved: BTreeMap<String, (ResolvedInput, Uuid)> = BTreeMap::new();
    for (name, &dataset_id) in &request.inputs {
        let Some(input) = run_inputs.get(name.as_str()) else {
            continue; // reported above
        };
        let Some(dataset) = load_dataset(&state, dataset_id).await? else {
            problems.push(LaunchProblem::UnknownDataset {
                input: name.clone(),
                dataset: dataset_id,
            });
            continue;
        };
        if dataset.compute_env != request.compute_env && algorithm.is_some() {
            problems.push(LaunchProblem::DatasetElsewhere {
                input: name.clone(),
                dataset: dataset_id,
                dataset_env: dataset.compute_env.clone(),
                compute_env: request.compute_env.clone(),
            });
        }
        problems.extend(check_file_counts(name, input, &dataset));
        resolved.insert(
            name.clone(),
            (resolve(input.cardinality, dataset.files), dataset.agent_id),
        );
    }

    // 6. Each job runs where its datasets are (outputs and inputs stay on
    //    agents' disks in P0c, DESIGN.md §4.1): its `pinned_agent`.
    let mut jobs_inputs = Vec::new();
    for (index, interface) in interfaces.iter().enumerate() {
        let mut inputs = BTreeMap::new();
        let mut agents = BTreeSet::new();
        for input in &interface.inputs {
            if let Some((files, agent)) = resolved.get(&input.name) {
                inputs.insert(input.name.clone(), files.clone());
                agents.insert(*agent);
            }
        }
        if agents.len() > 1 {
            problems.push(LaunchProblem::InputsOnDifferentAgents { job: index });
        }
        jobs_inputs.push((inputs, agents.into_iter().next()));
    }

    if !problems.is_empty() {
        return Err(invalid_launch(&problems));
    }
    let algorithm = algorithm.expect("a missing compute environment is a problem above");

    // 7. One transaction: every job's project (blobs and tree), the run,
    //    its jobs and their dependencies, and which dataset fed which
    //    input. Module blobs are usually stored already (by the node
    //    library at startup, or an earlier launch); storing skips them.
    let mut tx = state.pool.begin().await?;
    let mut jobs = Vec::new();
    for (index, (job, (run_inputs, pinned_agent))) in
        plan.jobs().iter().zip(jobs_inputs).enumerate()
    {
        let project = plan.nextflow_project(JobId(index), &state.registry)?;
        // `into_bytes` turns each `String` into its UTF-8 bytes without
        // copying: it takes ownership, so `project` is used up here.
        let files = project
            .files
            .into_iter()
            .map(|(path, text)| (path, text.into_bytes()))
            .collect();
        let tree: BlobRef = store_tree(&mut tx, algorithm, files).await?;
        jobs.push(NewJob {
            kind: JobKind::Batch,
            project_tree: tree,
            interface: interfaces[index].clone(),
            run_inputs,
            depends_on: job.depends_on.iter().map(|id| id.0).collect(),
            pinned_agent,
        });
    }
    let run_id = queue::enqueue_run(
        &mut tx,
        &NewRun {
            compute_env: request.compute_env.clone(),
            pipeline_version_id: Some(request.pipeline_version_id),
            jobs,
        },
    )
    .await?;
    for (name, dataset_id) in &request.inputs {
        sqlx::query("INSERT INTO run_inputs (run_id, name, dataset_id) VALUES ($1, $2, $3)")
            .bind(run_id)
            .bind(name)
            .bind(dataset_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(Launched { run_id })))
}

async fn load_dataset(state: &AppState, dataset_id: Uuid) -> Result<Option<Dataset>, ApiError> {
    let found: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT d.agent_id, a.compute_env FROM datasets d JOIN agents a ON a.id = d.agent_id
         WHERE d.id = $1",
    )
    .bind(dataset_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((agent_id, compute_env)) = found else {
        return Ok(None);
    };
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT sample, uri, digest, size FROM dataset_files WHERE dataset_id = $1
         ORDER BY sample, position",
    )
    .bind(dataset_id)
    .fetch_all(&state.pool)
    .await?;
    let mut files = Vec::new();
    for (sample, uri, digest, size) in rows {
        let file = InputFile {
            sample: Some(sample.clone()),
            uri,
            digest: digest.parse::<Digest>().map_err(anyhow::Error::msg)?,
            size: u64::try_from(size).map_err(anyhow::Error::from)?,
        };
        files.push((sample, file));
    }
    Ok(Some(Dataset {
        agent_id,
        compute_env,
        files,
    }))
}

/// Paired reads need exactly two files per sample (R1, R2), single-end
/// reads one. Caught here, at launch, the run request is what's wrong;
/// caught on the agent, it would be a failed job hours later.
fn check_file_counts(name: &str, input: &JobInput, dataset: &Dataset) -> Vec<LaunchProblem> {
    let (SemanticType::Fastq { paired }, Cardinality::PerSample) = (&input.ty, input.cardinality)
    else {
        return Vec::new();
    };
    let expected = if *paired { 2 } else { 1 };
    let mut per_sample: BTreeMap<&str, usize> = BTreeMap::new();
    for (sample, _) in &dataset.files {
        *per_sample.entry(sample).or_default() += 1;
    }
    per_sample
        .into_iter()
        .filter(|&(_, found)| found != expected)
        .map(|(sample, found)| LaunchProblem::WrongFileCount {
            input: name.to_string(),
            sample: sample.to_string(),
            ty: if *paired {
                "paired reads"
            } else {
                "single-end reads"
            }
            .to_string(),
            expected,
            found,
        })
        .collect()
}

/// A dataset's files as one job input: per sample they keep their sample
/// names; a per-dataset input takes every file as one collection.
fn resolve(cardinality: Cardinality, files: Vec<(String, InputFile)>) -> ResolvedInput {
    let files = files
        .into_iter()
        .map(|(_, mut file)| {
            if cardinality == Cardinality::PerDataset {
                file.sample = None;
            }
            file
        })
        .collect();
    ResolvedInput { cardinality, files }
}

// ---------------------------------------------------------------------------
// Status

#[derive(Debug, Serialize)]
struct RunView {
    run_id: Uuid,
    /// `running`, `succeeded`, `failed` or `cancelled` (DESIGN.md §6).
    state: String,
    created_at: String,
    finished_at: Option<String>,
    pipeline_version_id: Option<Uuid>,
    compute_env: Option<String>,
    /// Run input → the dataset that fed it.
    inputs: BTreeMap<String, Uuid>,
    jobs: Vec<JobView>,
}

#[derive(Debug, Serialize)]
struct JobView {
    index: i32,
    state: String,
    attempt: i32,
    /// The agent holding or last holding the job.
    agent_id: Option<Uuid>,
    /// A cancel is on its way to the agent (the job is stopping).
    cancel_requested: bool,
    progress: Option<String>,
    failure_reason: Option<String>,
    tasks: Vec<TaskView>,
    warnings: Vec<String>,
    outputs: Vec<OutputView>,
}

/// A task as the run monitor shows it: the essentials of a `TaskRecord`.
#[derive(Debug, Serialize)]
struct TaskView {
    name: String,
    status: TaskStatus,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct OutputView {
    name: String,
    sample: Option<String>,
    uri: String,
    digest: String,
    size: i64,
    /// The agent whose disk holds the file.
    agent_id: Uuid,
}

/// The database formats timestamps, so every one in the API looks the same
/// (RFC 3339, UTC, milliseconds), without a date-time library here.
macro_rules! rfc3339 {
    ($column:literal) => {
        concat!(
            "to_char(",
            $column,
            " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')"
        )
    };
}

async fn status(
    State(state): State<AppState>,
    _user: User,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<RunView>, ApiError> {
    let Path(run_id) = id?;
    let pool = &state.pool;

    // `concat!` builds the query at compile time, so it is still a
    // `&'static str`, as sqlx requires.
    let run: Option<(String, String, Option<String>, Option<Uuid>)> = sqlx::query_as(concat!(
        "SELECT state, ",
        rfc3339!("created_at"),
        ", ",
        rfc3339!("finished_at"),
        ", pipeline_version_id FROM runs WHERE id = $1"
    ))
    .bind(run_id)
    .fetch_optional(pool)
    .await?;
    let (run_state, created_at, finished_at, pipeline_version_id) =
        run.ok_or_else(|| ApiError::not_found("run"))?;

    let inputs: Vec<(String, Uuid)> =
        sqlx::query_as("SELECT name, dataset_id FROM run_inputs WHERE run_id = $1")
            .bind(run_id)
            .fetch_all(pool)
            .await?;

    type JobRow = (
        Uuid,
        i32,
        String,
        i32,
        Option<Uuid>,
        bool,
        Option<String>,
        Option<String>,
        Option<JsonColumn<JobResult>>,
        String,
    );
    let rows: Vec<JobRow> = sqlx::query_as(
        "SELECT id, job_index, state, attempt, agent_id, cancel_requested, progress,
                failure_reason, result, compute_env
         FROM jobs WHERE run_id = $1 ORDER BY job_index",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;

    type OutputRow = (Uuid, String, Option<String>, String, String, i64, Uuid);
    let output_rows: Vec<OutputRow> = sqlx::query_as(
        "SELECT o.job_id, o.name, o.sample, o.uri, o.digest, o.size, o.agent_id
         FROM job_outputs o JOIN jobs j ON j.id = o.job_id
         WHERE j.run_id = $1 ORDER BY j.job_index, o.name, o.position",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    let mut outputs: BTreeMap<Uuid, Vec<OutputView>> = BTreeMap::new();
    for (job_id, name, sample, uri, digest, size, agent_id) in output_rows {
        outputs.entry(job_id).or_default().push(OutputView {
            name,
            sample,
            uri,
            digest,
            size,
            agent_id,
        });
    }

    let compute_env = rows.first().map(|row| row.9.clone());
    let jobs = rows
        .into_iter()
        .map(
            |(
                id,
                index,
                state,
                attempt,
                agent_id,
                cancel_requested,
                progress,
                failure,
                result,
                _,
            )| {
                // A result is stored once the job has reported one; a retry
                // keeps the last attempt's, so its tasks stay visible.
                let (tasks, warnings) = match result {
                    Some(JsonColumn(result)) => (
                        result
                            .tasks
                            .into_iter()
                            .map(|task| TaskView {
                                name: task.name,
                                status: task.status,
                                exit_code: task.exit_code,
                                duration_ms: task.duration_ms,
                            })
                            .collect(),
                        result.warnings,
                    ),
                    None => (Vec::new(), Vec::new()),
                };
                JobView {
                    index,
                    state,
                    attempt,
                    agent_id,
                    cancel_requested,
                    progress,
                    failure_reason: failure,
                    tasks,
                    warnings,
                    // `remove` takes the job's outputs out of the map, so
                    // they move into the view instead of being copied.
                    outputs: outputs.remove(&id).unwrap_or_default(),
                }
            },
        )
        .collect();

    Ok(Json(RunView {
        run_id,
        state: run_state,
        created_at,
        finished_at,
        pipeline_version_id,
        compute_env,
        inputs: inputs.into_iter().collect(),
        jobs,
    }))
}

// ---------------------------------------------------------------------------
// Cancel

#[derive(Serialize)]
struct Cancelled {
    run_id: Uuid,
    state: &'static str,
}

/// Jobs that haven't started are cancelled at once; running ones are told
/// through their next heartbeat (DESIGN.md §6), so the run is `cancelled`
/// straight away while its jobs may take a little longer to stop.
async fn cancel(
    State(state): State<AppState>,
    _user: User,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Cancelled>, ApiError> {
    let Path(run_id) = id?;
    if queue::cancel_run(&state.pool, run_id).await? {
        return Ok(Json(Cancelled {
            run_id,
            state: "cancelled",
        }));
    }
    // Not cancelled: either there's no such run, or it already finished.
    let run_state: Option<String> = sqlx::query_scalar("SELECT state FROM runs WHERE id = $1")
        .bind(run_id)
        .fetch_optional(&state.pool)
        .await?;
    match run_state {
        None => Err(ApiError::not_found("run")),
        Some(run_state) => Err(ApiError::client(
            StatusCode::CONFLICT,
            "run_finished",
            format!("the run has already finished ({run_state}), so there is nothing to cancel"),
        )),
    }
}

#[cfg(test)]
mod tests;
