//! Node specifications: the curated definition behind each palette entry (DESIGN.md §3.4).
//!
//! The canvas only says "use fastqc 0.1.0"; the spec says what that node's
//! ports and parameters are. Container, parameter schema and resource hints
//! arrive once codegen needs them.

use std::collections::BTreeMap;

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

impl NodeSpec {
    pub fn input(&self, name: &str) -> Option<&PortSpec> {
        self.inputs.iter().find(|port| port.name == name)
    }

    pub fn output(&self, name: &str) -> Option<&PortSpec> {
        self.outputs.iter().find(|port| port.name == name)
    }
}

/// All node specs the compiler knows about, looked up by (id, version).
#[derive(Debug, Clone, Default)]
pub struct SpecRegistry {
    specs: BTreeMap<(String, String), NodeSpec>,
}

impl SpecRegistry {
    pub fn new(specs: Vec<NodeSpec>) -> SpecRegistry {
        let mut registry = SpecRegistry::default();
        for spec in specs {
            registry
                .specs
                .insert((spec.id.clone(), spec.version.clone()), spec);
        }
        registry
    }

    pub fn get(&self, id: &str, version: &str) -> Option<&NodeSpec> {
        self.specs.get(&(id.to_string(), version.to_string()))
    }
}

/// Registry with the specs in `testdata/specs/`, shared by tests across modules.
#[cfg(test)]
pub(crate) fn test_registry() -> SpecRegistry {
    let sources = [
        include_str!("../testdata/specs/fastqc.spec.json"),
        include_str!("../testdata/specs/multiqc.spec.json"),
    ];
    let specs = sources
        .iter()
        .map(|json| serde_json::from_str(json).unwrap())
        .collect();
    SpecRegistry::new(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_looks_up_by_id_and_version() {
        let registry = test_registry();
        assert!(registry.get("fastqc", "0.1.0").is_some());
        assert!(registry.get("fastqc", "9.9.9").is_none());
        assert!(registry.get("nope", "0.1.0").is_none());
    }

    #[test]
    fn finds_ports_by_name() {
        let registry = test_registry();
        let multiqc = registry.get("multiqc", "0.1.0").unwrap();
        assert_eq!(multiqc.input("reports").unwrap().ty, SemanticType::QcReport);
        assert!(
            multiqc.input("html").is_none(),
            "html is an output, not an input"
        );
        assert!(multiqc.output("html").is_some());
    }

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
