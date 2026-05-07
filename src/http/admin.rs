//! Admin web page (the "archive") embedded into the binary via `rust-embed`.
//!
//! Routes:
//! - `GET /` and `GET /admin`  → `index.html`
//! - `GET /web/{path}`         → any asset under `src/web/`
//!
//! All data flows through the existing JSON HTTP API
//! (`GET /memories?tenant=…`, `POST /memories/feedback`,
//! `POST /memories/search`). No bespoke admin endpoints.
//!
//! "Delete" semantics: the page POSTs `feedback_kind: "incorrect"`, which
//! transitions the memory's status to `Archived`. mem has no hard delete by
//! design (mempalace-diff §6 lifecycle / §7 verbatim discipline).

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::Embed;

use crate::app::AppState;

#[derive(Embed)]
#[folder = "src/web/"]
struct WebAsset;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(serve_index))
        .route("/admin", get(serve_index))
        .route("/web/{*path}", get(serve_asset))
}

async fn serve_index() -> Response {
    asset_response("index.html")
}

async fn serve_asset(Path(path): Path<String>) -> Response {
    // Reject path traversal even though rust-embed only sees `src/web/`.
    if path.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    asset_response(&path)
}

/// Resolve an embedded asset, attach a content-type derived from the file
/// extension, and return the response body. 404 on miss.
fn asset_response(path: &str) -> Response {
    match WebAsset::get(path) {
        Some(file) => {
            let mime = mime_for(path);
            ([(header::CONTENT_TYPE, mime)], file.data.into_owned()).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Hardcoded extension → MIME map. Keeps the binary off the `mime_guess`
/// dependency tree; the `src/web/` folder only ships a small, fixed set of
/// asset kinds.
fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
