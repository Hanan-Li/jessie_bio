//! Staging: check a job's inputs and set up its run directory
//! (DESIGN.md §4.1, local mode step 2).
//!
//! Every problem is collected, not just the first, so whoever launched the
//! job can fix them all in one go. Nothing is written until all checks pass.
//!
//! ```text
//! <run dir>/
//!   inputs/<name>.csv   staged manifests: validated, paths made absolute
//!   work/               Nextflow's work directory
//!   results/            published outputs
//!   logs/               Nextflow's stdout and stderr
//! ```
//!
//! The project directory is only read, so one compiled project can be run
//! many times.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use protocol::job::JobInterface;
use protocol::manifest::{Manifest, SampleFiles};
use tokio::fs;

/// One `--input <name>=<manifest>` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputArg {
    pub name: String,
    pub manifest: PathBuf,
}

impl InputArg {
    /// Parse `name=path`. Used by clap, so the error is a plain `String`.
    pub fn parse(text: &str) -> Result<InputArg, String> {
        match text.split_once('=') {
            Some((name, path)) if !name.is_empty() && !path.is_empty() => Ok(InputArg {
                name: name.to_string(),
                manifest: PathBuf::from(path),
            }),
            _ => Err(format!("expected <name>=<manifest>, got `{text}`")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StagingError {
    #[error("cannot read the job interface {path}: {reason}")]
    BadInterface { path: String, reason: String },

    #[error("no manifest given for input `{0}` (pass --input {0}=<manifest>)")]
    MissingInput(String),

    #[error("`{0}` is not an input of this job")]
    UnknownInput(String),

    #[error("input `{0}` was given more than once")]
    DuplicateInput(String),

    #[error("manifest for `{name}` ({path}): {reason}")]
    BadManifest {
        name: String,
        path: String,
        reason: String,
    },

    #[error("manifest for `{name}` lists {file}, which does not exist")]
    MissingFile { name: String, file: String },

    #[error("run directory {0} is not empty")]
    RunDirNotEmpty(String),

    #[error("could not set up the run directory: {0}")]
    Io(String),
}

/// A job whose inputs have been checked and whose run directory is ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedRun {
    pub project: PathBuf,
    pub run_dir: PathBuf,
    pub interface: JobInterface,
    /// Input name → its staged manifest in `<run dir>/inputs/`.
    pub inputs: BTreeMap<String, PathBuf>,
}

impl StagedRun {
    pub fn work_dir(&self) -> PathBuf {
        self.run_dir.join("work")
    }

    pub fn results_dir(&self) -> PathBuf {
        self.run_dir.join("results")
    }

    /// Arguments for `nextflow`, run from the run directory. A stub run
    /// executes each module's `stub:` block and turns Docker off, so it
    /// needs no containers or real data.
    pub fn nextflow_args(&self, stub: bool) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            self.project.join("main.nf").display().to_string(),
            "-work-dir".to_string(),
            self.work_dir().display().to_string(),
            "-output-dir".to_string(),
            self.results_dir().display().to_string(),
        ];
        if stub {
            args.push("-stub-run".to_string());
            args.push("-c".to_string());
            args.push(self.run_dir.join("stub.config").display().to_string());
        }
        for (name, manifest) in &self.inputs {
            args.push(format!("--{name}"));
            args.push(manifest.display().to_string());
        }
        args
    }
}

pub async fn stage(
    project: &Path,
    inputs: &[InputArg],
    run_dir: &Path,
    stub: bool,
) -> Result<StagedRun, Vec<StagingError>> {
    let project = absolute(project).map_err(|error| vec![error])?;
    let run_dir = absolute(run_dir).map_err(|error| vec![error])?;

    // Without the interface there is nothing to check inputs against.
    let interface = read_interface(&project)
        .await
        .map_err(|error| vec![error])?;

    let mut errors = Vec::new();
    if is_non_empty_dir(&run_dir).await {
        errors.push(StagingError::RunDirNotEmpty(run_dir.display().to_string()));
    }

    // Match the given inputs against the interface's.
    let mut given: BTreeMap<&str, &Path> = BTreeMap::new();
    for input in inputs {
        if given.insert(&input.name, &input.manifest).is_some() {
            errors.push(StagingError::DuplicateInput(input.name.clone()));
        }
    }
    for name in given.keys() {
        if !interface.inputs.iter().any(|input| input.name == *name) {
            errors.push(StagingError::UnknownInput(name.to_string()));
        }
    }

    // Read and check each expected input's manifest.
    let mut manifests: BTreeMap<String, Manifest> = BTreeMap::new();
    for input in &interface.inputs {
        let Some(&path) = given.get(input.name.as_str()) else {
            errors.push(StagingError::MissingInput(input.name.clone()));
            continue;
        };
        match load_manifest(&input.name, path, input.cardinality).await {
            Ok(manifest) => {
                manifests.insert(input.name.clone(), manifest);
            }
            Err(mut found) => errors.append(&mut found),
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // All checks passed: lay out the run directory.
    let layout = write_run_dir(&run_dir, &manifests, stub).await;
    let staged_inputs = layout.map_err(|error| vec![StagingError::Io(format!("{error:#}"))])?;
    Ok(StagedRun {
        project,
        run_dir,
        interface,
        inputs: staged_inputs,
    })
}

fn absolute(path: &Path) -> Result<PathBuf, StagingError> {
    std::path::absolute(path).map_err(|error| StagingError::Io(error.to_string()))
}

async fn read_interface(project: &Path) -> Result<JobInterface, StagingError> {
    let path = project.join("job.json");
    let bad = |reason: String| StagingError::BadInterface {
        path: path.display().to_string(),
        reason,
    };
    let text = fs::read_to_string(&path)
        .await
        .map_err(|error| bad(error.to_string()))?;
    serde_json::from_str(&text).map_err(|error| bad(error.to_string()))
}

async fn is_non_empty_dir(dir: &Path) -> bool {
    match fs::read_dir(dir).await {
        Ok(mut entries) => matches!(entries.next_entry().await, Ok(Some(_))),
        Err(_) => false, // doesn't exist (or isn't a directory): fine
    }
}

/// Read a manifest, check its shape, make relative paths absolute (relative
/// to the manifest's own directory), and check every file exists.
async fn load_manifest(
    name: &str,
    path: &Path,
    cardinality: protocol::types::Cardinality,
) -> Result<Manifest, Vec<StagingError>> {
    let bad = |reason: String| {
        vec![StagingError::BadManifest {
            name: name.to_string(),
            path: path.display().to_string(),
            reason,
        }]
    };
    let text = fs::read_to_string(path)
        .await
        .map_err(|error| bad(error.to_string()))?;
    let manifest =
        Manifest::from_csv(&text, cardinality).map_err(|error| bad(format!("{error:#}")))?;

    let base = path.parent().unwrap_or(Path::new("."));
    let base = absolute(base).map_err(|error| vec![error])?;
    let resolve = |file: &str| base.join(file).display().to_string();

    let manifest = match manifest {
        Manifest::PerSample(samples) => Manifest::PerSample(
            samples
                .into_iter()
                .map(|sample| SampleFiles {
                    sample: sample.sample,
                    paths: sample.paths.iter().map(|file| resolve(file)).collect(),
                })
                .collect(),
        ),
        Manifest::PerDataset(paths) => {
            Manifest::PerDataset(paths.iter().map(|file| resolve(file)).collect())
        }
    };

    let files: Vec<&String> = match &manifest {
        Manifest::PerSample(samples) => samples.iter().flat_map(|s| &s.paths).collect(),
        Manifest::PerDataset(paths) => paths.iter().collect(),
    };
    if files.is_empty() {
        return Err(bad("lists no files".to_string()));
    }
    let mut errors = Vec::new();
    for file in files {
        if !fs::try_exists(file).await.unwrap_or(false) {
            errors.push(StagingError::MissingFile {
                name: name.to_string(),
                file: file.clone(),
            });
        }
    }
    if errors.is_empty() {
        Ok(manifest)
    } else {
        Err(errors)
    }
}

async fn write_run_dir(
    run_dir: &Path,
    manifests: &BTreeMap<String, Manifest>,
    stub: bool,
) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    for sub in ["inputs", "work", "results", "logs"] {
        fs::create_dir_all(run_dir.join(sub)).await?;
    }
    if stub {
        fs::write(run_dir.join("stub.config"), "docker.enabled = false\n").await?;
    }
    let mut staged = BTreeMap::new();
    for (name, manifest) in manifests {
        let path = run_dir.join("inputs").join(format!("{name}.csv"));
        fs::write(&path, manifest.to_csv()?).await?;
        staged.insert(name.clone(), path);
    }
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project directory holding the compiler's real job.json for the
    /// fastqc → multiqc canvas: one input, `fastqc_1__reads`, per sample.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let job_json = include_str!("../../compiler/testdata/expected/fastqc_multiqc/job.json");
        std::fs::write(dir.path().join("job.json"), job_json).unwrap();
        dir
    }

    /// Two read files plus a manifest listing them by relative path.
    fn reads(dir: &Path) -> PathBuf {
        std::fs::write(dir.join("S1_R1.fq"), "@r\n").unwrap();
        std::fs::write(dir.join("S1_R2.fq"), "@r\n").unwrap();
        let manifest = dir.join("reads.csv");
        std::fs::write(&manifest, "sample,path\nS1,S1_R1.fq\nS1,S1_R2.fq\n").unwrap();
        manifest
    }

    fn input(name: &str, manifest: &Path) -> InputArg {
        InputArg {
            name: name.to_string(),
            manifest: manifest.to_path_buf(),
        }
    }

    #[test]
    fn parses_input_arguments() {
        assert_eq!(
            InputArg::parse("fastqc_1__reads=reads.csv"),
            Ok(input("fastqc_1__reads", Path::new("reads.csv")))
        );
        assert!(InputArg::parse("no-equals-sign").is_err());
        assert!(InputArg::parse("=reads.csv").is_err());
    }

    #[tokio::test]
    async fn stages_a_valid_job() {
        let project = project();
        let data = tempfile::tempdir().unwrap();
        let manifest = reads(data.path());
        let run = tempfile::tempdir().unwrap();
        let run_dir = run.path().join("run");

        let staged = stage(
            project.path(),
            &[input("fastqc_1__reads", &manifest)],
            &run_dir,
            false,
        )
        .await
        .unwrap();

        for sub in ["inputs", "work", "results", "logs"] {
            assert!(run_dir.join(sub).is_dir(), "{sub}/ was not created");
        }
        // The staged manifest has the relative paths made absolute.
        let staged_csv = std::fs::read_to_string(&staged.inputs["fastqc_1__reads"]).unwrap();
        let r1 = data.path().join("S1_R1.fq");
        assert!(
            staged_csv.contains(&format!("S1,{}", r1.display())),
            "{staged_csv}"
        );

        let args = staged.nextflow_args(false);
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--fastqc_1__reads".to_string()));
        assert!(!args.contains(&"-stub-run".to_string()));
    }

    #[tokio::test]
    async fn stub_runs_get_a_config_that_turns_docker_off() {
        let project = project();
        let data = tempfile::tempdir().unwrap();
        let manifest = reads(data.path());
        let run = tempfile::tempdir().unwrap();

        let staged = stage(
            project.path(),
            &[input("fastqc_1__reads", &manifest)],
            run.path(),
            true,
        )
        .await
        .unwrap();

        let config = std::fs::read_to_string(run.path().join("stub.config")).unwrap();
        assert_eq!(config, "docker.enabled = false\n");
        assert!(
            staged
                .nextflow_args(true)
                .contains(&"-stub-run".to_string())
        );
    }

    #[tokio::test]
    async fn reports_every_problem_and_writes_nothing() {
        let project = project();
        let data = tempfile::tempdir().unwrap();
        let manifest = reads(data.path());
        let run = tempfile::tempdir().unwrap();
        let run_dir = run.path().join("run");

        // The required input is missing, and an unknown one is given twice.
        let errors = stage(
            project.path(),
            &[input("typo", &manifest), input("typo", &manifest)],
            &run_dir,
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(
            errors,
            vec![
                StagingError::DuplicateInput("typo".to_string()),
                StagingError::UnknownInput("typo".to_string()),
                StagingError::MissingInput("fastqc_1__reads".to_string()),
            ]
        );
        assert!(!run_dir.exists(), "nothing is written when staging fails");
    }

    #[tokio::test]
    async fn rejects_a_manifest_of_the_wrong_shape() {
        let project = project();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("a.zip"), "").unwrap();
        let manifest = data.path().join("per_dataset.csv");
        std::fs::write(&manifest, "path\na.zip\n").unwrap();
        let run = tempfile::tempdir().unwrap();

        let errors = stage(
            project.path(),
            &[input("fastqc_1__reads", &manifest)],
            &run.path().join("run"),
            false,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&errors[..], [StagingError::BadManifest { reason, .. }]
                if reason.contains("expected [\"sample\", \"path\"]")),
            "{errors:?}"
        );
    }

