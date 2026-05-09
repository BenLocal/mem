use axum::{
    extract::Query,
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::{app::AppState, domain::capability_capsule::EditPendingRequest, error::AppError};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/reviews/pending", get(list_pending))
        .route("/reviews/pending/accept", post(accept_pending))
        .route("/reviews/pending/reject", post(reject_pending))
        .route(
            "/reviews/pending/edit_accept",
            post(edit_and_accept_pending),
        )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct PendingReviewQuery {
    #[serde(default = "default_tenant")]
    tenant: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct PendingReviewActionRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    capability_capsule_id: String,
}

async fn list_pending(
    State(app): State<AppState>,
    Query(query): Query<PendingReviewQuery>,
) -> Result<Json<Vec<crate::domain::capability_capsule::CapabilityCapsuleRecord>>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .list_pending_review(&query.tenant)
            .await?,
    ))
}

async fn accept_pending(
    State(app): State<AppState>,
    Json(request): Json<PendingReviewActionRequest>,
) -> Result<Json<crate::domain::capability_capsule::CapabilityCapsuleRecord>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .accept_pending(&request.tenant, &request.capability_capsule_id)
            .await?,
    ))
}

async fn reject_pending(
    State(app): State<AppState>,
    Json(request): Json<PendingReviewActionRequest>,
) -> Result<Json<crate::domain::capability_capsule::CapabilityCapsuleRecord>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .reject_pending(&request.tenant, &request.capability_capsule_id)
            .await?,
    ))
}

async fn edit_and_accept_pending(
    State(app): State<AppState>,
    Json(request): Json<HttpEditPendingRequest>,
) -> Result<Json<crate::domain::capability_capsule::EditPendingResponse>, AppError> {
    let tenant = request.tenant.clone();
    Ok(Json(
        app.capability_capsule_service
            .edit_and_accept_pending(&tenant, request.into())
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct HttpEditPendingRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    capability_capsule_id: String,
    summary: String,
    content: String,
    evidence: Vec<String>,
    code_refs: Vec<String>,
    tags: Vec<String>,
}

impl From<HttpEditPendingRequest> for EditPendingRequest {
    fn from(request: HttpEditPendingRequest) -> Self {
        Self {
            capability_capsule_id: request.capability_capsule_id,
            summary: request.summary,
            content: request.content,
            evidence: request.evidence,
            code_refs: request.code_refs,
            tags: request.tags,
        }
    }
}

fn default_tenant() -> String {
    "local".to_string()
}
