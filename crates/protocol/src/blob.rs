//! Content-addressed blobs and trees (DESIGN.md §4.1, "project files by
//! digest"), modelled on Bazel's content-addressable store.
//!
//! - A **blob** is a file's bytes, named by their digest.
//! - A **tree** lists path → blob for a whole project. It is itself
//!   stored as a blob, so a project is identified by its tree's digest:
//!   change one byte of any file and the project digest changes too.
//!
//! The API builds trees from the compiler's output; the agent fetches the
//! tree and the blobs it lacks, and verifies every one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::digest::{Digest, HashAlgorithm, digest_bytes};
use crate::version::FormatVersion;

/// How to find a blob and check it: its digest and its size. The size lets
/// a reader catch a truncated download before hashing, and refuse
/// unexpectedly large blobs before downloading them (as Bazel's `Digest`
/// carries `size_bytes`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BlobRef {
    pub digest: Digest,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    #[error("blob {expected} should be {expected_size} bytes, got {actual_size}")]
    WrongSize {
        expected: Digest,
        expected_size: u64,
        actual_size: u64,
    },
    #[error("blob {expected} has the wrong content: it hashes to {actual}")]
    WrongDigest { expected: Digest, actual: Digest },
    #[error("tree path `{path}` is not allowed: {reason}")]
    BadPath { path: String, reason: String },
    #[error("not a valid tree: {0}")]
    BadTree(String),
}

impl BlobRef {
    /// The reference for some bytes.
    pub fn of(algorithm: HashAlgorithm, bytes: &[u8]) -> BlobRef {
        BlobRef {
            digest: digest_bytes(algorithm, bytes),
            size: bytes.len() as u64,
        }
    }

    /// Check that `bytes` are exactly this blob: the size first (cheap),
    /// then the digest. Content addressing makes this check free: the
    /// name *is* the hash.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), BlobError> {
        let actual_size = bytes.len() as u64;
        if actual_size != self.size {
            return Err(BlobError::WrongSize {
                expected: self.digest.clone(),
                expected_size: self.size,
                actual_size,
            });
        }
        let actual = digest_bytes(self.digest.algorithm(), bytes);
        if actual != self.digest {
            return Err(BlobError::WrongDigest {
                expected: self.digest.clone(),
                actual,
            });
        }
        Ok(())
    }
}

/// A blob's bytes together with its reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub reference: BlobRef,
    pub bytes: Vec<u8>,
}

impl Blob {
    pub fn new(algorithm: HashAlgorithm, bytes: Vec<u8>) -> Blob {
        Blob {
            reference: BlobRef::of(algorithm, &bytes),
            bytes,
        }
    }
}

/// A project's files: relative path → blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    #[serde(default)]
    pub format_version: FormatVersion,
    /// Sorted (a BTreeMap), so the same files always give the same bytes,
    /// and so the same digest.
    pub files: BTreeMap<String, BlobRef>,
}

impl Tree {
    /// Build a tree from files, returning it with the file blobs to store.
    /// Identical files become one blob.
    pub fn from_files(
        algorithm: HashAlgorithm,
        files: BTreeMap<String, Vec<u8>>,
    ) -> Result<(Tree, Vec<Blob>), BlobError> {
        let mut listing = BTreeMap::new();
        let mut blobs: BTreeMap<Digest, Blob> = BTreeMap::new();
        for (path, bytes) in files {
            check_path(&path)?;
            let blob = Blob::new(algorithm, bytes);
            listing.insert(path, blob.reference.clone());
            blobs.entry(blob.reference.digest.clone()).or_insert(blob);
        }
        let tree = Tree {
            format_version: FormatVersion::current(),
            files: listing,
        };
        Ok((tree, blobs.into_values().collect()))
    }

    /// The tree as a blob of its own; its digest is the project's digest.
    pub fn to_blob(&self, algorithm: HashAlgorithm) -> Blob {
        let bytes = serde_json::to_vec(self).expect("a tree always serializes");
        Blob::new(algorithm, bytes)
    }

