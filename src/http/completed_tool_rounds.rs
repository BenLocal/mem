use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::{
    app::AppState,
    error::AppError,
    http::admin_auth::AdminAuthorized,
    service::{CompletedToolRoundRead, CompletedToolRoundRebuildReport},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/transcript-rounds/rebuild", post(rebuild_session))
        .route("/admin/transcript-rounds", get(latest_session_rounds))
}

#[derive(Debug, Deserialize)]
struct RebuildRequest {
    tenant: String,
    session_id: String,
    #[serde(default)]
    dry_run: bool,
}

async fn rebuild_session(
    State(state): State<AppState>,
    _admin: AdminAuthorized,
    Json(request): Json<RebuildRequest>,
) -> Result<Json<CompletedToolRoundRebuildReport>, AppError> {
    let report = state
        .completed_tool_round_service
        .rebuild_session(&request.tenant, &request.session_id, request.dry_run)
        .await?;
    Ok(Json(report))
}

#[derive(Debug, Deserialize)]
struct LatestQuery {
    tenant: String,
    session_id: String,
}

async fn latest_session_rounds(
    State(state): State<AppState>,
    _admin: AdminAuthorized,
    Query(query): Query<LatestQuery>,
) -> Result<Json<CompletedToolRoundRead>, AppError> {
    let result = state
        .completed_tool_round_service
        .latest(&query.tenant, &query.session_id)
        .await?;
    Ok(Json(result))
}
