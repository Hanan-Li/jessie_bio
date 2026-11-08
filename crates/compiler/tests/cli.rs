//! End-to-end tests of the `compiler` binary.
//!
//! Files in `tests/` are *integration tests*: each is compiled as its own
//! crate that can only use the public API, the way an outside user would.
//! Cargo builds the binary first and tells us where it is.

use std::path::Path;
use std::process::Command;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

fn compiler() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_compiler"));
    command.current_dir(repo_root());
    command
}

#[test]
fn compiles_the_test_canvas() {
    let out = tempfile::tempdir().unwrap();
    let result = compiler()
        .arg("crates/compiler/testdata/fastqc_multiqc.canvas.json")
        .arg("--out")
        .arg(out.path())
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains("job 0: Nextflow segment [fastqc_1, multiqc_1]"),
        "{stdout}"
    );
    for file in [
        "main.nf",
        "nextflow.config",
        "job.json",
        "modules/fastqc/0.1.0/main.nf",
        "modules/multiqc/0.1.0/main.nf",
    ] {
        assert!(
            out.path().join("job-0").join(file).is_file(),
            "missing {file}"
        );
    }
}

#[test]
fn invalid_canvas_lists_every_error_and_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let canvas = dir.path().join("bad.canvas.json");
    std::fs::write(
        &canvas,
        r#"{"nodes": [{"id": "qc 1", "spec": "fastqc", "version": "0.1.0"},
                      {"id": "x", "spec": "nope", "version": "0.1.0"}],
            "edges": []}"#,
    )
    .unwrap();

    let result = compiler()
        .arg(&canvas)
        .arg("--out")
        .arg(dir.path().join("out"))
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("node id `qc 1`"), "{stderr}");
    assert!(stderr.contains("`nope` version 0.1.0"), "{stderr}");
    assert!(
        !dir.path().join("out").exists(),
        "nothing is written for an invalid canvas"
    );
}

#[test]
fn refuses_a_non_empty_output_directory() {
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("old.txt"), "keep me").unwrap();

    let result = compiler()
        .arg("crates/compiler/testdata/fastqc_multiqc.canvas.json")
        .arg("--out")
        .arg(out.path())
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("is not empty"));
}
