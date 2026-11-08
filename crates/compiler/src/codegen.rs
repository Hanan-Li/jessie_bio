//! Code generation: one segment job → a runnable Nextflow project
//! (DESIGN.md §3.3).
//!
//! The project is returned as data, a map of relative path → file text,
//! so the compiler stays pure. Callers write it out (the CLI to a
//! directory; later the control plane, stored with the run).
//!
//! Every decision is made here in Rust: process aliases, which channel
//! feeds each process input, where `.collect()` goes, the order of calls.
//! The results are handed to `templates/main.nf.j2` as plain "view"
//! structs, and the template only lays out the file.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use minijinja::syntax::SyntaxConfig;
use minijinja::value::Serde;
use minijinja::{Environment, context};
use petgraph::Direction::Incoming;
use petgraph::stable_graph::NodeIndex;
use petgraph::visit::EdgeRef;
use protocol::job::data_name;
use protocol::manifest::{PATH_COLUMN, SAMPLE_COLUMN};
use protocol::types::Cardinality;
use serde::Serialize;

use crate::execution_plan::{ExecutionPlan, JobId, JobKind};
use crate::pipeline_graph::Wiring;
use crate::spec::{Execution, SpecRegistry};

const MAIN_TEMPLATE: &str = include_str!("../templates/main.nf.j2");
const NEXTFLOW_CONFIG: &str = include_str!("../templates/nextflow.config");

/// A generated Nextflow project: relative path → file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextflowProject {
    pub files: BTreeMap<String, String>,
}

// ---- Views: what the template sees ----

#[derive(Serialize)]
struct IncludeView {
    process: String,
    alias: String,
    path: String,
}

/// A channel built from an input manifest: `name = lines[0]`, then each
/// further line is a chained operator.
#[derive(Serialize)]
struct InputView {
    name: String,
    lines: Vec<String>,
}

#[derive(Serialize)]
struct CallView {
    alias: String,
    args: Vec<ArgView>,
}

/// One argument of a process call: a channel expression, one operator per
/// line. Every argument but the last ends with a comma.
#[derive(Serialize)]
struct ArgView {
    lines: Vec<String>,
}

#[derive(Serialize)]
struct OutputView {
    name: String,
    alias: String,
    port: String,
    per_sample: bool,
}

