//! Operator-driven maintenance endpoints:
//! - `POST /admin/vacuum` — immediate Lance manifest prune across every
//!   managed table without waiting for the daily worker tick. Same backend
//!   as `crate::worker::vacuum_worker::sweep_once`.
//! - `POST /admin/reindex` — force-rebuild every managed ANN/scalar/FTS
//!   index regardless of its unindexed delta, for index *parameter* changes
//!   (e.g. the IVF partition-count fix) that the delta-driven worker won't
//!   pick up on its own.
//!
//! These endpoints intentionally live outside `admin.rs` (which
//! embeds the admin web SPA — a pure asset surface) so the SPA stays
//! data-agnostic.

use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::{
    app::AppState, error::AppError, storage::lance_store::IndexMaintenanceStats,
    storage::VacuumStats,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/vacuum", post(vacuum))
        .route("/admin/reindex", post(reindex))
}

/// `POST /admin/vacuum` body. Both fields are optional.
///
/// `older_than_days` overrides the configured cutoff for this one
/// call only — useful when an operator wants to reclaim everything
/// right now. `preserve_unverified` (default `false` to match the
/// `aggressive=true` worker default) opts back into Lance's 7-day
/// in-flight safety floor for this one call — set when running
/// alongside another writer on the same lance dir.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct VacuumRequest {
    #[serde(default)]
    older_than_days: Option<i64>,
    #[serde(default)]
    preserve_unverified: bool,
}

async fn vacuum(
    State(app): State<AppState>,
    request: Option<Json<VacuumRequest>>,
) -> Result<Json<VacuumStats>, AppError> {
    let body = request.map(|Json(r)| r).unwrap_or(VacuumRequest {
        older_than_days: None,
        preserve_unverified: false,
    });
    let cutoff = body
        .older_than_days
        .unwrap_or(app.config.vacuum.older_than_days as i64);
    // Body flag wins over config: explicit `preserve_unverified=true`
    // on a single call always restores the 7-day floor for that
    // call, regardless of whether the daemon's worker runs in
    // aggressive mode.
    let aggressive = !body.preserve_unverified && app.config.vacuum.aggressive;
    let stats = app
        .capability_capsule_service
        .vacuum(cutoff, aggressive)
        .await
        .map_err(AppError::from)?;
    Ok(Json(stats))
}

/// `POST /admin/reindex` — force-rebuild every managed index regardless of
/// its unindexed delta. No body. Returns the per-pass index-maintenance
/// stats. Non-Lance backends no-op and return zero-stats.
async fn reindex(State(app): State<AppState>) -> Result<Json<IndexMaintenanceStats>, AppError> {
    let stats = app
        .capability_capsule_service
        .reindex()
        .await
        .map_err(AppError::from)?;
    Ok(Json(stats))
}
