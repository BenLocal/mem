use axum::{
    body::{Body, Bytes},
    extract::Request,
    middleware::Next,
    response::Response,
};
use http_body_util::BodyExt;
use tracing::info;

pub async fn log_request_response(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = buffer_body(body).await;

    info!(
        method = %parts.method,
        uri = %parts.uri,
        body = %String::from_utf8_lossy(&bytes),
        "request"
    );

    let req = Request::from_parts(parts, Body::from(bytes));
    let res = next.run(req).await;

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
