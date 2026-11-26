//! Writing a leased job to disk (DESIGN.md §4.1, "the agent writes all of
//! this to disk in the layout local mode reads").
//!
//! A lease is a job description in a message; local mode reads a job from
//! directories. This module turns the first into the second, so `agent
//! serve` can run the same stage → run → collect code as `agent run`:
//!
//! ```text
//! <work dir>/jobs/<job_id>/<attempt>/
//!   lease.json           the lease as received, for debugging
//!   project/             the project's files, from its tree
//!   manifests/<name>.csv each resolved input, as a manifest
//!   run/                 the run directory (filled by staging)
//! ```
//!
//! Each attempt gets its own directory. Leases are at-least-once: an agent
//! cut off by a network partition may still be running attempt 1 when
//! attempt 2 starts, and the two must never share files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use protocol::blob::{BlobError, Tree};
use protocol::job::JobInterface;
use protocol::lease::{LeaseResponse, ResolvedInput};
use protocol::manifest::{Manifest, SampleFiles};
use protocol::types::Cardinality;
use tokio::fs;

use crate::blobs::{BlobCache, BlobSource};
use crate::stage::InputArg;

/// A leased job, on disk and ready for [`crate::stage::stage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedJob {
    /// `jobs/<job_id>/<attempt>/`
    pub dir: PathBuf,
    pub project: PathBuf,
    /// One per job input, pointing into `manifests/`.
    pub inputs: Vec<InputArg>,
    /// Empty: staging fills it.
    pub run_dir: PathBuf,
}

/// Why a lease couldn't be prepared. The two kinds get different answers
/// in `agent serve`: a lease that contradicts itself will fail the same way
/// on every agent, so retrying it is pointless; a failed download or a
/// full disk might not happen next time.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    /// The lease is wrong: a bug in the control plane, or a tampered
    /// message. Every problem is listed.
    #[error("the lease is not usable: {}", .0.join("; "))]
    BadLease(Vec<String>),
    /// Fetching blobs or writing files failed.
    #[error(transparent)]
    Io(#[from] anyhow::Error),
}

/// Check the lease, then fetch its project (through the cache) and write
/// the job's directory. Nothing is written until the lease's own fields
/// have been checked.
pub async fn prepare(
    work_dir: &Path,
    lease: &LeaseResponse,
    cache: &BlobCache,
    source: &impl BlobSource,
) -> Result<PreparedJob, PrepareError> {
    let manifests = check_lease(lease).map_err(PrepareError::BadLease)?;

    let dir = work_dir
        .join("jobs")
        .join(&lease.lease.job_id)
        .join(lease.lease.attempt.to_string());
    let project = dir.join("project");
    let manifests_dir = dir.join("manifests");
    let run_dir = dir.join("run");

    // A new attempt number comes with every lease, so this directory can
    // only exist if something is badly wrong (two agents sharing a work
    // directory, or a reused attempt). Refuse rather than mix files.
    let exists = fs::try_exists(&dir)
        .await
        .map_err(|error| anyhow::anyhow!("checking {}: {error}", dir.display()))?;
    if exists {
        return Err(PrepareError::Io(anyhow::anyhow!(
            "{} already exists",
            dir.display()
        )));
    }
    for sub in [&project, &manifests_dir, &run_dir] {
        create_dir(sub).await?;
    }
    let lease_json = serde_json::to_vec_pretty(lease).map_err(anyhow::Error::from)?;
    write(&dir.join("lease.json"), &lease_json).await?;

    // The tree, then every file it lists. Each comes through the cache, so
    // each is verified against its digest before it is used.
    let tree_bytes = cache.get(&lease.project, source).await?;
    let tree = Tree::from_blob_bytes(&tree_bytes).map_err(bad_tree)?;
    check_job_json(&tree, &lease.interface, cache, source).await?;
    for (path, blob) in &tree.files {
        let bytes = cache.get(blob, source).await?;
        // Safe to join: `from_blob_bytes` checked every path stays inside.
        let dest = project.join(path);
        if let Some(parent) = dest.parent() {
            create_dir(parent).await?;
        }
        write(&dest, &bytes).await?;
    }

    let mut inputs = Vec::new();
    for (name, manifest) in manifests {
        let path = manifests_dir.join(format!("{name}.csv"));
        let csv = manifest.to_csv()?;
        write(&path, csv.as_bytes()).await?;
        inputs.push(InputArg {
            name,
            manifest: path,
        });
    }

    Ok(PreparedJob {
        dir,
        project,
        inputs,
        run_dir,
    })
}

