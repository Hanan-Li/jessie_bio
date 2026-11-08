//! Canvas → PipelineGraph (the IR) → segments → Nextflow DSL2.
//!
//! P0a skeleton: the real pipeline stages arrive as we build them.

pub mod canvas;
pub mod execution_plan;
pub mod job_interface;
pub mod library;
pub mod pipeline_graph;
pub mod spec;
pub mod types;
pub mod validate;

/// Compile a canvas into Nextflow DSL2 source text.
///
/// Placeholder: ignores the canvas and emits an empty workflow.
pub fn compile(_canvas_json: &str) -> String {
    String::from("workflow {\n}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_canvas_compiles_to_empty_workflow() {
        assert_eq!(compile("{}"), "workflow {\n}\n");
    }
}
