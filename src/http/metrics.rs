//! `GET /metrics` — JSON snapshot of the process-wide runtime counters
//! (`crate::metrics`). Generic over the router state (the registry is a global,
//! so no `AppState` is needed) and intentionally unauthenticated / read-only,
//! like `/health`. See `crate::metrics` for what each counter means.

use axum::{routing::get, Json, Router};

use crate::metrics::{metrics, MetricsSnapshot};

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/metrics", get(get_metrics))
}

async fn get_metrics() -> Json<MetricsSnapshot> {
    Json(metrics().snapshot())
}
