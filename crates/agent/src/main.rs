//! `agent`: runs compiled jobs and reports what happened (DESIGN.md §4.1).
//!
//! P0b is local mode: jobs come from directories on disk, not from a
//! server. `check` (preflight), `stage` (inputs + run directory), and
//! `run` (check, stage, then run Nextflow and supervise it).
//!
//! Exit codes, one per kind of outcome, so scripts (and later the control
//! plane) can tell them apart:
//! - 0: success
//! - 1: the agent itself failed (a bug, or an unexpected I/O error)
//! - 2: this machine can't run jobs (preflight failed): fix the host
//! - 3: the job's inputs are wrong (staging failed): fix the request
//! - 4: the job ran and failed (Nextflow exited non-zero)
//! - 5: the job hit its time limit and was stopped
//! - 130: the job was cancelled (Ctrl-C), the shell convention for SIGINT
//! - 143: the agent was shut down (SIGTERM) mid-job; the job should be
//!   retried. 128 + 15, the shell convention for SIGTERM.
//!
//! Stopping: the first Ctrl-C or SIGTERM asks Nextflow to stop and waits
//! up to a grace period; a second one kills it at once.

mod collect;
mod hashing;
mod preflight;
mod run;
mod stage;
mod trace;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use protocol::digest::HashAlgorithm;
use protocol::result::{JobResult, JobStatus, TaskStatus};
use tokio::signal::unix::{SignalKind, signal};

use crate::run::{Outcome, StopReason};
use crate::stage::{InputArg, StagedRun};

const ENVIRONMENT_FAILED: u8 = 2;
const INPUTS_FAILED: u8 = 3;
const JOB_FAILED: u8 = 4;
const TIMED_OUT: u8 = 5;
const CANCELLED: u8 = 130;
const INTERRUPTED: u8 = 143;

/// How long Nextflow gets to shut down cleanly after SIGTERM.
const GRACE: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(about = "Runs compiled jobs with Nextflow and reports what happened.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check that this machine can run jobs (Java, Nextflow, Docker).
    Check {
        /// Don't require Docker (enough for stub runs).
        #[arg(long)]
        no_docker: bool,
    },
    /// Check a job's inputs and set up its run directory, without running.
    Stage {
        /// A compiled project directory (holds main.nf and job.json).
        project: PathBuf,
        /// A job input: `--input <name>=<manifest.csv>`. Repeat for each.
        #[arg(long = "input", value_parser = InputArg::parse)]
        inputs: Vec<InputArg>,
        /// Where to put this run's files. Must be empty or not exist.
        #[arg(long)]
        run_dir: PathBuf,
        /// Prepare a stub run: no containers or real data needed.
        #[arg(long)]
        stub: bool,
    },
    /// Check, stage, then run a job with Nextflow and supervise it.
    Run {
        /// A compiled project directory (holds main.nf and job.json).
        project: PathBuf,
        /// A job input: `--input <name>=<manifest.csv>`. Repeat for each.
        #[arg(long = "input", value_parser = InputArg::parse)]
        inputs: Vec<InputArg>,
        /// Where to put this run's files. Must be empty or not exist.
        #[arg(long)]
        run_dir: PathBuf,
        /// Stub run: modules' `stub:` blocks, Docker off.
        #[arg(long)]
        stub: bool,
        /// Stop the job after this many seconds.
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// Hash algorithm for outputs.
        #[arg(long, default_value = "sha256", value_parser = |text: &str| text.parse::<HashAlgorithm>())]
        hash: HashAlgorithm,
    },
}

