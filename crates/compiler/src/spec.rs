//! Node specifications: the curated definition behind each palette entry (DESIGN.md §3.4).
//!
//! The canvas only says "use fastqc 0.1.0"; the spec says what that node's
//! ports and parameters are. Container, parameter schema and resource hints
//! arrive once codegen needs them.

use serde::Deserialize;

use crate::types::{Cardinality, SemanticType};

/// How a node executes (DESIGN.md §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionClass {
    /// Compiled into a Nextflow segment.
    Batch,
    /// Run directly in a container by the agent, bypassing Nextflow.
    Analysis,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortSpec {
    pub name: String,
    // `type` is a Rust keyword, so the field is `ty` and serde renames it.
    #[serde(rename = "type")]
    pub ty: SemanticType,
    pub cardinality: Cardinality,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    pub id: String,
    pub version: String,
    pub class: ExecutionClass,
    pub inputs: Vec<PortSpec>,
    pub outputs: Vec<PortSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fastqc_spec() {
        let json = r#"{
            "id": "fastqc",
            "version": "0.1.0",
            "class": "batch",
            "inputs": [
                {"name": "reads", "type": {"format": "fastq", "paired": true}, "cardinality": "per_sample"}
            ],
            "outputs": [
                {"name": "report", "type": {"format": "qc_report"}, "cardinality": "per_sample"}
            ]
        }"#;

        let spec: NodeSpec = serde_json::from_str(json).unwrap();

        assert_eq!(spec.class, ExecutionClass::Batch);
        assert_eq!(spec.inputs[0].ty, SemanticType::Fastq { paired: true });
        assert_eq!(spec.outputs[0].cardinality, Cardinality::PerSample);
    }
}
