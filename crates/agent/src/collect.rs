//! Collecting a finished job's outputs (DESIGN.md §4.1, local mode step 5).
//!
//! The generated code publishes each output under `results/<name>/`, one
//! subdirectory per sample for per-sample data. For every output in
//! `job.json`, this walks that layout, hashes each file, and writes an
//! output manifest to `outputs/<name>.csv`: the same CSV format a later job
//! reads as its input.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use protocol::digest::HashAlgorithm;
use protocol::manifest::{Manifest, SampleFiles};
use protocol::result::{OutputFile, OutputResult};
use protocol::types::Cardinality;
use tokio::fs;

use crate::hashing::hash_file;
use crate::stage::StagedRun;

pub async fn collect(
    staged: &StagedRun,
    algorithm: HashAlgorithm,
) -> anyhow::Result<Vec<OutputResult>> {
    let manifests_dir = staged.run_dir.join("outputs");
    fs::create_dir_all(&manifests_dir).await?;

    let mut results = Vec::new();
    for output in &staged.interface.outputs {
        let dir = staged.results_dir().join(&output.name);
        let files = match output.cardinality {
            Cardinality::PerSample => per_sample_files(&dir, algorithm).await,
            Cardinality::PerDataset => files_in(&dir, None, algorithm).await,
        }
        .with_context(|| format!("collecting output `{}`", output.name))?;

        let manifest_path = manifests_dir.join(format!("{}.csv", output.name));
        fs::write(
            &manifest_path,
            manifest_of(&files, output.cardinality).to_csv()?,
        )
        .await?;

        results.push(OutputResult {
            name: output.name.clone(),
            cardinality: output.cardinality,
            ty: output.ty.clone(),
            manifest: manifest_path.display().to_string(),
            files,
        });
    }
    Ok(results)
}

/// `results/<name>/<sample>/<file>`: each subdirectory is one sample.
async fn per_sample_files(dir: &Path, algorithm: HashAlgorithm) -> anyhow::Result<Vec<OutputFile>> {
    let mut files = Vec::new();
    for sample_dir in sorted_entries(dir).await? {
        if !fs::metadata(&sample_dir).await?.is_dir() {
            bail!(
                "{} is a file where a sample directory was expected",
                sample_dir.display()
            );
        }
        let sample = name_of(&sample_dir)?;
        files.append(&mut files_in(&sample_dir, Some(sample), algorithm).await?);
    }
    Ok(files)
}

/// Hash every file directly in `dir`, in name order.
async fn files_in(
    dir: &Path,
    sample: Option<String>,
    algorithm: HashAlgorithm,
) -> anyhow::Result<Vec<OutputFile>> {
    let mut files = Vec::new();
    for path in sorted_entries(dir).await? {
        if fs::metadata(&path).await?.is_dir() {
            bail!(
                "{} is a directory; directory outputs are not supported yet",
                path.display()
            );
        }
        let (digest, size) = hash_file(path.clone(), algorithm).await?;
        files.push(OutputFile {
            sample: sample.clone(),
            path: path.display().to_string(),
            size,
            digest,
        });
    }
    Ok(files)
}

/// Entries of `dir` sorted by name, or none if it doesn't exist (an output
/// the job produced no files for).
async fn sorted_entries(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("listing {}", dir.display())),
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn name_of(path: &Path) -> anyhow::Result<String> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => Ok(name.to_string()),
        None => bail!("{} has no usable name", path.display()),
    }
}

