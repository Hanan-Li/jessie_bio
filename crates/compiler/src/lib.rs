//! Canvas → PipelineGraph (the IR) → ExecutionPlan (jobs) → Nextflow projects.
//!
//! ```text
//! library::load_library   nodes/ on disk → SpecRegistry
//! Canvas::from_json        browser JSON → Canvas
//! validate::validate       Canvas + SpecRegistry → PipelineGraph
//! ExecutionPlan::build     PipelineGraph → jobs (segments, analysis nodes)
//! ExecutionPlan::interface each job's inputs and outputs (job.json)
//! ExecutionPlan::nextflow_project  one segment → main.nf + config + modules
//! ```

pub mod canvas;
pub mod codegen;
pub mod execution_plan;
pub mod job_interface;
pub mod library;
pub mod pipeline_graph;
pub mod spec;
pub mod types;
pub mod validate;
