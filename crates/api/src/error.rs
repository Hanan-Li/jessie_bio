//! The API's one error shape (DESIGN.md §4.3):
//!
//! ```json
//! { "error": { "code": "database_unavailable", "message": "…", "details": null } }
//! ```
//!
//! Handlers return `Result<_, ApiError>`; axum turns the error into this
//! response through the `IntoResponse` trait.

use axum::Json;
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
