//! Checking and saving pipelines (DESIGN.md §4.3):
//!
//! ```text
//! POST /api/validate   { canvas }        → 200 { valid: true }, or 422 with every problem
//! POST /api/pipelines  { name, canvas }  → 201 { pipeline_id, pipeline_version_id, version }
//! ```
//!
//! Validate stores nothing: the canvas editor calls it on every change to
//! highlight broken nodes. Saving validates too and then stores the canvas
//! as a new, **immutable** version of the named pipeline: a run records
//! the exact version it ran, so changing a pipeline means a new version,
//! never an edit.

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use compiler::canvas::Canvas;
use compiler::pipeline_graph::PipelineGraph;
use compiler::spec::SpecRegistry;
use compiler::validate::{ValidationError, validate};
use serde::{Deserialize, Serialize};
use sqlx::types::Json as JsonColumn;
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;
use crate::users::User;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/validate", post(validate_canvas))
        .route("/api/pipelines", post(create_pipeline))
}

/// `canvas` is typed: a body that isn't canvas-shaped (a misspelled
/// field, a missing `edges`) is refused while parsing, as
/// `invalid_request`, before validation even starts.
#[derive(Deserialize)]
struct ValidateRequest {
    canvas: Canvas,
}

#[derive(Serialize)]
struct Valid {
    valid: bool,
}

async fn validate_canvas(
    State(state): State<AppState>,
    _user: User,
    body: Result<Json<ValidateRequest>, JsonRejection>,
) -> Result<Json<Valid>, ApiError> {
    let Json(request) = body?;
    check(&request.canvas, &state.registry)?;
    Ok(Json(Valid { valid: true }))
}

/// Validate a canvas, turning a failure into the API's `422`.
pub(crate) fn check(canvas: &Canvas, registry: &SpecRegistry) -> Result<PipelineGraph, ApiError> {
    validate(canvas, registry).map_err(|errors| invalid_pipeline(&errors))
}

/// `422 invalid_pipeline`, with every validation error in `details`: its
/// fields (typed, from `ValidationError`'s `Serialize`) plus a readable
/// `message`, so the frontend can mark each broken node and show why.
fn invalid_pipeline(errors: &[ValidationError]) -> ApiError {
    let details: Vec<serde_json::Value> = errors
        .iter()
        .map(|error| {
            let mut detail = serde_json::to_value(error).expect("validation errors serialize");
            detail["message"] = error.to_string().into();
            detail
        })
        .collect();
    let count = errors.len();
    let problems = if count == 1 { "problem" } else { "problems" };
    ApiError::unprocessable(
        "invalid_pipeline",
        format!("the pipeline has {count} {problems}"),
        details.into(),
    )
}

#[derive(Deserialize)]
struct CreatePipeline {
    name: String,
    canvas: Canvas,
}

#[derive(Serialize)]
struct Created {
    pipeline_id: Uuid,
    pipeline_version_id: Uuid,
    version: i32,
}

