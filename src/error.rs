use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

use crate::{
    service::capability_capsule_service::ServiceError,
    storage::{GraphError, StorageError},
};

pub type Result<T> = std::result::Result<T, anyhow::Error>;

#[derive(Debug)]
pub struct AppError(anyhow::Error);

#[derive(Debug, Error)]
#[error("admin authorization required")]
struct UnauthorizedAdmin;

impl AppError {
    pub fn unauthorized_admin() -> Self {
        Self(UnauthorizedAdmin.into())
    }
}

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

fn internal_error_context(error: &anyhow::Error) -> (&'static str, Option<&'static str>) {
    let storage_error = error.downcast_ref::<StorageError>().or_else(|| {
        error
            .downcast_ref::<ServiceError>()
            .and_then(|service_error| match service_error {
                ServiceError::Storage(storage_error) => Some(storage_error),
                _ => None,
            })
    });
    match storage_error {
        Some(StorageError::Backend { backend, .. }) => ("storage_backend", Some(*backend)),
        Some(_) => ("storage_internal", None),
        None if error.downcast_ref::<GraphError>().is_some() => ("graph_internal", None),
        None => ("internal", None),
    }
}

fn internal_server_error(error: &anyhow::Error) -> Response {
    let (error_kind, backend) = internal_error_context(error);
    tracing::error!(
        error_kind,
        backend = backend.unwrap_or("none"),
        "request failed with an internal error"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal server error" })),
    )
        .into_response()
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if self.0.downcast_ref::<UnauthorizedAdmin>().is_some() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "admin authorization required" })),
            )
                .into_response();
        }
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
                ServiceError::Storage(StorageError::Conflict(msg)) => {
                    (StatusCode::CONFLICT, Json(json!({ "error": msg }))).into_response()
                }
                ServiceError::Storage(StorageError::Unsupported(capability)) => (
                    StatusCode::NOT_IMPLEMENTED,
                    Json(json!({ "error": format!("unsupported capability: {capability}") })),
                )
                    .into_response(),
                _ => internal_server_error(&self.0),
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
        if let Some(StorageError::Conflict(msg)) = self.0.downcast_ref::<StorageError>() {
            return (StatusCode::CONFLICT, Json(json!({ "error": msg }))).into_response();
        }
        if let Some(StorageError::Unsupported(capability)) = self.0.downcast_ref::<StorageError>() {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({ "error": format!("unsupported capability: {capability}") })),
            )
                .into_response();
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
        internal_server_error(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::get,
        Router,
    };
    use tower::ServiceExt;

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
    fn review_conflict_maps_to_409() {
        let err = StorageError::Conflict("review conflict");
        assert_eq!(status_of(AppError::from(err)), StatusCode::CONFLICT);
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

    #[tokio::test]
    async fn internal_storage_errors_are_neutral_500s() {
        const PRIVATE_DETAIL: &str = "postgres://db.internal/private_table";

        async fn bare_error() -> std::result::Result<(), AppError> {
            Err(StorageError::backend("postgres", std::io::Error::other(PRIVATE_DETAIL)).into())
        }

        async fn service_error() -> std::result::Result<(), AppError> {
            Err(ServiceError::Storage(StorageError::backend(
                "postgres",
                std::io::Error::other(PRIVATE_DETAIL),
            ))
            .into())
        }

        let app = Router::new()
            .route("/bare", get(bare_error))
            .route("/service", get(service_error));
        for uri in ["/bare", "/service"] {
            let response = app
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                json!({ "error": "internal server error" })
            );
            assert!(!String::from_utf8_lossy(&body).contains(PRIVATE_DETAIL));
        }
    }

    #[test]
    fn internal_error_log_context_excludes_backend_source() {
        const PRIVATE_DETAIL: &str = "postgres://db.internal/private_table";
        let error = AppError::from(StorageError::backend(
            "postgres",
            std::io::Error::other(PRIVATE_DETAIL),
        ));

        let context = internal_error_context(&error.0);
        assert_eq!(context, ("storage_backend", Some("postgres")));
        assert!(!format!("{context:?}").contains(PRIVATE_DETAIL));
    }
}
