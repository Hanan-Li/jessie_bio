//! Segmentation: split a `PipelineGraph` into the jobs that will actually run
//! (DESIGN.md §3.3).
//!
//! - Batch nodes are grouped into **segments**; each segment becomes one
//!   Nextflow run, so Nextflow can pipeline samples through it.
//! - Each analysis node is its own job: the agent runs its container
//!   directly, without Nextflow.
//!
//! ## Why segments must be convex
//!
//! Grouping *every* connected batch node together is not enough:
//!
//! ```text
//!   A (batch) ─────────────► B (batch)
//!       └──► X (analysis) ───────┘
//! ```
//!
//! A and B are connected, but one segment {A, B} would have to run both
//! before X (A feeds X) and after X (X feeds B). A segment must be
//! **convex**: no path may leave it and come back in.
//!
//! ## How grouping works
//!
//! Each node gets an *analysis depth*: the largest number of analysis nodes
//! on any path leading into it. Two batch nodes joined by an edge share a
//! segment only if their depths are equal. A path that leaves a segment
//! through an analysis node and comes back would have to arrive with a
//! larger depth, so it can never re-enter the same segment.
//!
//! The rule is conservative: it can split a segment that could safely have
//! stayed whole (e.g. when an independent analysis node feeds the middle of
//! a batch chain). Every boundary costs a Nextflow barrier, not correctness.

use std::collections::{BTreeMap, BTreeSet};

use petgraph::Direction::Incoming;
use petgraph::algo::toposort;
use petgraph::graph::DiGraph;
use petgraph::stable_graph::NodeIndex;
use petgraph::unionfind::UnionFind;
use petgraph::visit::{EdgeRef, IntoEdgeReferences, NodeIndexable};

use crate::pipeline_graph::PipelineGraph;
use crate::spec::ExecutionClass;

/// The jobs to run for one pipeline, in dependency order.
///
/// Like `PipelineGraph`, the fields are private so the guarantees hold:
/// every node is in exactly one job, and every job comes after the jobs it
/// depends on.
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    // The plan owns its graph: the `NodeIndex` values inside `jobs` only
    // mean something for this graph, so the two travel together.
    pipeline: PipelineGraph,
    jobs: Vec<Job>,
}

/// A job's position in `ExecutionPlan::jobs`.
///
/// A "newtype": a `usize` wrapped in its own type, so a job id can't be
/// mixed up with a node index or any other number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub kind: JobKind,
    /// Jobs whose outputs this job consumes. All of them come earlier in
    /// the plan.
    pub depends_on: Vec<JobId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobKind {
    /// One Nextflow run. `nodes` is in topological order, which is the
    /// order codegen emits process calls in.
    Segment { nodes: Vec<NodeIndex> },
    /// One analysis node, run directly by the agent.
    Analysis { node: NodeIndex },
}

impl Job {
    /// The graph nodes this job runs, whatever its kind.
    pub fn nodes(&self) -> &[NodeIndex] {
        match &self.kind {
            JobKind::Segment { nodes } => nodes,
            JobKind::Analysis { node } => std::slice::from_ref(node),
        }
    }
}

