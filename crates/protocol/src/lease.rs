//! Messages between an agent and the control plane (DESIGN.md §4.1, agent
//! protocol):
//!
//! ```text
//! POST /api/agent/lease                LeaseRequest     → LeaseResponse, or 204
//! POST /api/agent/jobs/{id}/heartbeat  HeartbeatRequest → HeartbeatReply, or 409
//! POST /api/agent/jobs/{id}/result     ResultReport     → 200, or 409
//! GET  /api/agent/blobs/{digest}                        → the blob's bytes
//! ```
//!
//! Every message carries `format_version` and ignores unknown fields, so
//! agents and the control plane can be upgraded at different times.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::blob::BlobRef;
use crate::digest::{Digest, HashAlgorithm};
use crate::job::JobInterface;
use crate::result::JobResult;
use crate::types::Cardinality;
use crate::version::FormatVersion;

/// The agent asking for work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRequest {
    #[serde(default)]
    pub format_version: FormatVersion,
    /// The agent's own version, so the control plane can see which agents
    /// need upgrading.
    pub agent_version: String,
}

/// A job, leased to the agent that asked: its entire job description.
/// Metadata only: no data bytes, and the project's files are fetched by
/// digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseResponse {
    #[serde(default)]
    pub format_version: FormatVersion,
    pub lease: Lease,
    pub run: RunRef,
    pub settings: Settings,
    /// The project's tree (the compiler's files: `main.nf`, the config,
    /// the modules, `job.json`). The agent fetches it, then the blobs it
    /// lists, verifying each one.
    pub project: BlobRef,
    /// The job's interface, also in the project as `job.json`; here so the
    /// agent can check `inputs` before fetching anything.
    pub interface: JobInterface,
    /// Each job input, already resolved by the scheduler, keyed by data
    /// name (`fastqc_1__reads`).
    pub inputs: BTreeMap<String, ResolvedInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub job_id: String,
    /// The fencing token: every heartbeat and the result must echo it, and
    /// only the current attempt is accepted.
    pub attempt: u32,
    /// When the lease runs out unless renewed (RFC 3339, the database's
    /// clock). Informational: the agent paces itself by
    /// `heartbeat_every_secs`, never by comparing clocks.
    pub expires_at: String,
    /// Set by the control plane, so it can be tuned without upgrading
    /// agents.
    pub heartbeat_every_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRef {
    pub run_id: String,
    /// The job's position in the run's plan.
    pub job_index: usize,
}

/// Decided per compute environment by the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub nextflow_version: String,
    pub hash_algorithm: HashAlgorithm,
    pub time_limit_secs: Option<u64>,
}

/// One job input with its files known: the dataset's files for a run
/// input, or the files an earlier job produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedInput {
    pub cardinality: Cardinality,
    pub files: Vec<InputFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFile {
    /// Set for per-sample inputs.
    pub sample: Option<String>,
    /// A path on the agent's machine (P0c); later also `s3://…` and other
    /// URIs.
    pub uri: String,
    /// What the file was when it was registered, so the agent can check it
    /// hasn't changed.
    pub digest: Digest,
    pub size: u64,
}

/// "Still alive, still working on this job."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    #[serde(default)]
    pub format_version: FormatVersion,
    pub attempt: u32,
    /// Free text for the run monitor, e.g. "3 of 24 tasks done".
    #[serde(default)]
    pub progress: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatReply {
    #[serde(default)]
    pub format_version: FormatVersion,
    pub action: HeartbeatAction,
}

/// What the control plane wants the agent to do. No `Unknown` fallback:
/// an agent must never guess at an instruction it doesn't understand, so a
/// new action is a breaking change (a `format_version` bump).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatAction {
    Continue,
    /// The run was cancelled: stop the job and report `cancelled`.
    Cancel,
}

/// The agent's report when the job ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultReport {
    #[serde(default)]
    pub format_version: FormatVersion,
    pub attempt: u32,
    pub result: JobResult,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::blob::BlobRef;
    use crate::digest::digest_bytes;

    fn lease_response() -> LeaseResponse {
        let sha = HashAlgorithm::Sha256;
        LeaseResponse {
            format_version: FormatVersion::current(),
            lease: Lease {
                job_id: "8f1c".to_string(),
                attempt: 2,
                expires_at: "2026-11-16T14:05:00Z".to_string(),
                heartbeat_every_secs: 20,
            },
            run: RunRef {
                run_id: "3b9e".to_string(),
                job_index: 0,
            },
            settings: Settings {
                nextflow_version: "26.10.5".to_string(),
                hash_algorithm: sha,
                time_limit_secs: Some(43200),
            },
            project: BlobRef::of(sha, b"{}"),
            interface: JobInterface {
                format_version: FormatVersion::current(),
                inputs: vec![],
                outputs: vec![],
            },
            inputs: BTreeMap::from([(
                "fastqc_1__reads".to_string(),
                ResolvedInput {
                    cardinality: Cardinality::PerSample,
                    files: vec![InputFile {
                        sample: Some("gut".to_string()),
                        uri: "/data/ggal/ggal_gut_1.fq".to_string(),
                        digest: digest_bytes(sha, b"reads"),
                        size: 691873,
                    }],
                },
            )]),
        }
    }

    #[test]
    fn lease_response_json_shape() {
        let json = serde_json::to_value(lease_response()).unwrap();

        assert_eq!(json["format_version"], 1);
        assert_eq!(json["lease"]["attempt"], 2);
        assert_eq!(json["settings"]["hash_algorithm"], "sha256");
        assert!(
            json["project"]["digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert_eq!(
            json["inputs"]["fastqc_1__reads"]["cardinality"],
            "per_sample"
        );
        assert_eq!(
            serde_json::from_value::<LeaseResponse>(json).unwrap(),
            lease_response()
        );
    }

    #[test]
    fn an_old_agent_ignores_fields_it_does_not_know() {
        // A newer control plane added `priority` and a new setting.
        let mut json = serde_json::to_value(lease_response()).unwrap();
        json["priority"] = json!("high");
        json["settings"]["max_memory_gb"] = json!(64);
        assert_eq!(
            serde_json::from_value::<LeaseResponse>(json).unwrap(),
            lease_response()
        );
    }

    #[test]
    fn a_lease_from_a_newer_format_is_refused() {
        let mut json = serde_json::to_value(lease_response()).unwrap();
        json["format_version"] = json!(2);
        let err = serde_json::from_value::<LeaseResponse>(json).unwrap_err();
        assert!(err.to_string().contains("upgrade it"), "{err}");
    }

    #[test]
    fn heartbeat_actions() {
        let reply: HeartbeatReply = serde_json::from_value(json!({ "action": "cancel" })).unwrap();
        assert_eq!(reply.action, HeartbeatAction::Cancel);
        assert_eq!(reply.format_version.get(), 1, "a missing version means 1");
        // An instruction the agent doesn't know must not be guessed at.
        assert!(serde_json::from_value::<HeartbeatReply>(json!({ "action": "pause" })).is_err());
    }
}
