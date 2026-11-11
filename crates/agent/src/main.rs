//! `agent`: runs compiled jobs and reports what happened (DESIGN.md §4.1).
//!
//! P0b is local mode: jobs come from directories on disk, not from a
//! server. So far: `check` (preflight) and `stage` (inputs + run
//! directory). `run` arrives with process supervision.
//!
//! Exit codes, one per kind of failure, so scripts (and later the control
//! plane) can tell them apart:
//! - 0: success
//! - 1: the agent itself failed (a bug, or an unexpected I/O error)
//! - 2: this machine can't run jobs (preflight failed): fix the host
//! - 3: the job's inputs are wrong (staging failed): fix the request

mod preflight;
mod stage;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::stage::InputArg;

const ENVIRONMENT_FAILED: u8 = 2;
const INPUTS_FAILED: u8 = 3;

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
            if check(!stub).await != ExitCode::SUCCESS {
                return Ok(ExitCode::from(ENVIRONMENT_FAILED));
            }
            match stage::stage(&project, &inputs, &run_dir, stub).await {
                Ok(staged) => {
                    println!("staged into {}", staged.run_dir.display());
                    println!(
                        "nextflow command (from the run directory):\n  nextflow {}",
                        staged.nextflow_args(stub).join(" ")
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(errors) => {
                    eprintln!("cannot stage {}:", project.display());
                    for error in &errors {
                        eprintln!("  - {error}");
                    }
                    Ok(ExitCode::from(INPUTS_FAILED))
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
