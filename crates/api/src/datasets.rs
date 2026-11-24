//! Registering datasets (DESIGN.md §4.3, "Datasets and input digests"):
//!
//! ```text
//! POST /api/datasets { name, location: { agent_id }, samples: [
//!     { sample: "gut", files: [ { uri: "/data/ggal/ggal_gut_1.fq", digest: "sha256:…", size: 691873 }, … ] } ] }
//!   → 201 { dataset_id }
//! ```
//!
//! A dataset is a set of samples, each with its files in order (R1 before
//! R2), on one agent's disk. The control plane never reads the bytes, so
//! the digests come from whoever registers the dataset: in P0c the
//! `scripts/register-dataset.sh` stand-in, which runs `sha256sum`; from P1
//! the agent itself, through the inventory flow.

use std::collections::BTreeSet;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use protocol::digest::Digest;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;
use crate::users::User;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/datasets", post(create_dataset))
}

#[derive(Debug, Deserialize)]
struct NewDataset {
    name: String,
    location: Location,
    samples: Vec<NewSample>,
}

/// Where the files are. Only "on this agent's disk" in P0c; S3 and other
/// storage come later, as more fields or a tagged enum.
#[derive(Debug, Deserialize)]
struct Location {
    agent_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct NewSample {
    sample: String,
    files: Vec<NewFile>,
}

#[derive(Debug, Deserialize)]
struct NewFile {
    uri: String,
    /// Parsed while reading the body: a malformed digest is refused before
    /// the handler runs.
    digest: Digest,
    size: u64,
}

#[derive(Serialize)]
struct Created {
    dataset_id: Uuid,
}

async fn create_dataset(
    State(state): State<AppState>,
    _user: User,
    body: Result<Json<NewDataset>, JsonRejection>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let Json(dataset) = body?;

    let mut problems = check(&dataset);
    let agent_known: Option<Uuid> = sqlx::query_scalar("SELECT id FROM agents WHERE id = $1")
        .bind(dataset.location.agent_id)
        .fetch_optional(&state.pool)
        .await?;
    if agent_known.is_none() {
        problems.push(format!(
            "agent {} does not exist",
            dataset.location.agent_id
        ));
    }
    if !problems.is_empty() {
        let count = problems.len();
        return Err(ApiError::unprocessable(
            "invalid_dataset",
            format!(
                "the dataset has {count} problem{}",
                if count == 1 { "" } else { "s" }
            ),
            problems.into(),
        ));
    }

    // The dataset and all its files, or nothing.
    let mut tx = state.pool.begin().await?;
    let dataset_id: Uuid =
        sqlx::query_scalar("INSERT INTO datasets (name, agent_id) VALUES ($1, $2) RETURNING id")
            .bind(dataset.name.trim())
            .bind(dataset.location.agent_id)
            .fetch_one(&mut *tx)
            .await?;
    for sample in &dataset.samples {
        for (position, file) in sample.files.iter().enumerate() {
            sqlx::query(
                "INSERT INTO dataset_files (dataset_id, sample, position, uri, digest, size)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(dataset_id)
            .bind(&sample.sample)
            .bind(i32::try_from(position).map_err(anyhow::Error::from)?)
            .bind(&file.uri)
            .bind(file.digest.to_string())
            .bind(i64::try_from(file.size).map_err(anyhow::Error::from)?)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(Created { dataset_id })))
}

/// Every problem with a dataset that can be seen without the database, in
/// plain words. All are collected, not just the first, as in validation.
fn check(dataset: &NewDataset) -> Vec<String> {
    let mut problems = Vec::new();
    if dataset.name.trim().is_empty() {
        problems.push("a dataset needs a name".to_string());
    }
    if dataset.samples.is_empty() {
        problems.push("a dataset needs at least one sample".to_string());
    }
    let mut seen = BTreeSet::new();
    for sample in &dataset.samples {
        let name = &sample.sample;
        if !is_valid_sample_name(name) {
            problems.push(format!(
                "sample name `{name}` must start with a letter or digit and contain only \
                 letters, digits, `_`, `-` and `.`"
            ));
        }
        if !seen.insert(name.as_str()) {
            problems.push(format!("sample `{name}` is listed more than once"));
        }
        if sample.files.is_empty() {
            problems.push(format!("sample `{name}` has no files"));
        }
        for file in &sample.files {
            // P0c datasets are on an agent's disk, so a URI is an absolute
            // path there. Relative to what, otherwise?
            if !file.uri.starts_with('/') {
                problems.push(format!(
                    "file `{}` of sample `{name}` must be an absolute path",
                    file.uri
                ));
            }
        }
    }
    problems
}

/// Sample names travel far: into CSV manifests, Nextflow's `meta.id`, and
/// directory names (`results/<output>/<sample>/`) on the agent's machine.
/// A name like `../../x` must never get that far, so names are kept to a
/// safe set of characters.
fn is_valid_sample_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use sqlx::PgPool;

    use super::is_valid_sample_name;
    use crate::testing::*;

    #[test]
    fn sample_names_cannot_escape_a_directory() {
        for good in ["gut", "S1", "liver_2", "A-1.rep2"] {
            assert!(is_valid_sample_name(good), "{good}");
        }
        for bad in ["", "..", "../x", ".hidden", "a/b", "a b", "a,b", "-x"] {
            assert!(!is_valid_sample_name(bad), "{bad:?}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn registers_samples_and_their_files_in_order(pool: PgPool) {
        let state = ui_state(&pool).await;
        let agent = setup(&pool).await;

        let reply = register_reads(&state, agent.agent.id).await;

        assert_eq!(reply.status, StatusCode::CREATED);
        let rows: Vec<(String, i32, String)> = sqlx::query_as(
            "SELECT sample, position, uri FROM dataset_files ORDER BY sample, position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("gut".to_string(), 0, "/data/ggal/ggal_gut_1.fq".to_string()),
                ("gut".to_string(), 1, "/data/ggal/ggal_gut_2.fq".to_string()),
            ]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn lists_every_problem(pool: PgPool) {
        let state = ui_state(&pool).await;
        let file = json!({ "uri": "relative/a.fq", "digest": digest_of("a"), "size": 1 });
        let body = json!({
            "name": "",
            "location": { "agent_id": uuid::Uuid::new_v4() },
            "samples": [
                { "sample": "../evil", "files": [file] },
                { "sample": "s1", "files": [] },
                { "sample": "s1", "files": [] }
            ]
        });

        let reply = call(&state, "POST", "/api/datasets", Some(DEV_TOKEN), Some(body)).await;

        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        let error = &reply.json()["error"];
        assert_eq!(error["code"], "invalid_dataset");
        let details: Vec<String> = serde_json::from_value(error["details"].clone()).unwrap();
        assert_eq!(details.len(), 7, "{details:#?}");
        let has = |text: &str| details.iter().any(|detail| detail.contains(text));
        assert!(has("needs a name"));
        assert!(has("sample name `../evil`"));
        assert!(has("must be an absolute path"));
        assert!(has("sample `s1` has no files"));
        assert!(has("sample `s1` is listed more than once"));
        assert!(has("does not exist"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_malformed_digest_is_refused(pool: PgPool) {
        let state = ui_state(&pool).await;
        let agent = setup(&pool).await;
        let body = json!({
            "name": "reads",
            "location": { "agent_id": agent.agent.id },
            "samples": [{ "sample": "gut", "files": [
                { "uri": "/data/a.fq", "digest": "md5:abc", "size": 1 }
            ] }]
        });
        let reply = call(&state, "POST", "/api/datasets", Some(DEV_TOKEN), Some(body)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.json()["error"]["code"], "invalid_request");
    }
}
