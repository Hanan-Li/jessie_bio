//! The agent's blob cache (DESIGN.md §4.1, "project files by digest").
//!
//! A project's files arrive as blobs named by their digest. The cache keeps
//! every blob it has verified, so a module shared by many projects (FastQC
//! 0.1.0) is downloaded once per machine, not once per job:
//!
//! ```text
//! ~/.cache/jessie-agent/blobs/
//!   sha256/ab/cdef…    one file per blob: <algorithm>/<2 hex>/<rest of hex>
//!   tmp/               downloads in progress
//! ```
//!
//! Only verified bytes enter the cache. A download is written to `tmp/`
//! and then renamed into place: a rename within one filesystem is atomic,
//! so another agent sharing the cache sees either no file or the whole
//! file, never half of one.
//!
//! The two-character directory level keeps any one directory small, as
//! git's `objects/` does.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use protocol::blob::BlobRef;
use tokio::fs;

/// The largest blob the agent accepts. Project files are a few KB of
/// text; a reference that claims more is refused before downloading.
pub const MAX_BLOB_SIZE: u64 = 16 * 1024 * 1024;

/// Somewhere blobs can be fetched from: the control plane over HTTPS in
/// `agent serve`, a map in memory in tests.
///
/// A *trait* is a set of methods a type promises to have, like an
/// interface in Java or Go. Code written against `impl BlobSource` works
/// with any type that implements it, and the compiler generates a separate
/// copy of that code for each type it is used with (no runtime cost).
///
/// `async fn` in traits is allowed since Rust 1.75. The catch: callers
/// can't tell whether the returned future may move between threads
/// (`Send`). That's fine here because a job is prepared on the task that
/// leased it, not handed to `tokio::spawn`.
pub trait BlobSource {
    /// The blob's bytes, unverified: the cache checks them.
    async fn fetch(&self, blob: &BlobRef) -> anyhow::Result<Vec<u8>>;
}

pub struct BlobCache {
    root: PathBuf,
}

/// Makes temporary file names unique within this process; the process id
/// makes them unique between agents sharing one cache.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

impl BlobCache {
    pub fn new(root: PathBuf) -> BlobCache {
        BlobCache { root }
    }

    /// Where a blob lives in the cache. The digest's hex is checked when
    /// the digest is parsed, so it can't contain `/` or `..`.
    pub fn path_of(&self, blob: &BlobRef) -> PathBuf {
        let hex = blob.digest.hex();
        self.root
            .join(blob.digest.algorithm().name())
            .join(&hex[..2])
            .join(&hex[2..])
    }

