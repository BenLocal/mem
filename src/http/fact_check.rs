//! `POST /fact_check` — pre-ingest sanity check against the entity
//! registry + KG. Pure read; never writes. See
//! [`crate::service::fact_check_service`] for the algorithm and
//! `docs/mempalace-diff-v3.md` §5 for the design.

use axum::{extract::State, routing::post, Json, Router};

use crate::{
    app::AppState,
    error::AppError,
    service::{FactCheckError, FactCheckReport, FactCheckRequest},
};

pub fn router() -> Router<AppState> {
    Router::new().route("/fact_check", post(post_fact_check))
}

async fn post_fact_check(
    State(state): State<AppState>,
    Json(req): Json<FactCheckRequest>,
) -> Result<Json<FactCheckReport>, AppError> {
    match state.fact_check_service.check(req).await {
        Ok(report) => Ok(Json(report)),
        Err(FactCheckError::Storage(e)) => Err(AppError::from(e)),
        Err(FactCheckError::Graph(e)) => Err(AppError::from(e)),
    }
}