// `#[tokio::main]` turns `async fn main` into a normal `main` that starts
// the tokio runtime and runs this future on it until it finishes.
#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    match Cli::parse().command {
        Command::Check { no_docker } => Ok(check(!no_docker).await),
        Command::Stage {
            project,
            inputs,
            run_dir,
            stub,
        } => {
            let staged = match check_and_stage(&project, &inputs, &run_dir, stub).await {
                Ok(staged) => staged,
                Err(code) => return Ok(code),
            };
            println!(
                "nextflow command (from the run directory):\n  nextflow {}",
                staged.nextflow_args(stub).join(" ")
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Run {
            project,
            inputs,
            run_dir,
            stub,
            timeout_secs,
            hash,
        } => {
            let staged = match check_and_stage(&project, &inputs, &run_dir, stub).await {
                Ok(staged) => staged,
                Err(code) => return Ok(code),
            };
            let limit = timeout_secs.map(Duration::from_secs);
            // Stop on Ctrl-C, SIGTERM or the time limit, whichever comes first.
            let stop = async {
                tokio::select! {
                    reason = next_stop_signal() => reason,
                    _ = run::sleep_or_forever(limit) => StopReason::TimedOut,
                }
            };
            // Once stopping, another signal means "now".
            let force = async {
                next_stop_signal().await;
            };
            let command = run::nextflow_command(&staged, stub);
            let outcome =
                run::supervise(command, &staged.logs_dir(), true, stop, force, GRACE).await?;
            let result = finish(outcome, &staged, hash).await?;
            Ok(report(outcome, &staged, &result))
        }
    }
}

/// Waits for the next Ctrl-C (SIGINT) or SIGTERM to reach the agent.
///
/// An `async fn` body runs only when first polled, so each call starts
/// listening at that moment. That's why `force` above only reacts to a
/// signal *after* the one that triggered `stop`: it isn't polled until
/// stopping has begun.
async fn next_stop_signal() -> StopReason {
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(error) => {
            // Without a SIGTERM handler we can still honour Ctrl-C.
            eprintln!("warning: cannot listen for SIGTERM: {error}");
            let _ = tokio::signal::ctrl_c().await;
            return StopReason::Cancelled;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => StopReason::Cancelled,
        _ = terminate.recv() => StopReason::Terminated,
    }
}

/// Preflight, then staging. On failure, prints why and returns the exit
/// code to use.
async fn check_and_stage(
    project: &Path,
    inputs: &[InputArg],
    run_dir: &Path,
    stub: bool,
) -> Result<StagedRun, ExitCode> {
    if check(!stub).await != ExitCode::SUCCESS {
        return Err(ExitCode::from(ENVIRONMENT_FAILED));
    }
    match stage::stage(project, inputs, run_dir, stub).await {
        Ok(staged) => {
            println!("staged into {}", staged.run_dir.display());
            Ok(staged)
        }
        Err(errors) => {
            eprintln!("cannot stage {}:", project.display());
            for error in &errors {
                eprintln!("  - {error}");
            }
            Err(ExitCode::from(INPUTS_FAILED))
        }
    }
}

