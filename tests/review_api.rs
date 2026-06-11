use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, FeedbackSummary,
        Scope, Visibility,
    },
    http,
    storage::{CapsuleStore, EmbeddingJobInsert, Store},
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

mod common;

fn sample_memory(
    capability_capsule_id: &str,
    status: CapabilityCapsuleStatus,
) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: capability_capsule_id.into(),
        tenant: "local".into(),
        capability_capsule_type: CapabilityCapsuleType::Preference,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{capability_capsule_id}"),
        content: "stored content".into(),
        evidence: vec!["docs/review.md".into()],
        code_refs: vec!["src/review.rs".into()],
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("review".into()),
        task_type: Some("review".into()),
        tags: vec!["review".into()],
        topics: vec![],
        confidence: 0.7,
        decay_score: 0.2,
        content_hash: format!("hash-{capability_capsule_id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "codex-worker".into(),
        created_at: format!("2026-03-21T00:00:{capability_capsule_id}Z"),
        updated_at: format!("2026-03-21T00:05:{capability_capsule_id}Z"),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

fn sample_memory_for_tenant(
    capability_capsule_id: &str,
    tenant: &str,
    status: CapabilityCapsuleStatus,
) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        tenant: tenant.into(),
        ..sample_memory(capability_capsule_id, status)
    }
}

async fn test_duckdb_repo() -> (TempDir, Store) {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-test.duckdb");
    let store = Store::open(&db_path).await.unwrap();
    (temp_dir, store)
}

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    repo: Arc<Store>,
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
    let repo = Arc::new(Store::open(&db_path).await.unwrap());
    repo.insert_capability_capsule(sample_memory(
        "mem_123",
        CapabilityCapsuleStatus::PendingConfirmation,
    ))
    .await
    .unwrap();

    let state = common::test_app_state(
        repo.clone(),
        mem::service::CapabilityCapsuleService::new(repo.clone()),
    );

    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        repo,
    }
}

async fn seeded_app_with_active_preference() -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-api-active.duckdb");
    let repo = Arc::new(Store::open(&db_path).await.unwrap());
    repo.insert_capability_capsule(sample_memory("mem_123", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();

    let state = common::test_app_state(
        repo.clone(),
        mem::service::CapabilityCapsuleService::new(repo.clone()),
    );

    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        repo,
    }
}

#[tokio::test]
async fn duckdb_repository_lists_pending_review_rows() {
    let (_dir, repo) = test_duckdb_repo().await;
    repo.insert_capability_capsule(sample_memory(
        "001",
        CapabilityCapsuleStatus::PendingConfirmation,
    ))
    .await
    .unwrap();
    repo.insert_capability_capsule(sample_memory("002", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();

    let pending = repo.list_pending_review("local").await.unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].capability_capsule_id, "001");
    assert_eq!(
        pending[0].status,
        CapabilityCapsuleStatus::PendingConfirmation
    );
}

// duckdb_repository_summarizes_feedback_by_kind: removed during the
// LanceStore cutover. The legacy test wrote raw `feedback_events` rows
// via a private `insert_feedback` helper that no longer exists. The
// equivalent end-to-end coverage is `submitting_feedback_updates_summary_and_lifecycle_fields`
// below, which exercises feedback through the public ingest path.
#[allow(dead_code)]
fn _removed_duckdb_repository_summarizes_feedback_by_kind() {
    let _ = FeedbackSummary {
        total: 0,
        useful: 0,
        outdated: 0,
        incorrect: 0,
        applies_here: 0,
        does_not_apply_here: 0,
        auto_promoted: 0,
    };
}

