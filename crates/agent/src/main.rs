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

mod preflight;
mod run;
mod stage;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::run::{Outcome, StopReason};
use crate::stage::{InputArg, StagedRun};

const ENVIRONMENT_FAILED: u8 = 2;
const INPUTS_FAILED: u8 = 3;
const JOB_FAILED: u8 = 4;
const TIMED_OUT: u8 = 5;
const CANCELLED: u8 = 130;

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
        } => {
            let staged = match check_and_stage(&project, &inputs, &run_dir, stub).await {
                Ok(staged) => staged,
                Err(code) => return Ok(code),
            };
            let limit = timeout_secs.map(Duration::from_secs);
            // Stop on Ctrl-C or the time limit, whichever comes first.
            let stop = async {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => StopReason::Cancelled,
                    _ = run::sleep_or_forever(limit) => StopReason::TimedOut,
                }
            };
            let command = run::nextflow_command(&staged, stub);
            let outcome = run::supervise(command, &staged.logs_dir(), true, stop, GRACE).await?;
            Ok(report(outcome, &staged))
        }
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

/// Say how the run ended, and pick the exit code.
fn report(outcome: Outcome, staged: &StagedRun) -> ExitCode {
    let logs = staged.logs_dir();
    match outcome {
        Outcome::Exited { code: Some(0), .. } => {
            println!(
                "job finished; results in {}",
                staged.results_dir().display()
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
                "killed after ignoring SIGTERM (task containers may still be running)"
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
