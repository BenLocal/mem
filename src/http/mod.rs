pub mod embeddings;
pub mod graph;
pub mod health;
pub mod logging;
pub mod memory;
pub mod review;

use axum::{middleware, Router};

use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(memory::router())
        .merge(embeddings::router())
        .merge(review::router())
        .merge(graph::router())
        .layer(middleware::from_fn(logging::log_request_response))
}
