//! The control plane (DESIGN.md §4.3, §6): the HTTP API, and later the job
//! queue and scheduler, all backed by Postgres.
//!
//! P0c PR 2 is the skeleton: configuration, the database and its
//! migrations, loading the node library, and a health check.

pub mod error;
pub mod node_library;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::PgPool;

use crate::error::ApiError;

/// The migrations in `migrations/`, compiled into the binary, so a deployed
/// API brings its own schema and needs no separate migration tool.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// What every handler can reach. `PgPool` is a cheap handle onto a shared
/// pool of connections (internally an `Arc`), so cloning the state per
/// request costs nothing.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

/// Healthy means "can serve requests": the process is up *and* the
/// database answers.
async fn health(State(state): State<AppState>) -> Result<Json<Health>, ApiError> {
    sqlx::query("SELECT 1")
        .execute(&state.pool)
        .await
        .map_err(|error| ApiError::unavailable("database_unavailable", error))?;
    Ok(Json(Health { status: "ok" }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        // `oneshot` sends one request straight into the router, with no
        // network or port involved.
        let response = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn healthy_when_the_database_answers(pool: PgPool) {
        let (status, body) = get_json(app(AppState { pool }), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "status": "ok" }));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unhealthy_when_the_database_is_gone(pool: PgPool) {
        let state = AppState { pool: pool.clone() };
        pool.close().await;

        let (status, body) = get_json(app(state), "/healthz").await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "database_unavailable");
        assert!(body["error"]["message"].is_string());
    }
}
