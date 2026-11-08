//! Canvas + spec registry → `PipelineGraph`, or every problem found along the way.
//!
//! Validation reports all errors rather than stopping at the first, so the
//! canvas UI can highlight every broken node and edge at once.

use std::collections::{BTreeMap, BTreeSet};

use petgraph::algo::toposort;
use petgraph::stable_graph::{NodeIndex, StableDiGraph};

use crate::canvas::Canvas;
use crate::pipeline_graph::{GraphEdge, GraphNode, PipelineGraph, Wiring};
use crate::spec::SpecRegistry;
use crate::types::{SemanticType, can_connect};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("more than one node has the id `{0}`")]
    DuplicateNodeId(String),

    #[error(
        "node id `{0}` must start with a letter, contain only letters, digits and underscores, and not contain `__`"
    )]
    InvalidNodeId(String),

    #[error("node `{node}` uses `{spec}` version {version}, which does not exist")]
    UnknownSpec {
        node: String,
        spec: String,
        version: String,
    },

    #[error("an edge refers to node `{0}`, which is not on the canvas")]
    UnknownNode(String),

    #[error("node `{node}` has no output called `{port}`")]
    UnknownOutputPort { node: String, port: String },

    #[error("node `{node}` has no input called `{port}`")]
    UnknownInputPort { node: String, port: String },

    #[error("cannot connect `{from}` ({output:?}) to `{to}` ({input:?})")]
    TypeMismatch {
        from: String,
        to: String,
        output: SemanticType,
        input: SemanticType,
    },

    #[error("input `{node}.{port}` is connected more than once")]
    InputConnectedTwice { node: String, port: String },

    #[error("node `{0}` is part of a cycle")]
    Cycle(String),
}

pub fn validate(
    canvas: &Canvas,
    registry: &SpecRegistry,
) -> Result<PipelineGraph, Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut graph = StableDiGraph::new();
    // Canvas id → graph index, for nodes whose spec was found.
    let mut indices: BTreeMap<String, NodeIndex> = BTreeMap::new();
    // Every id on the canvas, including nodes with unknown specs.
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();

    for node in &canvas.nodes {
        if !seen_ids.insert(node.id.clone()) {
            errors.push(ValidationError::DuplicateNodeId(node.id.clone()));
            continue;
        }
        // Still added to the graph below: the id is only a naming problem,
        // and its edges can be checked as usual.
        if !is_valid_node_id(&node.id) {
            errors.push(ValidationError::InvalidNodeId(node.id.clone()));
        }
        let Some(spec) = registry.get(&node.spec, &node.version) else {
            errors.push(ValidationError::UnknownSpec {
                node: node.id.clone(),
                spec: node.spec.clone(),
                version: node.version.clone(),
            });
            continue;
        };
        let index = graph.add_node(GraphNode {
            id: node.id.clone(),
            spec: spec.clone(),
            params: node.params.clone(),
        });
        indices.insert(node.id.clone(), index);
    }

    // (node id, input port) pairs that already have an incoming edge.
    let mut connected_inputs: BTreeSet<(String, String)> = BTreeSet::new();

    for edge in &canvas.edges {
        let (from, to) = (&edge.from, &edge.to);

        for id in [&from.node, &to.node] {
            if !seen_ids.contains(id) {
                errors.push(ValidationError::UnknownNode(id.clone()));
            }
        }
        // If either end is missing (unknown node, or unknown spec already
        // reported above), there are no ports to check.
        let (Some(&from_index), Some(&to_index)) = (indices.get(&from.node), indices.get(&to.node))
        else {
            continue;
        };

        let output = graph[from_index].spec.output(&from.port);
        let input = graph[to_index].spec.input(&to.port);
        if output.is_none() {
            errors.push(ValidationError::UnknownOutputPort {
                node: from.node.clone(),
                port: from.port.clone(),
            });
        }
        if input.is_none() {
            errors.push(ValidationError::UnknownInputPort {
                node: to.node.clone(),
                port: to.port.clone(),
            });
        }
        let (Some(output), Some(input)) = (output, input) else {
            continue;
        };

        if !can_connect(&output.ty, &input.ty) {
            errors.push(ValidationError::TypeMismatch {
                from: format!("{}.{}", from.node, from.port),
                to: format!("{}.{}", to.node, to.port),
                output: output.ty.clone(),
                input: input.ty.clone(),
            });
            continue;
        }

        if !connected_inputs.insert((to.node.clone(), to.port.clone())) {
            errors.push(ValidationError::InputConnectedTwice {
                node: to.node.clone(),
                port: to.port.clone(),
            });
            continue;
        }

        let graph_edge = GraphEdge {
            from_port: from.port.clone(),
            to_port: to.port.clone(),
            wiring: Wiring::between(output.cardinality, input.cardinality),
        };
        graph.add_edge(from_index, to_index, graph_edge);
    }

    if let Err(cycle) = toposort(&graph, None) {
        errors.push(ValidationError::Cycle(graph[cycle.node_id()].id.clone()));
    }

    if errors.is_empty() {
        Ok(PipelineGraph::new(graph))
    } else {
        Err(errors)
    }
}