impl ExecutionPlan {
    /// Generate the Nextflow project for one segment job.
    pub fn nextflow_project(
        &self,
        job: JobId,
        registry: &SpecRegistry,
    ) -> anyhow::Result<NextflowProject> {
        let JobKind::Segment { nodes } = &self.jobs()[job.0].kind else {
            bail!(
                "job {} is an analysis job; only segments compile to Nextflow",
                job.0
            );
        };
        let graph = self.pipeline().graph();
        let interface = self.interface(job);
        let mut files = BTreeMap::new();

        // 1. One include per node, aliased by node id so the same module can
        //    appear twice. The module's source is copied into the project.
        let mut alias_of: BTreeMap<NodeIndex, String> = BTreeMap::new();
        let mut includes = Vec::new();
        for &node in nodes {
            let graph_node = &graph[node];
            let spec = &graph_node.spec;
            let Execution::Batch { module } = &spec.execution else {
                unreachable!("segments contain only batch nodes");
            };
            let alias = format!("{}_{}", module.process, graph_node.id);
            let path = format!("modules/{}/{}/{}", spec.id, spec.version, module.file);
            let source = registry
                .module_source(&spec.id, &spec.version)
                .with_context(|| format!("no module source for {} {}", spec.id, spec.version))?;
            files.insert(path.clone(), source.to_string());
            includes.push(IncludeView {
                process: module.process.clone(),
                alias: alias.clone(),
                path,
            });
            alias_of.insert(node, alias);
        }

        // 2. One channel per job input, read from its manifest.
        let inputs: Vec<InputView> = interface
            .inputs
            .iter()
            .map(|input| InputView {
                name: input.name.clone(),
                lines: manifest_channel(&input.name, input.cardinality),
            })
            .collect();

        // 3. One call per node, in topological order. Each process input
        //    is fed by a job input channel, or by another node's output in
        //    this job, adapted by the edge's wiring.
        let mut calls = Vec::new();
        for &node in nodes {
            let graph_node = &graph[node];
            let mut args = Vec::new();
            for port in &graph_node.spec.inputs {
                let feeding_edge = graph
                    .edges_directed(node, Incoming)
                    .find(|edge| edge.weight().to_port == port.name);
                let lines = match feeding_edge {
                    // Fed by nothing: a run input, already a channel.
                    None => vec![data_name(&graph_node.id, &port.name)],
                    Some(edge) => {
                        let from_port = &edge.weight().from_port;
                        let source = match alias_of.get(&edge.source()) {
                            // Same job: the upstream process's output.
                            Some(alias) => format!("{alias}.out.{from_port}"),
                            // Earlier job: the channel built from its manifest.
                            None => data_name(&graph[edge.source()].id, from_port),
                        };
                        wire(source, edge.weight().wiring, &graph_node.id)
                    }
                };
                args.push(ArgView { lines });
            }
            let last = args.len().saturating_sub(1);
            for arg in &mut args[..last] {
                arg.lines
                    .last_mut()
                    .expect("an argument has at least one line")
                    .push(',');
            }
            calls.push(CallView {
                alias: alias_of[&node].clone(),
                args,
            });
        }

        // 4. Publish every output, per sample in its own directory.
        let alias_by_id: BTreeMap<&str, &str> = alias_of
            .iter()
            .map(|(&node, alias)| (graph[node].id.as_str(), alias.as_str()))
            .collect();
        let outputs: Vec<OutputView> = interface
            .outputs
            .iter()
            .map(|output| OutputView {
                name: output.name.clone(),
                alias: alias_by_id[output.node.as_str()].to_string(),
                port: output.port.clone(),
                per_sample: output.cardinality == Cardinality::PerSample,
            })
            .collect();

        let mut env = Environment::new();
        // Drop the newline after a {% tag %} and the indentation before it,
        // so the template can be laid out readably without leaving blank
        // lines and stray spaces in the output. Keep the file's final
        // newline, which minijinja strips by default.
        env.set_syntax(
            SyntaxConfig::builder()
                .trim_blocks(true)
                .lstrip_blocks(true)
                .keep_trailing_newline(true)
                .build()?,
        );
        env.add_template("main.nf", MAIN_TEMPLATE)?;
        // `Serde(...)` hands a value to the template through its serde
        // `Serialize` impl.
        let main_nf = env.get_template("main.nf")?.render(context! {
            includes => Serde(includes),
            inputs => Serde(inputs),
            calls => Serde(calls),
            outputs => Serde(outputs),
        })?;

        files.insert("main.nf".to_string(), main_nf);
        files.insert("nextflow.config".to_string(), NEXTFLOW_CONFIG.to_string());
        let mut job_json = serde_json::to_string_pretty(&interface)?;
        job_json.push('\n');
        files.insert("job.json".to_string(), job_json);

        Ok(NextflowProject { files })
    }
}

/// Channel expression that reads a manifest back into Nextflow's usual
/// shape: `[meta, files]` per sample, or one `[meta, files]` for a dataset.
fn manifest_channel(name: &str, cardinality: Cardinality) -> Vec<String> {
    let head = vec![
        format!("channel.fromPath(params.{name})"),
        ".splitCsv(header: true)".to_string(),
    ];
    let rest = match cardinality {
        Cardinality::PerSample => vec![
            format!(".map {{ row -> tuple(row.{SAMPLE_COLUMN}, file(row.{PATH_COLUMN})) }}"),
            // Rows come one per file; regroup each sample's files, in order.
            ".groupTuple()".to_string(),
            ".map { sample, files -> tuple([id: sample], files) }".to_string(),
        ],
        Cardinality::PerDataset => vec![
            format!(".map {{ row -> file(row.{PATH_COLUMN}) }}"),
            ".collect()".to_string(),
            format!(".map {{ files -> tuple([id: '{name}'], files) }}"),
        ],
    };
    head.into_iter().chain(rest).collect()
}

