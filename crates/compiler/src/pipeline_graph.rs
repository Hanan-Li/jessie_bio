//! `PipelineGraph`: a canvas that has passed validation.
//!
//! This is the compiler's intermediate representation (IR). Later stages
//! (segmentation, codegen) take a `PipelineGraph`, never a raw `Canvas`, so
//! they can rely on every spec and port existing, every edge being
//! type-correct and the graph having no cycles.
//!
//! The only way to get one is `validate::validate`: the graph field is
//! private and the constructor is `pub(crate)`, so code outside this crate
//! cannot build an unchecked `PipelineGraph`.

use std::collections::BTreeMap;

use petgraph::stable_graph::{NodeIndex, StableDiGraph};

use crate::spec::NodeSpec;
use crate::types::Cardinality;

#[derive(Debug, Clone)]
pub struct PipelineGraph {
    // StableDiGraph rather than DiGraph: removing a node from a DiGraph moves
    // the last node into its slot, silently changing that node's index.
    // Cache pruning removes nodes, so indices must stay put.
    graph: StableDiGraph<GraphNode, GraphEdge>,
}

impl PipelineGraph {
    /// Only validation may call this: it is what upholds the guarantees above.
    pub(crate) fn new(graph: StableDiGraph<GraphNode, GraphEdge>) -> PipelineGraph {
        PipelineGraph { graph }
    }

    /// Read-only access to the graph. A shared reference (`&`) lets callers
    /// run any petgraph query but never add or remove nodes or edges.
    pub fn graph(&self) -> &StableDiGraph<GraphNode, GraphEdge> {
        &self.graph
    }

    /// Find a node's graph index by its canvas id.
    pub fn index_of(&self, id: &str) -> Option<NodeIndex> {
        self.graph
            .node_indices()
            .find(|&index| self.graph[index].id == id)
    }
}

/// A canvas node joined with its spec.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: String,
    pub spec: NodeSpec,
    pub params: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge {
    pub from_port: String,
    pub to_port: String,
    pub wiring: Wiring,
}

/// How items flow along an edge, decided by the cardinality at each end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wiring {
    /// Same cardinality on both ends: items pass through one to one.
    Direct,
    /// Per-sample into per-dataset: collect every sample's item into one list.
    /// Codegen emits `.collect()`.
    Gather,
    /// Per-dataset into per-sample: every sample receives the same item.
    Broadcast,
}

impl Wiring {
    pub fn between(output: Cardinality, input: Cardinality) -> Wiring {
        match (output, input) {
            (Cardinality::PerSample, Cardinality::PerDataset) => Wiring::Gather,
            (Cardinality::PerDataset, Cardinality::PerSample) => Wiring::Broadcast,
            (Cardinality::PerSample, Cardinality::PerSample)
            | (Cardinality::PerDataset, Cardinality::PerDataset) => Wiring::Direct,
        }
    }
}
