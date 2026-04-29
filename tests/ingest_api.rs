use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    app::{self, AppState},
    domain::memory::{
        IngestMemoryRequest, MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility, WriteMode,
    },
    http,
    pipeline::ingest::{compute_content_hash, initial_status},
    storage::DuckDbRepository,
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

struct TestApp {
    _temp_dir: Option<TempDir>,
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
        _temp_dir: None,
        router: app::router().await.expect("app router should build"),
    }
}

fn sample_memory(
    memory_id: &str,
    tenant: &str,
    version: u64,
    supersedes_memory_id: Option<&str>,
) -> MemoryRecord {
    MemoryRecord {
        memory_id: memory_id.into(),
        tenant: tenant.into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version,
        summary: format!("summary-{memory_id}"),
        content: format!("content-{memory_id}"),
        evidence: vec![format!("evidence-{memory_id}")],
        code_refs: vec![format!("src/{memory_id}.rs")],
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("memory".into()),
        task_type: Some("implementation".into()),
        tags: vec![format!("tag-{memory_id}")],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: format!("hash-{memory_id}"),
        idempotency_key: None,
        supersedes_memory_id: supersedes_memory_id.map(str::to_string),
        source_agent: "api".into(),
        created_at: format!("2026-03-21T00:00:0{version}Z"),
        updated_at: format!("2026-03-21T00:05:0{version}Z"),
        last_validated_at: None,
    }
}

async fn seeded_app(memories: Vec<MemoryRecord>) -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("ingest-api.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    for memory in memories {
        repo.insert_memory(memory).await.unwrap();
    }

    let state = AppState {
        memory_service: mem::service::MemoryService::new(repo),
        config: mem::config::Config::local(),
    };

    TestApp {
        _temp_dir: Some(temp_dir),
        router: http::router().with_state(state),
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
        summary: None,
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

    let hash = compute_content_hash(&request);
    assert_eq!(hash, compute_content_hash(&request));
    // Cross-process stability canary: literal sha256 of the canonical JSON
    // for this exact fixture. Any change here means the hash function changed
    // and existing palaces will need re-migration. Update only with a matching
    // migration story.
    assert_eq!(
        hash, "5de20b10dba9788355a360cc8c7631c8f9f2df1a7b9afc807e2877923fbcc7d6",
        "content_hash drifted — old palaces will silently fail dedupe"
    );
    assert_eq!(hash.len(), 64, "expected sha256 hex, got: {hash}");
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

#[tokio::test]
async fn get_memory_returns_full_record() {
    let app = test_app().await;
    let created = app
        .post_json(
            "/memories",
            json!({
                "memory_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo",
                "write_mode": "auto",
                "idempotency_key": "detail-lookup"
            }),
        )
        .await;

    assert_eq!(created.status(), 201);
    let memory_id = created.json()["memory_id"]
        .as_str()
        .expect("memory id should be present")
        .to_string();

    let response = app.get(&format!("/memories/{memory_id}")).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["memory"]["memory_id"], memory_id);
    assert_eq!(
        response.json()["memory"]["content"],
        "invalidate cache when schema changes"
    );
    assert!(response.json()["memory"]["content_hash"].is_string());
    assert!(response.json()["version_chain"].is_array());
    assert_eq!(response.json()["graph_links"], json!([]));
    assert_eq!(response.json()["feedback_summary"]["total"], 0);
}

#[tokio::test]
async fn get_memory_returns_not_found_for_missing_memory() {
    let app = test_app().await;

    let response = app.get("/memories/missing").await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}

#[tokio::test]
async fn get_memory_returns_not_found_for_wrong_tenant() {
    let app = seeded_app(vec![sample_memory("mem_123", "tenant-a", 1, None)]).await;

    let response = app.get("/memories/mem_123?tenant=tenant-b").await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}

#[tokio::test]
async fn get_memory_defaults_tenant_to_local() {
    let app = seeded_app(vec![sample_memory("mem_123", "tenant-a", 1, None)]).await;

    let response = app.get("/memories/mem_123").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["memory"]["memory_id"], "mem_123");
    assert_eq!(response.json()["memory"]["tenant"], "tenant-a");
}

#[tokio::test]
async fn get_memory_returns_full_version_chain_for_successor_ids() {
    let app = seeded_app(vec![
        sample_memory("mem_v1", "local", 1, None),
        sample_memory("mem_v2", "local", 2, Some("mem_v1")),
        sample_memory("mem_v3", "local", 3, Some("mem_v2")),
    ])
    .await;

    let response = app.get("/memories/mem_v2?tenant=local").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["memory"]["memory_id"], "mem_v2");
    assert_eq!(
        response.json()["version_chain"],
        json!([
            {
                "memory_id": "mem_v3",
                "version": 3,
                "status": "active",
                "updated_at": "2026-03-21T00:05:03Z",
                "supersedes_memory_id": "mem_v2"
            },
            {
                "memory_id": "mem_v2",
                "version": 2,
                "status": "active",
                "updated_at": "2026-03-21T00:05:02Z",
                "supersedes_memory_id": "mem_v1"
            },
            {
                "memory_id": "mem_v1",
                "version": 1,
                "status": "active",
                "updated_at": "2026-03-21T00:05:01Z"
            }
        ])
    );
}