    #[tokio::test]
    async fn reports_each_missing_file() {
        let project = project();
        let data = tempfile::tempdir().unwrap();
        let manifest = data.path().join("reads.csv");
        std::fs::write(&manifest, "sample,path\nS1,gone_R1.fq\nS1,gone_R2.fq\n").unwrap();
        let run = tempfile::tempdir().unwrap();

        let errors = stage(
            project.path(),
            &[input("fastqc_1__reads", &manifest)],
            &run.path().join("run"),
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors
                .iter()
                .all(|e| matches!(e, StagingError::MissingFile { .. }))
        );
    }

    #[tokio::test]
    async fn refuses_a_non_empty_run_directory() {
        let project = project();
        let data = tempfile::tempdir().unwrap();
        let manifest = reads(data.path());
        let run = tempfile::tempdir().unwrap();
        std::fs::write(run.path().join("leftover.txt"), "old run").unwrap();

        let errors = stage(
            project.path(),
            &[input("fastqc_1__reads", &manifest)],
            run.path(),
            false,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&errors[..], [StagingError::RunDirNotEmpty(_)]),
            "{errors:?}"
        );
    }

    #[tokio::test]
    async fn a_project_without_job_json_is_rejected_first() {
        let empty = tempfile::tempdir().unwrap();
        let run = tempfile::tempdir().unwrap();
        let errors = stage(empty.path(), &[], &run.path().join("run"), false)
            .await
            .unwrap_err();
        assert!(
            matches!(&errors[..], [StagingError::BadInterface { .. }]),
            "{errors:?}"
        );
    }
}
