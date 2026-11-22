//! Shared test fixtures: a compute environment, agents, runs shaped like
//! the test canvas (FastQC, then MultiQC over its reports), the app state,
//! and calling the HTTP API.
//!
//! Compiled only for `cargo test` (`#[cfg(test)]` on the `mod` line).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use compiler::library::load_library;
use compiler::spec::SpecRegistry;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use protocol::blob::BlobRef;
use protocol::digest::{HashAlgorithm, digest_bytes};
use protocol::job::{InputSource, JobId, JobInput, JobInterface, JobOutput};
use protocol::lease::{InputFile, ResolvedInput};
use protocol::result::{JobResult, JobStatus, OutputFile, OutputResult};
use protocol::types::{Cardinality, SemanticType};
use protocol::version::FormatVersion;
use sqlx::PgPool;
use uuid::Uuid;

use crate::agents::{self, Agent, NewComputeEnv};
use crate::blobs::store_tree;
use crate::queue::{JobKind, NewJob, NewRun, enqueue_run};
use crate::users::dev_token_digest;
use crate::{AppState, app, node_library};

/// The development token the UI tests' state accepts.
pub const DEV_TOKEN: &str = "dev-token-for-tests";

/// State for tests of the agent endpoints: no node library, no
/// development token.
pub fn bare_state(pool: &PgPool) -> AppState {
    AppState {
        pool: pool.clone(),
        registry: Arc::new(SpecRegistry::default()),
        dev_token: None,
    }
}

