pub mod memory;
pub mod health;
pub mod review;
pub mod graph;

use axum::Router;

use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(memory::router())
        .merge(review::router())
        .merge(graph::router())
}
