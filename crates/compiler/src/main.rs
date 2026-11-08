//! `compiler`: compile a canvas file into Nextflow projects.
//!
//! ```text
//! cargo run -p compiler -- crates/compiler/testdata/fastqc_multiqc.canvas.json --out out/
//! ```
//!
//! Writes one `job-<n>/` directory per segment job. Analysis jobs are listed
//! but not compiled yet. This binary does the file I/O; the library it
//! calls stays pure.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::Parser;
use compiler::canvas::Canvas;
use compiler::codegen::NextflowProject;
use compiler::execution_plan::{ExecutionPlan, JobId, JobKind};
use compiler::library::load_library;
use compiler::spec::SpecRegistry;
use compiler::validate::validate;

/// Compile a pipeline canvas into Nextflow projects, one per segment job.
#[derive(Parser)]
struct Args {
    /// The canvas JSON file.
    canvas: PathBuf,
    /// The node library directory.
    #[arg(long, default_value = "nodes")]
    library: PathBuf,
    /// Directory to write the projects into. Must be empty or not exist.
    #[arg(long)]
    out: PathBuf,
}

// Returning `ExitCode` lets an invalid canvas exit with status 1 after
// printing every error, while other failures (`anyhow` errors) still print
// their full cause chain.
fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse();

    let nodes = load_library(&args.library)
        .with_context(|| format!("loading node library {}", args.library.display()))?;
    let registry = SpecRegistry::from_library(nodes);

    let text = fs::read_to_string(&args.canvas)
        .with_context(|| format!("reading {}", args.canvas.display()))?;
    let canvas = Canvas::from_json(&text)?;

    let pipeline = match validate(&canvas, &registry) {
        Ok(pipeline) => pipeline,
        Err(errors) => {
            eprintln!("{} is not a valid pipeline:", args.canvas.display());
            for error in &errors {
                eprintln!("  - {error}");
            }
            return Ok(ExitCode::FAILURE);
        }
    };
    let plan = ExecutionPlan::build(pipeline);

    ensure_empty(&args.out)?;
    let graph = plan.pipeline().graph();
    for (index, job) in plan.jobs().iter().enumerate() {
        let node_ids: Vec<&str> = job.nodes().iter().map(|&n| graph[n].id.as_str()).collect();
        let depends_on: Vec<String> = job.depends_on.iter().map(|id| id.0.to_string()).collect();
        let after = if depends_on.is_empty() {
            String::new()
        } else {
            format!(", after job {}", depends_on.join(", "))
        };
        match &job.kind {
            JobKind::Segment { .. } => {
                let dir = args.out.join(format!("job-{index}"));
                let project = plan.nextflow_project(JobId(index), &registry)?;
                write_project(&project, &dir)?;
                println!(
                    "job {index}: Nextflow segment [{}]{after} -> {}",
                    node_ids.join(", "),
                    dir.display()
                );
            }
            JobKind::Analysis { .. } => {
                println!(
                    "job {index}: analysis [{}]{after} (not compiled yet)",
                    node_ids.join(", ")
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Refuse to write into a directory that already has files in it, so a
/// typo in `--out` can't mix new projects with old ones.
fn ensure_empty(dir: &Path) -> anyhow::Result<()> {
    if dir.exists() && fs::read_dir(dir)?.next().is_some() {
        bail!("{} is not empty", dir.display());
    }
    Ok(())
}

fn write_project(project: &NextflowProject, dir: &Path) -> anyhow::Result<()> {
    for (path, text) in &project.files {
        let file = dir.join(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(())
}
