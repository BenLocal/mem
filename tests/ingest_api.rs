use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    app,
    domain::memory::{IngestMemoryRequest, MemoryStatus, MemoryType, Scope, Visibility, WriteMode},
    pipeline::ingest::{compute_content_hash, initial_status},
    storage::DuckDbRepository,
};
use serde_json::{json, Value};
use tempfile::tempdir;
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
        router: app::router().await.expect("app router should build"),
    }
}

#[test]
fn initial_status_routes_memory_types_as_expected() {
    assert_eq!(
        initial_status(&MemoryType::Preference, &WriteMode::Propose),
        MemoryStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&MemoryType::Workflow, &WriteMode::Auto),
        MemoryStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&MemoryType::Implementation, &WriteMode::Auto),
        MemoryStatus::Active
    );
}

#[test]
fn content_hash_is_deterministic_for_same_request() {
    let request = IngestMemoryRequest {
        tenant: "tenant-a".into(),
        memory_type: MemoryType::Implementation,
        content: "invalidate cache when schema changes".into(),
        evidence: vec!["notes".into()],
        code_refs: vec!["src/cache.rs".into()],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("cache".into()),
        task_type: Some("bugfix".into()),
        tags: vec!["cache".into()],
        source_agent: "api".into(),
        idempotency_key: Some("idem".into()),
        write_mode: WriteMode::Auto,
    };

    assert_eq!(compute_content_hash(&request), compute_content_hash(&request));
}

#[tokio::test]
async fn repository_dedupe_lookup_prefers_same_tenant_memory() {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("ingest-dedupe.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();

    let make_memory = |tenant: &str, memory_id: &str| mem::domain::memory::MemoryRecord {
        memory_id: memory_id.into(),
        tenant: tenant.into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "same content".into(),
        content: "same content".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: "hash-123".into(),
        idempotency_key: Some("idem-123".into()),
        supersedes_memory_id: None,
        source_agent: "api".into(),
        created_at: "1".into(),
        updated_at: "1".into(),
        last_validated_at: None,
    };

    repo.insert_memory(make_memory("tenant-a", "mem-a"))
        .await
        .unwrap();
    repo.insert_memory(make_memory("tenant-b", "mem-b"))
        .await
        .unwrap();

    let found = repo
        .find_by_idempotency_or_hash("tenant-b", &Some("idem-123".into()), "hash-123")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(found.memory_id, "mem-b");
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
