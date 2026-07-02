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
        .route("/reviews/idle_archive", post(idle_archive))
        .route("/reviews/evolution", post(evolution))
        .route("/reviews/evolution/rollback", post(evolution_rollback))
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

/// Manual / cron trigger for the idle-archive sweep (governance Step 2).
/// Body shape: `{"tenant": "local", "dry_run": true}`.
///
/// `dry_run` defaults to `true` — preview the ids that *would* be archived
/// on the next real sweep without writing. `dry_run=false` actually
/// archives, but only when the worker is enabled
/// (`MEM_IDLE_ARCHIVE_ENABLED=1`); while disabled, a real run is a no-op and
/// returns an empty list (the destructive path is gated on the master switch
/// — unlike auto_promote, which is non-destructive). So the safe operator
/// flow is: hit this with `dry_run:true`, review, then enable the worker.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct IdleArchiveRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct IdleArchiveResponse {
    dry_run: bool,
    /// When `dry_run=true`, ids that *would* be archived on the next real
    /// sweep. When `dry_run=false`, ids actually archived in this call.
    capability_capsule_ids: Vec<String>,
}

async fn idle_archive(
    State(app): State<AppState>,
    Json(request): Json<IdleArchiveRequest>,
) -> Result<Json<IdleArchiveResponse>, AppError> {
    let ids = app
        .capability_capsule_service
        .idle_archive_sweep(&request.tenant, &app.config.idle_archive, request.dry_run)
        .await?;
    Ok(Json(IdleArchiveResponse {
        dry_run: request.dry_run,
        capability_capsule_ids: ids,
    }))
}

/// Capsule self-evolution sweep (doc `docs/evolution-worker.md` §9).
/// `dry_run` defaults to true — same operator flow as idle-archive:
/// preview, review the proposals, then enable the worker. The dry-run
/// preview works regardless of `MEM_EVOLUTION_ENABLED`; a real sweep
/// is a no-op while the switch is off.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct EvolutionRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    #[serde(default = "default_true")]
    dry_run: bool,
}

async fn evolution(
    State(app): State<AppState>,
    Json(request): Json<EvolutionRequest>,
) -> Result<Json<crate::worker::evolution_worker::EvolutionReport>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .evolution_sweep(&request.tenant, &app.config.evolution, request.dry_run)
            .await?,
    ))
}

/// §11 rollback of one EXECUTED evolution candidate — the exact inverse
/// of what a real sweep executed (merge: losers → Active + `merged_into`
/// edges closed; generalize: proposal → Archived + edges closed). No
/// `dry_run` field: rollback IS the safety valve, and it errors (400)
/// on unknown / non-executed candidate ids instead of no-op'ing.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct EvolutionRollbackRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    candidate_id: String,
}

async fn evolution_rollback(
    State(app): State<AppState>,
    Json(request): Json<EvolutionRollbackRequest>,
) -> Result<Json<crate::worker::evolution_worker::RollbackReport>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .evolution_rollback(&request.tenant, &request.candidate_id)
            .await?,
    ))
}

fn default_tenant() -> String {
    "local".to_string()
}

fn default_true() -> bool {
    true
}
