//! Tests for `runs`, in a file of their own because there are many.
//! `mod tests;` at the end of `runs.rs` makes this file the module
//! `runs::tests`, so `super::*` still reaches the private items there.

use axum::http::StatusCode;
use protocol::blob::Tree;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::AppState;
use crate::agents::Agent;
use crate::queue::{self, Completion};
use crate::testing::*;

/// Save the test canvas as a pipeline; returns its version id.
async fn save_pipeline(state: &AppState, canvas: Value) -> String {
    let body = Some(json!({ "name": "QC", "canvas": canvas }));
    let reply = call(state, "POST", "/api/pipelines", Some(DEV_TOKEN), body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    reply.json()["pipeline_version_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn launch_run(state: &AppState, body: Value) -> Reply {
    call(state, "POST", "/api/runs", Some(DEV_TOKEN), Some(body)).await
}

async fn get_run(state: &AppState, run_id: &str) -> Value {
    let reply = call(
        state,
        "GET",
        &format!("/api/runs/{run_id}"),
        Some(DEV_TOKEN),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()
}

/// A pipeline, a dataset on the `laptop` agent, and a launch request for
/// them: everything a run needs.
struct Ready {
    state: AppState,
    agent: TestAgent,
    launch: Value,
}

async fn ready(pool: &PgPool) -> Ready {
    let state = ui_state(pool).await;
    let agent = setup(pool).await;
    let version = save_pipeline(&state, test_canvas()).await;
    let dataset = register_reads(&state, agent.agent.id).await.json()["dataset_id"].clone();
    let launch = json!({
        "pipeline_version_id": version,
        "compute_env": ENV,
        "inputs": { "fastqc_1__reads": dataset }
    });
    Ready {
        state,
        agent,
        launch,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_launched_run_is_leased_with_its_compiled_project(pool: PgPool) {
    let Ready {
        state,
        agent,
        launch,
    } = ready(&pool).await;

    let reply = launch_run(&state, launch.clone()).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let run_id = reply.json()["run_id"].as_str().unwrap().to_string();
    let run: Uuid = run_id.parse().unwrap();

    // FastQC and MultiQC are both batch nodes, so the plan is one segment.
    assert_eq!(run_state(&pool, run).await, ("running".to_string(), false));
    let (jobs, version): (i64, Option<Uuid>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM jobs WHERE run_id = r.id), r.pipeline_version_id
         FROM runs r WHERE r.id = $1",
    )
    .bind(run)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 1);
    assert_eq!(json!(version), launch["pipeline_version_id"]);
    assert_eq!(job_state(&pool, run, 0).await, "queued");
    // Provenance: which dataset fed the input.
    let fed: Uuid = sqlx::query_scalar("SELECT dataset_id FROM run_inputs WHERE run_id = $1")
        .bind(run)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(json!(fed), launch["inputs"]["fastqc_1__reads"]);

    // The agent's view: the lease carries the dataset's files, R1 then R2,
    // and the project tree lists what the compiler generated.
    let lease = queue::acquire(&pool, &agent.agent, "0.1.0")
        .await
        .unwrap()
        .expect("the launched job is queued for this agent");
    assert_eq!(lease.run.run_id, run_id);
    let reads = &lease.inputs["fastqc_1__reads"];
    let uris: Vec<&str> = reads.files.iter().map(|f| f.uri.as_str()).collect();
    assert_eq!(
        uris,
        ["/data/ggal/ggal_gut_1.fq", "/data/ggal/ggal_gut_2.fq"]
    );
    assert_eq!(reads.files[0].sample.as_deref(), Some("gut"));
    assert_eq!(
        reads.files[0].digest.to_string(),
        digest_of("ggal_gut_1.fq")
    );

    let tree_bytes: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM blobs WHERE digest = $1")
        .bind(lease.project.digest.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    lease.project.verify(&tree_bytes).unwrap();
    let tree = Tree::from_blob_bytes(&tree_bytes).unwrap();
    let paths: Vec<&str> = tree.files.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        [
            "job.json",
            "main.nf",
            "modules/fastqc/0.1.0/main.nf",
            "modules/multiqc/0.1.0/main.nf",
            "nextflow.config"
        ]
    );
    // The module blob is the one the node library published at startup:
    // stored once, shared by every project that uses it.
    let modules: i64 =
        sqlx::query_scalar("SELECT count(*) FROM node_specs n WHERE n.module_digest = $1")
            .bind(
                tree.files["modules/fastqc/0.1.0/main.nf"]
                    .digest
                    .to_string(),
            )
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(modules, 1);

    // The job is pinned to the agent whose disk holds the reads.
    let pinned: Option<Uuid> =
        sqlx::query_scalar("SELECT pinned_agent FROM jobs WHERE run_id = $1::uuid")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pinned, Some(agent.agent.id));

    // The agent reports success, and the run is done: launch fed a real
    // queue.
    let job_id = lease.lease.job_id.parse().unwrap();
    let done = queue::complete(
        &pool,
        &agent.agent,
        job_id,
        lease.lease.attempt,
        &fastqc_succeeded("fastqc_1__zip"),
    )
    .await
    .unwrap();
    assert!(matches!(done, Completion::Recorded(_)));
    assert_eq!(run_state(&pool, run).await, ("succeeded".to_string(), true));
}

#[sqlx::test(migrations = "./migrations")]
async fn launching_twice_reuses_the_stored_project(pool: PgPool) {
    let Ready { state, launch, .. } = ready(&pool).await;
    assert_eq!(
        launch_run(&state, launch.clone()).await.status,
        StatusCode::CREATED
    );
    let blobs = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM blobs")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = blobs().await;
    assert_eq!(launch_run(&state, launch).await.status, StatusCode::CREATED);
    assert_eq!(blobs().await, before, "the same project is the same blobs");
}

#[sqlx::test(migrations = "./migrations")]
async fn launch_lists_every_problem_with_the_request(pool: PgPool) {
    let Ready {
        state, mut launch, ..
    } = ready(&pool).await;
    launch["compute_env"] = json!("nowhere");
    launch["inputs"] = json!({
        "multiqc_1__multiqc_files": Uuid::new_v4(),
    });

    let reply = launch_run(&state, launch).await;

    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let error = &reply.json()["error"];
    assert_eq!(error["code"], "invalid_launch");
    let codes: Vec<&str> = error["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|detail| detail["code"].as_str().unwrap())
        .collect();
    assert_eq!(
        codes,
        ["unknown_compute_env", "missing_input", "unknown_input"]
    );
    assert_eq!(error["details"][1]["input"], "fastqc_1__reads");
    assert!(
        error["details"][0]["message"]
            .as_str()
            .unwrap()
            .contains("`nowhere` does not exist")
    );
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(runs, 0, "nothing is stored for a refused launch");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_dataset_must_be_reachable_from_the_compute_environment(pool: PgPool) {
    let Ready {
        state, mut launch, ..
    } = ready(&pool).await;
    create_env(&pool, "hpc").await;
    let hpc_agent: Agent = create_agent(&pool, "hpc", "hpc-1").await.agent;
    let elsewhere = register_reads(&state, hpc_agent.id).await.json()["dataset_id"].clone();
    launch["inputs"]["fastqc_1__reads"] = elsewhere;

    let reply = launch_run(&state, launch.clone()).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.json()["error"]["details"][0]["code"],
        "dataset_elsewhere"
    );
    assert_eq!(reply.json()["error"]["details"][0]["dataset_env"], "hpc");

    launch["inputs"]["fastqc_1__reads"] = json!(Uuid::new_v4());
    let reply = launch_run(&state, launch).await;
    assert_eq!(
        reply.json()["error"]["details"][0]["code"],
        "unknown_dataset"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn paired_reads_need_two_files_per_sample(pool: PgPool) {
    let Ready {
        state,
        agent,
        mut launch,
    } = ready(&pool).await;
    let body = json!({
        "name": "R1 only",
        "location": { "agent_id": agent.agent.id },
        "samples": [{ "sample": "gut", "files": [
            { "uri": "/data/ggal/ggal_gut_1.fq", "digest": digest_of("r1"), "size": 1 }
        ] }]
    });
    let reply = call(&state, "POST", "/api/datasets", Some(DEV_TOKEN), Some(body)).await;
    launch["inputs"]["fastqc_1__reads"] = reply.json()["dataset_id"].clone();

    let reply = launch_run(&state, launch).await;

    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let detail = &reply.json()["error"]["details"][0];
    assert_eq!(detail["code"], "wrong_file_count");
    assert_eq!(detail["sample"], "gut");
    assert_eq!(detail["expected"], 2);
    assert_eq!(detail["found"], 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_pipeline_version_is_404(pool: PgPool) {
    let Ready {
        state, mut launch, ..
    } = ready(&pool).await;
    let nobody = Uuid::new_v4();
    launch["pipeline_version_id"] = json!(nobody);
    let reply = launch_run(&state, launch).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.json()["error"]["message"],
        "pipeline version not found"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn two_unconnected_nodes_are_two_jobs(pool: PgPool) {
    // Two FastQC nodes with nothing between them: two segments, so two
    // jobs, each with its own input and both queued at once.
    let Ready {
        state, mut launch, ..
    } = ready(&pool).await;
    let canvas = json!({
        "nodes": [
            { "id": "fastqc_1", "spec": "fastqc", "version": "0.1.0" },
            { "id": "fastqc_2", "spec": "fastqc", "version": "0.1.0" }
        ],
        "edges": []
    });
    launch["pipeline_version_id"] = json!(save_pipeline(&state, canvas).await);
    let dataset = launch["inputs"]["fastqc_1__reads"].clone();
    launch["inputs"]["fastqc_2__reads"] = dataset;

    let reply = launch_run(&state, launch).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let run: Uuid = reply.json()["run_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(job_state(&pool, run, 0).await, "queued");
    assert_eq!(job_state(&pool, run, 1).await, "queued");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_run_shows_its_jobs_tasks_and_outputs(pool: PgPool) {
    let Ready {
        state,
        agent,
        launch,
    } = ready(&pool).await;
    let run_id = launch_run(&state, launch.clone()).await.json()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let run = get_run(&state, &run_id).await;
    assert_eq!(run["state"], "running");
    assert_eq!(run["compute_env"], ENV);
    assert_eq!(run["pipeline_version_id"], launch["pipeline_version_id"]);
    assert_eq!(run["inputs"], launch["inputs"]);
    assert_eq!(run["finished_at"], Value::Null);
    assert_eq!(run["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(run["jobs"][0]["state"], "queued");

    let lease = queue::acquire(&pool, &agent.agent, "0.1.0")
        .await
        .unwrap()
        .unwrap();
    let mut result = fastqc_succeeded("fastqc_1__zip");
    result.tasks = vec![protocol::result::TaskRecord {
        task_id: 1,
        hash: "4e/01d43c".to_string(),
        name: "FASTQC_fastqc_1 (gut)".to_string(),
        process: "FASTQC_fastqc_1".to_string(),
        tag: Some("gut".to_string()),
        status: protocol::result::TaskStatus::Completed,
        exit_code: Some(0),
        duration_ms: Some(1200),
        realtime_ms: Some(900),
    }];
    let job_id = lease.lease.job_id.parse().unwrap();
    let done = queue::complete(&pool, &agent.agent, job_id, lease.lease.attempt, &result)
        .await
        .unwrap();
    assert!(matches!(done, Completion::Recorded(_)));

    let run = get_run(&state, &run_id).await;
    assert_eq!(run["state"], "succeeded");
    assert!(run["finished_at"].as_str().unwrap().ends_with('Z'));
    let job = &run["jobs"][0];
    assert_eq!(job["state"], "succeeded");
    assert_eq!(job["attempt"], 1);
    assert_eq!(job["agent_id"], json!(agent.agent.id));
    assert_eq!(
        job["tasks"],
        json!([{ "name": "FASTQC_fastqc_1 (gut)", "status": "completed",
                 "exit_code": 0, "duration_ms": 1200 }])
    );
    assert_eq!(job["outputs"][0]["name"], "fastqc_1__zip");
    assert_eq!(job["outputs"][0]["sample"], "gut");
    assert_eq!(job["outputs"][0]["digest"], digest_of("fastqc_1__zip"));

    // A finished run can't be cancelled.
    let reply = call(
        &state,
        "POST",
        &format!("/api/runs/{run_id}/cancel"),
        Some(DEV_TOKEN),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json()["error"]["code"], "run_finished");
}

#[sqlx::test(migrations = "./migrations")]
async fn cancelling_a_queued_run_cancels_its_jobs(pool: PgPool) {
    let Ready { state, launch, .. } = ready(&pool).await;
    let run_id = launch_run(&state, launch).await.json()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let reply = call(
        &state,
        "POST",
        &format!("/api/runs/{run_id}/cancel"),
        Some(DEV_TOKEN),
        None,
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["state"], "cancelled");
    let run = get_run(&state, &run_id).await;
    assert_eq!(run["state"], "cancelled");
    assert_eq!(run["jobs"][0]["state"], "cancelled");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_running_job_is_asked_to_stop(pool: PgPool) {
    let Ready {
        state,
        agent,
        launch,
    } = ready(&pool).await;
    let run_id = launch_run(&state, launch).await.json()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let lease = queue::acquire(&pool, &agent.agent, "0.1.0")
        .await
        .unwrap()
        .unwrap();

    let uri = format!("/api/runs/{run_id}/cancel");
    let reply = call(&state, "POST", &uri, Some(DEV_TOKEN), None).await;
    assert_eq!(reply.status, StatusCode::OK);

    let job = &get_run(&state, &run_id).await["jobs"][0];
    assert_eq!(
        job["state"], "leased",
        "still running until the agent stops it"
    );
    assert_eq!(job["cancel_requested"], true);
    let action = queue::heartbeat(
        &pool,
        &agent.agent,
        lease.lease.job_id.parse().unwrap(),
        lease.lease.attempt,
        None,
    )
    .await
    .unwrap();
    assert_eq!(action, Some(protocol::lease::HeartbeatAction::Cancel));
}

#[sqlx::test(migrations = "./migrations")]
async fn unknown_runs_are_404(pool: PgPool) {
    let state = ui_state(&pool).await;
    let nobody = Uuid::new_v4();
    let reply = call(
        &state,
        "GET",
        &format!("/api/runs/{nobody}"),
        Some(DEV_TOKEN),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = call(
        &state,
        "POST",
        &format!("/api/runs/{nobody}/cancel"),
        Some(DEV_TOKEN),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[test]
fn a_per_dataset_input_drops_sample_names() {
    use protocol::lease::InputFile;
    use protocol::types::Cardinality;

    let file = |sample: &str| {
        (
            sample.to_string(),
            InputFile {
                sample: Some(sample.to_string()),
                uri: format!("/data/{sample}.fq"),
                digest: protocol::digest::digest_bytes(SHA, sample.as_bytes()),
                size: 1,
            },
        )
    };
    let resolved = super::resolve(Cardinality::PerDataset, vec![file("a"), file("b")]);
    assert_eq!(resolved.cardinality, Cardinality::PerDataset);
    assert!(resolved.files.iter().all(|f| f.sample.is_none()));
    assert_eq!(resolved.files.len(), 2);

    let resolved = super::resolve(Cardinality::PerSample, vec![file("a")]);
    assert_eq!(resolved.files[0].sample.as_deref(), Some("a"));
}
