//! A small fake control plane for tests: axum on a random local port,
//! speaking the agent protocol (DESIGN.md §4.1). Tests set what it
//! answers and read back what it was sent, so the client and the serve
//! loop are tested over real HTTP. The real API is tested against the same
//! `protocol` types in the `api` crate; the two meet in the end-to-end test.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

pub const TOKEN: &str = "jat_test-token";

/// How the fake answers heartbeats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Heartbeats {
    #[default]
    Continue,
    Cancel,
    /// 409: the lease is lost.
    Conflict,
}

/// What the fake answers, and what it was sent.
#[derive(Default)]
pub struct Fake {
    /// Handed out one per lease call; when empty, 204.
    pub leases: VecDeque<Value>,
    pub heartbeats: Heartbeats,
    /// Results for any attempt but this one are answered 409.
    pub current_attempt: u64,
    pub blobs: BTreeMap<String, Vec<u8>>,
    /// Every request body received: ("lease" | "heartbeat <id>" |
    /// "result <id>", body).
    pub seen: Vec<(String, Value)>,
}

impl Fake {
    pub fn new() -> Fake {
        Fake {
            current_attempt: 1,
            ..Fake::default()
        }
    }

    /// The result bodies received, in order.
    pub fn results(&self) -> Vec<Value> {
        self.seen
            .iter()
            .filter(|(path, _)| path.starts_with("result "))
            .map(|(_, body)| body.clone())
            .collect()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.seen
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .count()
    }
}

/// `Arc` so the server task and the test share one `Fake`; `Mutex` so
/// either can change it (only one at a time).
pub type Shared = Arc<Mutex<Fake>>;

/// Start the fake; returns its state and its base URL.
pub async fn start(fake: Fake) -> (Shared, String) {
    let shared = Arc::new(Mutex::new(fake));
    let app = Router::new()
        .route("/api/agent/lease", post(lease))
        .route("/api/agent/jobs/{id}/heartbeat", post(heartbeat))
        .route("/api/agent/jobs/{id}/result", post(result))
        .route("/api/agent/blobs/{digest}", get(blob))
        .with_state(shared.clone());
    // Port 0: the operating system picks a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (shared, format!("http://{address}"))
}

/// Wait (up to 10 s) until `done` holds for the fake's state.
pub async fn wait_until(fake: &Shared, done: impl Fn(&Fake) -> bool) {
    for _ in 0..1000 {
        if done(&fake.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the fake control plane never reached the expected state");
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({ "error": { "code": code, "message": message, "details": null } });
    (status, axum::Json(body)).into_response()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"))
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "unauthorized", "no")
}

async fn lease(
    State(fake): State<Shared>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let mut fake = fake.lock().unwrap();
    fake.seen.push(("lease".to_string(), body));
    match fake.leases.pop_front() {
        Some(lease) => axum::Json(lease).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn heartbeat(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let mut fake = fake.lock().unwrap();
    fake.seen.push((format!("heartbeat {id}"), body));
    let action = match fake.heartbeats {
        Heartbeats::Continue => "continue",
        Heartbeats::Cancel => "cancel",
        Heartbeats::Conflict => {
            return error(StatusCode::CONFLICT, "lease_lost", "gone");
        }
    };
    axum::Json(json!({ "format_version": 1, "action": action })).into_response()
}

async fn result(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let mut fake = fake.lock().unwrap();
    let stale = body["attempt"] != fake.current_attempt;
    fake.seen.push((format!("result {id}"), body));
    if stale {
        return error(StatusCode::CONFLICT, "lease_lost", "stale");
    }
    axum::Json(json!({ "job_state": "succeeded" })).into_response()
}

async fn blob(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Path(digest): Path<String>,
) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    match fake.lock().unwrap().blobs.get(&digest) {
        Some(bytes) => bytes.clone().into_response(),
        None => error(StatusCode::NOT_FOUND, "not_found", "no such blob"),
    }
}
