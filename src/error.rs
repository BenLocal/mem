use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::{service::memory_service::ServiceError, storage::StorageError};

pub type Result<T> = std::result::Result<T, anyhow::Error>;

#[derive(Debug)]
pub struct AppError(anyhow::Error);

impl From<StorageError> for AppError {
    fn from(error: StorageError) -> Self {
        Self(error.into())
    }
}

impl From<ServiceError> for AppError {
    fn from(error: ServiceError) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self.0.downcast_ref::<ServiceError>() {
            Some(ServiceError::NotFound) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "memory not found" })),
            )
                .into_response(),
            Some(ServiceError::Storage(StorageError::InvalidInput(msg))) => {
                (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
            }
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": self.0.to_string() })),
            )
                .into_response(),
        }
    }
}
