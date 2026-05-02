use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::app;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::util::ServiceExt;

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
}

struct TestResponse {
    status: StatusCode,
    body: String,
}

impl TestResponse {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("body should be valid json")
    }
}

impl TestApp {
    async fn new() -> Self {
        Self::new_with_provider(mem::config::EmbeddingProviderKind::Fake).await
    }

    async fn new_with_provider(provider: mem::config::EmbeddingProviderKind) -> Self {
        let temp_dir = TempDir::new().expect("tempdir should create");
        let mut cfg = mem::config::Config::local();
        cfg.db_path = temp_dir.path().join("mem.duckdb");
        cfg.embedding.provider = provider;
        if matches!(provider, mem::config::EmbeddingProviderKind::Fake) {
            cfg.embedding.model = "fake".to_string();
            cfg.embedding.dim = 64;
        }
        let router = app::router_with_config(cfg)
            .await
            .expect("app router should build");
        Self {
            _temp_dir: temp_dir,
            router,
        }
    }

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
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        TestResponse {
            status,
            body: String::from_utf8(bytes.to_vec()).expect("body should be utf-8"),
        }
    }
}

#[tokio::test]
async fn embeddings_providers_describes_fake_backend() {
    // Pin to Fake explicitly so the assertion stays stable independent of
    // whatever `Config::local()` defaults to (currently EmbedAnything since
    // commit 47aff1e). Tests the API contract for the Fake provider.
    let app = TestApp::new_with_provider(mem::config::EmbeddingProviderKind::Fake).await;
    let res = app.get("/embeddings/providers").await;
    assert_eq!(res.status, StatusCode::OK);
    let j = res.json();
    assert_eq!(j["provider"], "fake");
    assert_eq!(j["model"], "fake");
    assert!(j["dimension"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn memory_detail_includes_embedding_meta_after_ingest() {
    let app = TestApp::new().await;
    let ingest = app
        .post_json(
            "/memories",
            json!({
                "tenant": "local",
                "memory_type": "implementation",
                "content": "embedding meta api test",
                "evidence": [],
                "code_refs": [],
                "scope": "repo",
                "visibility": "shared",
                "project": "p",
                "repo": "r",
                "module": "m",
                "tags": [],
                "source_agent": "test",
                "write_mode": "auto"
            }),
        )
        .await;
    assert_eq!(ingest.status, StatusCode::CREATED);
    let ingest_json = ingest.json();
    let memory_id = ingest_json["memory_id"].as_str().unwrap().to_string();

    let detail = app
        .get(&format!("/memories/{memory_id}?tenant=local"))
        .await;
    assert_eq!(detail.status, StatusCode::OK);
    let j = detail.json();
    assert_eq!(j["embedding"]["status"], "pending");

    let jobs = app.get("/embeddings/jobs?tenant=local").await;
    assert_eq!(jobs.status, StatusCode::OK);
    let jobs_json = jobs.json();
    let arr = jobs_json.as_array().expect("jobs array");
    assert!(
        arr.iter()
            .any(|row| row["memory_id"].as_str() == Some(memory_id.as_str())),
        "expected a job for ingested memory"
    );
}

#[tokio::test]
async fn embeddings_rebuild_enqueues_when_forced() {
    let app = TestApp::new().await;
    let ingest = app
        .post_json(
            "/memories",
            json!({
                "tenant": "local",
                "memory_type": "implementation",
                "content": "rebuild force test",
                "evidence": [],
                "code_refs": [],
                "scope": "repo",
                "visibility": "shared",
                "project": "p",
                "repo": "r",
                "module": "m",
                "tags": [],
                "source_agent": "test",
                "write_mode": "auto"
            }),
        )
        .await;
    assert_eq!(ingest.status, StatusCode::CREATED);
    let ingest_json = ingest.json();
    let memory_id = ingest_json["memory_id"].as_str().unwrap().to_string();

    let rebuild = app
        .post_json(
            "/embeddings/rebuild",
            json!({
                "tenant": "local",
                "memory_ids": [&memory_id],
                "force": true
            }),
        )
        .await;
    assert_eq!(rebuild.status, StatusCode::OK);
    assert_eq!(rebuild.json()["enqueued"], 1);
}