/// Check what can be checked without I/O, and build each input's
/// manifest. Staging checks the manifests again against `job.json` and the
/// disk; this catches a lease that is wrong in itself, and names that
/// would become paths.
fn check_lease(lease: &LeaseResponse) -> Result<BTreeMap<String, Manifest>, Vec<String>> {
    let mut errors = Vec::new();

    // The job id and input names become directory and file names.
    if !is_safe_name(&lease.lease.job_id, &['-']) {
        errors.push(format!(
            "job id `{}` is not a safe directory name",
            lease.lease.job_id
        ));
    }

    let mut manifests = BTreeMap::new();
    for input in &lease.interface.inputs {
        let Some(resolved) = lease.inputs.get(&input.name) else {
            errors.push(format!("input `{}` has no files", input.name));
            continue;
        };
        if resolved.cardinality != input.cardinality {
            errors.push(format!(
                "input `{}` is {:?} in the interface but {:?} in the lease",
                input.name, input.cardinality, resolved.cardinality
            ));
            continue;
        }
        match manifest_for(&input.name, resolved) {
            Ok(manifest) => {
                manifests.insert(input.name.clone(), manifest);
            }
            Err(mut found) => errors.append(&mut found),
        }
    }
    for name in lease.inputs.keys() {
        if !lease
            .interface
            .inputs
            .iter()
            .any(|input| &input.name == name)
        {
            errors.push(format!("`{name}` is not an input of this job"));
        } else if !is_safe_name(name, &['_']) {
            errors.push(format!("input name `{name}` is not a safe file name"));
        }
    }

    if errors.is_empty() {
        Ok(manifests)
    } else {
        Err(errors)
    }
}

/// One input's files as a manifest. Per-sample files are grouped by
/// sample in the order they arrive, so paired reads stay R1 then R2.
fn manifest_for(name: &str, resolved: &ResolvedInput) -> Result<Manifest, Vec<String>> {
    if resolved.files.is_empty() {
        return Err(vec![format!("input `{name}` has no files")]);
    }
    let mut errors = Vec::new();
    let manifest = match resolved.cardinality {
        Cardinality::PerSample => {
            let mut samples: Vec<SampleFiles> = Vec::new();
            for file in &resolved.files {
                let Some(sample) = &file.sample else {
                    errors.push(format!(
                        "input `{name}` is per sample, but {} has no sample",
                        file.uri
                    ));
                    continue;
                };
                // `iter_mut().find` gives a mutable reference into the
                // vector, so the path is pushed onto the existing entry.
                match samples.iter_mut().find(|s| &s.sample == sample) {
                    Some(entry) => entry.paths.push(file.uri.clone()),
                    None => samples.push(SampleFiles {
                        sample: sample.clone(),
                        paths: vec![file.uri.clone()],
                    }),
                }
            }
            Manifest::PerSample(samples)
        }
        Cardinality::PerDataset => {
            for file in &resolved.files {
                if let Some(sample) = &file.sample {
                    errors.push(format!(
                        "input `{name}` is per dataset, but {} belongs to sample `{sample}`",
                        file.uri
                    ));
                }
            }
            Manifest::PerDataset(resolved.files.iter().map(|f| f.uri.clone()).collect())
        }
    };
    if errors.is_empty() {
        Ok(manifest)
    } else {
        Err(errors)
    }
}

/// The lease carries the interface so the agent can check inputs before
/// fetching anything; the project carries it as `job.json`, which staging
/// reads. They must agree, or the inputs checked aren't the ones run.
async fn check_job_json(
    tree: &Tree,
    interface: &JobInterface,
    cache: &BlobCache,
    source: &impl BlobSource,
) -> Result<(), PrepareError> {
    let Some(blob) = tree.files.get("job.json") else {
        return Err(PrepareError::BadLease(vec![
            "the project has no job.json".to_string(),
        ]));
    };
    let bytes = cache.get(blob, source).await?;
    let in_project: JobInterface = serde_json::from_slice(&bytes)
        .map_err(|error| PrepareError::BadLease(vec![format!("job.json: {error}")]))?;
    if &in_project != interface {
        return Err(PrepareError::BadLease(vec![
            "the project's job.json differs from the lease's interface".to_string(),
        ]));
    }
    Ok(())
}

