pub mod memory;
pub mod health;

use axum::Router;

pub fn router() -> Router {
    let app_state = memory::AppState::local();

    Router::new()
        .merge(health::router())
        .merge(memory::router().with_state(app_state))
}
