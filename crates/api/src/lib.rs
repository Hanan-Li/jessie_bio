//! The control plane (DESIGN.md §4.3, §6): the HTTP API, the job queue
//! and its reaper, all backed by Postgres.
//!
//! - P0c PR 2, the skeleton: configuration, the database and its
//!   migrations, loading the node library, and a health check.
//! - P0c PR 3: the queue (`queue`), agents and their tokens (`agents`),
//!   the agent endpoints (`agent_api`), blobs (`blobs`) and the reaper.
//! - P0c PR 4, the UI endpoints, in parts. Part 1: the development token
//!   (`users`) and validating a canvas (`pipelines`). Part 2: saving
//!   pipelines (`pipelines`) and registering datasets (`datasets`).

pub mod agent_api;
pub mod agents;
pub mod blobs;
pub mod datasets;
pub mod error;
pub mod node_library;
pub mod pipelines;
pub mod queue;
#[cfg(test)]
mod testing;
pub mod users;

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use compiler::spec::SpecRegistry;
use protocol::digest::Digest;
use serde::Serialize;
use sqlx::PgPool;

use crate::error::ApiError;

/// The migrations in `migrations/`, compiled into the binary, so a deployed
/// API brings its own schema and needs no separate migration tool.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// What every handler can reach. axum clones the state for each request,
/// so everything in it must be cheap to clone:
///
/// - `PgPool` is a handle onto a shared pool of connections (internally an
///   `Arc`).
/// - The registry is wrapped in an `Arc` ("atomically reference-counted"):
///   one registry in memory, and each clone is just another pointer to it
///   plus a count, so cloning never copies the specs. `Arc` gives shared
///   *read-only* access, which is all handlers need: the node library is
///   only published at startup.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// The published node specs and modules (`node_library::load_registry`).
    pub registry: Arc<SpecRegistry>,
    /// The development token's digest (`users`); `None` closes the UI
    /// endpoints to everyone.
    pub dev_token: Option<Digest>,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .merge(agent_api::routes())
        .merge(pipelines::routes())
        .merge(datasets::routes())
        .with_state(state)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

/// The reaper (DESIGN.md §6): every `every`, take back expired leases.
/// Runs until the process exits. An error is logged and retried on the
/// next tick: a database blip mustn't stop reaping for good.
pub async fn run_reaper(pool: PgPool, every: std::time::Duration) {
    let mut ticks = tokio::time::interval(every);
    // If a tick is late (a slow reap), don't fire the missed ones in a burst.
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        match queue::reap(&pool).await {
            Ok(0) => {}
            Ok(count) => println!("reaper: took back {count} expired lease(s)"),
            Err(error) => eprintln!("reaper: {error:#}"),
        }
    }
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
        let (status, body) = get_json(app(crate::testing::bare_state(&pool)), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({ "status": "ok" }));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unhealthy_when_the_database_is_gone(pool: PgPool) {
        let state = crate::testing::bare_state(&pool);
        pool.close().await;

        let (status, body) = get_json(app(state), "/healthz").await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "database_unavailable");
        assert!(body["error"]["message"].is_string());
    }
}
