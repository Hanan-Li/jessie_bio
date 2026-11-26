//! The agent's side of the agent protocol over HTTPS (DESIGN.md §4.1):
//!
//! ```text
//! POST /api/agent/lease                  → a lease, or 204 (nothing to do)
//! POST /api/agent/jobs/{id}/heartbeat    → continue | cancel, or 409 (lease lost)
//! POST /api/agent/jobs/{id}/result       → 200, or 409 (stale attempt)
//! GET  /api/agent/blobs/{digest}         → the blob's bytes
//! ```
//!
//! The agent only ever calls out, so it works behind an institution's
//! firewall. Each method returns what the agent must *act* on: "no work",
//! "lease lost" and "stale" are ordinary outcomes, not errors, so they are
//! enum variants the caller has to `match`; errors are for what went wrong
//! (the server is down, the token was refused).
//!
//! The HTTP library is `reqwest`, which honours `HTTPS_PROXY` and
//! `NO_PROXY`: on-prem networks often allow outbound traffic only through
//! a proxy.

use std::time::Duration;

use anyhow::{Context, bail};
use protocol::blob::BlobRef;
use protocol::lease::{
    HeartbeatAction, HeartbeatReply, HeartbeatRequest, LeaseRequest, LeaseResponse, ResultReport,
};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;

use crate::blobs::{BlobSource, MAX_BLOB_SIZE};

/// Long enough for a slow network, short enough that a hung connection
/// can't stall heartbeats past the lease (60 s by default).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// What the agent sends as `agent_version`, from Cargo.toml at build time.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Client {
    http: reqwest::Client,
    /// e.g. `https://jessie.example.org`, without a trailing `/`.
    base: String,
    token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// The lease was renewed; the reply says what to do next.
    Renewed(HeartbeatAction),
    /// 409: this agent no longer holds this attempt. Stop and discard.
    LeaseLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultOutcome {
    /// Recorded (or already recorded: a repeated result also gets 200).
    Accepted,
    /// 409: a newer attempt holds the job, so this result doesn't count.
    Stale,
}

/// The API's error shape: `{ "error": { "code", "message", … } }`.
#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorFields,
}

#[derive(Deserialize)]
struct ErrorFields {
    code: String,
    message: String,
}

