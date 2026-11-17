//! Loading the node library from disk: `nodes/<id>/<version>/` (DESIGN.md §3.4).
//!
//! This is the only file I/O in the compiler crate. It runs once, up front;
//! validation, segmentation and codegen take what it loaded and stay pure.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail, ensure};

use crate::spec::{Execution, NodeSpec};
use crate::types::SemanticType;

/// One node version from the library: its spec, the directory holding its
/// files, and (for batch nodes) the Nextflow module's source text, which
/// codegen copies into each generated project.
#[derive(Debug, Clone)]
pub struct LibraryNode {
    pub spec: NodeSpec,
    pub dir: PathBuf,
    pub module_source: Option<String>,
}

/// Load every node version under `root`, sorted by id then version.
///
/// Each one is checked on the way in: the spec parses, its id and version
/// match its directory, and a batch node's module file exists and declares
/// the process the spec names.
pub fn load_library(root: &Path) -> anyhow::Result<Vec<LibraryNode>> {
    let mut nodes = Vec::new();
    for id_dir in subdirectories(root)? {
        for version_dir in subdirectories(&id_dir)? {
            let node = load_node(&version_dir)
                .with_context(|| format!("loading node {}", version_dir.display()))?;
            nodes.push(node);
        }
    }
    Ok(nodes)
}

fn load_node(dir: &Path) -> anyhow::Result<LibraryNode> {
    let spec_path = dir.join("spec.json");
    let text = fs::read_to_string(&spec_path)
        .with_context(|| format!("reading {}", spec_path.display()))?;
    let spec: NodeSpec = serde_json::from_str(&text).context("spec.json is malformed")?;

    let version = name_of(dir)?;
    let id = name_of(dir.parent().context("node directory has no parent")?)?;
    ensure!(
        spec.id == id && spec.version == version,
        "spec says {} {}, but it lives in {id}/{version}/",
        spec.id,
        spec.version
    );
    // Readers of other programs' documents map formats they don't know to
    // `Unknown`; in a curated spec that can only be a typo.
    for port in spec.inputs.iter().chain(&spec.outputs) {
        ensure!(
            port.ty != SemanticType::Unknown,
            "port `{}` has a format this compiler doesn't know",
            port.name
        );
    }

    let mut module_source = None;
    if let Execution::Batch { module } = &spec.execution {
        // Only plain relative paths: a module must live inside its node's
        // directory, never `../elsewhere` or `/etc/...`.
        let relative = Path::new(&module.file);
        let plain = relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        ensure!(
            plain,
            "module file `{}` must be inside the node directory",
            module.file
        );

        let module_path = dir.join(relative);
        let source = fs::read_to_string(&module_path)
            .with_context(|| format!("reading {}", module_path.display()))?;
        let declaration = format!("process {} {{", module.process);
        ensure!(
            source.contains(&declaration),
            "{} does not declare `{declaration}`",
            module.file
        );
        module_source = Some(source);
    }

    Ok(LibraryNode {
        spec,
        dir: dir.to_path_buf(),
        module_source,
    })
}

/// Subdirectories of `dir`, sorted by name so loading order is deterministic.
/// Plain files (like `LICENSE-nf-core-modules`) are skipped.
fn subdirectories(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// The last component of a path as UTF-8 text.
fn name_of(path: &Path) -> anyhow::Result<&str> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => Ok(name),
        None => bail!("{} has no usable name", path.display()),
    }
}

/// The repo's `nodes/` directory, for tests.
#[cfg(test)]
pub(crate) fn repo_library_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/compiler; the library is at the repo root.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nodes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_repo_library() {
        let nodes = load_library(&repo_library_dir()).unwrap();
        let ids: Vec<_> = nodes
            .iter()
            .map(|node| format!("{} {}", node.spec.id, node.spec.version))
            .collect();
        assert_eq!(ids, vec!["fastqc 0.1.0", "multiqc 0.1.0"]);
    }

    /// A one-node library in a temporary directory, deleted when dropped.
    fn library_with(id: &str, version: &str, spec: &str, module: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(id).join(version);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("spec.json"), spec).unwrap();
        fs::write(dir.join("main.nf"), module).unwrap();
        root
    }

    fn batch_spec(id: &str, version: &str, process: &str, file: &str) -> String {
        serde_json::json!({
            "id": id,
            "version": version,
            "execution": { "class": "batch", "module": { "process": process, "file": file } },
            "inputs": [],
            "outputs": [],
        })
        .to_string()
    }

    fn error_of(root: &tempfile::TempDir) -> String {
        format!("{:#}", load_library(root.path()).unwrap_err())
    }

    #[test]
    fn accepts_a_well_formed_node() {
        let spec = batch_spec("tool", "1.0.0", "TOOL", "main.nf");
        let root = library_with("tool", "1.0.0", &spec, "process TOOL {\n}\n");
        assert_eq!(load_library(root.path()).unwrap().len(), 1);
    }

    #[test]
    fn rejects_spec_in_the_wrong_directory() {
        let spec = batch_spec("tool", "2.0.0", "TOOL", "main.nf");
        let root = library_with("tool", "1.0.0", &spec, "process TOOL {\n}\n");
        assert!(error_of(&root).contains("spec says tool 2.0.0"));
    }

    #[test]
    fn rejects_module_missing_the_process() {
        let spec = batch_spec("tool", "1.0.0", "TOOL", "main.nf");
        let root = library_with("tool", "1.0.0", &spec, "process OTHER {\n}\n");
        assert!(error_of(&root).contains("does not declare `process TOOL {`"));
    }

    #[test]
    fn rejects_a_port_with_an_unknown_format() {
        let spec = serde_json::json!({
            "id": "tool",
            "version": "1.0.0",
            "execution": { "class": "batch", "module": { "process": "TOOL", "file": "main.nf" } },
            "inputs": [{ "name": "reads", "type": { "format": "fastqq" }, "cardinality": "per_sample" }],
            "outputs": [],
        })
        .to_string();
        let root = library_with("tool", "1.0.0", &spec, "process TOOL {\n}\n");
        assert!(error_of(&root).contains("port `reads` has a format this compiler doesn't know"));
    }

    #[test]
    fn rejects_module_outside_the_node_directory() {
        let spec = batch_spec("tool", "1.0.0", "TOOL", "../../secrets.nf");
        let root = library_with("tool", "1.0.0", &spec, "process TOOL {\n}\n");
        assert!(error_of(&root).contains("must be inside the node directory"));
    }
}