/// Adapt a channel to the consuming port, per the edge's wiring.
fn wire(source: String, wiring: Wiring, consumer: &str) -> Vec<String> {
    match wiring {
        Wiring::Direct => vec![source],
        // Drop each sample's meta, collect every file into one list, and
        // give the list a dataset-level meta (nf-core modules expect one).
        Wiring::Gather => vec![
            source,
            ".map { _meta, files -> files }".to_string(),
            ".collect()".to_string(),
            format!(".map {{ files -> tuple([id: '{consumer}'], files) }}"),
        ],
        // A value channel: every sample's task reads the same item.
        Wiring::Broadcast => vec![source, ".first()".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::canvas::Canvas;
    use crate::execution_plan::testing::{generic_registry, job_ids, plan};
    use crate::spec::test_registry;
    use crate::validate::validate;

    fn expected_dir(case: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/expected")
            .join(case)
    }

    /// Compare the generated files against `testdata/expected/<case>/`.
    /// Module files are copies from the node library, so they aren't kept
    /// in the expected directory.
    ///
    /// After an intended change, regenerate the expected files with
    /// `UPDATE_EXPECTED=1 cargo test`, then review the diff in git.
    fn assert_matches_expected(project: &NextflowProject, case: &str) {
        let dir = expected_dir(case);
        let generated: BTreeMap<&str, &str> = project
            .files
            .iter()
            .filter(|(path, _)| !path.starts_with("modules/"))
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();

        if std::env::var_os("UPDATE_EXPECTED").is_some() {
            let _ = fs::remove_dir_all(&dir);
            for (path, text) in &generated {
                let file = dir.join(path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, text).unwrap();
            }
            return;
        }

        let mut on_disk: Vec<String> = fs::read_dir(&dir)
            .unwrap_or_else(|_| panic!("no {}; run with UPDATE_EXPECTED=1", dir.display()))
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        on_disk.sort();
        let names: Vec<&str> = generated.keys().copied().collect();
        assert_eq!(on_disk, names, "files in {}", dir.display());

        for (path, text) in generated {
            let expected = fs::read_to_string(dir.join(path)).unwrap();
            assert!(
                text == expected,
                "{path} differs from {}. If the change is intended, run \
                 `UPDATE_EXPECTED=1 cargo test` and review the diff.\n\n\
                 --- generated ---\n{text}",
                dir.join(path).display()
            );
        }
    }

    #[test]
    fn fastqc_multiqc_project() {
        let canvas =
            Canvas::from_json(include_str!("../testdata/fastqc_multiqc.canvas.json")).unwrap();
        let registry = test_registry();
        let plan = ExecutionPlan::build(validate(&canvas, &registry).unwrap());

        let project = plan.nextflow_project(JobId(0), &registry).unwrap();

        assert_matches_expected(&project, "fastqc_multiqc");
        // Modules are copied verbatim from the library.
        assert_eq!(
            project.files["modules/fastqc/0.1.0/main.nf"],
            registry.module_source("fastqc", "0.1.0").unwrap()
        );
        assert!(project.files.contains_key("modules/multiqc/0.1.0/main.nf"));
    }

    #[test]
    fn segment_reading_an_earlier_jobs_output() {
        // b1 → a1 → b2: job 2 reads a1's output from its manifest, and has
        // one unconnected input the run supplies.
        let plan = plan(&["b1", "a1", "b2"], &[("b1", "a1"), ("a1", "b2")]);
        assert_eq!(job_ids(&plan), vec![vec!["b1"], vec!["a1"], vec!["b2"]]);

        let project = plan
            .nextflow_project(JobId(2), &generic_registry())
            .unwrap();

        assert_matches_expected(&project, "from_earlier_job");
    }

    #[test]
    fn analysis_jobs_have_no_nextflow_project() {
        let plan = plan(&["b1", "a1"], &[("b1", "a1")]);
        let err = plan
            .nextflow_project(JobId(1), &generic_registry())
            .unwrap_err();
        assert!(err.to_string().contains("is an analysis job"), "{err}");
    }

    #[test]
    fn missing_module_source_is_an_error() {
        let plan = plan(&["b1"], &[]);
        // A registry without module sources can validate, but not generate.
        let err = plan
            .nextflow_project(JobId(0), &SpecRegistry::default())
            .unwrap_err();
        assert!(
            err.to_string().contains("no module source for batch"),
            "{err}"
        );
    }

    #[test]
    fn wiring_shapes() {
        assert_eq!(wire("ch".to_string(), Wiring::Direct, "n"), vec!["ch"]);
        assert_eq!(
            wire("ch".to_string(), Wiring::Gather, "n"),
            vec![
                "ch",
                ".map { _meta, files -> files }",
                ".collect()",
                ".map { files -> tuple([id: 'n'], files) }",
            ]
        );
        assert_eq!(
            wire("ch".to_string(), Wiring::Broadcast, "n"),
            vec!["ch", ".first()"]
        );
    }
}
