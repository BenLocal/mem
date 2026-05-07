pub mod admin;
pub mod embeddings;
pub mod entities;
pub mod graph;
pub mod health;
pub mod logging;
pub mod memory;
pub mod review;
pub mod transcripts;

use axum::{middleware, Router};

use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(memory::router())
        .merge(embeddings::router())
        .merge(review::router())
        .merge(graph::router())
        .merge(transcripts::router())
        .merge(entities::router())
        .merge(admin::router())
        .layer(middleware::from_fn(logging::log_request_response))
}