impl ExecutionPlan {
    pub fn build(pipeline: PipelineGraph) -> ExecutionPlan {
        let graph = pipeline.graph();
        let is_analysis = |node: NodeIndex| graph[node].spec.class == ExecutionClass::Analysis;

        let node_order =
            toposort(graph, None).expect("a PipelineGraph has no cycles: validation rejects them");

        // 1. Analysis depth of every node, visiting each node after all of
        //    its predecessors.
        let mut depth: BTreeMap<NodeIndex, usize> = BTreeMap::new();
        for &node in &node_order {
            let node_depth = graph
                .neighbors_directed(node, Incoming)
                .map(|pred| depth[&pred] + usize::from(is_analysis(pred)))
                .max()
                .unwrap_or(0);
            depth.insert(node, node_depth);
        }

        // 2. Group batch nodes: join the two ends of every batch → batch
        //    edge whose ends have equal depth.
        let mut groups = UnionFind::<usize>::new(graph.node_bound());
        for edge in graph.edge_references() {
            let (from, to) = (edge.source(), edge.target());
            if !is_analysis(from) && !is_analysis(to) && depth[&from] == depth[&to] {
                groups.union(from.index(), to.index());
            }
        }

        // 3. Collect each group's nodes (and each analysis node alone) into
        //    a provisional job. Walking in topological order keeps the
        //    nodes inside each job topologically ordered too.
        let mut job_nodes: Vec<Vec<NodeIndex>> = Vec::new();
        let mut job_of_node: BTreeMap<NodeIndex, usize> = BTreeMap::new();
        let mut job_of_group: BTreeMap<usize, usize> = BTreeMap::new();
        for &node in &node_order {
            let job = if is_analysis(node) {
                job_nodes.push(Vec::new());
                job_nodes.len() - 1
            } else {
                let group = groups.find(node.index());
                *job_of_group.entry(group).or_insert_with(|| {
                    job_nodes.push(Vec::new());
                    job_nodes.len() - 1
                })
            };
            job_nodes[job].push(node);
            job_of_node.insert(node, job);
        }

        // 4. Build the job graph (an edge wherever data crosses between
        //    jobs) and sort it, so every job comes after its dependencies.
        let mut job_graph: DiGraph<(), ()> = DiGraph::new();
        let job_indices: Vec<_> = job_nodes.iter().map(|_| job_graph.add_node(())).collect();
        for edge in graph.edge_references() {
            let from_job = job_of_node[&edge.source()];
            let to_job = job_of_node[&edge.target()];
            if from_job != to_job {
                // `update_edge` adds the edge only if it isn't there yet.
                job_graph.update_edge(job_indices[from_job], job_indices[to_job], ());
            }
        }
        let job_order =
            toposort(&job_graph, None).expect("segments are convex, so jobs have no cycles");

        // 5. Number the jobs in that order and resolve their dependencies.
        let final_id: BTreeMap<usize, JobId> = job_order
            .iter()
            .enumerate()
            .map(|(position, index)| (index.index(), JobId(position)))
            .collect();
        let jobs = job_order
            .iter()
            .map(|&index| {
                let nodes = job_nodes[index.index()].clone();
                let depends_on: BTreeSet<JobId> = job_graph
                    .neighbors_directed(index, Incoming)
                    .map(|dep| final_id[&dep.index()])
                    .collect();
                let kind = match nodes[..] {
                    [node] if is_analysis(node) => JobKind::Analysis { node },
                    _ => JobKind::Segment { nodes },
                };
                Job {
                    kind,
                    depends_on: depends_on.into_iter().collect(),
                }
            })
            .collect();

        // `graph` borrowed `pipeline`; that borrow ended with its last use
        // above, so `pipeline` can now be moved into the plan.
        ExecutionPlan { pipeline, jobs }
    }

