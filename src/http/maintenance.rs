//! Operator-driven maintenance endpoints. Currently just one:
//! `POST /admin/vacuum` to trigger an immediate Lance manifest prune
//! across every managed table without waiting for the daily worker
//! tick. Same backend as `crate::worker::vacuum_worker::sweep_once`.
//!
//! These endpoints intentionally live outside `admin.rs` (which
//! embeds the admin web SPA — a pure asset surface) so the SPA stays
//! data-agnostic.

use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::{app::AppState, error::AppError, storage::VacuumStats};

pub fn router() -> Router<AppState> {
    Router::new().route("/admin/vacuum", post(vacuum))
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
