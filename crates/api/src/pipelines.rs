//! Checking pipelines (DESIGN.md §4.3):
//!
//! ```text
//! POST /api/validate   { canvas }        → 200 { valid: true }, or 422 with every problem
//! ```
//!
//! Validate stores nothing: the canvas editor calls it on every change to
//! highlight broken nodes. Saving pipelines comes in the next part of
//! P0c PR 4.

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::routing::post;
use axum::{Json, Router};
use compiler::canvas::Canvas;
use compiler::pipeline_graph::PipelineGraph;
use compiler::spec::SpecRegistry;
use compiler::validate::{ValidationError, validate};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::ApiError;
use crate::users::User;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/validate", post(validate_canvas))
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
}
