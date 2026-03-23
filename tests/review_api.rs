use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    app::AppState,
    domain::memory::{FeedbackSummary, MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility},
    http,
    storage::duckdb::{DuckDbRepository, EmbeddingJobInsert},
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

fn sample_memory(memory_id: &str, status: MemoryStatus) -> MemoryRecord {
    MemoryRecord {
        memory_id: memory_id.into(),
        tenant: "local".into(),
        memory_type: MemoryType::Preference,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{memory_id}"),
        content: "stored content".into(),
        evidence: vec!["docs/review.md".into()],
        code_refs: vec!["src/review.rs".into()],
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("review".into()),
        task_type: Some("review".into()),
        tags: vec!["review".into()],
        confidence: 0.7,
        decay_score: 0.2,
        content_hash: format!("hash-{memory_id}"),
        idempotency_key: None,
        supersedes_memory_id: None,
        source_agent: "codex-worker".into(),
        created_at: format!("2026-03-21T00:00:{memory_id}Z"),
        updated_at: format!("2026-03-21T00:05:{memory_id}Z"),
        last_validated_at: None,
    }
}

fn sample_memory_for_tenant(memory_id: &str, tenant: &str, status: MemoryStatus) -> MemoryRecord {
    MemoryRecord {
        tenant: tenant.into(),
        ..sample_memory(memory_id, status)
    }
}

async fn test_duckdb_repo() -> DuckDbRepository {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-test.duckdb");
    DuckDbRepository::open(&db_path).await.unwrap()
}

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    repo: DuckDbRepository,
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

async fn seeded_app_with_pending_preference() -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-api.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    repo.insert_memory(sample_memory("mem_123", MemoryStatus::PendingConfirmation))
        .await
        .unwrap();

    let state = AppState {
        memory_service: mem::service::MemoryService::new(repo.clone()),
        config: mem::config::Config::local(),
    };

    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        repo,
    }
}

async fn seeded_app_with_active_preference() -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-api-active.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    repo.insert_memory(sample_memory("mem_123", MemoryStatus::Active))
        .await
        .unwrap();

    let state = AppState {
        memory_service: mem::service::MemoryService::new(repo.clone()),
        config: mem::config::Config::local(),
    };

    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        repo,
    }
}

#[tokio::test]
async fn duckdb_repository_lists_pending_review_rows() {
    let repo = test_duckdb_repo().await;
    repo.insert_memory(sample_memory("001", MemoryStatus::PendingConfirmation))
        .await
        .unwrap();
    repo.insert_memory(sample_memory("002", MemoryStatus::Active))
        .await
        .unwrap();

    let pending = repo.list_pending_review("local").await.unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].memory_id, "001");
    assert_eq!(pending[0].status, MemoryStatus::PendingConfirmation);
}

#[tokio::test]
async fn duckdb_repository_summarizes_feedback_by_kind() {
    let repo = test_duckdb_repo().await;
    repo.insert_feedback(mem::storage::FeedbackEvent {
        feedback_id: "fb_001".into(),
        memory_id: "mem_123".into(),
        feedback_kind: "useful".into(),
        created_at: "2026-03-21T00:00:01Z".into(),
    })
    .await
    .unwrap();
    repo.insert_feedback(mem::storage::FeedbackEvent {
        feedback_id: "fb_002".into(),
        memory_id: "mem_123".into(),
        feedback_kind: "outdated".into(),
        created_at: "2026-03-21T00:00:02Z".into(),
    })
    .await
    .unwrap();

    let summary = repo.feedback_summary("mem_123").await.unwrap();

    assert_eq!(
        summary,
        FeedbackSummary {
            total: 2,
            useful: 1,
            outdated: 1,
            incorrect: 0,
            applies_here: 0,
            does_not_apply_here: 0,
        }
    );
}

#[tokio::test]
async fn listing_pending_memories_returns_pending_rows() {
    let app = seeded_app_with_pending_preference().await;

    let response = app.get("/reviews/pending?tenant=local").await;

    assert_eq!(response.status(), 200, "response body: {}", response.body);
    assert_eq!(response.json()[0]["memory_id"], "mem_123");
    assert_eq!(response.json()[0]["status"], "pending_confirmation");
}

#[tokio::test]
async fn accepting_pending_memory_marks_it_active() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/accept",
            json!({
                "tenant": "local",
                "memory_id": "mem_123"
            }),
        )
        .await;

    assert_eq!(response.status(), 200, "response body: {}", response.body);
    assert_eq!(response.json()["status"], "active");
}

