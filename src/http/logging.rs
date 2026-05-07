use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::header,
    middleware::Next,
    response::Response,
};
use http_body_util::BodyExt;
use tracing::info;

/// Paths whose request/response bodies are noise (static assets served by the
/// admin web page, plus implicit favicon requests). For these we still log a
/// status + size one-liner — just skip dumping ~14 KB of CSS / JS into the
/// log.
fn is_static_asset(path: &str) -> bool {
    matches!(path, "/" | "/admin" | "/favicon.ico") || path.starts_with("/web/")
}

pub async fn log_request_response(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let skip_body = is_static_asset(&path);

    let (parts, body) = req.into_parts();
    let bytes = buffer_body(body).await;

    if skip_body {
        info!(
            method = %parts.method,
            uri = %parts.uri,
            "request (static)"
        );
    } else {
        info!(
            method = %parts.method,
            uri = %parts.uri,
            body = %String::from_utf8_lossy(&bytes),
            "request"
        );
    }

    let req = Request::from_parts(parts, Body::from(bytes));
    let res = next.run(req).await;

    if skip_body {
        // Don't read the response body — pass it through untouched. Log
        // status + content-length so latency / size tracking still works.
        let size = res
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        info!(
            status = %res.status(),
            content_length = size.as_deref().unwrap_or("?"),
            "response (static)"
        );
        return res;
    }

    let (parts, body) = res.into_parts();
    let bytes = buffer_body(body).await;

    info!(
        status = %parts.status,
        body = %String::from_utf8_lossy(&bytes),
        "response"
    );

    Response::from_parts(parts, Body::from(bytes))
}

async fn buffer_body(body: Body) -> Bytes {
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default()
}