/// Letters, digits and the given extra characters, at most 128 of them:
/// nothing that could be `..`, `/` or empty.
fn is_safe_name(name: &str, extra: &[char]) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

fn bad_tree(error: BlobError) -> PrepareError {
    PrepareError::BadLease(vec![format!("the project tree: {error}")])
}

async fn create_dir(dir: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))
}

async fn write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    fs::write(path, bytes)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use protocol::blob::BlobRef;
    use protocol::digest::HashAlgorithm;
    use protocol::lease::{InputFile, Lease, RunRef, Settings};
    use protocol::version::FormatVersion;

    use super::*;
    use crate::blobs::testing::MemorySource;
    use crate::stage;

    const SHA: HashAlgorithm = HashAlgorithm::Sha256;
    const EXPECTED: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../compiler/testdata/expected/fastqc_multiqc"
    );

    /// The compiler's real output for the fastqc → multiqc canvas, as the
    /// API would store it: its blobs in a source, its tree's reference.
    fn project() -> (BlobRef, JobInterface, MemorySource) {
        let mut files = BTreeMap::new();
        for name in ["job.json", "main.nf", "nextflow.config"] {
            let bytes = std::fs::read(Path::new(EXPECTED).join(name)).unwrap();
            files.insert(name.to_string(), bytes);
        }
        // A file in a subdirectory, as modules are.
        files.insert(
            "modules/fastqc/main.nf".to_string(),
            b"process FASTQC {}\n".to_vec(),
        );
        let interface = serde_json::from_slice(&files["job.json"]).unwrap();

        let (tree, blobs) = Tree::from_files(SHA, files).unwrap();
        let tree_blob = tree.to_blob(SHA);
        let mut source = MemorySource::default();
        for blob in blobs {
            source.add(&blob.reference, blob.bytes);
        }
        source.add(&tree_blob.reference, tree_blob.bytes);
        (tree_blob.reference, interface, source)
    }

    fn file(sample: Option<&str>, uri: &Path) -> InputFile {
        InputFile {
            sample: sample.map(str::to_string),
            uri: uri.display().to_string(),
            digest: protocol::digest::digest_bytes(SHA, b"@r\n"),
            size: 3,
        }
    }

    fn lease(project: BlobRef, interface: JobInterface, files: Vec<InputFile>) -> LeaseResponse {
        LeaseResponse {
            format_version: FormatVersion::current(),
            lease: Lease {
                job_id: "0b7e5a1c-3f2d-4c8e-9a6b-1d2e3f4a5b6c".to_string(),
                attempt: 1,
                expires_at: "2026-11-26T20:00:00Z".to_string(),
                heartbeat_every_secs: 20,
            },
            run: RunRef {
                run_id: "run".to_string(),
                job_index: 0,
            },
            settings: Settings {
                nextflow_version: "26.10.5".to_string(),
                hash_algorithm: SHA,
                time_limit_secs: None,
            },
            project,
            interface,
            inputs: BTreeMap::from([(
                "fastqc_1__reads".to_string(),
                ResolvedInput {
                    cardinality: Cardinality::PerSample,
                    files,
                },
            )]),
        }
    }

    /// Paired reads for two samples, interleaved as a server might list them.
    fn reads(data: &Path) -> Vec<InputFile> {
        for name in ["gut_1.fq", "gut_2.fq", "liver_1.fq", "liver_2.fq"] {
            std::fs::write(data.join(name), "@r\n").unwrap();
        }
        vec![
            file(Some("gut"), &data.join("gut_1.fq")),
            file(Some("liver"), &data.join("liver_1.fq")),
            file(Some("gut"), &data.join("gut_2.fq")),
            file(Some("liver"), &data.join("liver_2.fq")),
        ]
    }

    #[tokio::test]
    async fn writes_a_job_that_staging_accepts() {
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(work.path().join("cache"));
        let (tree, interface, source) = project();
        let lease = lease(tree, interface, reads(data.path()));

        let job = prepare(work.path(), &lease, &cache, &source).await.unwrap();

        assert_eq!(
            job.dir,
            work.path()
                .join("jobs/0b7e5a1c-3f2d-4c8e-9a6b-1d2e3f4a5b6c/1")
        );
        // The project's files, byte for byte.
        for name in ["job.json", "main.nf", "nextflow.config"] {
            assert_eq!(
                std::fs::read(job.project.join(name)).unwrap(),
                std::fs::read(Path::new(EXPECTED).join(name)).unwrap(),
                "{name}"
            );
        }
        assert!(job.project.join("modules/fastqc/main.nf").exists());
        assert!(job.dir.join("lease.json").exists());

        // Samples grouped, each sample's files in order.
        let manifest = std::fs::read_to_string(&job.inputs[0].manifest).unwrap();
        let d = data.path().display();
        assert_eq!(
            manifest,
            format!(
                "sample,path\ngut,{d}/gut_1.fq\ngut,{d}/gut_2.fq\n\
                 liver,{d}/liver_1.fq\nliver,{d}/liver_2.fq\n"
            )
        );

        // And the unchanged local-mode staging takes it from here.
        let staged = stage::stage(&job.project, &job.inputs, &job.run_dir, true)
            .await
            .unwrap();
        assert_eq!(staged.run_dir, job.run_dir);
    }

    #[tokio::test]
    async fn a_second_job_with_the_same_project_fetches_nothing() {
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(work.path().join("cache"));
        let (tree, interface, source) = project();
        let mut lease = lease(tree, interface, reads(data.path()));
        prepare(work.path(), &lease, &cache, &source).await.unwrap();
        let fetched = source.fetch_count();

        lease.lease.attempt = 2;
        prepare(work.path(), &lease, &cache, &source).await.unwrap();

        assert_eq!(source.fetch_count(), fetched);
    }

    #[tokio::test]
    async fn reports_every_problem_in_the_lease_and_writes_nothing() {
        let work = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(work.path().join("cache"));
        let (tree, interface, source) = project();
        let mut lease = lease(tree, interface, vec![file(None, Path::new("/data/x.fq"))]);
        lease.lease.job_id = "../../etc".to_string();
        lease.inputs.insert(
            "extra".to_string(),
            ResolvedInput {
                cardinality: Cardinality::PerDataset,
                files: vec![],
            },
        );

        let err = prepare(work.path(), &lease, &cache, &source)
            .await
            .unwrap_err();

        let PrepareError::BadLease(errors) = err else {
            panic!("expected a bad lease, got {err}");
        };
        assert_eq!(errors.len(), 3, "{errors:#?}");
        assert!(errors[0].contains("not a safe directory name"));
        assert!(errors[1].contains("has no sample"));
        assert!(errors[2].contains("not an input of this job"));
        assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
        assert_eq!(source.fetch_count(), 0);
    }

    #[tokio::test]
    async fn a_cardinality_mismatch_is_a_bad_lease() {
        let work = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(work.path().join("cache"));
        let (tree, interface, source) = project();
        let mut lease = lease(tree, interface, vec![file(None, Path::new("/data/x.fq"))]);
        lease.inputs.get_mut("fastqc_1__reads").unwrap().cardinality = Cardinality::PerDataset;

        let err = prepare(work.path(), &lease, &cache, &source)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("PerSample in the interface"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_job_json_that_disagrees_with_the_lease_is_refused() {
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(work.path().join("cache"));
        let (tree, mut interface, source) = project();
        // The lease claims an output the project doesn't produce.
        let mut extra = interface.outputs[0].clone();
        extra.name = "fastqc_1__extra".to_string();
        interface.outputs.push(extra);
        let lease = lease(tree, interface, reads(data.path()));

        let err = prepare(work.path(), &lease, &cache, &source)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("differs from"), "{err}");
    }

    #[tokio::test]
    async fn refuses_an_attempt_directory_that_already_exists() {
        let work = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(work.path().join("cache"));
        let (tree, interface, source) = project();
        let lease = lease(tree, interface, reads(data.path()));
        prepare(work.path(), &lease, &cache, &source).await.unwrap();

        let err = prepare(work.path(), &lease, &cache, &source)
            .await
            .unwrap_err();
        assert!(matches!(err, PrepareError::Io(_)));
        assert!(err.to_string().contains("already exists"), "{err}");
    }
}