    pub fn pipeline(&self) -> &PipelineGraph {
        &self.pipeline
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::canvas::Canvas;
    use crate::spec::{NodeSpec, SpecRegistry, test_registry};
    use crate::validate::validate;

    /// Generic test nodes: two optional inputs, one output, all QC reports
    /// per sample, so any node can feed any other.
    fn generic_spec(id: &str, class: &str) -> NodeSpec {
        let port = |name: &str| json!({ "name": name, "type": { "format": "qc_report" }, "cardinality": "per_sample" });
        serde_json::from_value(json!({
            "id": id,
            "version": "0.1.0",
            "class": class,
            "inputs": [port("in1"), port("in2")],
            "outputs": [port("out")],
        }))
        .unwrap()
    }

    /// Build a plan from node names and edges. Names starting with `a` are
    /// analysis nodes; all others are batch. Edges go `out` → the first
    /// free input of the target.
    fn plan(nodes: &[&str], edges: &[(&str, &str)]) -> ExecutionPlan {
        let registry = SpecRegistry::new(vec![
            generic_spec("batch", "batch"),
            generic_spec("analysis", "analysis"),
        ]);
        let canvas_nodes: Vec<_> = nodes
            .iter()
            .map(|id| {
                let spec = if id.starts_with('a') {
                    "analysis"
                } else {
                    "batch"
                };
                json!({ "id": id, "spec": spec, "version": "0.1.0" })
            })
            .collect();
        let mut inputs_used: BTreeMap<&str, usize> = BTreeMap::new();
        let canvas_edges: Vec<_> = edges
            .iter()
            .map(|&(from, to)| {
                let used = inputs_used.entry(to).or_insert(0);
                *used += 1;
                json!({
                    "from": { "node": from, "port": "out" },
                    "to": { "node": to, "port": format!("in{used}") },
                })
            })
            .collect();
        let canvas = json!({ "nodes": canvas_nodes, "edges": canvas_edges }).to_string();
        let pipeline = validate(&Canvas::from_json(&canvas).unwrap(), &registry).unwrap();
        ExecutionPlan::build(pipeline)
    }

    /// Each job's node ids, in plan order, for readable assertions.
    fn job_ids(plan: &ExecutionPlan) -> Vec<Vec<String>> {
        let graph = plan.pipeline().graph();
        plan.jobs()
            .iter()
            .map(|job| job.nodes().iter().map(|&n| graph[n].id.clone()).collect())
            .collect()
    }

    /// The guarantees every plan must meet, whatever its shape.
    fn assert_plan_invariants(plan: &ExecutionPlan) {
        let graph = plan.pipeline().graph();
        let mut job_of = BTreeMap::new();
        let mut position_in_job = BTreeMap::new();
        for (job_index, job) in plan.jobs().iter().enumerate() {
            for (position, &node) in job.nodes().iter().enumerate() {
                assert!(job_of.insert(node, job_index).is_none(), "node in two jobs");
                position_in_job.insert(node, position);
            }
            for dep in &job.depends_on {
                assert!(
                    dep.0 < job_index,
                    "job {job_index} depends on later job {}",
                    dep.0
                );
            }
        }
        assert_eq!(job_of.len(), graph.node_count(), "every node is in a job");

        for edge in graph.edge_references() {
            let (from, to) = (job_of[&edge.source()], job_of[&edge.target()]);
            if from == to {
                assert!(
                    position_in_job[&edge.source()] < position_in_job[&edge.target()],
                    "nodes inside a job are in topological order"
                );
            } else {
                assert!(
                    plan.jobs()[to].depends_on.contains(&JobId(from)),
                    "job {to} consumes job {from}'s output, so must depend on it"
                );
            }
        }
    }

    #[test]
    fn connected_batch_nodes_form_one_segment() {
        let canvas =
            Canvas::from_json(include_str!("../testdata/fastqc_multiqc.canvas.json")).unwrap();
        let plan = ExecutionPlan::build(validate(&canvas, &test_registry()).unwrap());

        assert_plan_invariants(&plan);
        assert_eq!(job_ids(&plan), vec![vec!["fastqc_1", "multiqc_1"]]);
        assert!(matches!(plan.jobs()[0].kind, JobKind::Segment { .. }));
    }

    #[test]
    fn each_analysis_node_is_its_own_job() {
        // b1 → b2 → a1 → a2: the typical shape (heavy batch, then light analysis).
        let plan = plan(
            &["b1", "b2", "a1", "a2"],
            &[("b1", "b2"), ("b2", "a1"), ("a1", "a2")],
        );

        assert_plan_invariants(&plan);
        assert_eq!(
            job_ids(&plan),
            vec![vec!["b1", "b2"], vec!["a1"], vec!["a2"]]
        );
        assert!(matches!(plan.jobs()[1].kind, JobKind::Analysis { .. }));
        assert_eq!(plan.jobs()[1].depends_on, vec![JobId(0)]);
        assert_eq!(plan.jobs()[2].depends_on, vec![JobId(1)]);
    }

    #[test]
    fn analysis_node_between_batch_nodes_splits_the_segment() {
        let plan = plan(&["b1", "a1", "b2"], &[("b1", "a1"), ("a1", "b2")]);

        assert_plan_invariants(&plan);
        assert_eq!(job_ids(&plan), vec![vec!["b1"], vec!["a1"], vec!["b2"]]);
    }

    #[test]
    fn segments_are_convex() {
        // b1 → b2 directly AND b1 → a1 → b2. Grouping b1 and b2 would make
        // their segment both feed and depend on a1.
        let plan = plan(
            &["b1", "a1", "b2"],
            &[("b1", "a1"), ("a1", "b2"), ("b1", "b2")],
        );

        assert_plan_invariants(&plan);
        assert_eq!(job_ids(&plan), vec![vec!["b1"], vec!["a1"], vec!["b2"]]);
        assert_eq!(plan.jobs()[2].depends_on, vec![JobId(0), JobId(1)]);
    }

    #[test]
    fn disconnected_batch_chains_are_separate_segments() {
        let plan = plan(&["b1", "b2", "b3", "b4"], &[("b1", "b2"), ("b3", "b4")]);

        assert_plan_invariants(&plan);
        assert_eq!(plan.jobs().len(), 2);
        assert!(plan.jobs().iter().all(|job| job.depends_on.is_empty()));
    }

    #[test]
    fn independent_analysis_input_splits_conservatively() {
        // a1 is independent of b1, so {b1, b2} could safely be one segment,
        // but b2's depth (1) differs from b1's (0). Documents the
        // conservative behaviour; a smarter rule could merge them.
        let plan = plan(&["b1", "a1", "b2"], &[("b1", "b2"), ("a1", "b2")]);

        assert_plan_invariants(&plan);
        assert_eq!(plan.jobs().len(), 3);
    }

    #[test]
    fn diamond_of_batch_nodes_is_one_segment() {
        // b1 fans out to b2 and b3, which both feed b4.
        let plan = plan(
            &["b1", "b2", "b3", "b4"],
            &[("b1", "b2"), ("b1", "b3"), ("b2", "b4"), ("b3", "b4")],
        );

        assert_plan_invariants(&plan);
        assert_eq!(plan.jobs().len(), 1);
    }
}