async fn create_pipeline(
    State(state): State<AppState>,
    _user: User,
    body: Result<Json<CreatePipeline>, JsonRejection>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let Json(request) = body?;
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "a pipeline needs a name",
        ));
    }
    // Only a valid canvas is ever stored, so every stored version can be
    // launched (as long as its nodes stay published, which they do).
    check(&request.canvas, &state.registry)?;

    let mut tx = state.pool.begin().await?;
    // Create the pipeline the first time its name is used. `DO NOTHING`
    // rather than "look, then insert": two first saves at once can't both
    // insert, because the unique index decides.
    sqlx::query(
        "INSERT INTO pipelines (name) VALUES ($1)
         ON CONFLICT (project_id, name) DO NOTHING",
    )
    .bind(name)
    .execute(&mut *tx)
    .await?;
    // Lock the pipeline's row, so two saves at once number their versions
    // one after the other instead of both picking the same next number.
    let pipeline_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM pipelines WHERE project_id IS NULL AND name = $1 FOR UPDATE",
    )
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    let (pipeline_version_id, version): (Uuid, i32) = sqlx::query_as(
        "INSERT INTO pipeline_versions (pipeline_id, version, canvas)
         SELECT $1, COALESCE(max(version), 0) + 1, $2 FROM pipeline_versions WHERE pipeline_id = $1
         RETURNING id, version",
    )
    .bind(pipeline_id)
    .bind(JsonColumn(&request.canvas))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(Created {
            pipeline_id,
            pipeline_version_id,
            version,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use sqlx::PgPool;

    use crate::testing::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn ui_endpoints_need_the_development_token(pool: PgPool) {
        let state = ui_state(&pool).await;
        let agent = setup(&pool).await;
        let body = Some(json!({ "canvas": test_canvas() }));
        for token in [None, Some("wrong"), Some(agent.token.as_str())] {
            let reply = call(&state, "POST", "/api/validate", token, body.clone()).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "token {token:?}");
            assert_eq!(reply.json()["error"]["code"], "unauthorized");
        }
        // And the development token doesn't open the agent endpoints.
        let reply = call(
            &state,
            "POST",
            "/api/agent/lease",
            Some(DEV_TOKEN),
            Some(json!({ "agent_version": "0.1.0" })),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

        // A server started without a development token lets nobody in.
        let closed = bare_state(&pool);
        let reply = call(&closed, "POST", "/api/validate", Some(DEV_TOKEN), body).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn validate_accepts_the_test_canvas(pool: PgPool) {
        let state = ui_state(&pool).await;
        let body = Some(json!({ "canvas": test_canvas() }));
        let reply = call(&state, "POST", "/api/validate", Some(DEV_TOKEN), body).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), json!({ "valid": true }));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn validate_lists_every_problem_typed(pool: PgPool) {
        let state = ui_state(&pool).await;
        let canvas = json!({
            "nodes": [
                { "id": "fastqc_1", "spec": "fastqc", "version": "9.9.9" },
                { "id": "multiqc_1", "spec": "multiqc", "version": "0.1.0" }
            ],
            "edges": [
                { "from": { "node": "ghost", "port": "zip" },
                  "to": { "node": "multiqc_1", "port": "multiqc_files" } }
            ]
        });
        let body = Some(json!({ "canvas": canvas }));
        let reply = call(&state, "POST", "/api/validate", Some(DEV_TOKEN), body).await;

        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        let error = &reply.json()["error"];
        assert_eq!(error["code"], "invalid_pipeline");
        assert_eq!(error["message"], "the pipeline has 2 problems");
        assert_eq!(error["details"][0]["code"], "unknown_spec");
        assert_eq!(error["details"][0]["node"], "fastqc_1");
        assert_eq!(error["details"][0]["version"], "9.9.9");
        assert!(
            error["details"][0]["message"]
                .as_str()
                .unwrap()
                .contains("does not exist")
        );
        assert_eq!(error["details"][1]["code"], "unknown_node");
        assert_eq!(error["details"][1]["node"], "ghost");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_body_that_is_not_a_canvas_is_an_invalid_request(pool: PgPool) {
        let state = ui_state(&pool).await;
        let body = Some(json!({ "canvas": { "nodes": [], "edgse": [] } }));
        let reply = call(&state, "POST", "/api/validate", Some(DEV_TOKEN), body).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.json()["error"]["code"], "invalid_request");
        assert!(
            reply.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("edgse")
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn saving_again_makes_a_new_version(pool: PgPool) {
        let state = ui_state(&pool).await;
        let save = || Some(json!({ "name": "QC", "canvas": test_canvas() }));
        let first = call(&state, "POST", "/api/pipelines", Some(DEV_TOKEN), save()).await;
        assert_eq!(first.status, StatusCode::CREATED);
        assert_eq!(first.json()["version"], 1);
        let second = call(&state, "POST", "/api/pipelines", Some(DEV_TOKEN), save()).await;
        assert_eq!(second.json()["version"], 2);
        assert_eq!(first.json()["pipeline_id"], second.json()["pipeline_id"]);
        assert_ne!(
            first.json()["pipeline_version_id"],
            second.json()["pipeline_version_id"]
        );

        let other = Some(json!({ "name": "Other", "canvas": test_canvas() }));
        let other = call(&state, "POST", "/api/pipelines", Some(DEV_TOKEN), other).await;
        assert_eq!(other.json()["version"], 1);
        assert_ne!(other.json()["pipeline_id"], first.json()["pipeline_id"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_invalid_pipeline_is_not_stored(pool: PgPool) {
        let state = ui_state(&pool).await;
        let mut canvas = test_canvas();
        canvas["nodes"][0]["version"] = json!("9.9.9");
        let body = Some(json!({ "name": "QC", "canvas": canvas }));
        let reply = call(&state, "POST", "/api/pipelines", Some(DEV_TOKEN), body).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

        let blank = Some(json!({ "name": "  ", "canvas": test_canvas() }));
        let reply = call(&state, "POST", "/api/pipelines", Some(DEV_TOKEN), blank).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM pipelines")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 0);
    }
}