#[tokio::test]
async fn listing_pending_memories_returns_pending_rows() {
    let app = seeded_app_with_pending_preference().await;

    let response = app.get("/reviews/pending?tenant=local").await;

    assert_eq!(response.status(), 200, "response body: {}", response.body);
    assert_eq!(response.json()[0]["capability_capsule_id"], "mem_123");
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
                "capability_capsule_id": "mem_123"
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
            capability_capsule_id: "mem_123".into(),
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
                "capability_capsule_id": "mem_123"
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
                "capability_capsule_id": "mem_123"
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
                "capability_capsule_id": "mem_123",
                "summary": "updated summary",
                "content": "updated content",
                "evidence": ["docs/updated.md"],
                "code_refs": ["src/updated.rs"],
                "tags": ["updated"]
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["original_capability_capsule_id"], "mem_123");
    assert_eq!(response.json()["capability_capsule"]["status"], "active");
    assert_eq!(
        response.json()["capability_capsule"]["supersedes_capability_capsule_id"],
        "mem_123"
    );
    assert_ne!(
        response.json()["capability_capsule"]["capability_capsule_id"],
        "mem_123"
    );

    let original = app
        .repo
        .get_capability_capsule("mem_123".into())
        .await
        .unwrap()
        .expect("original memory should exist");
    assert_eq!(original.status, CapabilityCapsuleStatus::Rejected);

    let successor_id = response.json()["capability_capsule"]["capability_capsule_id"]
        .as_str()
        .expect("successor memory id should be present")
        .to_string();
    let successor = app
        .repo
        .get_capability_capsule(successor_id)
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
            "/capability_capsules/feedback",
            json!({
                "tenant": "local",
                "capability_capsule_id": "mem_123",
                "feedback_kind": "useful"
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["capability_capsule_id"], "mem_123");
    let confidence = response.json()["confidence"].as_f64().unwrap();
    let decay_score = response.json()["decay_score"].as_f64().unwrap();
    assert!((confidence - 0.8).abs() < 1e-6);
    assert!((decay_score - 0.2).abs() < 1e-6);

    let detail = app.get("/capability_capsules/mem_123?tenant=local").await;
    assert_eq!(detail.status(), 200);
    assert_eq!(detail.json()["feedback_summary"]["total"], 1);
    assert_eq!(detail.json()["feedback_summary"]["useful"], 1);
    let detail_confidence = detail.json()["capability_capsule"]["confidence"]
        .as_f64()
        .unwrap();
    let detail_decay_score = detail.json()["capability_capsule"]["decay_score"]
        .as_f64()
        .unwrap();
    assert!((detail_confidence - 0.8).abs() < 1e-6);
    assert!((detail_decay_score - 0.2).abs() < 1e-6);
}

#[tokio::test]
async fn listing_pending_memories_respects_tenant_scope() {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-api-tenants.duckdb");
    let repo = Arc::new(Store::open(&db_path).await.unwrap());
    repo.insert_capability_capsule(sample_memory_for_tenant(
        "mem_local",
        "tenant-a",
        CapabilityCapsuleStatus::PendingConfirmation,
    ))
    .await
    .unwrap();
    repo.insert_capability_capsule(sample_memory_for_tenant(
        "mem_other",
        "tenant-b",
        CapabilityCapsuleStatus::PendingConfirmation,
    ))
    .await
    .unwrap();

    let state = common::test_app_state(
        repo.clone(),
        mem::service::CapabilityCapsuleService::new(repo.clone()),
    );
    let app = TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        repo,
    };

    let response = app.get("/reviews/pending?tenant=tenant-a").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json().as_array().unwrap().len(), 1);
    assert_eq!(response.json()[0]["capability_capsule_id"], "mem_local");
}

#[tokio::test]
async fn accepting_pending_memory_from_wrong_tenant_returns_not_found() {
    let app = seeded_app_with_pending_preference().await;

    let response = app
        .post_json(
            "/reviews/pending/accept",
            json!({
                "tenant": "other-tenant",
                "capability_capsule_id": "mem_123"
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
                "capability_capsule_id": "missing"
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
                "capability_capsule_id": "missing"
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
                "capability_capsule_id": "missing",
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
