//! `agent serve`: lease jobs from the control plane, run them, report
//! (DESIGN.md §4.1, agent protocol; §6, the queue).
//!
//! ```text
//! loop:
//!   lease ── 204 ──► sleep (1 s, doubling to 30 s) ──► lease again
//!     │
//!     ▼ a job
//!   start heartbeats (a separate task, every heartbeat_every_secs)
//!   prepare: fetch the project by digest, write jobs/<id>/<attempt>/
//!   stage → run Nextflow → collect      (the same code as `agent run`)
//!   post the result (retried until accepted or stale)
//!   stop heartbeats
//! ```
//!
//! While Nextflow runs, five things can stop it, each a branch of the
//! `stop` future handed to `run::supervise`: the job's time limit, a
//! heartbeat answered "cancel" (the run was cancelled), a heartbeat
//! answered 409 (the lease is lost: stop and discard, report nothing), and
//! Ctrl-C or SIGTERM to the agent (shut down; the job is reported
//! `interrupted` so the queue retries it elsewhere). A second Ctrl-C or
//! SIGTERM kills Nextflow at once.
//!
//! Heartbeats keep going after "cancel" and while the result is posted, so
//! the lease doesn't run out while Nextflow is shutting down or the network
//! is slow.

use std::path::PathBuf;
use std::time::Duration;

use protocol::lease::{HeartbeatAction, HeartbeatRequest, LeaseResponse, ResultReport};
use protocol::result::{JobResult, JobStatus};
use protocol::version::FormatVersion;
use tokio::sync::watch;

use crate::blobs::BlobCache;
use crate::client::{Client, HeartbeatOutcome, ResultOutcome};
use crate::job_dir::{self, PrepareError};
use crate::run::{self, StopReason};
use crate::stage;

/// Polling backs off from this…
const POLL_MIN: Duration = Duration::from_secs(1);
/// …to this, while there is no work or the control plane can't be reached.
const POLL_MAX: Duration = Duration::from_secs(30);
/// Reporting a result is retried up to this many times (about five minutes
/// with the backoff): long enough to ride out a restart of the control
/// plane, short enough that a shutting-down agent doesn't hang forever.
const RESULT_TRIES: u32 = 12;

pub struct Config {
    /// Where each attempt gets its directory (`jobs/<job_id>/<attempt>/`).
    pub work_dir: PathBuf,
    /// Stub runs: modules' `stub:` blocks, no Docker (CI's end-to-end test).
    pub stub: bool,
    /// `nextflow`, except in tests.
    pub nextflow: PathBuf,
    /// How long Nextflow gets to stop cleanly.
    pub grace: Duration,
}

/// Counts Ctrl-C/SIGTERM signals: 1 means "shut down", 2 means "now".
///
/// A `watch` channel holds one value that a sender updates and any number
/// of receivers can wait on. Each part of the loop that must notice a
/// signal holds its own clone of the receiver.
pub type Signals = watch::Receiver<u32>;

/// Listen for Ctrl-C and SIGTERM for the rest of the process's life,
/// counting them. Both mean "shut down": a sysadmin's Ctrl-C and
/// systemd's SIGTERM ask the same thing of a service.
pub fn listen_for_signals() -> anyhow::Result<Signals> {
    use tokio::signal::unix::{SignalKind, signal};

    let (count, signals) = watch::channel(0u32);
    // Registered here, before returning, so no signal can slip past
    // between now and the task's first poll.
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            count.send_modify(|count| *count += 1);
            if *count.borrow() == 1 {
                eprintln!(
                    "stopping: a running job is stopped and reported for retry; \
                     Ctrl-C again to kill it at once"
                );
            }
        }
    });
    Ok(signals)
}

/// What the heartbeat task tells the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Continue,
    Cancel,
    LeaseLost,
}

