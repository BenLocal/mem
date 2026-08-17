use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

use crate::{
    app::AppState,
    error::AppError,
    http::admin_auth::{CompilerAuthorized, ReviewerAuthorized},
    service::{
        AcceptSkillProposalRequest, AcceptSkillProposalResponse, RejectSkillProposalRequest,
    },
    service::{
        CompleteSkillDecisionRequest, PublishSkillProposalOutcome, PublishSkillProposalRequest,
        SkillCompileClaimBatch, SkillCompilePreviewBatch,
    },
    storage::StorageError,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/skill-proposals/claim", post(claim))
        .route("/admin/skill-proposals/preview", post(preview))
        .route("/admin/skill-proposals/renew", post(renew))
        .route("/admin/skill-proposals/complete", post(complete))
        .route("/admin/skill-proposals/publish", post(publish))
        .route("/admin/skill-proposals/accept", post(accept))
        .route("/admin/skill-proposals/reject", post(reject))
        .route("/admin/skill-proposals/fail", post(fail))
}

#[derive(Debug, Deserialize)]
struct ClaimRequest {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    tenant: Option<String>,
}

#[derive(Deserialize)]
struct LeaseRequest {
    job_id: String,
    lease_token: String,
}

#[derive(Deserialize)]
struct FailRequest {
    job_id: String,
    lease_token: String,
    error_code: String,
}

#[derive(Debug, Serialize)]
struct EmptyResponse {
    ok: bool,
}

async fn claim(
    State(state): State<AppState>,
    compiler: CompilerAuthorized,
    Json(request): Json<ClaimRequest>,
) -> Result<Json<SkillCompileClaimBatch>, AppError> {
    if let Some(tenant) = request.tenant.as_deref() {
        compiler.require_tenant(tenant)?;
    }
    let tenant = compiler.tenant_scope().or(request.tenant.as_deref());
    Ok(Json(
        service(&state)?
            .claim_for_tenant(request.limit, tenant)
            .await?,
    ))
}

async fn preview(
    State(state): State<AppState>,
    compiler: CompilerAuthorized,
    Json(request): Json<ClaimRequest>,
) -> Result<Json<SkillCompilePreviewBatch>, AppError> {
    if let Some(tenant) = request.tenant.as_deref() {
        compiler.require_tenant(tenant)?;
    }
    let tenant = compiler.tenant_scope().or(request.tenant.as_deref());
    Ok(Json(
        service(&state)?
            .preview_for_tenant(request.limit, tenant)
            .await?,
    ))
}

async fn renew(
    State(state): State<AppState>,
    compiler: CompilerAuthorized,
    Json(request): Json<LeaseRequest>,
) -> Result<Json<EmptyResponse>, AppError> {
    service(&state)?
        .renew_for_tenant(
            &request.job_id,
            &request.lease_token,
            compiler.tenant_scope(),
        )
        .await?;
    Ok(Json(EmptyResponse { ok: true }))
}

async fn publish(
    State(state): State<AppState>,
    compiler: CompilerAuthorized,
    Json(request): Json<PublishSkillProposalRequest>,
) -> Result<Json<PublishSkillProposalOutcome>, AppError> {
    Ok(Json(
        service(&state)?
            .publish_for_tenant(request, compiler.tenant_scope())
            .await?,
    ))
}

async fn accept(
    State(state): State<AppState>,
    reviewer: ReviewerAuthorized,
    Json(request): Json<AcceptSkillProposalRequest>,
) -> Result<Json<AcceptSkillProposalResponse>, AppError> {
    reviewer.require_tenant(&request.tenant)?;
    let service = state
        .skill_governance_service
        .as_deref()
        .ok_or_else(|| AppError::from(StorageError::Unsupported("Skill bundle governance")))?;
    Ok(Json(service.accept(request).await?))
}

async fn reject(
    State(state): State<AppState>,
    reviewer: ReviewerAuthorized,
    Json(request): Json<RejectSkillProposalRequest>,
) -> Result<Json<EmptyResponse>, AppError> {
    reviewer.require_tenant(&request.tenant)?;
    let service = state
        .skill_governance_service
        .as_deref()
        .ok_or_else(|| AppError::from(StorageError::Unsupported("Skill bundle governance")))?;
    service.reject(request).await?;
    Ok(Json(EmptyResponse { ok: true }))
}

async fn complete(
    State(state): State<AppState>,
    compiler: CompilerAuthorized,
    Json(request): Json<CompleteSkillDecisionRequest>,
) -> Result<Json<crate::domain::SkillCompileDecisionRecord>, AppError> {
    Ok(Json(
        service(&state)?
            .complete_decision_for_tenant(request, compiler.tenant_scope())
            .await?,
    ))
}

async fn fail(
    State(state): State<AppState>,
    compiler: CompilerAuthorized,
    Json(request): Json<FailRequest>,
) -> Result<Json<EmptyResponse>, AppError> {
    service(&state)?
        .fail_for_tenant(
            &request.job_id,
            &request.lease_token,
            &request.error_code,
            compiler.tenant_scope(),
        )
        .await?;
    Ok(Json(EmptyResponse { ok: true }))
}

fn service(state: &AppState) -> Result<&crate::service::SkillProposalService, AppError> {
    state
        .skill_proposal_service
        .as_deref()
        .ok_or_else(|| AppError::from(StorageError::Unsupported("Skill proposal compiler")))
}

fn default_limit() -> usize {
    1
}
