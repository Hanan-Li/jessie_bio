//! The `/api/agent/*` endpoints (DESIGN.md §4.1, §4.3): the only part of
//! the API an agent token opens, and the only part an agent can call.
//!
//! ```text
//! POST /api/agent/lease                  LeaseRequest     → LeaseResponse, or 204
//! POST /api/agent/jobs/{id}/heartbeat    HeartbeatRequest → HeartbeatReply, or 409
//! POST /api/agent/jobs/{id}/result       ResultReport     → 200, or 409
//! GET  /api/agent/blobs/{digest}                          → the blob's bytes, or 404
//! ```
//!
//! Every handler takes an [`Agent`] argument, so axum checks the token
//! before the handler runs; the queue then scopes each call to that
//! agent's own leases and compute environment.

use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use protocol::digest::Digest;
use protocol::lease::{HeartbeatReply, HeartbeatRequest, LeaseRequest, ResultReport};
use protocol::version::FormatVersion;
use serde::Serialize;
use uuid::Uuid;

use crate::AppState;
use crate::agents::Agent;
use crate::blobs;
use crate::error::ApiError;
use crate::queue::{self, Completion};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/agent/lease", post(lease))
        .route("/api/agent/jobs/{id}/heartbeat", post(heartbeat))
        .route("/api/agent/jobs/{id}/result", post(result))
        .route("/api/agent/blobs/{digest}", get(blob))
}

fn lease_lost() -> ApiError {
    ApiError::client(
        StatusCode::CONFLICT,
        "lease_lost",
        "this agent no longer holds this attempt of the job (its lease expired, \
         or a newer attempt holds it): stop the job and discard its work",
    )
}

