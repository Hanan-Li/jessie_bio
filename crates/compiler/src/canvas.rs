//! The canvas: the graph the frontend sends, exactly as the user drew it.
//!
//! This is untrusted input. Parsing only checks its shape; whether the graph
//! makes sense (ports exist, types match, no cycles) is checked later.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canvas {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// One box on the canvas: an instance of a node spec with its parameter values.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Unique within the canvas, e.g. "fastqc_1".
    pub id: String,
    /// Which node spec this is an instance of, e.g. "fastqc".
    pub spec: String,
    pub version: String,
    /// A BTreeMap keeps keys sorted, so iteration order is deterministic.
    /// That matters for golden files and, later, for cache-key hashing.
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
}

/// A wire from one node's output port to another node's input port.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: PortRef,
    pub to: PortRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortRef {
    pub node: String,
    pub port: String,
}

impl Canvas {
    pub fn from_json(json: &str) -> anyhow::Result<Canvas> {
        let canvas = serde_json::from_str(json).context("canvas JSON is malformed")?;
        Ok(canvas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FASTQC_MULTIQC: &str = include_str!("../testdata/fastqc_multiqc.canvas.json");

    #[test]
    fn parses_two_node_canvas() {
        let canvas = Canvas::from_json(FASTQC_MULTIQC).unwrap();

        assert_eq!(canvas.nodes.len(), 2);
        assert_eq!(canvas.nodes[0].spec, "fastqc");
        assert_eq!(
            canvas.edges[0].to,
            PortRef {
                node: "multiqc_1".to_string(),
                port: "multiqc_files".to_string(),
            }
        );
    }

    #[test]
    fn parses_params() {
        // Parsing accepts parameters; validation decides whether they're allowed.
        let json = r#"{"nodes": [{"id": "a", "spec": "s", "version": "1",
                                   "params": {"title": "My report"}}], "edges": []}"#;
        let canvas = Canvas::from_json(json).unwrap();
        assert_eq!(canvas.nodes[0].params["title"], "My report");
    }

    #[test]
    fn params_default_to_empty() {
        let canvas = Canvas::from_json(FASTQC_MULTIQC).unwrap();
        assert!(canvas.nodes[0].params.is_empty());
    }

    #[test]
    fn rejects_misspelled_field() {
        let json = r#"{"nodes": [], "edgse": []}"#;
        let err = Canvas::from_json(json).unwrap_err();
        // `{:#}` prints the whole error chain: our context, then serde's cause.
        let message = format!("{err:#}");
        assert!(message.contains("canvas JSON is malformed"), "{message}");
        assert!(message.contains("edgse"), "{message}");
    }
}
