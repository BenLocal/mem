pub mod admin;
pub mod admin_auth;
pub mod capability_capsule;
pub mod completed_tool_rounds;
pub mod embeddings;
pub mod entities;
pub mod fact_check;
pub mod graph;
pub mod health;
pub mod logging;
pub mod maintenance;
pub mod metrics;
pub mod mine_cursors;
pub mod review;
pub mod skill_proposals;
pub mod skills;
pub mod transcripts;

use axum::{middleware, Router};

use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(capability_capsule::router())
        .merge(completed_tool_rounds::router())
        .merge(embeddings::router())
        .merge(review::router())
        .merge(skill_proposals::router())
        .merge(skills::router())
        .merge(graph::router())
        .merge(transcripts::router())
        .merge(entities::router())
        .merge(fact_check::router())
        .merge(maintenance::router())
        .merge(metrics::router::<AppState>())
        .merge(mine_cursors::router())
        .merge(admin::router())
        .layer(middleware::from_fn(logging::log_request_response))
}
