//! Job interfaces: what a job consumes and produces (DESIGN.md §3.3).
//!
//! The compiler works out each job's interface; the agent uses it to know
//! which manifests to hand the job and which outputs to collect, hash and
//! report afterwards.
//!
//! Every piece of data in a run has one name, `<node>__<port>`, used
//! everywhere: the job input that receives it, the job output that produced
//! it, and the Nextflow `params.<name>` that points at its manifest.

use serde::{Deserialize, Serialize};

use crate::types::{Cardinality, SemanticType};

/// A job's position in its execution plan.
///
/// A "newtype": a `usize` wrapped in its own type so it can't be mixed up
/// with any other number. `transparent` makes it plain `3` in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub usize);

/// The name of the data on one node's port, unique within a run.
///
/// Node ids may not contain `__` (validation enforces it), so two different
/// node/port pairs can never produce the same name.
pub fn data_name(node: &str, port: &str) -> String {
    format!("{node}__{port}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobInterface {
    /// Sorted by name.
    pub inputs: Vec<JobInput>,
    /// Every output port of every node in the job, sorted by name. All are
    /// published and hashed, not only those a later job reads, so the
    /// per-node cache has every result.
    pub outputs: Vec<JobOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobInput {
    /// The data's name, and the Nextflow parameter that receives the path
    /// to its manifest.
    pub name: String,
    /// The shape of the data as it arrives. For data from an earlier job,
    /// that's the producing port's cardinality: a gather (`.collect()`)
    /// happens inside this job, after the manifest is read.
    pub cardinality: Cardinality,
    #[serde(rename = "type")]
    pub ty: SemanticType,
    pub source: InputSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputSource {
    /// An input port nothing on the canvas feeds. The run supplies it, e.g.
    /// the reads from the samplesheet.
    Run { node: String, port: String },
    /// An output of an earlier job in the same plan.
    Job {
        job: JobId,
        node: String,
        port: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobOutput {
    pub name: String,
    pub node: String,
    pub port: String,
    pub cardinality: Cardinality,
    #[serde(rename = "type")]
    pub ty: SemanticType,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_json_shape() {
        let interface = JobInterface {
            inputs: vec![JobInput {
                name: data_name("fastqc_1", "reads"),
                cardinality: Cardinality::PerSample,
                ty: SemanticType::Fastq { paired: true },
                source: InputSource::Run {
                    node: "fastqc_1".to_string(),
                    port: "reads".to_string(),
                },
            }],
            outputs: vec![],
        };

        let json = serde_json::to_value(&interface).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "inputs": [{
                    "name": "fastqc_1__reads",
                    "cardinality": "per_sample",
                    "type": { "format": "fastq", "paired": true },
                    "source": { "from": "run", "node": "fastqc_1", "port": "reads" },
                }],
                "outputs": [],
            })
        );
        let back: JobInterface = serde_json::from_value(json).unwrap();
        assert_eq!(back, interface);
    }

    #[test]
    fn job_id_is_a_plain_number_in_json() {
        assert_eq!(serde_json::to_string(&JobId(3)).unwrap(), "3");
    }
}