impl Client {
    pub fn new(base: &str, token: &str) -> anyhow::Result<Client> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(format!("jessie-agent/{AGENT_VERSION}"))
            .build()
            .context("setting up the HTTP client")?;
        Ok(Client {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    /// Ask for work. `None` means there is none right now: poll again later.
    pub async fn lease(&self) -> anyhow::Result<Option<LeaseResponse>> {
        let request = LeaseRequest {
            format_version: Default::default(),
            agent_version: AGENT_VERSION.to_string(),
        };
        let response = self
            .send(self.post("/api/agent/lease").json(&request))
            .await?;
        match response.status() {
            StatusCode::NO_CONTENT => Ok(None),
            StatusCode::OK => {
                // A lease in a newer format than this agent understands is
                // refused while parsing (`FormatVersion`), with an
                // "upgrade the agent" message.
                let lease = response.json().await.context("reading the lease")?;
                Ok(Some(lease))
            }
            _ => Err(error_from(response).await),
        }
    }

    pub async fn heartbeat(
        &self,
        job_id: &str,
        request: &HeartbeatRequest,
    ) -> anyhow::Result<HeartbeatOutcome> {
        let path = format!("/api/agent/jobs/{job_id}/heartbeat");
        let response = self.send(self.post(&path).json(request)).await?;
        match response.status() {
            StatusCode::OK => {
                let reply: HeartbeatReply = response
                    .json()
                    .await
                    .context("reading the heartbeat reply")?;
                Ok(HeartbeatOutcome::Renewed(reply.action))
            }
            StatusCode::CONFLICT => Ok(HeartbeatOutcome::LeaseLost),
            _ => Err(error_from(response).await),
        }
    }

    pub async fn result(
        &self,
        job_id: &str,
        report: &ResultReport,
    ) -> anyhow::Result<ResultOutcome> {
        let path = format!("/api/agent/jobs/{job_id}/result");
        let response = self.send(self.post(&path).json(report)).await?;
        match response.status() {
            StatusCode::OK => Ok(ResultOutcome::Accepted),
            StatusCode::CONFLICT => Ok(ResultOutcome::Stale),
            _ => Err(error_from(response).await),
        }
    }

    fn post(&self, path: &str) -> RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    /// Send, turning "couldn't reach the server" into a readable error, and
    /// a refused token into one that says what to do.
    async fn send(&self, request: RequestBuilder) -> anyhow::Result<Response> {
        let response = request
            .send()
            .await
            .with_context(|| format!("calling the control plane at {}", self.base))?;
        if response.status() == StatusCode::UNAUTHORIZED {
            bail!(
                "the control plane refused the agent token (401): check it, or create a new \
                 one with `api create-agent`"
            );
        }
        Ok(response)
    }
}

impl BlobSource for Client {
    /// Fetch a blob, reading at most the size its reference promises: a
    /// server that sends more is refused before the extra bytes are held
    /// in memory. The cache then verifies the digest.
    async fn fetch(&self, blob: &BlobRef) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/api/agent/blobs/{}", self.base, blob.digest);
        let request = self.http.get(url).bearer_auth(&self.token);
        let mut response = self.send(request).await?;
        if response.status() != StatusCode::OK {
            return Err(error_from(response).await);
        }
        let limit = blob.size.min(MAX_BLOB_SIZE);
        let mut bytes = Vec::with_capacity(limit as usize);
        // `chunk()` gives the body piece by piece as it arrives;
        // `Ok(None)` means it has ended.
        while let Some(chunk) = response.chunk().await.context("reading the blob")? {
            if (bytes.len() + chunk.len()) as u64 > limit {
                bail!("the server sent more than the {limit} bytes this blob should have");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// An unexpected status, described with the API's own error message when
/// the body has one.
async fn error_from(response: Response) -> anyhow::Error {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    match serde_json::from_str::<ErrorBody>(&text) {
        Ok(body) => anyhow::anyhow!(
            "the control plane answered {status}: {} ({})",
            body.error.message,
            body.error.code
        ),
        Err(_) => anyhow::anyhow!("the control plane answered {status}: {text}"),
    }
}

#[cfg(test)]
mod tests {
    //! Against a small fake control plane (axum on a random local port), so
    //! these test real HTTP: headers, status codes, bodies. The real API is
    //! tested against the same protocol types in the `api` crate, and the
    //! two meet in the end-to-end test.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode as AxumStatus};
    use axum::response::{IntoResponse, Response as AxumResponse};
    use axum::routing::{get, post};
    use protocol::digest::HashAlgorithm;
    use protocol::job::JobInterface;
    use protocol::lease::{Lease, RunRef, Settings};
    use protocol::result::{JobResult, JobStatus};
    use protocol::version::FormatVersion;
    use serde_json::{Value, json};

    use super::*;

    const TOKEN: &str = "jat_test-token";

    /// What the fake server does, and what it saw.
    #[derive(Default)]
    struct Fake {
        /// Answer to the next lease call; `None` → 204.
        lease: Option<Value>,
        /// The heartbeat status code to answer with.
        heartbeat_conflict: bool,
        cancel: bool,
        blobs: BTreeMap<String, Vec<u8>>,
        /// Request bodies received, by path.
        seen: Vec<(String, Value)>,
    }

    type Shared = Arc<Mutex<Fake>>;

    fn authorized(headers: &HeaderMap) -> bool {
        headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {TOKEN}"))
    }

    fn unauthorized() -> AxumResponse {
        (
            AxumStatus::UNAUTHORIZED,
            axum::Json(json!({ "error": { "code": "unauthorized", "message": "no" } })),
        )
            .into_response()
    }

    async fn lease(
        State(fake): State<Shared>,
        headers: HeaderMap,
        body: axum::Json<Value>,
    ) -> AxumResponse {
        if !authorized(&headers) {
            return unauthorized();
        }
        let mut fake = fake.lock().unwrap();
        fake.seen.push(("lease".to_string(), body.0));
        match fake.lease.take() {
            Some(lease) => axum::Json(lease).into_response(),
            None => AxumStatus::NO_CONTENT.into_response(),
        }
    }

    async fn heartbeat(
        State(fake): State<Shared>,
        Path(id): Path<String>,
        body: axum::Json<Value>,
    ) -> AxumResponse {
        let mut fake = fake.lock().unwrap();
        fake.seen.push((format!("heartbeat {id}"), body.0));
        if fake.heartbeat_conflict {
            return (
                AxumStatus::CONFLICT,
                axum::Json(json!({ "error": { "code": "lease_lost", "message": "gone" } })),
            )
                .into_response();
        }
        let action = if fake.cancel { "cancel" } else { "continue" };
        axum::Json(json!({ "format_version": 1, "action": action })).into_response()
    }

    async fn result(
        State(fake): State<Shared>,
        Path(id): Path<String>,
        body: axum::Json<Value>,
    ) -> AxumResponse {
        let mut fake = fake.lock().unwrap();
        let stale = body.0["attempt"] != 1;
        fake.seen.push((format!("result {id}"), body.0));
        if stale {
            return AxumStatus::CONFLICT.into_response();
        }
        axum::Json(json!({ "job_state": "succeeded" })).into_response()
    }

    async fn blob(State(fake): State<Shared>, Path(digest): Path<String>) -> AxumResponse {
        match fake.lock().unwrap().blobs.get(&digest) {
            Some(bytes) => bytes.clone().into_response(),
            None => (
                AxumStatus::NOT_FOUND,
                axum::Json(json!({ "error": { "code": "not_found", "message": "no such blob" } })),
            )
                .into_response(),
        }
    }

    /// Start the fake on a random port; returns its state and a client.
    async fn serve(fake: Fake) -> (Shared, Client) {
        let shared = Arc::new(Mutex::new(fake));
        let app = Router::new()
            .route("/api/agent/lease", post(lease))
            .route("/api/agent/jobs/{id}/heartbeat", post(heartbeat))
            .route("/api/agent/jobs/{id}/result", post(result))
            .route("/api/agent/blobs/{digest}", get(blob))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // A trailing `/` is accepted and trimmed.
        let client = Client::new(&format!("http://{address}/"), TOKEN).unwrap();
        (shared, client)
    }

    fn lease_json() -> Value {
        let lease = LeaseResponse {
            format_version: FormatVersion::current(),
            lease: Lease {
                job_id: "job-1".to_string(),
                attempt: 1,
                expires_at: "2026-11-26T20:00:00Z".to_string(),
                heartbeat_every_secs: 20,
            },
            run: RunRef {
                run_id: "run-1".to_string(),
                job_index: 0,
            },
            settings: Settings {
                nextflow_version: "26.10.5".to_string(),
                hash_algorithm: HashAlgorithm::Sha256,
                time_limit_secs: None,
            },
            project: BlobRef::of(HashAlgorithm::Sha256, b"{}"),
            interface: JobInterface {
                format_version: FormatVersion::current(),
                inputs: vec![],
                outputs: vec![],
            },
            inputs: BTreeMap::new(),
        };
        serde_json::to_value(lease).unwrap()
    }

    fn report(attempt: u32) -> ResultReport {
        ResultReport {
            format_version: FormatVersion::current(),
            attempt,
            result: JobResult {
                format_version: FormatVersion::current(),
                status: JobStatus::Succeeded,
                tasks: vec![],
                outputs: vec![],
                warnings: vec![],
            },
        }
    }

    #[tokio::test]
    async fn no_work_is_none_and_a_lease_is_parsed() {
        let (fake, client) = serve(Fake::default()).await;
        assert_eq!(client.lease().await.unwrap(), None);

        fake.lock().unwrap().lease = Some(lease_json());
        let lease = client.lease().await.unwrap().expect("a lease");
        assert_eq!(lease.lease.job_id, "job-1");

        // The agent says which version it is.
        let seen = &fake.lock().unwrap().seen;
        assert_eq!(seen[0].1["agent_version"], AGENT_VERSION);
    }

    #[tokio::test]
    async fn a_lease_in_a_newer_format_says_to_upgrade() {
        let mut newer = lease_json();
        newer["format_version"] = json!(2);
        let (_fake, client) = serve(Fake {
            lease: Some(newer),
            ..Fake::default()
        })
        .await;

        let err = client.lease().await.unwrap_err();
        assert!(format!("{err:#}").contains("upgrade"), "{err:#}");
    }

    #[tokio::test]
    async fn a_refused_token_is_an_error_that_says_so() {
        let (_fake, good) = serve(Fake::default()).await;
        let bad = Client::new(&good.base, "jat_wrong").unwrap();

        let err = bad.lease().await.unwrap_err();
        assert!(err.to_string().contains("refused the agent token"), "{err}");
    }

    #[tokio::test]
    async fn heartbeats_renew_cancel_or_find_the_lease_lost() {
        let (fake, client) = serve(Fake::default()).await;
        let beat = HeartbeatRequest {
            format_version: FormatVersion::current(),
            attempt: 1,
            progress: Some("2 of 3 tasks done".to_string()),
        };

        assert_eq!(
            client.heartbeat("job-1", &beat).await.unwrap(),
            HeartbeatOutcome::Renewed(HeartbeatAction::Continue)
        );
        fake.lock().unwrap().cancel = true;
        assert_eq!(
            client.heartbeat("job-1", &beat).await.unwrap(),
            HeartbeatOutcome::Renewed(HeartbeatAction::Cancel)
        );
        fake.lock().unwrap().heartbeat_conflict = true;
        assert_eq!(
            client.heartbeat("job-1", &beat).await.unwrap(),
            HeartbeatOutcome::LeaseLost
        );

        let seen = &fake.lock().unwrap().seen;
        assert_eq!(seen[0].0, "heartbeat job-1");
        assert_eq!(seen[0].1["progress"], "2 of 3 tasks done");
    }

    #[tokio::test]
    async fn results_are_accepted_or_stale() {
        let (_fake, client) = serve(Fake::default()).await;
        assert_eq!(
            client.result("job-1", &report(1)).await.unwrap(),
            ResultOutcome::Accepted
        );
        assert_eq!(
            client.result("job-1", &report(2)).await.unwrap(),
            ResultOutcome::Stale
        );
    }

    #[tokio::test]
    async fn fetches_blobs_and_refuses_oversized_bodies() {
        let small = BlobRef::of(HashAlgorithm::Sha256, b"process FASTQC {}");
        let mut fake = Fake::default();
        fake.blobs
            .insert(small.digest.to_string(), b"process FASTQC {}".to_vec());
        let (fake, client) = serve(fake).await;

        assert_eq!(client.fetch(&small).await.unwrap(), b"process FASTQC {}");

        // The server sends more than the reference promised.
        fake.lock()
            .unwrap()
            .blobs
            .insert(small.digest.to_string(), vec![b'x'; 1000]);
        let err = client.fetch(&small).await.unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
    }

    #[tokio::test]
    async fn an_api_error_is_reported_with_its_message() {
        let (_fake, client) = serve(Fake::default()).await;
        let missing = BlobRef::of(HashAlgorithm::Sha256, b"nobody has this");

        let err = client.fetch(&missing).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "the control plane answered 404 Not Found: no such blob (not_found)"
        );
    }
}
