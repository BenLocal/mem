pub mod embeddings;
pub mod graph;
pub mod health;
pub mod memory;
pub mod review;

use axum::Router;

use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(memory::router())
        .merge(embeddings::router())
        .merge(review::router())
        .merge(graph::router())
}
