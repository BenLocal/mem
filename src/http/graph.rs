use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::{app::AppState, error::AppError};

pub fn router() -> Router<AppState> {
    Router::new().route("/graph/neighbors/{node_id}", get(graph_neighbors))
}

async fn graph_neighbors(
    State(app): State<AppState>,
    Path(node_id): Path<String>,
) -> Result<Json<Vec<crate::domain::memory::GraphEdge>>, AppError> {
    Ok(Json(app.memory_service.graph_neighbors(&node_id).await?))
}
