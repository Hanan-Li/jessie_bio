//! Content-addressed blobs in Postgres (DESIGN.md §4.1, "project files by
//! digest"): storing them, storing project trees, and serving them to
//! agents.

use std::collections::BTreeMap;

use protocol::blob::{Blob, BlobRef, Tree};
use protocol::digest::HashAlgorithm;
use sqlx::{PgPool, Postgres, Transaction};

/// Store a blob unless it's already there. Blobs are named by their
/// content, so an existing row with this digest already has these bytes.
///
/// `&mut Transaction` rather than `&PgPool`: the caller decides which
/// transaction the write belongs to, so it commits or rolls back together
/// with everything else the caller does.
pub async fn insert_blob(tx: &mut Transaction<'_, Postgres>, blob: &Blob) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO blobs (digest, size, bytes) VALUES ($1, $2, $3)
         ON CONFLICT (digest) DO NOTHING",
    )
    .bind(blob.reference.digest.to_string())
    .bind(i64::try_from(blob.reference.size)?)
    .bind(&blob.bytes)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Store a project: each file as a blob, the tree as a blob of its own,
/// and which blobs the tree lists (for [`fetch_for_env`]). Returns the
/// tree's reference, which identifies the project.
pub async fn store_tree(
    tx: &mut Transaction<'_, Postgres>,
    algorithm: HashAlgorithm,
    files: BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<BlobRef> {
    let (tree, file_blobs) = Tree::from_files(algorithm, files)?;
    for blob in &file_blobs {
        insert_blob(tx, blob).await?;
    }
    let tree_blob = tree.to_blob(algorithm);
    insert_blob(tx, &tree_blob).await?;

    let tree_digest = tree_blob.reference.digest.to_string();
    for (path, file) in &tree.files {
        sqlx::query(
            "INSERT INTO tree_entries (tree_digest, path, blob_digest) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(&tree_digest)
        .bind(path)
        .bind(file.digest.to_string())
        .execute(&mut **tx)
        .await?;
    }
    Ok(tree_blob.reference)
}

/// A blob's bytes, but only if a job in `compute_env` uses it: as its
/// project tree, or as a file that tree lists. A valid agent token can't
/// read other projects' code by guessing digests (DESIGN.md §4.3).
///
/// `None` both when the blob doesn't exist and when this environment may
/// not read it, so the answer reveals nothing about other environments.
pub async fn fetch_for_env(
    pool: &PgPool,
    compute_env: &str,
    digest: &str,
) -> Result<Option<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT b.bytes FROM blobs b
         WHERE b.digest = $1
           AND EXISTS (
               SELECT 1 FROM jobs j
               WHERE j.compute_env = $2
                 AND (j.project_tree = $1
                      OR EXISTS (SELECT 1 FROM tree_entries t
                                 WHERE t.tree_digest = j.project_tree AND t.blob_digest = $1)))",
    )
    .bind(digest)
    .bind(compute_env)
    .fetch_optional(pool)
    .await
}
