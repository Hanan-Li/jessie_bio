//! Working out what each job consumes and produces (DESIGN.md §3.3).
//!
//! For every input port of every node in a job, look at what feeds it:
//!
//! - nothing → a **run input** the run must supply (e.g. reads from the
//!   samplesheet)
//! - a node in the same job → internal wiring, not part of the interface
//! - a node in another job → an input **from that job's outputs**
//!
//! Outputs are simpler: every output port of every node in the job.

use std::collections::BTreeMap;

use petgraph::Direction::Incoming;
use petgraph::visit::EdgeRef;
use protocol::job::{InputSource, JobInput, JobInterface, JobOutput, data_name};

use crate::execution_plan::{ExecutionPlan, JobId};

// An `impl` block for a type can live in any module of the crate that
// defines the type. Keeping this one here keeps execution_plan.rs about
// segmentation only.
impl ExecutionPlan {
    pub fn interface(&self, job: JobId) -> JobInterface {
        let graph = self.pipeline().graph();
        // Keyed by name: sorts the interface, and merges repeats. If two
        // nodes in this job read the same upstream output, the job still
        // receives it once.
        let mut inputs: BTreeMap<String, JobInput> = BTreeMap::new();
        let mut outputs: BTreeMap<String, JobOutput> = BTreeMap::new();

        for &node in self.jobs()[job.0].nodes() {
            let graph_node = &graph[node];

            for port in &graph_node.spec.inputs {
                let feeding_edge = graph
                    .edges_directed(node, Incoming)
                    .find(|edge| edge.weight().to_port == port.name);

                let input = match feeding_edge {
                    None => JobInput {
                        name: data_name(&graph_node.id, &port.name),
                        cardinality: port.cardinality,
                        ty: port.ty.clone(),
                        source: InputSource::Run {
                            node: graph_node.id.clone(),
                            port: port.name.clone(),
                        },
                    },
                    Some(edge) => {
                        let upstream = edge.source();
                        let upstream_job = self.job_of(upstream);
                        if upstream_job == job {
                            continue; // wired inside the job
                        }
                        let upstream_node = &graph[upstream];
                        let from_port = &edge.weight().from_port;
                        let produced = upstream_node
                            .spec
                            .output(from_port)
                            .expect("validation checked every edge's ports exist");
                        JobInput {
                            name: data_name(&upstream_node.id, from_port),
                            cardinality: produced.cardinality,
                            ty: produced.ty.clone(),
                            source: InputSource::Job {
                                job: upstream_job,
                                node: upstream_node.id.clone(),
                                port: from_port.clone(),
                            },
                        }
                    }
                };
                inputs.insert(input.name.clone(), input);
            }

            for port in &graph_node.spec.outputs {
                let output = JobOutput {
                    name: data_name(&graph_node.id, &port.name),
                    node: graph_node.id.clone(),
                    port: port.name.clone(),
                    cardinality: port.cardinality,
                    ty: port.ty.clone(),
                };
                outputs.insert(output.name.clone(), output);
            }
        }

        JobInterface {
            inputs: inputs.into_values().collect(),
            outputs: outputs.into_values().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::types::{Cardinality, SemanticType};
    use serde_json::json;

    use super::*;
    use crate::canvas::Canvas;
    use crate::execution_plan::testing::{job_ids, plan};
    use crate::library::{load_library, repo_library_dir};
    use crate::spec::{NodeSpec, SpecRegistry, test_registry};
    use crate::validate::validate;

    fn names_of_inputs(interface: &JobInterface) -> Vec<&str> {
        interface
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect()
    }

    fn names_of_outputs(interface: &JobInterface) -> Vec<&str> {
        interface
            .outputs
            .iter()
            .map(|output| output.name.as_str())
            .collect()
    }

    #[test]
    fn fastqc_multiqc_interface() {
        let canvas =
            Canvas::from_json(include_str!("../testdata/fastqc_multiqc.canvas.json")).unwrap();
        let plan = ExecutionPlan::build(validate(&canvas, &test_registry()).unwrap());

        let interface = plan.interface(JobId(0));

        // The only input is the reads nothing feeds; fastqc → multiqc is
        // wired inside the segment.
        assert_eq!(
            interface.inputs,
            vec![JobInput {
                name: "fastqc_1__reads".to_string(),
                cardinality: Cardinality::PerSample,
                ty: SemanticType::Fastq { paired: true },
                source: InputSource::Run {
                    node: "fastqc_1".to_string(),
                    port: "reads".to_string(),
                },
            }]
        );
        // Every node's outputs, including fastqc's zip that multiqc already
        // consumed inside the job: all results are kept for the cache.
        assert_eq!(
            names_of_outputs(&interface),
            vec!["fastqc_1__html", "fastqc_1__zip", "multiqc_1__report"]
        );
    }

    #[test]
    fn input_from_an_earlier_job_names_that_job() {
        // b1 → a1 → b2: three jobs.
        let plan = plan(&["b1", "a1", "b2"], &[("b1", "a1"), ("a1", "b2")]);
        assert_eq!(job_ids(&plan), vec![vec!["b1"], vec!["a1"], vec!["b2"]]);

        let interface = plan.interface(JobId(2));

        // b2.in1 comes from a1 (job 1); b2.in2 is fed by nothing.
        assert_eq!(names_of_inputs(&interface), vec!["a1__out", "b2__in2"]);
        assert_eq!(
            interface.inputs[0].source,
            InputSource::Job {
                job: JobId(1),
                node: "a1".to_string(),
                port: "out".to_string(),
            }
        );
        assert!(matches!(
            interface.inputs[1].source,
            InputSource::Run { .. }
        ));
        assert_eq!(names_of_outputs(&interface), vec!["b2__out"]);
    }

    #[test]
    fn upstream_output_read_twice_is_one_input() {
        // a1 feeds both b2 and b3, which share a segment (b2 → b3).
        let plan = plan(
            &["a1", "b2", "b3"],
            &[("a1", "b2"), ("a1", "b3"), ("b2", "b3")],
        );
        assert_eq!(job_ids(&plan), vec![vec!["a1"], vec!["b2", "b3"]]);

        let interface = plan.interface(JobId(1));

        // a1__out once, not twice. b3's second input comes from b2 inside
        // the job, so only b2.in2 is left for the run to supply.
        assert_eq!(names_of_inputs(&interface), vec!["a1__out", "b2__in2"]);
        assert_eq!(names_of_outputs(&interface), vec!["b2__out", "b3__out"]);
    }

    #[test]
    fn gathered_input_keeps_the_producers_cardinality() {
        // fastqc (batch, per-sample zip) → an analysis node whose input is
        // per-dataset: a gather across a job boundary. The interface
        // describes data as it arrives, so the input is per-sample; the
        // .collect() happens inside the consuming job.
        let summary: NodeSpec = serde_json::from_value(json!({
            "id": "qc_summary",
            "version": "0.1.0",
            "execution": { "class": "analysis", "container": "example/qc-summary:latest" },
            "inputs": [{ "name": "reports", "type": { "format": "qc_report" }, "cardinality": "per_dataset" }],
            "outputs": [],
        }))
        .unwrap();
        let mut specs: Vec<NodeSpec> = load_library(&repo_library_dir())
            .unwrap()
            .into_iter()
            .map(|node| node.spec)
            .collect();
        specs.push(summary);
        let canvas = Canvas::from_json(
            &json!({
                "nodes": [
                    { "id": "qc", "spec": "fastqc", "version": "0.1.0" },
                    { "id": "summary", "spec": "qc_summary", "version": "0.1.0" },
                ],
                "edges": [{
                    "from": { "node": "qc", "port": "zip" },
                    "to": { "node": "summary", "port": "reports" },
                }],
            })
            .to_string(),
        )
        .unwrap();
        let pipeline = validate(&canvas, &SpecRegistry::new(specs)).unwrap();
        let plan = ExecutionPlan::build(pipeline);
        assert_eq!(job_ids(&plan), vec![vec!["qc"], vec!["summary"]]);

        let interface = plan.interface(JobId(1));

        assert_eq!(names_of_inputs(&interface), vec!["qc__zip"]);
        assert_eq!(interface.inputs[0].cardinality, Cardinality::PerSample);
    }
}