#[tokio::test]
async fn accepting_pending_memory_with_embedding_job_marks_it_active() {
    let app = seeded_app_with_pending_preference().await;
    let now = "2026-03-21T00:00:10Z".to_string();
    let enqueued = app
        .repo
        .try_enqueue_embedding_job(EmbeddingJobInsert {
            job_id: "ej_00000000000000000001".into(),
            tenant: "local".into(),
            memory_id: "mem_123".into(),
            target_content_hash: "hash-mem_123".into(),
            provider: "fake".into(),
            available_at: now.clone(),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .expect("embedding job insert should succeed");
    assert!(enqueued);

    let response = app
        .post_json(
            "/reviews/pending/accept",
            json!({
                "tenant": "local",
                "memory_id": "mem_123"
            }),
        )
        .await;

    assert_eq!(response.status(), 200, "response body: {}", response.body);
    assert_eq!(response.json()["status"], "active");
}

#[tokio::test]
async fn rejecting_pending_memory_marks_it_rejected() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/reject",
            json!({
                "tenant": "local",
                "memory_id": "mem_123"
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["status"], "rejected");
}

#[tokio::test]
async fn editing_pending_memory_rejects_original_and_creates_active_successor() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/edit_accept",
            json!({
                "tenant": "local",
                "memory_id": "mem_123",
                "summary": "updated summary",
                "content": "updated content",
                "evidence": ["docs/updated.md"],
                "code_refs": ["src/updated.rs"],
                "tags": ["updated"]
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["original_memory_id"], "mem_123");
    assert_eq!(response.json()["memory"]["status"], "active");
    assert_eq!(response.json()["memory"]["supersedes_memory_id"], "mem_123");
    assert_ne!(response.json()["memory"]["memory_id"], "mem_123");

    let original = app
        .repo
        .get_memory("mem_123".into())
        .await
        .unwrap()
        .expect("original memory should exist");
    assert_eq!(original.status, MemoryStatus::Rejected);

    let successor_id = response.json()["memory"]["memory_id"]
        .as_str()
        .expect("successor memory id should be present")
        .to_string();
    let successor = app
        .repo
        .get_memory(successor_id)
        .await
        .unwrap()
        .expect("successor memory should exist");
    assert_eq!(successor.version, 2);
    assert_eq!(successor.tenant, "local");
    assert_eq!(successor.scope, Scope::Repo);
    assert_eq!(successor.visibility, Visibility::Shared);
    assert_eq!(successor.project.as_deref(), Some("memory-service"));
    assert_eq!(successor.repo.as_deref(), Some("mem"));
    assert_eq!(successor.module.as_deref(), Some("review"));
}

#[tokio::test]
async fn submitting_feedback_updates_summary_and_lifecycle_fields() {
    let app = seeded_app_with_active_preference().await;

    let response = app
        .post_json(
            "/memories/feedback",
            json!({
                "tenant": "local",
                "memory_id": "mem_123",
                "feedback_kind": "useful"
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["memory_id"], "mem_123");
    let confidence = response.json()["confidence"].as_f64().unwrap();
    let decay_score = response.json()["decay_score"].as_f64().unwrap();
    assert!((confidence - 0.8).abs() < 1e-6);
    assert!((decay_score - 0.2).abs() < 1e-6);

    let detail = app.get("/memories/mem_123?tenant=local").await;
    assert_eq!(detail.status(), 200);
    assert_eq!(detail.json()["feedback_summary"]["total"], 1);
    assert_eq!(detail.json()["feedback_summary"]["useful"], 1);
    let detail_confidence = detail.json()["memory"]["confidence"].as_f64().unwrap();
    let detail_decay_score = detail.json()["memory"]["decay_score"].as_f64().unwrap();
    assert!((detail_confidence - 0.8).abs() < 1e-6);
    assert!((detail_decay_score - 0.2).abs() < 1e-6);
}

#[tokio::test]
async fn listing_pending_memories_respects_tenant_scope() {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-api-tenants.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    repo.insert_memory(sample_memory_for_tenant(
        "mem_local",
        "tenant-a",
        MemoryStatus::PendingConfirmation,
    ))
    .await
    .unwrap();
    repo.insert_memory(sample_memory_for_tenant(
        "mem_other",
        "tenant-b",
        MemoryStatus::PendingConfirmation,
    ))
    .await
    .unwrap();

    let state = AppState {
        memory_service: mem::service::MemoryService::new(repo.clone()),
        config: mem::config::Config::local(),
    };
    let app = TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        repo,
    };

    let response = app.get("/reviews/pending?tenant=tenant-a").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json().as_array().unwrap().len(), 1);
    assert_eq!(response.json()[0]["memory_id"], "mem_local");
}

#[tokio::test]
async fn accepting_pending_memory_from_wrong_tenant_returns_not_found() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/accept",
            json!({
                "tenant": "other-tenant",
                "memory_id": "mem_123"
            }),
        )
        .await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}

#[tokio::test]
async fn accepting_unknown_pending_memory_returns_not_found() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/accept",
            json!({
                "tenant": "local",
                "memory_id": "missing"
            }),
        )
        .await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}

#[tokio::test]
async fn rejecting_unknown_pending_memory_returns_not_found() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/reject",
            json!({
                "tenant": "local",
                "memory_id": "missing"
            }),
        )
        .await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}

#[tokio::test]
async fn editing_unknown_pending_memory_returns_not_found() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/edit_accept",
            json!({
                "tenant": "local",
                "memory_id": "missing",
                "summary": "updated summary",
                "content": "updated content",
                "evidence": ["docs/updated.md"],
                "code_refs": ["src/updated.rs"],
                "tags": ["updated"]
            }),
        )
        .await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}
