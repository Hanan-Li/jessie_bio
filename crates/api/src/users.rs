//! Who may call the UI endpoints (DESIGN.md §4.3, §8.3).
//!
//! P0c has no user accounts: one **development token**, given to the API at
//! startup (`--dev-token` or `JESSIE_DEV_TOKEN`), opens every `/api/*`
//! endpoint except `/api/agent/*`. Clerk sessions replace it in P1, behind
//! this same extractor, so no handler changes then.
//!
//! The two kinds of credential never overlap: an agent token (`jat_…`) is
//! refused here, and the development token is refused by the agent
//! endpoints (`agents::Agent`), because each extractor checks only its own
//! kind.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use protocol::digest::{Digest, HashAlgorithm, digest_bytes};

use crate::AppState;
use crate::error::ApiError;

/// A caller that presented the development token. A handler that takes
/// `_user: User` as an argument only runs for such callers.
///
/// A struct with no fields: today it carries no information beyond "the
/// check passed". With accounts it will hold the user's id and roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct User;

/// What the API keeps of the development token: its digest, never the
/// token itself.
pub fn dev_token_digest(token: &str) -> Digest {
    digest_bytes(HashAlgorithm::Sha256, token.as_bytes())
}

impl FromRequestParts<AppState> for User {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<User, ApiError> {
        // Without a configured token nobody gets in: a server started
        // without one serves only agents.
        let expected = state
            .dev_token
            .as_ref()
            .ok_or_else(ApiError::unauthorized_user)?;
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(ApiError::unauthorized_user)?;
        // Compare digests, not the tokens themselves: `==` on strings stops
        // at the first differing byte, so how long it takes could hint at
        // how much of a guess was right. Comparing digests only leaks
        // something about digests, which doesn't help anyone guess the
        // token.
        if dev_token_digest(token.trim()) == *expected {
            Ok(User)
        } else {
            Err(ApiError::unauthorized_user())
        }
    }
}
