use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use tower::util::ServiceExt;

#[path = "../src/app.rs"]
mod app;

#[path = "../src/http/mod.rs"]
mod http;

struct TestApp {
    router: axum::Router,
}

struct TestResponse {
    status: StatusCode,
    body: String,
}

impl TestResponse {
    fn status(&self) -> u16 {
        self.status.as_u16()
    }

    async fn text(self) -> String {
        self.body
    }
}

impl TestApp {
    async fn get(&self, path: &str) -> TestResponse {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("request should build");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("request should succeed");
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        TestResponse {
            status,
            body: String::from_utf8(body.to_vec()).expect("body should be utf-8"),
        }
    }
}

async fn test_app() -> TestApp {
    TestApp {
        router: app::router(),
    }
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let app = test_app().await;
    let response = app.get("/health").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await, "ok");
}