/// After Nextflow ends: parse the trace, collect outputs (only if the job
/// succeeded), and write `result.json`.
async fn finish(
    outcome: Outcome,
    staged: &StagedRun,
    algorithm: HashAlgorithm,
) -> anyhow::Result<JobResult> {
    let status = match outcome {
        Outcome::Exited { code: Some(0), .. } => JobStatus::Succeeded,
        Outcome::Exited { code, .. } => JobStatus::Failed { exit_code: code },
        Outcome::Stopped {
            reason: StopReason::TimedOut,
            ..
        } => JobStatus::TimedOut,
        Outcome::Stopped {
            reason: StopReason::Cancelled,
            ..
        } => JobStatus::Cancelled,
        Outcome::Stopped {
            reason: StopReason::Terminated,
            ..
        } => JobStatus::Interrupted,
    };

    // No trace file means Nextflow stopped before starting any task. A
    // trace we can't parse is reported as a warning rather than an error:
    // the job's status and outputs are still worth recording.
    let mut warnings = Vec::new();
    let trace_path = staged.run_dir.join("trace.tsv");
    let tasks = match tokio::fs::read_to_string(&trace_path).await {
        Ok(text) => trace::parse(&text).unwrap_or_else(|error| {
            warnings.push(format!("could not parse trace.tsv: {error:#}"));
            Vec::new()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).context("reading trace.tsv"),
    };

    let outputs = if status == JobStatus::Succeeded {
        collect::collect(staged, algorithm).await?
    } else {
        Vec::new()
    };

    let result = JobResult {
        status,
        tasks,
        outputs,
        warnings,
    };
    let mut json = serde_json::to_string_pretty(&result)?;
    json.push('\n');
    tokio::fs::write(staged.run_dir.join("result.json"), json).await?;
    Ok(result)
}

/// Say how the run ended, and pick the exit code.
fn report(outcome: Outcome, staged: &StagedRun, result: &JobResult) -> ExitCode {
    let logs = staged.logs_dir();
    let count = |status: TaskStatus| result.tasks.iter().filter(|t| t.status == status).count();
    println!(
        "tasks: {} completed, {} cached, {} failed, {} aborted",
        count(TaskStatus::Completed),
        count(TaskStatus::Cached),
        count(TaskStatus::Failed),
        count(TaskStatus::Aborted)
    );
    for task in result
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Failed)
    {
        let exit = task
            .exit_code
            .map_or("?".to_string(), |code| code.to_string());
        eprintln!("  failed: {} (exit {exit})", task.name);
    }
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
    println!("result: {}", staged.run_dir.join("result.json").display());
    match outcome {
        Outcome::Exited { code: Some(0), .. } => {
            let files: Vec<_> = result.outputs.iter().flat_map(|o| &o.files).collect();
            let bytes: u64 = files.iter().map(|f| f.size).sum();
            println!(
                "job finished: {} outputs, {} files ({bytes} bytes) hashed; manifests in {}",
                result.outputs.len(),
                files.len(),
                staged.run_dir.join("outputs").display()
            );
            ExitCode::SUCCESS
        }
        Outcome::Exited { code, signal } => {
            let how = match (code, signal) {
                (Some(code), _) => format!("exit code {code}"),
                (None, Some(signal)) => format!("signal {signal}"),
                (None, None) => "an unknown cause".to_string(),
            };
            eprintln!(
                "job failed: nextflow ended with {how}; see {}",
                logs.display()
            );
            ExitCode::from(JOB_FAILED)
        }
        Outcome::Stopped { reason, forced } => {
            let how = if forced {
                "force-killed (task containers may still be running)"
            } else {
                "stopped cleanly"
            };
            match reason {
                StopReason::TimedOut => {
                    eprintln!("job hit its time limit: {how}");
                    ExitCode::from(TIMED_OUT)
                }
                StopReason::Cancelled => {
                    eprintln!("job cancelled: {how}");
                    ExitCode::from(CANCELLED)
                }
                StopReason::Terminated => {
                    eprintln!("agent shut down; job interrupted ({how}), retry it");
                    ExitCode::from(INTERRUPTED)
                }
            }
        }
    }
}

/// Run preflight and print what it found.
async fn check(need_docker: bool) -> ExitCode {
    match preflight::check(need_docker).await {
        Ok(env) => {
            let docker = env.docker.as_deref().unwrap_or("not checked");
            println!(
                "ok: java {}, nextflow {}, docker {docker}",
                env.java_major, env.nextflow
            );
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!("this machine can't run jobs yet:");
            for error in &errors {
                eprintln!("  - {error}");
            }
            ExitCode::from(ENVIRONMENT_FAILED)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::job::JobInterface;

    use super::*;

    #[tokio::test]
    async fn an_unparseable_trace_still_produces_a_result() {
        let run = tempfile::tempdir().unwrap();
        std::fs::write(run.path().join("trace.tsv"), "not\ta trace\n").unwrap();
        let staged = StagedRun {
            project: run.path().join("project"),
            run_dir: run.path().to_path_buf(),
            interface: JobInterface {
                inputs: vec![],
                outputs: vec![],
            },
            inputs: BTreeMap::new(),
        };
        let outcome = Outcome::Exited {
            code: Some(0),
            signal: None,
        };

        let result = finish(outcome, &staged, HashAlgorithm::Sha256)
            .await
            .unwrap();

        assert_eq!(result.status, JobStatus::Succeeded);
        assert!(result.tasks.is_empty());
        assert!(
            result.warnings[0].contains("could not parse trace.tsv"),
            "{:?}",
            result.warnings
        );
        let written = std::fs::read_to_string(run.path().join("result.json")).unwrap();
        assert!(written.contains("could not parse trace.tsv"));
    }
}
