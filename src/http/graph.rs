use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::AppState,
    domain::capability_capsule::{GraphEdge, GraphStats},
    error::AppError,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/graph/neighbors/{node_id}", get(graph_neighbors))
        .route("/graph/timeline/{node_id}", get(graph_timeline))
        .route("/graph/stats", get(graph_stats))
        .route("/graph/tunnels", get(graph_list_user_tunnels))
        .route("/graph/tunnels/find", get(graph_find_tunnels))
        .route("/graph/tunnels/follow/{node_id}", get(graph_follow_tunnels))
        .route("/graph/edges", post(graph_add_edge))
        .route("/graph/edges/invalidate", post(graph_invalidate_edge))
}

#[derive(Debug, Deserialize, Default)]
pub struct NeighborsQuery {
    /// Default 1 (single hop). Capped at 3 storage-side.
    #[serde(default)]
    pub max_hops: Option<u32>,
    /// Lexicographic timestamp (20-digit ms string) — when set, only
    /// edges active at `as_of` are returned (`valid_from <= as_of AND
    /// (valid_to IS NULL OR valid_to > as_of)`). Omit for "active now".
    #[serde(default)]
    pub as_of: Option<String>,
}

async fn graph_neighbors(
    State(app): State<AppState>,
    Path(node_id): Path<String>,
    Query(q): Query<NeighborsQuery>,
) -> Result<Json<Vec<GraphEdge>>, AppError> {
    let edges = match (q.max_hops, q.as_of.as_deref()) {
        (None, None) => {
            app.capability_capsule_service
                .graph_neighbors(&node_id)
                .await?
        }
        (hops, as_of) => {
            app.capability_capsule_service
                .graph_neighbors_within(&node_id, hops.unwrap_or(1), as_of)
                .await?
        }
    };
    Ok(Json(edges))
}

async fn graph_timeline(
    State(app): State<AppState>,
    Path(node_id): Path<String>,
) -> Result<Json<Vec<GraphEdge>>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .graph_timeline(&node_id)
            .await?,
    ))
}

async fn graph_stats(State(app): State<AppState>) -> Result<Json<GraphStats>, AppError> {
    Ok(Json(app.capability_capsule_service.graph_stats().await?))
}

#[derive(Debug, Deserialize, Default)]
pub struct ListUserTunnelsQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

async fn graph_list_user_tunnels(
    State(app): State<AppState>,
    Query(q): Query<ListUserTunnelsQuery>,
) -> Result<Json<Vec<GraphEdge>>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .graph_list_user_tunnels(q.limit.unwrap_or(50))
            .await?,
    ))
}

#[derive(Debug, Deserialize, Default)]
pub struct FindTunnelsQuery {
    #[serde(default)]
    pub prefix_a: Option<String>,
    #[serde(default)]
    pub prefix_b: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

async fn graph_find_tunnels(
    State(app): State<AppState>,
    Query(q): Query<FindTunnelsQuery>,
) -> Result<Json<Vec<GraphEdge>>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .graph_find_tunnels(
                q.prefix_a.as_deref().unwrap_or(""),
                q.prefix_b.as_deref().unwrap_or(""),
                q.limit.unwrap_or(50),
            )
            .await?,
    ))
}

#[derive(Debug, Deserialize, Default)]
pub struct FollowTunnelsQuery {
    #[serde(default)]
    pub max_hops: Option<u32>,
}

async fn graph_follow_tunnels(
    State(app): State<AppState>,
    axum::extract::Path(node_id): axum::extract::Path<String>,
    Query(q): Query<FollowTunnelsQuery>,
) -> Result<Json<Vec<GraphEdge>>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .graph_follow_tunnels(&node_id, q.max_hops.unwrap_or(1))
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct AddEdgeRequest {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    /// Optional caller-supplied `valid_from` (20-digit ms string).
    /// Empty / missing → server stamps `current_timestamp()`.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Optional pre-set `valid_to` — for the rare case where a caller
    /// wants to insert an already-closed historical edge in one shot.
    #[serde(default)]
    pub valid_to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AddEdgeResponse {
    pub written: bool,
}

async fn graph_add_edge(
    State(app): State<AppState>,
    Json(req): Json<AddEdgeRequest>,
) -> Result<Json<AddEdgeResponse>, AppError> {
    let edge = GraphEdge {
        from_node_id: req.from_node_id,
        to_node_id: req.to_node_id,
        relation: req.relation,
        valid_from: req.valid_from.unwrap_or_default(),
        valid_to: req.valid_to,
    };
    let written = app.capability_capsule_service.graph_add_edge(edge).await?;
    Ok(Json(AddEdgeResponse { written }))
}

#[derive(Debug, Deserialize)]
pub struct InvalidateEdgeRequest {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    /// Optional `valid_to` stamp. Empty / missing → server stamps
    /// `current_timestamp()`.
    #[serde(default)]
    pub ended_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InvalidateEdgeResponse {
    pub closed: usize,
}

async fn graph_invalidate_edge(
    State(app): State<AppState>,
    Json(req): Json<InvalidateEdgeRequest>,
) -> Result<Json<InvalidateEdgeResponse>, AppError> {
    let closed = app
        .capability_capsule_service
        .graph_invalidate_edge(
            &req.from_node_id,
            &req.relation,
            &req.to_node_id,
            req.ended_at.as_deref(),
        )
        .await?;
    Ok(Json(InvalidateEdgeResponse { closed }))
}
