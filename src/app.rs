use axum::Router;

use crate::http;

pub fn router() -> Router {
    http::router()
}