/// Serve until the first signal. Errors talking to the control plane are
/// logged and retried; only a local failure (e.g. the disk) ends the loop.
pub async fn serve(
    client: &Client,
    cache: &BlobCache,
    config: &Config,
    mut signals: Signals,
) -> anyhow::Result<()> {
    let mut delay = POLL_MIN;
    loop {
        // `select!` runs whichever finishes first and drops the other: a
        // signal while waiting for the server abandons the request.
        let polled = tokio::select! {
            _ = signals.wait_for(|count| *count >= 1) => break,
            polled = client.lease() => polled,
        };
        match polled {
            Ok(Some(lease)) => {
                delay = POLL_MIN;
                run_leased(client, cache, config, &signals, lease).await?;
                continue;
            }
            Ok(None) => {}
            Err(error) => eprintln!("warning: asking for work failed: {error:#}"),
        }
        tokio::select! {
            _ = signals.wait_for(|count| *count >= 1) => break,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(POLL_MAX);
    }
    println!("shutting down: no longer asking for work");
    Ok(())
}

/// Run one leased job from start to finish, heartbeating throughout.
async fn run_leased(
    client: &Client,
    cache: &BlobCache,
    config: &Config,
    signals: &Signals,
    lease: LeaseResponse,
) -> anyhow::Result<()> {
    let job_id = lease.lease.job_id.clone();
    let attempt = lease.lease.attempt;
    println!(
        "leased job {job_id} (attempt {attempt}, run {}, job {})",
        lease.run.run_id, lease.run.job_index
    );

    let (command_tx, command_rx) = watch::channel(Command::Continue);
    // `tokio::spawn` runs the heartbeats alongside this function. The task
    // may outlive any borrow, so it gets owned copies (`client.clone()`).
    let heartbeats = tokio::spawn(heartbeat_loop(
        client.clone(),
        job_id.clone(),
        attempt,
        Duration::from_secs(lease.lease.heartbeat_every_secs.max(1)),
        command_tx,
    ));

    let result = run_job(client, cache, config, signals, &lease, command_rx).await;
    let result = match result {
        Ok(Some(result)) => result,
        Ok(None) => {
            // Lease lost: another attempt owns the job now.
            heartbeats.abort();
            eprintln!("job {job_id}: lease lost; stopped it and discarded its work");
            return Ok(());
        }
        Err(error) => {
            // Something broke here, not in the job: report it as
            // interrupted, which the queue retries (§6).
            eprintln!("job {job_id}: the agent failed: {error:#}");
            failure(
                JobStatus::Interrupted,
                vec![format!("the agent failed: {error:#}")],
            )
        }
    };

    let status = result.status;
    let report = ResultReport {
        format_version: FormatVersion::current(),
        attempt,
        result,
    };
    post_result(client, &job_id, &report).await;
    // Dropping a JoinHandle would leave the task running; `abort` stops it.
    heartbeats.abort();
    println!("job {job_id}: {status:?}");
    Ok(())
}

/// Prepare, stage, run and collect. `None` means the lease was lost, so
/// there is nothing to report.
async fn run_job(
    client: &Client,
    cache: &BlobCache,
    config: &Config,
    signals: &Signals,
    lease: &LeaseResponse,
    mut commands: watch::Receiver<Command>,
) -> anyhow::Result<Option<JobResult>> {
    let job = match job_dir::prepare(&config.work_dir, lease, cache, client).await {
        Ok(job) => job,
        // The lease contradicts itself: it fails the same way everywhere,
        // so it's a failure, not a retry.
        Err(PrepareError::BadLease(problems)) => {
            return Ok(Some(failure(
                JobStatus::Failed { exit_code: None },
                problems,
            )));
        }
        // A download or a write failed: it might work next time.
        Err(PrepareError::Io(error)) => {
            return Ok(Some(failure(
                JobStatus::Interrupted,
                vec![format!("could not prepare the job: {error:#}")],
            )));
        }
    };

    let staged = match stage::stage(&job.project, &job.inputs, &job.run_dir, config.stub).await {
        Ok(staged) => staged,
        // Missing input files on this agent: retrying won't conjure them.
        Err(errors) => {
            let problems = errors.iter().map(|error| error.to_string()).collect();
            return Ok(Some(failure(
                JobStatus::Failed { exit_code: None },
                problems,
            )));
        }
    };

    let settings = &lease.settings;
    let limit = settings.time_limit_secs.map(Duration::from_secs);
    let mut shutdown = signals.clone();
    let mut force = signals.clone();
    let stop = async move {
        tokio::select! {
            _ = shutdown.wait_for(|count| *count >= 1) => StopReason::Terminated,
            _ = run::sleep_or_forever(limit) => StopReason::TimedOut,
            command = commands.wait_for(|command| *command != Command::Continue) => {
                match command.as_deref() {
                    Ok(Command::LeaseLost) => StopReason::LeaseLost,
                    // `Cancel`; or the heartbeat task ended, which happens
                    // only if it panicked: stopping is the safe choice.
                    _ => StopReason::Cancelled,
                }
            }
        }
    };
    let force = async move {
        let _ = force.wait_for(|count| *count >= 2).await;
    };
    let command = run::nextflow_command(
        &staged,
        config.stub,
        &config.nextflow,
        &settings.nextflow_version,
    );
    println!("running in {}", staged.run_dir.display());
    let outcome = run::supervise(
        command,
        &staged.logs_dir(),
        false,
        stop,
        force,
        config.grace,
    )
    .await?;
    let result = crate::finish(outcome, &staged, settings.hash_algorithm).await?;
    if matches!(
        outcome,
        run::Outcome::Stopped {
            reason: StopReason::LeaseLost,
            ..
        }
    ) {
        return Ok(None);
    }
    Ok(Some(result))
}

/// Heartbeat every `every` until aborted, passing on what the control
/// plane says. A failed heartbeat is only logged: the lease outlasts two
/// missed ones, and if it is really gone the next one that gets through
/// says so (409).
async fn heartbeat_loop(
    client: Client,
    job_id: String,
    attempt: u32,
    every: Duration,
    commands: watch::Sender<Command>,
) {
    let request = HeartbeatRequest {
        format_version: FormatVersion::current(),
        attempt,
        progress: None,
    };
    let mut ticks = tokio::time::interval(every);
    // The first tick is immediate, and the lease was only just granted.
    ticks.tick().await;
    loop {
        ticks.tick().await;
        match client.heartbeat(&job_id, &request).await {
            Ok(HeartbeatOutcome::Renewed(HeartbeatAction::Continue)) => {}
            Ok(HeartbeatOutcome::Renewed(HeartbeatAction::Cancel)) => {
                // Keep heartbeating: the lease must hold while the job
                // stops and its result is posted.
                if *commands.borrow() == Command::Continue {
                    println!("job {job_id}: the run was cancelled; stopping");
                }
                let _ = commands.send(Command::Cancel);
            }
            Ok(HeartbeatOutcome::LeaseLost) => {
                let _ = commands.send(Command::LeaseLost);
                return;
            }
            Err(error) => eprintln!("warning: job {job_id}: heartbeat failed: {error:#}"),
        }
    }
}

/// Post the result, retrying with backoff while the control plane can't
/// be reached. Results are idempotent, so a retry after a lost reply is
/// harmless (§4.1).
async fn post_result(client: &Client, job_id: &str, report: &ResultReport) {
    let mut delay = POLL_MIN;
    for _ in 0..RESULT_TRIES {
        match client.result(job_id, report).await {
            Ok(ResultOutcome::Accepted) => return,
            Ok(ResultOutcome::Stale) => {
                eprintln!("job {job_id}: a newer attempt holds the job; this result was discarded");
                return;
            }
            Err(error) => {
                eprintln!("warning: job {job_id}: reporting the result failed: {error:#}")
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(POLL_MAX);
    }
    eprintln!(
        "job {job_id}: gave up reporting the result; the lease will expire and the job be retried"
    );
}

/// A result for a job that never ran: no tasks, no outputs, the reasons as
/// warnings (they end up in the run's status for the scientist to see).
fn failure(status: JobStatus, warnings: Vec<String>) -> JobResult {
    JobResult {
        format_version: FormatVersion::current(),
        status,
        tasks: vec![],
        outputs: vec![],
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use protocol::blob::BlobRef;
    use protocol::digest::{HashAlgorithm, digest_bytes};
    use protocol::job::JobInterface;
    use protocol::lease::{InputFile, Lease, ResolvedInput, RunRef, Settings};
    use protocol::types::Cardinality;
    use serde_json::Value;

    use super::*;
    use crate::fake_server::{self, Fake, Heartbeats, Shared, TOKEN};
    use crate::job_dir::testing::project;

    const JOB: &str = "0b7e5a1c-3f2d-4c8e-9a6b-1d2e3f4a5b6c";

    /// Writes an HTML report for sample `gut`, containing `NXF_VER`, so a
    /// test can check both the outputs and the version passed through.
    const SUCCEEDS: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  if [ "$1" = "-output-dir" ]; then out="$2"; fi
  shift
done
mkdir -p "$out/fastqc_1__html/gut"
printf 'nextflow %s' "$NXF_VER" > "$out/fastqc_1__html/gut/gut.html"
"#;

    /// Runs until stopped. `exec` so SIGTERM reaches `sleep` directly.
    const RUNS_FOREVER: &str = "#!/bin/sh\nexec sleep 30\n";

    /// A test's world: a fake control plane holding the fastqc → multiqc
    /// project's blobs, reads on disk, and a fake `nextflow`.
    struct World {
        fake: Shared,
        client: Client,
        config: Config,
        /// Sends signals: 1 = shut down, 2 = now.
        signals: watch::Sender<u32>,
        project: BlobRef,
        interface: JobInterface,
        reads: Vec<InputFile>,
        _dirs: Vec<tempfile::TempDir>,
    }

    async fn world(script: &str) -> World {
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let (project, interface, source) = project();
        let mut fake = Fake::new();
        for (digest, bytes) in source.blobs {
            fake.blobs.insert(digest.to_string(), bytes);
        }
        let (fake, base) = fake_server::start(fake).await;

        let nextflow = work.path().join("fake-nextflow");
        std::fs::write(&nextflow, script).unwrap();
        std::fs::set_permissions(&nextflow, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut reads = Vec::new();
        for name in ["gut_1.fq", "gut_2.fq"] {
            let path = data.path().join(name);
            std::fs::write(&path, "@r\n").unwrap();
            reads.push(InputFile {
                sample: Some("gut".to_string()),
                uri: path.display().to_string(),
                digest: digest_bytes(HashAlgorithm::Sha256, b"@r\n"),
                size: 3,
            });
        }

        World {
            fake,
            client: Client::new(&base, TOKEN).unwrap(),
            config: Config {
                work_dir: work.path().join("work"),
                stub: true,
                nextflow,
                grace: Duration::from_secs(5),
            },
            signals: watch::channel(0).0,
            project,
            interface,
            reads,
            _dirs: vec![work, data],
        }
    }

    impl World {
        fn lease(&self) -> LeaseResponse {
            LeaseResponse {
                format_version: FormatVersion::current(),
                lease: Lease {
                    job_id: JOB.to_string(),
                    attempt: 1,
                    expires_at: "2026-11-26T20:00:00Z".to_string(),
                    heartbeat_every_secs: 1,
                },
                run: RunRef {
                    run_id: "run-1".to_string(),
                    job_index: 0,
                },
                settings: Settings {
                    nextflow_version: "26.10.5".to_string(),
                    hash_algorithm: HashAlgorithm::Sha256,
                    time_limit_secs: None,
                },
                project: self.project.clone(),
                interface: self.interface.clone(),
                inputs: BTreeMap::from([(
                    "fastqc_1__reads".to_string(),
                    ResolvedInput {
                        cardinality: Cardinality::PerSample,
                        files: self.reads.clone(),
                    },
                )]),
            }
        }

        fn offer(&self, lease: LeaseResponse) {
            let json = serde_json::to_value(lease).unwrap();
            self.fake.lock().unwrap().leases.push_back(json);
        }

        /// Run `serve` in the background; the handle ends when it does.
        fn start(&self) -> tokio::task::JoinHandle<anyhow::Result<()>> {
            let client = self.client.clone();
            let cache = BlobCache::new(self.config.work_dir.parent().unwrap().join("cache"));
            let config = Config {
                work_dir: self.config.work_dir.clone(),
                stub: self.config.stub,
                nextflow: self.config.nextflow.clone(),
                grace: self.config.grace,
            };
            let signals = self.signals.subscribe();
            tokio::spawn(async move { serve(&client, &cache, &config, signals).await })
        }

        fn signal(&self) {
            self.signals.send_modify(|count| *count += 1);
        }

        fn results(&self) -> Vec<ResultReport> {
            self.fake
                .lock()
                .unwrap()
                .results()
                .into_iter()
                .map(|body: Value| serde_json::from_value(body).unwrap())
                .collect()
        }

        fn job_dir(&self) -> std::path::PathBuf {
            self.config.work_dir.join("jobs").join(JOB).join("1")
        }
    }

    async fn wait_for_file(path: &Path) {
        for _ in 0..1000 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} never appeared", path.display());
    }

    #[tokio::test]
    async fn serves_a_job_and_reports_its_outputs() {
        let world = world(SUCCEEDS).await;
        world.offer(world.lease());
        let serving = world.start();

        fake_server::wait_until(&world.fake, |fake| fake.count("result") == 1).await;
        world.signal();
        serving.await.unwrap().unwrap();

        let results = world.results();
        assert_eq!(results[0].attempt, 1);
        let result = &results[0].result;
        assert_eq!(result.status, JobStatus::Succeeded, "{:?}", result.warnings);
        // The fake Nextflow's output, hashed, with the version from the
        // lease's settings in it.
        let html = result
            .outputs
            .iter()
            .find(|output| output.name == "fastqc_1__html")
            .unwrap();
        assert_eq!(html.files.len(), 1);
        assert_eq!(
            html.files[0].digest,
            digest_bytes(HashAlgorithm::Sha256, b"nextflow 26.10.5")
        );
        assert!(world.job_dir().join("run/result.json").exists());
    }

    #[tokio::test]
    async fn polls_again_when_there_is_no_work() {
        let world = world(SUCCEEDS).await;
        let serving = world.start();

        fake_server::wait_until(&world.fake, |fake| fake.count("lease") >= 2).await;
        world.signal();
        serving.await.unwrap().unwrap();
        assert!(world.results().is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_run_stops_the_job_and_reports_cancelled() {
        let world = world(RUNS_FOREVER).await;
        world.fake.lock().unwrap().heartbeats = Heartbeats::Cancel;
        world.offer(world.lease());
        let serving = world.start();

        fake_server::wait_until(&world.fake, |fake| fake.count("result") == 1).await;
        world.signal();
        serving.await.unwrap().unwrap();

        assert_eq!(world.results()[0].result.status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn a_lost_lease_stops_the_job_and_reports_nothing() {
        let world = world(RUNS_FOREVER).await;
        world.fake.lock().unwrap().heartbeats = Heartbeats::Conflict;
        world.offer(world.lease());
        let serving = world.start();

        // The job stopped: its local result.json is written.
        wait_for_file(&world.job_dir().join("run/result.json")).await;
        world.signal();
        serving.await.unwrap().unwrap();

        assert!(world.results().is_empty(), "a lost lease reports nothing");
        // One heartbeat got the 409; none were sent after it.
        assert_eq!(world.fake.lock().unwrap().count("heartbeat"), 1);
    }

    #[tokio::test]
    async fn shutting_down_reports_the_job_interrupted() {
        let world = world(RUNS_FOREVER).await;
        world.offer(world.lease());
        let serving = world.start();

        wait_for_file(&world.job_dir().join("run/logs")).await;
        world.signal();
        serving.await.unwrap().unwrap();

        let results = world.results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].result.status, JobStatus::Interrupted);
    }

    #[tokio::test]
    async fn a_bad_lease_is_reported_as_failed_with_the_reasons() {
        let world = world(SUCCEEDS).await;
        let mut lease = world.lease();
        lease.inputs.clear();
        world.offer(lease);
        let serving = world.start();

        fake_server::wait_until(&world.fake, |fake| fake.count("result") == 1).await;
        world.signal();
        serving.await.unwrap().unwrap();

        let result = &world.results()[0].result;
        assert_eq!(result.status, JobStatus::Failed { exit_code: None });
        assert!(
            result.warnings[0].contains("`fastqc_1__reads` has no files"),
            "{:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn missing_input_files_are_reported_as_failed() {
        let world = world(SUCCEEDS).await;
        let mut lease = world.lease();
        lease.inputs.get_mut("fastqc_1__reads").unwrap().files[0].uri =
            "/nonexistent/gut_1.fq".to_string();
        world.offer(lease);
        let serving = world.start();

        fake_server::wait_until(&world.fake, |fake| fake.count("result") == 1).await;
        world.signal();
        serving.await.unwrap().unwrap();

        let result = &world.results()[0].result;
        assert_eq!(result.status, JobStatus::Failed { exit_code: None });
        assert!(
            result.warnings[0].contains("/nonexistent/gut_1.fq"),
            "{:?}",
            result.warnings
        );
    }
}
