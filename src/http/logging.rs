use std::time::Instant;

use axum::{
    body::{Body, HttpBody},
    extract::Request,
    http::{header, HeaderMap},
    middleware::Next,
    response::Response,
};
use tracing::info;

pub async fn log_request_response(req: Request, next: Next) -> Response {
    let started_at = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let request_size = known_body_size(req.headers(), req.body());
    let res = next.run(req).await;
    let response_size = known_body_size(res.headers(), res.body());

    info!(
        method = %method,
        path = %path,
        status = %res.status(),
        latency_ms = started_at.elapsed().as_millis(),
        request_size_bytes = %request_size
            .map(|size| size.to_string())
            .as_deref()
            .unwrap_or("?"),
        response_size_bytes = %response_size
            .map(|size| size.to_string())
            .as_deref()
            .unwrap_or("?"),
        "http request"
    );

    res
}

fn known_body_size(headers: &HeaderMap, body: &Body) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .or_else(|| body.size_hint().exact())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use axum::{
        body::{Body, Bytes},
        http::{Request, StatusCode},
        middleware,
        response::IntoResponse,
        routing::post,
        Router,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;

    use super::log_request_response;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("capture lock").write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    impl CapturedLogs {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("capture lock").clone())
                .expect("tracing output is utf-8")
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_log_omits_verbatim_payloads_and_keeps_request_metadata() {
        const REQUEST_SECRET: &str = "request-secret-do-not-log";
        const QUERY_SECRET: &str = "query-secret-do-not-log";
        const RESPONSE_SECRET: &str = "response-secret-do-not-log";

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(captured.clone())
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let app = Router::new()
            .route(
                "/capsules",
                post(|| async { (StatusCode::CREATED, RESPONSE_SECRET).into_response() }),
            )
            .layer(middleware::from_fn(log_request_response));
        let request_body = format!(r#"{{"content":"{REQUEST_SECRET}"}}"#);
        let request = Request::builder()
            .method("POST")
            .uri(format!("/capsules?token={QUERY_SECRET}"))
            .header("content-type", "application/json")
            .body(Body::from(request_body))
            .expect("request");

        let response = app.oneshot(request).await.expect("middleware response");
        assert_eq!(response.status(), StatusCode::CREATED);
        let response_body = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        assert_eq!(
            response_body,
            Bytes::from_static(RESPONSE_SECRET.as_bytes())
        );

        let logs = captured.contents();
        assert!(
            !logs.contains(REQUEST_SECRET),
            "request body leaked: {logs}"
        );
        assert!(!logs.contains(QUERY_SECRET), "query string leaked: {logs}");
        assert!(
            !logs.contains(RESPONSE_SECRET),
            "response body leaked: {logs}"
        );
        assert!(logs.contains("method=POST"), "missing method: {logs}");
        assert!(logs.contains("path=/capsules"), "missing path: {logs}");
        assert!(
            logs.contains("status=201 Created"),
            "missing status: {logs}"
        );
        assert!(logs.contains("latency_ms="), "missing latency: {logs}");
        assert!(
            logs.contains("request_size_bytes=39"),
            "missing request size: {logs}"
        );
        assert!(
            logs.contains("response_size_bytes=26"),
            "missing response size: {logs}"
        );
    }
}
