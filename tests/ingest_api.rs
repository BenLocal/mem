use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use tower::util::ServiceExt;

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

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("body should be valid json")
    }
}

impl TestApp {
    async fn post_json(&self, path: &str, body: Value) -> TestResponse {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
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
        router: mem::app::router(),
    }
}

#[tokio::test]
async fn preference_memory_stays_pending_confirmation() {
    let app = test_app().await;
    let response = app
        .post_json(
            "/memories",
            json!({
                "memory_type": "preference",
                "content": "prefer concise answers",
                "scope": "global",
                "write_mode": "propose"
            }),
        )
        .await;

    assert_eq!(response.status(), 201);
    assert_eq!(response.json()["status"], "pending_confirmation");
}

#[tokio::test]
async fn implementation_memory_auto_activates() {
    let app = test_app().await;
    let response = app
        .post_json(
            "/memories",
            json!({
                "memory_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo",
                "write_mode": "auto"
            }),
        )
        .await;

    assert_eq!(response.status(), 201);
    assert_eq!(response.json()["status"], "active");
}

#[tokio::test]
async fn repeated_ingest_with_same_idempotency_key_returns_existing_memory() {
    let app = test_app().await;
    let request = json!({
        "memory_type": "implementation",
        "content": "invalidate cache when schema changes",
        "scope": "repo",
        "write_mode": "auto",
        "idempotency_key": "idem-123"
    });

    let first = app.post_json("/memories", request.clone()).await;
    let second = app.post_json("/memories", request).await;

    assert_eq!(first.status(), 201);
    assert_eq!(second.status(), 201);
    assert_eq!(first.json()["memory_id"], second.json()["memory_id"]);
    assert_eq!(second.json()["status"], "active");
}

#[tokio::test]
async fn same_idempotency_key_in_different_tenants_creates_distinct_memories() {
    let app = test_app().await;

    let first = app
        .post_json(
            "/memories",
            json!({
                "tenant": "tenant-a",
                "memory_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo",
                "write_mode": "auto",
                "idempotency_key": "idem-shared"
            }),
        )
        .await;

    let second = app
        .post_json(
            "/memories",
            json!({
                "tenant": "tenant-b",
                "memory_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo",
                "write_mode": "auto",
                "idempotency_key": "idem-shared"
            }),
        )
        .await;

    assert_eq!(first.status(), 201);
    assert_eq!(second.status(), 201);
    assert_ne!(first.json()["memory_id"], second.json()["memory_id"]);
}