/// Poll for work.
async fn lease(
    State(state): State<AppState>,
    agent: Agent,
    body: Result<Json<LeaseRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = body?;
    match queue::acquire(&state.pool, &agent, &request.agent_version).await? {
        Some(lease) => Ok(Json(lease).into_response()),
        // Nothing to do: the agent waits and polls again.
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn heartbeat(
    State(state): State<AppState>,
    agent: Agent,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<HeartbeatRequest>, JsonRejection>,
) -> Result<Json<HeartbeatReply>, ApiError> {
    let Path(job_id) = id?;
    let Json(request) = body?;
    let action = queue::heartbeat(
        &state.pool,
        &agent,
        job_id,
        request.attempt,
        request.progress.as_deref(),
    )
    .await?
    .ok_or_else(lease_lost)?;
    Ok(Json(HeartbeatReply {
        format_version: FormatVersion::current(),
        action,
    }))
}

#[derive(Serialize)]
struct ResultAccepted {
    /// The job's state after the result, for logs. Agents don't act on it.
    job_state: String,
}

async fn result(
    State(state): State<AppState>,
    agent: Agent,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<ResultReport>, JsonRejection>,
) -> Result<Json<ResultAccepted>, ApiError> {
    let Path(job_id) = id?;
    let Json(report) = body?;
    match queue::complete(&state.pool, &agent, job_id, report.attempt, &report.result).await? {
        Completion::Recorded(job_state) | Completion::AlreadyRecorded(job_state) => {
            Ok(Json(ResultAccepted {
                job_state: job_state.to_string(),
            }))
        }
        Completion::Stale => Err(lease_lost()),
    }
}

/// A project file, by digest. Blobs never change, so the response may be
/// cached forever; `private` because it is only for this agent's
/// environment, so a shared proxy mustn't hand it to anyone else.
async fn blob(
    State(state): State<AppState>,
    agent: Agent,
    digest: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(digest) = digest?;
    // Parse before querying: a malformed digest can't name any blob.
    let digest: Digest = digest.parse().map_err(|_| ApiError::not_found("blob"))?;
    let digest = digest.to_string();
    let bytes = blobs::fetch_for_env(&state.pool, &agent.compute_env, &digest)
        .await?
        .ok_or_else(|| ApiError::not_found("blob"))?;
    Ok((
        [
            (CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                CACHE_CONTROL,
                "private, max-age=31536000, immutable".to_string(),
            ),
            (ETAG, format!("\"{digest}\"")),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};
    use protocol::blob::{BlobRef, Tree};
    use protocol::lease::LeaseResponse;
    use protocol::result::JobStatus;
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use crate::AppState;
    use crate::testing::{self, *};

    /// Agent-endpoint calls need no node library or development token.
    async fn call(
        pool: &PgPool,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        let state: AppState = bare_state(pool);
        testing::call(&state, method, uri, token, body).await
    }

    fn poll() -> Option<Value> {
        Some(json!({ "agent_version": "0.1.0" }))
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn agent_endpoints_need_an_agent_token(pool: PgPool) {
        setup(&pool).await;
        for token in [None, Some("jat_not_a_real_token"), Some("dev-user-token")] {
            let reply = call(&pool, "POST", "/api/agent/lease", token, poll()).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
            assert_eq!(reply.json()["error"]["code"], "unauthorized");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn nothing_to_do_is_204(pool: PgPool) {
        let agent = setup(&pool).await;
        let reply = call(
            &pool,
            "POST",
            "/api/agent/lease",
            Some(&agent.token),
            poll(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert!(reply.bytes.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn lease_heartbeat_result_over_http(pool: PgPool) {
        let agent = setup(&pool).await;
        let run_id = launch_fastqc_multiqc(&pool).await;
        let token = Some(agent.token.as_str());

        let reply = call(&pool, "POST", "/api/agent/lease", token, poll()).await;
        assert_eq!(reply.status, StatusCode::OK);
        let lease: LeaseResponse = serde_json::from_slice(&reply.bytes).unwrap();
        let job = lease.lease.job_id.clone();

        let beat = format!("/api/agent/jobs/{job}/heartbeat");
        let reply = call(
            &pool,
            "POST",
            &beat,
            token,
            Some(json!({ "attempt": 1, "progress": "starting" })),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.json(),
            json!({ "format_version": 1, "action": "continue" })
        );

        // A stale attempt is told its lease is gone.
        let reply = call(&pool, "POST", &beat, token, Some(json!({ "attempt": 7 }))).await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(reply.json()["error"]["code"], "lease_lost");

        let report = json!({ "attempt": 1, "result": fastqc_succeeded("fastqc_1__zip") });
        let done = format!("/api/agent/jobs/{job}/result");
        let reply = call(&pool, "POST", &done, token, Some(report.clone())).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), json!({ "job_state": "succeeded" }));
        // Sent again (a lost reply): still 200.
        assert_eq!(
            call(&pool, "POST", &done, token, Some(report)).await.status,
            StatusCode::OK
        );

        assert_eq!(job_state(&pool, run_id, 1).await, "queued");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn another_agent_cannot_touch_the_job(pool: PgPool) {
        let agent = setup(&pool).await;
        let other = create_agent(&pool, ENV, "other").await;
        launch_fastqc_multiqc(&pool).await;
        let reply = call(
            &pool,
            "POST",
            "/api/agent/lease",
            Some(&agent.token),
            poll(),
        )
        .await;
        let lease: LeaseResponse = serde_json::from_slice(&reply.bytes).unwrap();
        let job = lease.lease.job_id;

        let body = json!({ "attempt": 1, "result": result(JobStatus::Succeeded) });
        let reply = call(
            &pool,
            "POST",
            &format!("/api/agent/jobs/{job}/result"),
            Some(&other.token),
            Some(body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        let reply = call(
            &pool,
            "POST",
            &format!("/api/agent/jobs/{job}/heartbeat"),
            Some(&other.token),
            Some(json!({ "attempt": 1 })),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn bad_requests_use_the_error_shape(pool: PgPool) {
        let agent = setup(&pool).await;
        let token = Some(agent.token.as_str());

        // A newer agent protocol than this server knows.
        let reply = call(
            &pool,
            "POST",
            "/api/agent/lease",
            token,
            Some(json!({ "format_version": 2, "agent_version": "9.0" })),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.json()["error"]["code"], "invalid_request");
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("upgrade")
        );

        let reply = call(
            &pool,
            "POST",
            "/api/agent/jobs/not-a-uuid/heartbeat",
            token,
            Some(json!({ "attempt": 1 })),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"]["code"], "invalid_request");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn blobs_are_served_verified_and_only_to_their_environment(pool: PgPool) {
        let agent = setup(&pool).await;
        create_env(&pool, "hpc").await;
        let outsider = create_agent(&pool, "hpc", "hpc-1").await;
        launch_fastqc_multiqc(&pool).await;
        let reply = call(
            &pool,
            "POST",
            "/api/agent/lease",
            Some(&agent.token),
            poll(),
        )
        .await;
        let lease: LeaseResponse = serde_json::from_slice(&reply.bytes).unwrap();

        // The tree, then a file it lists, each checked against its digest
        // the way the agent will.
        let tree_uri = format!("/api/agent/blobs/{}", lease.project.digest);
        let reply = call(&pool, "GET", &tree_uri, Some(&agent.token), None).await;
        assert_eq!(reply.status, StatusCode::OK);
        lease.project.verify(&reply.bytes).unwrap();
        assert_eq!(
            reply.headers[header::CACHE_CONTROL],
            "private, max-age=31536000, immutable"
        );
        assert_eq!(
            reply.headers[header::ETAG],
            format!("\"{}\"", lease.project.digest)
        );

        let tree = Tree::from_blob_bytes(&reply.bytes).unwrap();
        let file: &BlobRef = &tree.files["main.nf"];
        let reply = call(
            &pool,
            "GET",
            &format!("/api/agent/blobs/{}", file.digest),
            Some(&agent.token),
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        file.verify(&reply.bytes).unwrap();

        // An agent of another environment can't read it, even knowing the
        // digest; nor can anyone fetch a blob no job uses (the node
        // library's modules) or a malformed digest.
        let reply = call(&pool, "GET", &tree_uri, Some(&outsider.token), None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.json()["error"]["code"], "not_found");
        let unused = BlobRef::of(SHA, b"not used by any job");
        let reply = call(
            &pool,
            "GET",
            &format!("/api/agent/blobs/{}", unused.digest),
            Some(&agent.token),
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        let reply = call(
            &pool,
            "GET",
            "/api/agent/blobs/sha256:nothex",
            Some(&agent.token),
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }
}
