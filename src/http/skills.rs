use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header::CONTENT_TYPE, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::{
    app::AppState,
    error::AppError,
    http::admin_auth::{ReviewerAuthorized, RuntimeAuthorized},
    service::{
        BindSkillRequest, ResolveSkillLoadoutRequest, ResolvedSkillLoadout,
        RevokeSkillBundleRequest, SubmitSkillFeedbackRequest, SubmitSkillFeedbackResponse,
    },
    storage::StorageError,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/agent-loadouts/bind", post(bind))
        .route("/admin/agent-loadouts/resolve", post(resolve))
        .route("/admin/skills/feedback", post(feedback))
        .route("/admin/skills/revoke", post(revoke))
        .route(
            "/admin/skills/{skill_id}/versions/{bundle_version_id}/resources/{sha256}",
            get(resource),
        )
}

#[derive(Debug, Deserialize)]
struct ResourceQuery {
    tenant: String,
    agent_id: String,
    session_id: String,
}

async fn bind(
    State(state): State<AppState>,
    reviewer: ReviewerAuthorized,
    Json(request): Json<BindSkillRequest>,
) -> Result<Json<crate::domain::AgentLoadoutBinding>, AppError> {
    reviewer.require_tenant(&request.tenant)?;
    Ok(Json(service(&state)?.bind(request).await?))
}

async fn resolve(
    State(state): State<AppState>,
    runtime: RuntimeAuthorized,
    Json(request): Json<ResolveSkillLoadoutRequest>,
) -> Result<Json<ResolvedSkillLoadout>, AppError> {
    runtime.require_tenant(&request.tenant)?;
    Ok(Json(service(&state)?.resolve(request).await?))
}

async fn feedback(
    State(state): State<AppState>,
    runtime: RuntimeAuthorized,
    Json(request): Json<SubmitSkillFeedbackRequest>,
) -> Result<Json<SubmitSkillFeedbackResponse>, AppError> {
    runtime.require_tenant(&request.tenant)?;
    Ok(Json(service(&state)?.feedback(request).await?))
}

async fn resource(
    State(state): State<AppState>,
    runtime: RuntimeAuthorized,
    Path((skill_id, bundle_version_id, sha256)): Path<(String, String, String)>,
    Query(query): Query<ResourceQuery>,
) -> Result<Response<Body>, AppError> {
    runtime.require_tenant(&query.tenant)?;
    let resource = service(&state)?
        .get_resource(
            &query.tenant,
            &query.agent_id,
            &query.session_id,
            &skill_id,
            &bundle_version_id,
            &sha256,
        )
        .await?;
    Response::builder()
        .header(CONTENT_TYPE, resource.media_type)
        .body(Body::from(resource.content))
        .map_err(|_| AppError::from(StorageError::InvalidData("invalid resource response")))
}

async fn revoke(
    State(state): State<AppState>,
    reviewer: ReviewerAuthorized,
    Json(mut request): Json<RevokeSkillBundleRequest>,
) -> Result<Json<crate::domain::SkillBundleRevocation>, AppError> {
    reviewer.require_tenant(&request.tenant)?;
    request.revoked_by_role = Some(if reviewer.is_superuser() {
        "admin".to_string()
    } else {
        "reviewer".to_string()
    });
    Ok(Json(service(&state)?.revoke(request).await?))
}

fn service(state: &AppState) -> Result<&crate::service::SkillRuntimeService, AppError> {
    state
        .skill_runtime_service
        .as_deref()
        .ok_or_else(|| AppError::from(StorageError::Unsupported("Skill runtime")))
}
