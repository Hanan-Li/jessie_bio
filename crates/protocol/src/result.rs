//! `result.json`: what happened when a job ran (DESIGN.md §4.1, step 6).
//!
//! Written by the agent at the end of every run. In P0c it becomes the body
//! of the agent's "job finished" message to the API.

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::types::{Cardinality, SemanticType};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub status: JobStatus,
    /// One record per Nextflow task, from its trace file. Empty if Nextflow
    /// failed before starting any task.
    pub tasks: Vec<TaskRecord>,
    /// Every job output with its files. Collected only when the job
    /// succeeded, so a partial result is never mistaken for a full one.
    pub outputs: Vec<OutputResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobStatus {
    Succeeded,
    /// Nextflow exited non-zero. If a task failed, `tasks` says which;
    /// if none did, the problem was in Nextflow or the pipeline itself.
    Failed {
        exit_code: Option<i32>,
    },
    TimedOut,
    Cancelled,
}

/// Nextflow's final status for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Completed,
    /// Reused from Nextflow's own cache (`-resume`).
    Cached,
    Failed,
    /// Stopped because the run was stopped.
    Aborted,
    /// Anything else Nextflow reports.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRecord {
    pub task_id: u64,
    /// Nextflow's task hash, e.g. `4e/01d43c`: also its work subdirectory.
    pub hash: String,
    /// E.g. `FASTQC_fastqc_1 (gut)`.
    pub name: String,
    pub process: String,
    /// The task's tag; ours is the sample name, or the node id for
    /// per-dataset tasks.
    pub tag: Option<String>,
    pub status: TaskStatus,
    pub exit_code: Option<i32>,
    /// Wall time from submission to completion.
    pub duration_ms: Option<u64>,
    /// Time the task's script actually ran.
    pub realtime_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputResult {
    /// The data name, `<node>__<port>`.
    pub name: String,
    pub cardinality: Cardinality,
    #[serde(rename = "type")]
    pub ty: SemanticType,
    /// Where the agent wrote this output's manifest.
    pub manifest: String,
    pub files: Vec<OutputFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputFile {
    /// Set for per-sample outputs.
    pub sample: Option<String>,
    pub path: String,
    pub size: u64,
    pub digest: Digest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::HashAlgorithm;

    #[test]
    fn json_shape() {
        let result = JobResult {
            status: JobStatus::Failed { exit_code: Some(1) },
            tasks: vec![TaskRecord {
                task_id: 1,
                hash: "4e/01d43c".to_string(),
                name: "FASTQC_fastqc_1 (gut)".to_string(),
                process: "FASTQC_fastqc_1".to_string(),
                tag: Some("gut".to_string()),
                status: TaskStatus::Failed,
                exit_code: Some(2),
                duration_ms: Some(5200),
                realtime_ms: None,
            }],
            outputs: vec![OutputResult {
                name: "fastqc_1__zip".to_string(),
                cardinality: Cardinality::PerSample,
                ty: SemanticType::QcReport,
                manifest: "/run/outputs/fastqc_1__zip.csv".to_string(),
                files: vec![OutputFile {
                    sample: Some("gut".to_string()),
                    path: "/run/results/fastqc_1__zip/gut/gut_1_fastqc.zip".to_string(),
                    size: 224744,
                    digest: Digest::from_bytes(HashAlgorithm::Sha256, &[0; 32]).unwrap(),
                }],
            }],
        };

        let json = serde_json::to_value(&result).unwrap();

        assert_eq!(
            json["status"],
            serde_json::json!({ "status": "failed", "exit_code": 1 })
        );
        assert_eq!(json["tasks"][0]["status"], "failed");
        assert_eq!(
            json["outputs"][0]["type"],
            serde_json::json!({ "format": "qc_report" })
        );
        assert_eq!(
            json["outputs"][0]["files"][0]["digest"],
            format!("sha256:{}", "00".repeat(32))
        );
        assert_eq!(serde_json::from_value::<JobResult>(json).unwrap(), result);
    }
}
