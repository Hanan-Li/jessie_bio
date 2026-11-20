//! Compute environments, agents and agent tokens (DESIGN.md §4.3, §8.3:
//! "agents are not users").
//!
//! An agent proves who it is with a bearer token. The token is shown once,
//! when the agent is created; the database keeps only its digest, so
//! reading the database (or a backup) doesn't give anyone a working token.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use protocol::digest::{HashAlgorithm, digest_bytes};
use sqlx::PgPool;
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;

/// Tokens start with this, so one pasted somewhere it shouldn't be is easy
/// to recognise (and for secret scanners to find).
const TOKEN_PREFIX: &str = "jat_";

/// An agent that has proved who it is. Handlers that take this as an
/// argument only run for authenticated agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: Uuid,
    pub compute_env: String,
}

pub struct NewComputeEnv {
    pub id: String,
    pub nextflow_version: String,
    pub time_limit_secs: Option<u64>,
}

pub async fn create_compute_env(pool: &PgPool, env: &NewComputeEnv) -> anyhow::Result<()> {
    let time_limit = env.time_limit_secs.map(i64::try_from).transpose()?;
    sqlx::query(
        "INSERT INTO compute_envs (id, nextflow_version, time_limit_secs) VALUES ($1, $2, $3)",
    )
    .bind(&env.id)
    .bind(&env.nextflow_version)
    .bind(time_limit)
    .execute(pool)
    .await?;
    Ok(())
}

/// Create an agent in `compute_env`; returns its id and its token. The
/// token can't be recovered later: lose it and create a new agent.
pub async fn create_agent(
    pool: &PgPool,
    compute_env: &str,
    name: &str,
) -> anyhow::Result<(Uuid, String)> {
    // 244 random bits from the operating system's generator (UUID v4 is
    // 122 random bits each).
    let token = format!(
        "{TOKEN_PREFIX}{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO agents (name, compute_env, token_digest) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(name)
    .bind(compute_env)
    .bind(token_digest(&token))
    .fetch_one(pool)
    .await?;
    Ok((id, token))
}

/// Tokens are long and random, so a fast hash is enough: unlike a
/// password, there is nothing to brute-force. Looking the digest up with
/// an index also avoids comparing secrets byte by byte in our own code.
fn token_digest(token: &str) -> String {
    digest_bytes(HashAlgorithm::Sha256, token.as_bytes()).to_string()
}

pub async fn authenticate(pool: &PgPool, token: &str) -> Result<Option<Agent>, sqlx::Error> {
    if !token.starts_with(TOKEN_PREFIX) {
        return Ok(None);
    }
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT id, compute_env FROM agents WHERE token_digest = $1")
            .bind(token_digest(token))
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, compute_env)| Agent { id, compute_env }))
}

/// An axum *extractor*: implementing `FromRequestParts` lets a handler
/// take `agent: Agent` as an argument, and axum runs this first. If it
/// returns an error, the handler never runs, so no agent endpoint can
/// forget to check the token.
impl FromRequestParts<AppState> for Agent {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Agent, ApiError> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized)?;
        authenticate(&state.pool, token.trim())
            .await?
            .ok_or_else(ApiError::unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn env(pool: &PgPool) {
        create_compute_env(
            pool,
            &NewComputeEnv {
                id: "laptop".to_string(),
                nextflow_version: "26.10.5".to_string(),
                time_limit_secs: None,
            },
        )
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_token_identifies_its_agent(pool: PgPool) {
        env(&pool).await;
        let (id, token) = create_agent(&pool, "laptop", "dev").await.unwrap();

        let agent = authenticate(&pool, &token).await.unwrap();
        assert_eq!(
            agent,
            Some(Agent {
                id,
                compute_env: "laptop".to_string()
            })
        );
        assert_eq!(authenticate(&pool, "jat_wrong").await.unwrap(), None);
        assert_eq!(authenticate(&pool, "").await.unwrap(), None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn only_the_digest_is_stored(pool: PgPool) {
        env(&pool).await;
        let (_, token) = create_agent(&pool, "laptop", "dev").await.unwrap();

        let stored: String = sqlx::query_scalar("SELECT token_digest FROM agents")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(stored.starts_with("sha256:"));
        assert!(!stored.contains(token.trim_start_matches(TOKEN_PREFIX)));
    }
}
