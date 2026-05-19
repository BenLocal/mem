use axum::{
    extract::Query,
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

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
        .route("/reviews/auto_promote", post(auto_promote))
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

/// Manual / cron trigger for the auto-promote sweep. Body shape:
/// `{"tenant": "local", "dry_run": true}`.
///
/// `dry_run=true` (the default) only previews candidate ids; nothing
/// is written. `dry_run=false` actually promotes — even when the
/// background worker is disabled via `MEM_AUTO_PROMOTE_DISABLED=1`,
/// so this endpoint doubles as a one-shot CLI hook for operators
/// who want to run the sweep on demand without flipping the master
/// switch.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct AutoPromoteRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct AutoPromoteResponse {
    dry_run: bool,
    /// When `dry_run=true`, ids that *would* be promoted on the next
    /// real sweep. When `dry_run=false`, ids that were actually
    /// promoted in this call.
    capability_capsule_ids: Vec<String>,
}

async fn auto_promote(
    State(app): State<AppState>,
    Json(request): Json<AutoPromoteRequest>,
) -> Result<Json<AutoPromoteResponse>, AppError> {
    let ids = app
        .capability_capsule_service
        .auto_promote_sweep(&request.tenant, &app.config.auto_promote, request.dry_run)
        .await?;
    Ok(Json(AutoPromoteResponse {
        dry_run: request.dry_run,
        capability_capsule_ids: ids,
    }))
}

fn default_tenant() -> String {
    "local".to_string()
}

fn default_true() -> bool {
    true
}
