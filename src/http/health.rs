use axum::{routing::get, Router};

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/health", get(|| async { "ok" }))
}
