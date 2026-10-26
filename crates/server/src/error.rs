use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("request not permitted")]
    Forbidden,
    #[error("plan was changed elsewhere; reload before saving")]
    Conflict,
    #[error("not found")]
    NotFound,
    #[error("request limit reached")]
    RateLimited,
    #[error("{0}")]
    Unavailable(&'static str),
    #[error("internal server error")]
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_input"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Conflict => (StatusCode::CONFLICT, "revision_conflict"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Self::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        (
            status,
            Json(json!({"error": {"code": code, "message": self.to_string()}})),
        )
            .into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "database operation failed");
        Self::Unavailable("database temporarily unavailable")
    }
}

impl From<debarred_domain::Error> for AppError {
    fn from(error: debarred_domain::Error) -> Self {
        Self::BadRequest(error.to_string())
    }
}
