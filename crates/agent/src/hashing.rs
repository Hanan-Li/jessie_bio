//! Hashing files (DESIGN.md §4.1, local mode step 5).
//!
//! Files are read in 1 MiB chunks, so a 50 GB BAM never sits in memory.
//! Hashing is CPU work, not waiting, so it runs on tokio's blocking thread
//! pool (`spawn_blocking`) rather than on the async threads, which must stay
//! free to service streams and timers.
//!
//! Adding an algorithm: a variant in `protocol::digest::HashAlgorithm`, and
//! a variant plus match arms in `Hasher` below.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;
use protocol::digest::{Digest, HashAlgorithm};
use sha2::Digest as _;

const CHUNK: usize = 1 << 20; // 1 MiB

/// Hash a file and measure its size, off the async threads.
pub async fn hash_file(path: PathBuf, algorithm: HashAlgorithm) -> anyhow::Result<(Digest, u64)> {
    // `move` hands `path` to the closure, which runs on another thread.
    // The `?` handles "the blocking task panicked"; the inner Result is
    // the hashing's own.
    tokio::task::spawn_blocking(move || hash_file_blocking(&path, algorithm))
        .await
        .context("hashing task panicked")?
}

fn hash_file_blocking(path: &Path, algorithm: HashAlgorithm) -> anyhow::Result<(Digest, u64)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Hasher::new(algorithm);
    let mut buffer = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((hasher.finish()?, size))
}

/// One running hash, whatever the algorithm.
enum Hasher {
    Sha256(sha2::Sha256),
}

impl Hasher {
    fn new(algorithm: HashAlgorithm) -> Hasher {
        match algorithm {
            HashAlgorithm::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(hasher) => hasher.update(bytes),
        }
    }

    /// `self` by value: finishing consumes the hasher, so it can't be
    /// updated again afterwards. The compiler enforces that.
    fn finish(self) -> anyhow::Result<Digest> {
        let (algorithm, bytes) = match self {
            Hasher::Sha256(hasher) => (HashAlgorithm::Sha256, hasher.finalize().to_vec()),
        };
        Digest::from_bytes(algorithm, &bytes).map_err(anyhow::Error::msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn hash_bytes(bytes: &[u8]) -> (Digest, u64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, bytes).unwrap();
        hash_file(path, HashAlgorithm::Sha256).await.unwrap()
    }

    #[tokio::test]
    async fn matches_the_published_sha256_test_vectors() {
        // FIPS 180-2 test vectors.
        let (digest, size) = hash_bytes(b"abc").await;
        assert_eq!(
            digest.to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(size, 3);

        let (digest, size) = hash_bytes(b"").await;
        assert_eq!(
            digest.hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(size, 0);
    }

    #[tokio::test]
    async fn chunking_gives_the_same_hash_as_hashing_in_one_go() {
        // 2.5 chunks, with varied bytes so a misplaced chunk would show.
        let bytes: Vec<u8> = (0..(CHUNK * 5 / 2)).map(|i| (i % 251) as u8).collect();
        let (digest, size) = hash_bytes(&bytes).await;

        let expected = sha2::Sha256::digest(&bytes);
        assert_eq!(
            digest,
            Digest::from_bytes(HashAlgorithm::Sha256, &expected).unwrap()
        );
        assert_eq!(size, bytes.len() as u64);
    }

    #[tokio::test]
    async fn a_missing_file_is_an_error_naming_it() {
        let err = hash_file(PathBuf::from("/no/such/file"), HashAlgorithm::Sha256)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("/no/such/file"));
    }
}
