//! Node specifications: the curated definition behind each palette entry (DESIGN.md §3.4).
//!
//! The canvas only says "use fastqc 0.1.0"; the spec says what that node's
//! ports are and how it executes. Parameter schema and resource hints arrive
//! once codegen needs them.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::types::{Cardinality, SemanticType};

/// How a node executes (DESIGN.md §5.1), with what each class needs to run.
///
/// In JSON: `{"class": "batch", "module": {...}}` or
/// `{"class": "analysis", "container": "..."}`. A batch node without a module
/// cannot be represented.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum Execution {
    /// Compiled into a Nextflow segment.
    Batch { module: NextflowModule },
    /// Run directly in a container by the agent, bypassing Nextflow.
    Analysis { container: String },
}

/// The Nextflow process behind a batch node.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextflowModule {
    /// The process name declared in the module, e.g. `FASTQC`.
    pub process: String,
    /// The module file, relative to the node's directory, e.g. `main.nf`.
    pub file: String,
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
    pub execution: Execution,
    pub inputs: Vec<PortSpec>,
    pub outputs: Vec<PortSpec>,
}

impl NodeSpec {
    pub fn is_analysis(&self) -> bool {
        matches!(self.execution, Execution::Analysis { .. })
    }

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

/// Registry with the repo's real node library (`nodes/`), shared by tests.
#[cfg(test)]
pub(crate) fn test_registry() -> SpecRegistry {
    let nodes = crate::library::load_library(&crate::library::repo_library_dir()).unwrap();
    SpecRegistry::new(nodes.into_iter().map(|node| node.spec).collect())
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
        assert_eq!(
            multiqc.input("multiqc_files").unwrap().ty,
            SemanticType::QcReport
        );
        assert!(
            multiqc.input("report").is_none(),
            "report is an output, not an input"
        );
        assert!(multiqc.output("report").is_some());
    }

    #[test]
    fn parses_fastqc_spec() {
        let json = r#"{
            "id": "fastqc",
            "version": "0.1.0",
            "execution": {"class": "batch", "module": {"process": "FASTQC", "file": "main.nf"}},
            "inputs": [
                {"name": "reads", "type": {"format": "fastq", "paired": true}, "cardinality": "per_sample"}
            ],
            "outputs": [
                {"name": "zip", "type": {"format": "qc_report"}, "cardinality": "per_sample"}
            ]
        }"#;

        let spec: NodeSpec = serde_json::from_str(json).unwrap();

        assert!(!spec.is_analysis());
        assert_eq!(
            spec.execution,
            Execution::Batch {
                module: NextflowModule {
                    process: "FASTQC".to_string(),
                    file: "main.nf".to_string(),
                }
            }
        );
        assert_eq!(spec.inputs[0].ty, SemanticType::Fastq { paired: true });
        assert_eq!(spec.outputs[0].cardinality, Cardinality::PerSample);
    }

    #[test]
    fn batch_node_requires_a_module() {
        let json = r#"{
            "id": "broken",
            "version": "0.1.0",
            "execution": {"class": "batch"},
            "inputs": [],
            "outputs": []
        }"#;
        assert!(serde_json::from_str::<NodeSpec>(json).is_err());
    }
}