/// Node ids become part of Nextflow process aliases (`FASTQC as FASTQC_qc_1`),
/// so they must be valid Nextflow identifiers. They also may not contain
/// `__`, which separates node from port in data names (`qc_1__zip`).
fn is_valid_node_id(id: &str) -> bool {
    let mut chars = id.chars();
    let identifier = match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    };
    identifier && !id.contains("__")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::spec::{NodeSpec, test_registry};

    fn canvas(value: serde_json::Value) -> Canvas {
        Canvas::from_json(&value.to_string()).unwrap()
    }

    fn edge(from: &str, from_port: &str, to: &str, to_port: &str) -> serde_json::Value {
        json!({
            "from": { "node": from, "port": from_port },
            "to": { "node": to, "port": to_port },
        })
    }

    fn node(id: &str, spec: &str) -> serde_json::Value {
        json!({ "id": id, "spec": spec, "version": "0.1.0" })
    }

    #[test]
    fn valid_canvas_becomes_pipeline_graph() {
        let canvas =
            Canvas::from_json(include_str!("../testdata/fastqc_multiqc.canvas.json")).unwrap();

        let pipeline = validate(&canvas, &test_registry()).unwrap();

        assert_eq!(pipeline.graph().node_count(), 2);
        assert_eq!(pipeline.graph().edge_count(), 1);
        let multiqc = pipeline.index_of("multiqc_1").unwrap();
        assert_eq!(
            pipeline.graph()[multiqc].params["title"],
            "My first QC report"
        );
        let edge = pipeline.graph().edge_weights().next().unwrap();
        assert_eq!(edge.wiring, Wiring::Gather);
    }

    #[test]
    fn unconnected_inputs_are_allowed() {
        // fastqc.reads has no incoming edge: it becomes an input of the run.
        let canvas = canvas(json!({ "nodes": [node("fastqc_1", "fastqc")], "edges": [] }));
        assert!(validate(&canvas, &test_registry()).is_ok());
    }

    #[test]
    fn reports_duplicate_node_id() {
        let canvas = canvas(json!({
            "nodes": [node("qc", "fastqc"), node("qc", "multiqc")],
            "edges": [],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert_eq!(
            errors,
            vec![ValidationError::DuplicateNodeId("qc".to_string())]
        );
    }

    #[test]
    fn reports_invalid_node_ids() {
        let canvas = canvas(json!({
            "nodes": [
                node("qc 1", "fastqc"),
                node("1qc", "fastqc"),
                node("qc__1", "fastqc"),
                node("qc_1", "fastqc"),
            ],
            "edges": [],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert_eq!(
            errors,
            vec![
                ValidationError::InvalidNodeId("qc 1".to_string()),
                ValidationError::InvalidNodeId("1qc".to_string()),
                ValidationError::InvalidNodeId("qc__1".to_string()),
            ]
        );
    }

    #[test]
    fn reports_unknown_spec_and_skips_its_edges() {
        let canvas = canvas(json!({
            "nodes": [node("a", "fastqc"), node("b", "star_align")],
            "edges": [edge("a", "zip", "b", "reads")],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert_eq!(
            errors,
            vec![ValidationError::UnknownSpec {
                node: "b".to_string(),
                spec: "star_align".to_string(),
                version: "0.1.0".to_string(),
            }]
        );
    }

    #[test]
    fn reports_edge_to_missing_node() {
        let canvas = canvas(json!({
            "nodes": [node("a", "fastqc")],
            "edges": [edge("a", "zip", "ghost", "multiqc_files")],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert_eq!(
            errors,
            vec![ValidationError::UnknownNode("ghost".to_string())]
        );
    }

    #[test]
    fn reports_unknown_ports() {
        let canvas = canvas(json!({
            "nodes": [node("a", "fastqc"), node("b", "multiqc")],
            "edges": [edge("a", "pdf", "b", "report")],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert_eq!(
            errors,
            vec![
                ValidationError::UnknownOutputPort {
                    node: "a".to_string(),
                    port: "pdf".to_string(),
                },
                ValidationError::UnknownInputPort {
                    node: "b".to_string(),
                    port: "report".to_string(),
                },
            ]
        );
    }

    #[test]
    fn reports_type_mismatch() {
        // A QC report is not FASTQ.
        let canvas = canvas(json!({
            "nodes": [node("a", "fastqc"), node("b", "fastqc")],
            "edges": [edge("a", "zip", "b", "reads")],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert!(
            matches!(&errors[..], [ValidationError::TypeMismatch { from, .. }] if from == "a.zip"),
            "{errors:?}"
        );
    }

    #[test]
    fn reports_input_connected_twice() {
        let canvas = canvas(json!({
            "nodes": [node("a", "fastqc"), node("b", "fastqc"), node("m", "multiqc")],
            "edges": [
                edge("a", "zip", "m", "multiqc_files"),
                edge("b", "zip", "m", "multiqc_files"),
            ],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert_eq!(
            errors,
            vec![ValidationError::InputConnectedTwice {
                node: "m".to_string(),
                port: "multiqc_files".to_string(),
            }]
        );
    }

    #[test]
    fn reports_cycle() {
        // A spec whose output type matches its input type can be wired in a loop.
        let passthrough: NodeSpec = serde_json::from_value(json!({
            "id": "passthrough",
            "version": "0.1.0",
            "execution": { "class": "batch", "module": { "process": "PASSTHROUGH", "file": "main.nf" } },
            "inputs": [{ "name": "in", "type": { "format": "qc_report" }, "cardinality": "per_sample" }],
            "outputs": [{ "name": "out", "type": { "format": "qc_report" }, "cardinality": "per_sample" }],
        }))
        .unwrap();
        let registry = SpecRegistry::new(vec![passthrough]);
        let canvas = canvas(json!({
            "nodes": [node("a", "passthrough"), node("b", "passthrough")],
            "edges": [edge("a", "out", "b", "in"), edge("b", "out", "a", "in")],
        }));

        let errors = validate(&canvas, &registry).unwrap_err();

        assert!(
            matches!(&errors[..], [ValidationError::Cycle(_)]),
            "{errors:?}"
        );
    }

    #[test]
    fn reports_every_error_not_just_the_first() {
        let canvas = canvas(json!({
            "nodes": [node("a", "fastqc"), node("a", "fastqc"), node("x", "nope")],
            "edges": [edge("a", "zip", "ghost", "in")],
        }));
        let errors = validate(&canvas, &test_registry()).unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
    }
}
