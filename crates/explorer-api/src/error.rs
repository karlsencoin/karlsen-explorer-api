//! Error model. Failures are always reported as errors, never as a fake
//! "empty" success (the legacy API returned `{"balance": 0}` on backend
//! failures, which hid outages from callers such as the deposit watcher).
//!
//! The JSON body keeps the legacy FastAPI shape: `{"detail": "..."}`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    NotFound(&'static str),
    #[error("{0}")]
    NotImplemented(&'static str),
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("node error")]
    Node(#[from] crate::node::NodeError),
    /// A dependency this endpoint needs is not configured (e.g. no karlsend).
    #[error("{0}")]
    Unavailable(&'static str),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, detail) = match &self {
            ApiError::Validation(m) => (StatusCode::UNPROCESSABLE_ENTITY, m.clone()),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, (*m).to_string()),
            ApiError::NotImplemented(m) => (StatusCode::NOT_IMPLEMENTED, (*m).to_string()),
            ApiError::Database(e) => {
                // Details go to the log only, never to the caller.
                tracing::error!(error = %e, "database error");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service temporarily unavailable".to_string(),
                )
            }
            ApiError::Node(e) => {
                tracing::error!(error = %e, "node error");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service temporarily unavailable".to_string(),
                )
            }
            ApiError::Unavailable(m) => {
                tracing::error!(reason = m, "dependency not configured");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service temporarily unavailable".to_string(),
                )
            }
        };
        (status, Json(json!({ "detail": detail }))).into_response()
    }
}