/// State as the server builds it: the repo's node library published and
/// read back, and [`DEV_TOKEN`] accepted.
pub async fn ui_state(pool: &PgPool) -> AppState {
    let nodes = load_library(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nodes")).unwrap();
    node_library::sync(pool, nodes, SHA).await.unwrap();
    AppState {
        pool: pool.clone(),
        registry: Arc::new(node_library::load_registry(pool).await.unwrap()),
        dev_token: Some(dev_token_digest(DEV_TOKEN)),
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub bytes: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap()
    }
}

/// One request straight into the router (no network or port involved),
/// with `token` as the bearer token.
pub async fn call(
    state: &AppState,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = app(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

/// The compiler's test canvas: FastQC, then MultiQC over its reports.
pub fn test_canvas() -> Value {
    let text = include_str!("../../compiler/testdata/fastqc_multiqc.canvas.json");
    serde_json::from_str(text).unwrap()
}

/// A digest as JSON text, for request bodies.
pub fn digest_of(text: &str) -> String {
    digest_bytes(SHA, text.as_bytes()).to_string()
}

/// Register the paired reads of sample `gut` on `agent_id`, as
/// `scripts/register-dataset.sh` would.
pub async fn register_reads(state: &AppState, agent_id: Uuid) -> Reply {
    let file = |name: &str| json!({ "uri": format!("/data/ggal/{name}"), "digest": digest_of(name), "size": 100 });
    let body = json!({
        "name": "ggal reads",
        "location": { "agent_id": agent_id },
        "samples": [{ "sample": "gut", "files": [file("ggal_gut_1.fq"), file("ggal_gut_2.fq")] }]
    });
    call(state, "POST", "/api/datasets", Some(DEV_TOKEN), Some(body)).await
}

pub const SHA: HashAlgorithm = HashAlgorithm::Sha256;
pub const ENV: &str = "laptop";

pub struct TestAgent {
    pub agent: Agent,
    pub token: String,
}

pub async fn create_env(pool: &PgPool, id: &str) {
    agents::create_compute_env(
        pool,
        &NewComputeEnv {
            id: id.to_string(),
            nextflow_version: "26.10.5".to_string(),
            time_limit_secs: Some(3600),
        },
    )
    .await
    .unwrap();
}

pub async fn create_agent(pool: &PgPool, env: &str, name: &str) -> TestAgent {
    let (id, token) = agents::create_agent(pool, env, name).await.unwrap();
    TestAgent {
        agent: Agent {
            id,
            compute_env: env.to_string(),
        },
        token,
    }
}

/// The `laptop` environment with one agent.
pub async fn setup(pool: &PgPool) -> TestAgent {
    create_env(pool, ENV).await;
    create_agent(pool, ENV, "dev").await
}

/// A small project, stored; `label` makes each project's files distinct.
pub async fn store_project(pool: &PgPool, label: &str) -> BlobRef {
    let mut tx = pool.begin().await.unwrap();
    let files = BTreeMap::from([
        ("main.nf".to_string(), format!("// {label}\n").into_bytes()),
        (
            "modules/fastqc/main.nf".to_string(),
            b"process FASTQC {}\n".to_vec(),
        ),
    ]);
    let tree = store_tree(&mut tx, SHA, files).await.unwrap();
    tx.commit().await.unwrap();
    tree
}

fn reads_input() -> JobInput {
    JobInput {
        name: "fastqc_1__reads".to_string(),
        cardinality: Cardinality::PerSample,
        ty: SemanticType::Fastq { paired: true },
        source: InputSource::Run {
            node: "fastqc_1".to_string(),
            port: "reads".to_string(),
        },
    }
}

/// The dataset's reads, as launch resolves them: R1 then R2.
pub fn resolved_reads() -> BTreeMap<String, ResolvedInput> {
    let file = |name: &str| InputFile {
        sample: Some("gut".to_string()),
        uri: format!("/data/ggal/{name}"),
        digest: digest_bytes(SHA, name.as_bytes()),
        size: 100,
    };
    BTreeMap::from([(
        "fastqc_1__reads".to_string(),
        ResolvedInput {
            cardinality: Cardinality::PerSample,
            files: vec![file("ggal_gut_1.fq"), file("ggal_gut_2.fq")],
        },
    )])
}

/// A FastQC job: reads from the run, a QC report per sample out.
pub fn fastqc_job(tree: &BlobRef) -> NewJob {
    NewJob {
        kind: JobKind::Batch,
        project_tree: tree.clone(),
        interface: JobInterface {
            format_version: FormatVersion::current(),
            inputs: vec![reads_input()],
            outputs: vec![JobOutput {
                name: "fastqc_1__zip".to_string(),
                node: "fastqc_1".to_string(),
                port: "zip".to_string(),
                cardinality: Cardinality::PerSample,
                ty: SemanticType::QcReport,
            }],
        },
        run_inputs: resolved_reads(),
        depends_on: vec![],
        pinned_agent: None,
    }
}

/// A MultiQC job reading the `zip` output of each job in `upstream`.
pub fn multiqc_job(tree: &BlobRef, upstream: &[usize]) -> NewJob {
    let inputs = upstream
        .iter()
        .map(|&index| JobInput {
            name: format!("fastqc_{}__zip", index + 1),
            cardinality: Cardinality::PerSample,
            ty: SemanticType::QcReport,
            source: InputSource::Job {
                job: JobId(index),
                node: format!("fastqc_{}", index + 1),
                port: "zip".to_string(),
            },
        })
        .collect();
    NewJob {
        kind: JobKind::Batch,
        project_tree: tree.clone(),
        interface: JobInterface {
            format_version: FormatVersion::current(),
            inputs,
            outputs: vec![],
        },
        run_inputs: BTreeMap::new(),
        depends_on: upstream.to_vec(),
        pinned_agent: None,
    }
}

pub async fn launch(pool: &PgPool, jobs: Vec<NewJob>) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    let run_id = enqueue_run(
        &mut tx,
        &NewRun {
            compute_env: ENV.to_string(),
            jobs,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    run_id
}

/// The test canvas's shape: FastQC (job 0), then MultiQC (job 1).
pub async fn launch_fastqc_multiqc(pool: &PgPool) -> Uuid {
    let tree = store_project(pool, "fastqc-multiqc").await;
    launch(pool, vec![fastqc_job(&tree), multiqc_job(&tree, &[0])]).await
}

pub async fn job_state(pool: &PgPool, run_id: Uuid, index: i32) -> String {
    sqlx::query_scalar("SELECT state FROM jobs WHERE run_id = $1 AND job_index = $2")
        .bind(run_id)
        .bind(index)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The run's state, and whether it has finished.
pub async fn run_state(pool: &PgPool, run_id: Uuid) -> (String, bool) {
    sqlx::query_as("SELECT state, finished_at IS NOT NULL FROM runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Pretend the agent went silent long ago.
pub async fn expire_lease(pool: &PgPool, job_id: Uuid) {
    sqlx::query("UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
}

/// Skip a retry backoff.
pub async fn skip_backoff(pool: &PgPool, job_id: Uuid) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
        .bind(job_id)
        .execute(pool)
        .await
        .unwrap();
}

pub fn result(status: JobStatus) -> JobResult {
    JobResult {
        format_version: FormatVersion::current(),
        status,
        tasks: vec![],
        outputs: vec![],
        warnings: vec![],
    }
}

/// A FastQC job's success: one report for sample `gut`.
pub fn fastqc_succeeded(output_name: &str) -> JobResult {
    let mut result = result(JobStatus::Succeeded);
    result.outputs = vec![OutputResult {
        name: output_name.to_string(),
        cardinality: Cardinality::PerSample,
        ty: SemanticType::QcReport,
        manifest: format!("outputs/{output_name}.csv"),
        files: vec![OutputFile {
            sample: Some("gut".to_string()),
            path: format!("/runs/r1/results/{output_name}/gut/gut_fastqc.zip"),
            size: 42,
            digest: digest_bytes(SHA, output_name.as_bytes()),
        }],
    }];
    result
}
