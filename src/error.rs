use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::storage::StorageError;

pub type Result<T> = std::result::Result<T, anyhow::Error>;

#[derive(Debug)]
pub struct AppError(anyhow::Error);

impl From<StorageError> for AppError {
    fn from(error: StorageError) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}