/// The output manifest for a list of collected files.
fn manifest_of(files: &[OutputFile], cardinality: Cardinality) -> Manifest {
    match cardinality {
        Cardinality::PerDataset => {
            Manifest::PerDataset(files.iter().map(|file| file.path.clone()).collect())
        }
        Cardinality::PerSample => {
            // Files are already grouped by sample, in sample order.
            let mut samples: Vec<SampleFiles> = Vec::new();
            for file in files {
                let sample = file.sample.clone().unwrap_or_default();
                match samples.last_mut() {
                    Some(last) if last.sample == sample => last.paths.push(file.path.clone()),
                    _ => samples.push(SampleFiles {
                        sample,
                        paths: vec![file.path.clone()],
                    }),
                }
            }
            Manifest::PerSample(samples)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::job::{JobInterface, JobOutput};
    use protocol::types::SemanticType;

    use super::*;

    fn output(name: &str, cardinality: Cardinality) -> JobOutput {
        JobOutput {
            name: name.to_string(),
            node: "n".to_string(),
            port: "p".to_string(),
            cardinality,
            ty: SemanticType::QcReport,
        }
    }

    /// A staged run whose results/ already holds `files` (relative paths).
    fn staged_with(outputs: Vec<JobOutput>, files: &[&str]) -> (tempfile::TempDir, StagedRun) {
        let run = tempfile::tempdir().unwrap();
        for file in files {
            let path = run.path().join("results").join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, file.as_bytes()).unwrap();
        }
        let staged = StagedRun {
            project: run.path().join("project"),
            run_dir: run.path().to_path_buf(),
            interface: JobInterface {
                inputs: vec![],
                outputs,
            },
            inputs: BTreeMap::new(),
        };
        (run, staged)
    }

    #[tokio::test]
    async fn collects_per_sample_outputs_into_a_manifest() {
        let (run, staged) = staged_with(
            vec![output("qc__zip", Cardinality::PerSample)],
            &[
                "qc__zip/S2/b.zip",
                "qc__zip/S1/a_2.zip",
                "qc__zip/S1/a_1.zip",
            ],
        );

        let results = collect(&staged, HashAlgorithm::Sha256).await.unwrap();

        let files = &results[0].files;
        let listed: Vec<(Option<&str>, &str)> = files
            .iter()
            .map(|f| (f.sample.as_deref(), f.path.rsplit('/').next().unwrap()))
            .collect();
        assert_eq!(
            listed,
            vec![
                (Some("S1"), "a_1.zip"),
                (Some("S1"), "a_2.zip"),
                (Some("S2"), "b.zip")
            ]
        );
        assert_eq!(files[0].size, "qc__zip/S1/a_1.zip".len() as u64);

        let manifest = std::fs::read_to_string(&results[0].manifest).unwrap();
        let results_dir = run.path().join("results/qc__zip");
        assert_eq!(
            manifest,
            format!(
                "sample,path\nS1,{d}/S1/a_1.zip\nS1,{d}/S1/a_2.zip\nS2,{d}/S2/b.zip\n",
                d = results_dir.display()
            )
        );
        // The output manifest reads back as a valid input manifest.
        assert!(Manifest::from_csv(&manifest, Cardinality::PerSample).is_ok());
    }

    #[tokio::test]
    async fn collects_per_dataset_outputs() {
        let (_run, staged) = staged_with(
            vec![output("mqc__report", Cardinality::PerDataset)],
            &["mqc__report/multiqc_report.html"],
        );
        let results = collect(&staged, HashAlgorithm::Sha256).await.unwrap();
        assert_eq!(results[0].files.len(), 1);
        assert_eq!(results[0].files[0].sample, None);
    }

    #[tokio::test]
    async fn an_output_with_no_files_is_empty_not_an_error() {
        let (_run, staged) = staged_with(vec![output("none__out", Cardinality::PerSample)], &[]);
        let results = collect(&staged, HashAlgorithm::Sha256).await.unwrap();
        assert!(results[0].files.is_empty());
    }

    #[tokio::test]
    async fn directory_outputs_are_reported_clearly() {
        let (_run, staged) = staged_with(
            vec![output("mqc__data", Cardinality::PerDataset)],
            &["mqc__data/multiqc_data/stats.txt"],
        );
        let err = collect(&staged, HashAlgorithm::Sha256).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("collecting output `mqc__data`"),
            "{message}"
        );
        assert!(
            message.contains("directory outputs are not supported yet"),
            "{message}"
        );
    }
}
