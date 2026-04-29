pub mod embeddings;
pub mod graph;
pub mod health;
pub mod memory;
pub mod review;

use axum::{body::Body, http::Request, Router};
use tower_http::trace::TraceLayer;
use tracing::{info, info_span};

use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(memory::router())
        .merge(embeddings::router())
        .merge(review::router())
        .merge(graph::router())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<Body>| {
                    info_span!(
                        "http_request",
                        method = %request.method(),
                        uri = %request.uri(),
                    )
                })
                .on_request(|request: &Request<Body>, _span: &tracing::Span| {
                    info!(
                        method = %request.method(),
                        uri = %request.uri(),
                        "request"
                    );
                })
                .on_response(|response: &axum::http::Response<_>, latency: std::time::Duration, _span: &tracing::Span| {
                    info!(
                        status = %response.status(),
                        latency_ms = latency.as_millis(),
                        "response"
                    );
                }),
        )
}
