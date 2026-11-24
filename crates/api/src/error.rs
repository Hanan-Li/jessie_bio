//! The API's one error shape (DESIGN.md §4.3):
//!
//! ```json
//! { "error": { "code": "database_unavailable", "message": "…", "details": null } }
//! ```
//!
//! Handlers return `Result<_, ApiError>`; axum turns the error into this
//! response through the `IntoResponse` trait.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    /// Stable and machine-readable: the frontend branches on this.
    pub code: &'static str,
    /// For people; may change wording at any time.
    pub message: String,
    /// Structured extras, e.g. every validation error.
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    /// Something we depend on is down: the request may work later.
    pub fn unavailable(code: &'static str, cause: impl std::fmt::Display) -> ApiError {
        eprintln!("{code}: {cause}");
        ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code,
            message: "a service the API depends on is unavailable".to_string(),
            details: None,
        }
    }

    /// The caller's own mistake, described to them: a request they should
    /// not repeat unchanged.
    pub fn client(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn unauthorized() -> ApiError {
        ApiError::client(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid agent token is required (Authorization: Bearer <token>)",
        )
    }

    /// The UI endpoints' version: a user token, not an agent's.
    pub fn unauthorized_user() -> ApiError {
        ApiError::client(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid user token is required (Authorization: Bearer <token>)",
        )
    }

    /// A request that is well-formed JSON but asks for something invalid,
    /// with every problem found in `details`, so the caller can fix them
    /// all at once.
    pub fn unprocessable(
        code: &'static str,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> ApiError {
        ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code,
            message: message.into(),
            details: Some(details),
        }
    }

    /// Also used for things that exist but belong to someone else, so a
    /// caller can't learn what exists by probing.
    pub fn not_found(what: &str) -> ApiError {
        ApiError::client(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found"),
        )
    }

    /// A bug or an unexpected failure. The cause goes to the log, not to
    /// the caller, so internal details never leak in a response.
    pub fn internal(cause: impl std::fmt::Display) -> ApiError {
        eprintln!("internal error: {cause}");
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "internal error".to_string(),
            details: None,
        }
    }
}

/// `From` lets `?` convert a database error into an `ApiError` inside a
/// handler. Database errors are never the caller's fault here, so they
/// become 500s with the cause logged.
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> ApiError {
        ApiError::internal(error)
    }
}

/// axum's own extractors answer bad input with plain text. A handler takes
/// the extractor's `Result` instead of the value (`body: Result<Json<T>,
/// JsonRejection>`), and `body?` turns the rejection into our one error
/// shape through this `From`. A message from a newer format version lands
/// here too, with the "upgrade the agent" text from `FormatVersion`.
impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> ApiError {
        ApiError::client(rejection.status(), "invalid_request", rejection.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> ApiError {
        ApiError::client(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            rejection.body_text(),
        )
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> ApiError {
        // `{:#}` prints the whole chain of context, not just the outermost.
        ApiError::internal(format!("{error:#}"))
    }
}

#[derive(Serialize)]
struct Body<'a> {
    error: Inner<'a>,
}

#[derive(Serialize)]
struct Inner<'a> {
    code: &'a str,
    message: &'a str,
    details: &'a Option<serde_json::Value>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Body {
            error: Inner {
                code: self.code,
                message: &self.message,
                details: &self.details,
            },
        };
        (self.status, Json(body)).into_response()
    }
}