    /// Read a tree from a blob's bytes, checking every path. The caller
    /// verifies the bytes against their `BlobRef` first.
    pub fn from_blob_bytes(bytes: &[u8]) -> Result<Tree, BlobError> {
        let tree: Tree =
            serde_json::from_slice(bytes).map_err(|error| BlobError::BadTree(error.to_string()))?;
        for path in tree.files.keys() {
            check_path(path)?;
        }
        Ok(tree)
    }
}

/// Tree paths are written to disk by the agent, so they must stay inside
/// the directory they're written to: relative, `/`-separated, with no
/// empty, `.` or `..` parts. A tree from a compromised or buggy server
/// can't make the agent write `../../.bashrc`.
pub fn check_path(path: &str) -> Result<(), BlobError> {
    let bad = |reason: &str| BlobError::BadPath {
        path: path.to_string(),
        reason: reason.to_string(),
    };
    if path.is_empty() {
        return Err(bad("it is empty"));
    }
    if path.starts_with('/') {
        return Err(bad("it is absolute"));
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(bad("it contains a backslash or NUL"));
    }
    for part in path.split('/') {
        match part {
            "" => return Err(bad("it has an empty part (`//` or a trailing `/`)")),
            "." | ".." => return Err(bad("it has a `.` or `..` part")),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: HashAlgorithm = HashAlgorithm::Sha256;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(path, text)| (path.to_string(), text.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn verify_accepts_the_right_bytes() {
        let blob = BlobRef::of(SHA, b"process FASTQC {}");
        assert_eq!(blob.verify(b"process FASTQC {}"), Ok(()));
    }

    #[test]
    fn verify_catches_truncation_and_tampering() {
        let blob = BlobRef::of(SHA, b"process FASTQC {}");
        assert!(matches!(
            blob.verify(b"process FAS"),
            Err(BlobError::WrongSize { .. })
        ));
        assert!(matches!(
            blob.verify(b"process EVILQC {}"),
            Err(BlobError::WrongDigest { .. })
        ));
    }

    #[test]
    fn identical_files_become_one_blob() {
        let (tree, blobs) = Tree::from_files(
            SHA,
            files(&[
                ("a/main.nf", "same"),
                ("b/main.nf", "same"),
                ("main.nf", "other"),
            ]),
        )
        .unwrap();
        assert_eq!(tree.files.len(), 3);
        assert_eq!(blobs.len(), 2);
        assert_eq!(tree.files["a/main.nf"], tree.files["b/main.nf"]);
    }

    #[test]
    fn the_same_files_always_give_the_same_project_digest() {
        let one = files(&[("main.nf", "x"), ("nextflow.config", "y")]);
        let (tree_a, _) = Tree::from_files(SHA, one.clone()).unwrap();
        let (tree_b, _) = Tree::from_files(SHA, one).unwrap();
        assert_eq!(tree_a.to_blob(SHA).reference, tree_b.to_blob(SHA).reference);

        // One changed byte in one file changes the project digest.
        let (tree_c, _) =
            Tree::from_files(SHA, files(&[("main.nf", "X"), ("nextflow.config", "y")])).unwrap();
        assert_ne!(tree_a.to_blob(SHA).reference, tree_c.to_blob(SHA).reference);
    }

    #[test]
    fn a_tree_round_trips_through_its_blob() {
        let (tree, _) = Tree::from_files(SHA, files(&[("modules/fastqc/main.nf", "p")])).unwrap();
        let blob = tree.to_blob(SHA);
        blob.reference.verify(&blob.bytes).unwrap();
        assert_eq!(Tree::from_blob_bytes(&blob.bytes).unwrap(), tree);
    }

    #[test]
    fn rejects_paths_that_could_escape() {
        for path in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "a/./b",
            "a/",
            "a\\b",
        ] {
            assert!(check_path(path).is_err(), "{path:?} should be rejected");
        }
        assert!(check_path("modules/fastqc/0.1.0/main.nf").is_ok());
    }

    #[test]
    fn a_tree_blob_with_a_bad_path_is_rejected_on_read() {
        // A tree the agent receives is checked even though from_files
        // would never have built it.
        let blob = BlobRef::of(SHA, b"x");
        let json = serde_json::json!({ "format_version": 1, "files": { "../evil": blob } });
        let err = Tree::from_blob_bytes(json.to_string().as_bytes()).unwrap_err();
        assert!(matches!(err, BlobError::BadPath { .. }), "{err}");
    }
}