    /// The blob's bytes: from the cache if present, otherwise from
    /// `source`, verified, then cached.
    pub async fn get(&self, blob: &BlobRef, source: &impl BlobSource) -> anyhow::Result<Vec<u8>> {
        let path = self.path_of(blob);
        match fs::read(&path).await {
            Ok(bytes) => {
                // Verified when it was cached, but checking again is cheap
                // for files this small and catches a corrupted disk or a
                // hand-edited cache. A bad entry is replaced, not trusted.
                if blob.verify(&bytes).is_ok() {
                    return Ok(bytes);
                }
                eprintln!(
                    "warning: cached blob {} is corrupt; fetching it again",
                    blob.digest
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        }

        if blob.size > MAX_BLOB_SIZE {
            bail!(
                "blob {} claims to be {} bytes, more than the {MAX_BLOB_SIZE} an agent accepts",
                blob.digest,
                blob.size
            );
        }
        let bytes = source
            .fetch(blob)
            .await
            .with_context(|| format!("fetching blob {}", blob.digest))?;
        blob.verify(&bytes)?;
        self.store(&path, &bytes).await?;
        Ok(bytes)
    }

    /// Write verified bytes into place: a temporary file first, then a
    /// rename, so the cache never holds a partial blob.
    async fn store(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        let tmp_dir = self.root.join("tmp");
        fs::create_dir_all(&tmp_dir)
            .await
            .with_context(|| format!("creating {}", tmp_dir.display()))?;
        let parent = path.parent().expect("a cache path has a parent");
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;

        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = tmp_dir.join(format!("{}-{n}", std::process::id()));
        fs::write(&tmp, bytes)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        // If two agents fetch the same blob at once, both renames succeed
        // and the second replaces the first with identical bytes.
        fs::rename(&tmp, path)
            .await
            .with_context(|| format!("moving a blob into {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
pub mod testing {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use protocol::digest::Digest;

    use super::*;

    /// Blobs in memory, counting fetches so tests can see the cache work.
    /// `Mutex` because `fetch` takes `&self`: the counter needs *interior
    /// mutability*, changing behind a shared reference.
    #[derive(Default)]
    pub struct MemorySource {
        pub blobs: BTreeMap<Digest, Vec<u8>>,
        pub fetches: Mutex<Vec<Digest>>,
    }

    impl MemorySource {
        pub fn add(&mut self, blob: &BlobRef, bytes: Vec<u8>) {
            self.blobs.insert(blob.digest.clone(), bytes);
        }

        pub fn fetch_count(&self) -> usize {
            self.fetches.lock().unwrap().len()
        }
    }

    impl BlobSource for MemorySource {
        async fn fetch(&self, blob: &BlobRef) -> anyhow::Result<Vec<u8>> {
            self.fetches.lock().unwrap().push(blob.digest.clone());
            match self.blobs.get(&blob.digest) {
                Some(bytes) => Ok(bytes.clone()),
                None => bail!("no such blob"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::digest::HashAlgorithm;

    use super::testing::MemorySource;
    use super::*;

    const SHA: HashAlgorithm = HashAlgorithm::Sha256;

    fn source_with(bytes: &[u8]) -> (BlobRef, MemorySource) {
        let blob = BlobRef::of(SHA, bytes);
        let mut source = MemorySource::default();
        source.add(&blob, bytes.to_vec());
        (blob, source)
    }

    #[tokio::test]
    async fn fetches_once_then_serves_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(dir.path().to_path_buf());
        let (blob, source) = source_with(b"process FASTQC {}");

        assert_eq!(
            cache.get(&blob, &source).await.unwrap(),
            b"process FASTQC {}"
        );
        assert_eq!(
            cache.get(&blob, &source).await.unwrap(),
            b"process FASTQC {}"
        );
        assert_eq!(source.fetch_count(), 1, "the second read is a cache hit");

        // Stored where the layout says, and nothing left in tmp/.
        let hex = blob.digest.hex();
        let expected = dir.path().join("sha256").join(&hex[..2]).join(&hex[2..]);
        assert_eq!(std::fs::read(expected).unwrap(), b"process FASTQC {}");
        assert_eq!(
            std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
            0
        );
    }

    #[tokio::test]
    async fn refuses_bytes_that_do_not_match_and_caches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(dir.path().to_path_buf());
        let blob = BlobRef::of(SHA, b"process FASTQC {}");
        // A server (or a proxy) hands back something else, same length.
        let mut source = MemorySource::default();
        source.add(&blob, b"process EVILQC {}".to_vec());

        let err = cache.get(&blob, &source).await.unwrap_err();
        assert!(format!("{err:#}").contains("wrong content"), "{err:#}");
        assert!(!cache.path_of(&blob).exists());
    }

    #[tokio::test]
    async fn a_corrupt_cache_entry_is_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(dir.path().to_path_buf());
        let (blob, source) = source_with(b"process FASTQC {}");
        cache.get(&blob, &source).await.unwrap();

        std::fs::write(cache.path_of(&blob), b"bit rot").unwrap();

        assert_eq!(
            cache.get(&blob, &source).await.unwrap(),
            b"process FASTQC {}"
        );
        assert_eq!(source.fetch_count(), 2);
        assert_eq!(
            std::fs::read(cache.path_of(&blob)).unwrap(),
            b"process FASTQC {}"
        );
    }

    #[tokio::test]
    async fn refuses_an_oversized_blob_without_fetching_it() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlobCache::new(dir.path().to_path_buf());
        let (mut blob, source) = source_with(b"x");
        blob.size = MAX_BLOB_SIZE + 1;

        let err = cache.get(&blob, &source).await.unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
        assert_eq!(source.fetch_count(), 0);
    }
}
