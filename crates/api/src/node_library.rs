//! Loading the node library into Postgres at startup (DESIGN.md §4.3).
//!
//! Each node version in `nodes/` is published once: its spec as a row in
//! `node_specs`, its Nextflow module as a blob. A published version never
//! changes, and this is where that rule is enforced rather than merely
//! agreed: if `nodes/` has different content for an existing (id, version),
//! startup fails and nothing is written.

use anyhow::{Context, bail};
use compiler::library::LibraryNode;
use compiler::spec::{NodeSpec, SpecRegistry};
use protocol::blob::Blob;
use protocol::digest::{HashAlgorithm, digest_bytes};
use sqlx::PgPool;
use sqlx::types::Json;

use crate::blobs::insert_blob;

/// What a sync did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    pub published: usize,
    pub unchanged: usize,
}

pub async fn sync(
    pool: &PgPool,
    nodes: Vec<LibraryNode>,
    algorithm: HashAlgorithm,
) -> anyhow::Result<SyncReport> {
    // One transaction for the whole library: either every node is
    // published or (on any conflict) none is.
    let mut tx = pool.begin().await?;
    let mut report = SyncReport {
        published: 0,
        unchanged: 0,
    };

    for node in nodes {
        let spec = &node.spec;
        let spec_json = serde_json::to_vec(spec)?;
        let spec_digest = digest_bytes(algorithm, &spec_json).to_string();

        let module = node
            .module_source
            .map(|source| Blob::new(algorithm, source.into_bytes()));
        if let Some(blob) = &module {
            insert_blob(&mut tx, blob).await?;
        }
        let module_digest = module
            .as_ref()
            .map(|blob| blob.reference.digest.to_string());

        // `query_as` maps each row to a tuple: (TEXT, TEXT or NULL).
        let existing: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT spec_digest, module_digest FROM node_specs WHERE id = $1 AND version = $2",
        )
        .bind(&spec.id)
        .bind(&spec.version)
        .fetch_optional(&mut *tx)
        .await?;

        match existing {
            None => {
                sqlx::query(
                    "INSERT INTO node_specs (id, version, spec, spec_digest, module_digest)
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(&spec.id)
                .bind(&spec.version)
                .bind(Json(spec))
                .bind(&spec_digest)
                .bind(&module_digest)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("publishing {} {}", spec.id, spec.version))?;
                report.published += 1;
            }
            Some((published_spec, published_module))
                if published_spec == spec_digest && published_module == module_digest =>
            {
                report.unchanged += 1;
            }
            Some(_) => bail!(
                "node {} {} in nodes/ differs from the version already published; \
                 published versions never change, so publish the change as a new version",
                spec.id,
                spec.version
            ),
        }
    }

    tx.commit().await?;
    Ok(report)
}

/// Every published node, read back from the database: what validation,
/// planning and code generation use. The database, not `nodes/`, is the
/// source of truth, so a run always compiles against exactly what was
/// published, even after a node is removed from `nodes/`.
pub async fn load_registry(pool: &PgPool) -> anyhow::Result<SpecRegistry> {
    let rows: Vec<(Json<NodeSpec>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT n.spec, b.bytes FROM node_specs n
         LEFT JOIN blobs b ON b.digest = n.module_digest
         ORDER BY n.id, n.version",
    )
    .fetch_all(pool)
    .await?;

    let mut modules = Vec::new();
    let mut specs = Vec::new();
    for (Json(spec), module) in rows {
        if let Some(bytes) = module {
            let source = String::from_utf8(bytes)
                .with_context(|| format!("module of {} {} is not UTF-8", spec.id, spec.version))?;
            modules.push((spec.id.clone(), spec.version.clone(), source));
        }
        specs.push(spec);
    }
    let mut registry = SpecRegistry::new(specs);
    for (id, version, source) in &modules {
        registry.add_module_source(id, version, source);
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use compiler::library::load_library;

    use super::*;

    const SHA: HashAlgorithm = HashAlgorithm::Sha256;

    fn repo_library() -> Vec<LibraryNode> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nodes");
        load_library(&dir).unwrap()
    }

    /// Takes the whole query, not a table name: sqlx 0.9 accepts only
    /// `&'static str` SQL by default, so a query can't be assembled from
    /// runtime strings (the classic route to SQL injection).
    async fn count(pool: &PgPool, query: &'static str) -> i64 {
        sqlx::query_scalar(query).fetch_one(pool).await.unwrap()
    }

    // `#[sqlx::test]` creates a fresh database for each test (using
    // DATABASE_URL to reach the server), runs our migrations in it, hands
    // the test a pool, and drops the database afterwards.
    #[sqlx::test(migrations = "./migrations")]
    async fn publishes_the_repo_library(pool: PgPool) {
        let report = sync(&pool, repo_library(), SHA).await.unwrap();

        assert_eq!(
            report,
            SyncReport {
                published: 2,
                unchanged: 0
            }
        );
        assert_eq!(count(&pool, "SELECT count(*) FROM node_specs").await, 2);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM blobs").await,
            2,
            "the FastQC and MultiQC modules"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn syncing_again_changes_nothing(pool: PgPool) {
        sync(&pool, repo_library(), SHA).await.unwrap();
        let report = sync(&pool, repo_library(), SHA).await.unwrap();

        assert_eq!(
            report,
            SyncReport {
                published: 0,
                unchanged: 2
            }
        );
        assert_eq!(count(&pool, "SELECT count(*) FROM blobs").await, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_changed_version_is_refused_and_nothing_is_written(pool: PgPool) {
        sync(&pool, repo_library(), SHA).await.unwrap();

        // Someone edits fastqc 0.1.0's module in place instead of
        // publishing 0.2.0.
        let mut nodes = repo_library();
        let fastqc = nodes.iter_mut().find(|n| n.spec.id == "fastqc").unwrap();
        fastqc.module_source = Some("process FASTQC { /* edited */ }".to_string());
        let err = sync(&pool, nodes, SHA).await.unwrap_err();

        assert!(
            err.to_string().contains("fastqc 0.1.0 in nodes/ differs"),
            "{err}"
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM blobs").await,
            2,
            "the edited module's blob was rolled back"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_registry_is_read_back_from_the_database(pool: PgPool) {
        sync(&pool, repo_library(), SHA).await.unwrap();

        let registry = load_registry(&pool).await.unwrap();

        assert!(registry.get("fastqc", "0.1.0").is_some());
        assert!(registry.get("multiqc", "0.1.0").is_some());
        let on_disk = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nodes/fastqc/0.1.0/main.nf"),
        )
        .unwrap();
        assert_eq!(
            registry.module_source("fastqc", "0.1.0"),
            Some(on_disk.as_str())
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_stored_module_is_the_library_file(pool: PgPool) {
        sync(&pool, repo_library(), SHA).await.unwrap();

        let (digest, bytes): (String, Vec<u8>) = sqlx::query_as(
            "SELECT b.digest, b.bytes FROM node_specs n JOIN blobs b ON b.digest = n.module_digest
             WHERE n.id = 'fastqc' AND n.version = '0.1.0'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let on_disk = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nodes/fastqc/0.1.0/main.nf"),
        )
        .unwrap();
        assert_eq!(bytes, on_disk);
        assert_eq!(digest, digest_bytes(SHA, &on_disk).to_string());
    }
}
