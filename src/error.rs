use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::{
    service::capability_capsule_service::ServiceError,
    storage::{GraphError, StorageError},
};

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

impl From<GraphError> for AppError {
    fn from(error: GraphError) -> Self {
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
                ServiceError::Storage(StorageError::RateLimited(msg)) => {
                    (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": msg }))).into_response()
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
        // Rate limit (e.g. per-session ingest cap) → 429, distinct from 400, so
        // a caller / proxy can retry-after rather than treat it as malformed.
        if let Some(StorageError::RateLimited(msg)) = self.0.downcast_ref::<StorageError>() {
            return (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": msg }))).into_response();
        }
        // Graph-layer caller validation (K12: inverted bitemporal
        // interval) is a client error, not a backend fault → 400.
        if let Some(GraphError::InvalidInput(msg)) = self.0.downcast_ref::<GraphError>() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(err: AppError) -> StatusCode {
        err.into_response().status()
    }

    #[test]
    fn rate_limited_maps_to_429_not_400() {
        // The per-session ingest cap surfaces as RateLimited — a "slow down
        // and retry" signal, distinct from InvalidInput's 400.
        let svc = AppError::from(ServiceError::Storage(StorageError::RateLimited(
            "cap".into(),
        )));
        assert_eq!(status_of(svc), StatusCode::TOO_MANY_REQUESTS);
        // Bare StorageError path (transcript-style routes) maps the same.
        let bare = AppError::from(StorageError::RateLimited("cap".into()));
        assert_eq!(status_of(bare), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn not_found_and_invalid_input_keep_their_statuses() {
        assert_eq!(
            status_of(AppError::from(ServiceError::NotFound)),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(AppError::from(ServiceError::Storage(
                StorageError::InvalidInput("bad".into())
            ))),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(AppError::from(StorageError::InvalidInput("bad".into()))),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn storage_not_found_is_500_neutral() {
        // Internal-consistency miss → 500 (not 404) with a neutral body, by
        // design (must not leak the looked-up id).
        assert_eq!(
            status_of(AppError::from(StorageError::NotFound("capsule"))),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
