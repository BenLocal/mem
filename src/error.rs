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
        // Service-layer errors first (memory pipeline). NotFound carries a
        // canonical "memory not found" message; nested InvalidInput maps to 400.
        if let Some(svc) = self.0.downcast_ref::<ServiceError>() {
            return match svc {
                ServiceError::NotFound => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "memory not found" })),
                )
                    .into_response(),
                ServiceError::Storage(StorageError::InvalidInput(msg)) => {
                    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
                }
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": self.0.to_string() })),
                )
                    .into_response(),
            };
        }
        // Bare StorageError (transcript routes go through this path — they
        // don't wrap in ServiceError). InvalidInput → 400, NotFound → 500
        // (internal-consistency miss, neutral body to avoid leaking the
        // looked-up id), everything else → 500.
        if let Some(StorageError::InvalidInput(msg)) = self.0.downcast_ref::<StorageError>() {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
        }
        if let Some(StorageError::NotFound(_)) = self.0.downcast_ref::<StorageError>() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal lookup miss" })),
            )
                .into_response();
        }
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}
