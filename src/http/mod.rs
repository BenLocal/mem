pub mod admin;
pub mod capability_capsule;
pub mod embeddings;
pub mod entities;
pub mod fact_check;
pub mod graph;
pub mod health;
pub mod logging;
pub mod maintenance;
pub mod mine_cursors;
pub mod review;
pub mod transcripts;

use axum::{middleware, Router};

use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(capability_capsule::router())
        .merge(embeddings::router())
        .merge(review::router())
        .merge(graph::router())
        .merge(transcripts::router())
        .merge(entities::router())
        .merge(fact_check::router())
        .merge(maintenance::router())
        .merge(mine_cursors::router())
        .merge(admin::router())
        .layer(middleware::from_fn(logging::log_request_response))
}
