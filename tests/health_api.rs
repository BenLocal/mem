use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use tower::util::ServiceExt;

struct TestApp {
    router: axum::Router,
    // RAII: removes the per-test DuckDB tempdir on drop. Even though /health
    // doesn't write to the DB, AppState::from_config still opens the file —
    // sharing ~/.mem/mem.duckdb across parallel test binaries (default path
    // when MEM_DB_PATH is unset) is the same latent brittleness B3 fixed in
    // embeddings_api / ingest_api.
    _temp_dir: tempfile::TempDir,
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
    let temp = tempfile::TempDir::new().expect("tempdir should create");
    let mut cfg = mem::config::Config::local();
    cfg.db_path = temp.path().join("mem.duckdb");
    let router = mem::app::router_with_config(cfg)
        .await
        .expect("app router should build");
    TestApp {
        router,
        _temp_dir: temp,
    }
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let app = test_app().await;
    let response = app.get("/health").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await, "ok");
}
