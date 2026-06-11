use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    app,
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
        IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
    },
    http,
    pipeline::ingest::{compute_content_hash, initial_status},
    storage::Store,
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

mod common;

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
    let temp_dir = tempdir().expect("tempdir should create");
    let mut cfg = mem::config::Config::local();
    cfg.db_path = temp_dir.path().join("mem.duckdb");
    let router = app::router_with_config(cfg)
        .await
        .expect("app router should build");
    TestApp {
        _temp_dir: Some(temp_dir),
        router,
    }
}

fn sample_memory(
    capability_capsule_id: &str,
    tenant: &str,
    version: i64,
    supersedes_capability_capsule_id: Option<&str>,
) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: capability_capsule_id.into(),
        tenant: tenant.into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version,
        summary: format!("summary-{capability_capsule_id}"),
        content: format!("content-{capability_capsule_id}"),
        evidence: vec![format!("evidence-{capability_capsule_id}")],
        code_refs: vec![format!("src/{capability_capsule_id}.rs")],
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("memory".into()),
        task_type: Some("implementation".into()),
        tags: vec![format!("tag-{capability_capsule_id}")],
        topics: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: format!("hash-{capability_capsule_id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: supersedes_capability_capsule_id.map(str::to_string),
        source_agent: "api".into(),
        created_at: format!("2026-03-21T00:00:0{version}Z"),
        updated_at: format!("2026-03-21T00:05:0{version}Z"),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

async fn seeded_app(memories: Vec<CapabilityCapsuleRecord>) -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("ingest-api.duckdb");
    let repo = Arc::new(Store::open(&db_path).await.unwrap());
    for memory in memories {
        repo.insert_capability_capsule(memory).await.unwrap();
    }

    let state = common::test_app_state(
        repo.clone(),
        mem::service::CapabilityCapsuleService::new(repo),
    );

    TestApp {
        _temp_dir: Some(temp_dir),
        router: http::router().with_state(state),
    }
}

#[test]
fn initial_status_routes_memory_types_as_expected() {
    // Preference + Workflow: always PendingConfirmation regardless of write_mode.
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Preference, &WriteMode::Propose),
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Preference, &WriteMode::Auto),
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Workflow, &WriteMode::Auto),
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Workflow, &WriteMode::Propose),
        CapabilityCapsuleStatus::PendingConfirmation
    );
    // Implementation + Experience + Episode + Diary: Auto = Active.
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Implementation, &WriteMode::Auto),
        CapabilityCapsuleStatus::Active
    );
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Experience, &WriteMode::Auto),
        CapabilityCapsuleStatus::Active
    );
    // Propose on the non-special types now routes to PendingConfirmation
    // (previously Provisional). This is the load-bearing change: agent-
    // driven propose calls actually surface in `list_pending_review`.
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Implementation, &WriteMode::Propose),
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Experience, &WriteMode::Propose),
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Episode, &WriteMode::Propose),
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(
        initial_status(&CapabilityCapsuleType::Diary, &WriteMode::Propose),
        CapabilityCapsuleStatus::PendingConfirmation
    );
}

#[test]
fn content_hash_is_deterministic_for_same_request() {
    let request = IngestCapabilityCapsuleRequest {
        tenant: "tenant-a".into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
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
        topics: vec![],
        source_agent: "api".into(),
        idempotency_key: Some("idem".into()),
        write_mode: WriteMode::Auto,
        supersedes_capability_capsule_id: None,
        expires_at: None,
    };

    let hash = compute_content_hash(&request);
    assert_eq!(hash, compute_content_hash(&request));
    // Cross-process stability canary: literal sha256 of the canonical JSON
    // for this exact fixture. Any change here means the hash function changed
    // and existing palaces will need re-migration. Update only with a matching
    // migration story.
    assert_eq!(
        hash, "02af433bd23d6ca2343cf9f97cce376538f0f1034a599464e6f653c102177b84",
        "content_hash drifted — old palaces will silently fail dedupe"
    );
    assert_eq!(hash.len(), 64, "expected sha256 hex, got: {hash}");
}

#[tokio::test]
async fn repository_dedupe_lookup_prefers_same_tenant_memory() {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("ingest-dedupe.duckdb");
    let repo = Arc::new(Store::open(&db_path).await.unwrap());

    let make_memory = |tenant: &str, capability_capsule_id: &str| {
        mem::domain::capability_capsule::CapabilityCapsuleRecord {
            capability_capsule_id: capability_capsule_id.into(),
            tenant: tenant.into(),
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            status: CapabilityCapsuleStatus::Active,
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
            topics: vec![],
            confidence: 0.9,
            decay_score: 0.0,
            content_hash: "hash-123".into(),
            idempotency_key: Some("idem-123".into()),
            session_id: None,
            supersedes_capability_capsule_id: None,
            source_agent: "api".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_validated_at: None,
            last_used_at: None,
            last_recalled_at: None,
            expires_at: None,
        }
    };

    repo.insert_capability_capsule(make_memory("tenant-a", "mem-a"))
        .await
        .unwrap();
    repo.insert_capability_capsule(make_memory("tenant-b", "mem-b"))
        .await
        .unwrap();

    let found = repo
        .find_by_idempotency_or_hash("tenant-b", &Some("idem-123".into()), "hash-123")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(found.capability_capsule_id, "mem-b");
}

#[tokio::test]
async fn preference_memory_stays_pending_confirmation() {
    let app = test_app().await;
    let response = app
        .post_json(
            "/capability_capsules",
            json!({
                "capability_capsule_type": "preference",
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
            "/capability_capsules",
            json!({
                "capability_capsule_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo", "project": "test-project", "project": "test-project", "project": "test-project",
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
        "capability_capsule_type": "implementation",
        "content": "invalidate cache when schema changes",
        "scope": "repo", "project": "test-project",
        "write_mode": "auto",
        "idempotency_key": "idem-123"
    });

    let first = app.post_json("/capability_capsules", request.clone()).await;
    let second = app.post_json("/capability_capsules", request).await;

    assert_eq!(first.status(), 201);
    assert_eq!(second.status(), 201);
    assert_eq!(
        first.json()["capability_capsule_id"],
        second.json()["capability_capsule_id"]
    );
    assert_eq!(second.json()["status"], "active");
}

#[tokio::test]
async fn same_idempotency_key_in_different_tenants_creates_distinct_memories() {
    let app = test_app().await;

    let first = app
        .post_json(
            "/capability_capsules",
            json!({
                "tenant": "tenant-a",
                "capability_capsule_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo", "project": "test-project", "project": "test-project", "project": "test-project",
                "write_mode": "auto",
                "idempotency_key": "idem-shared"
            }),
        )
        .await;

    let second = app
        .post_json(
            "/capability_capsules",
            json!({
                "tenant": "tenant-b",
                "capability_capsule_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo", "project": "test-project", "project": "test-project", "project": "test-project",
                "write_mode": "auto",
                "idempotency_key": "idem-shared"
            }),
        )
        .await;

    assert_eq!(first.status(), 201);
    assert_eq!(second.status(), 201);
    assert_ne!(
        first.json()["capability_capsule_id"],
        second.json()["capability_capsule_id"]
    );
}

#[tokio::test]
async fn get_memory_returns_full_record() {
    let app = test_app().await;
    let created = app
        .post_json(
            "/capability_capsules",
            json!({
                "capability_capsule_type": "implementation",
                "content": "invalidate cache when schema changes",
                "scope": "repo", "project": "test-project", "project": "test-project", "project": "test-project",
                "write_mode": "auto",
                "idempotency_key": "detail-lookup"
            }),
        )
        .await;

    assert_eq!(created.status(), 201);
    let capability_capsule_id = created.json()["capability_capsule_id"]
        .as_str()
        .expect("memory id should be present")
        .to_string();

    let response = app
        .get(&format!("/capability_capsules/{capability_capsule_id}"))
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json()["capability_capsule"]["capability_capsule_id"],
        capability_capsule_id
    );
    assert_eq!(
        response.json()["capability_capsule"]["content"],
        "invalidate cache when schema changes"
    );
    assert!(response.json()["capability_capsule"]["content_hash"].is_string());
    assert!(response.json()["version_chain"].is_array());
    // Since ROADMAP #18 every ingest auto-buckets the new capsule
    // into a session and writes a memory→session `extracted_from`
    // edge. So `graph_links` is non-empty even when the capsule
    // sets no project/repo/topic/tag fields.
    let graph_links = response.json()["graph_links"]
        .as_array()
        .expect("graph_links should be a JSON array")
        .clone();
    let session_edges: Vec<_> = graph_links
        .iter()
        .filter(|e| e["relation"] == "extracted_from")
        .collect();
    assert_eq!(
        session_edges.len(),
        1,
        "exactly one session edge expected; got graph_links={graph_links:?}",
    );
    assert!(
        session_edges[0]["to_node_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("session:")),
        "session edge to_node_id must use session:<id> prefix; got {:?}",
        session_edges[0]["to_node_id"],
    );
    assert_eq!(response.json()["feedback_summary"]["total"], 0);
}

#[tokio::test]
async fn get_memory_returns_not_found_for_missing_memory() {
    let app = test_app().await;

    let response = app.get("/capability_capsules/missing").await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}

#[tokio::test]
async fn get_memory_returns_not_found_for_wrong_tenant() {
    let app = seeded_app(vec![sample_memory("mem_123", "tenant-a", 1, None)]).await;

    let response = app
        .get("/capability_capsules/mem_123?tenant=tenant-b")
        .await;

    assert_eq!(response.status(), 404);
    assert_eq!(response.json()["error"], "memory not found");
}

#[tokio::test]
async fn get_memory_defaults_tenant_to_local() {
    let app = seeded_app(vec![sample_memory("mem_123", "tenant-a", 1, None)]).await;

    let response = app.get("/capability_capsules/mem_123").await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json()["capability_capsule"]["capability_capsule_id"],
        "mem_123"
    );
    assert_eq!(response.json()["capability_capsule"]["tenant"], "tenant-a");
}

#[tokio::test]
async fn get_memory_returns_full_version_chain_for_successor_ids() {
    let app = seeded_app(vec![
        sample_memory("mem_v1", "local", 1, None),
        sample_memory("mem_v2", "local", 2, Some("mem_v1")),
        sample_memory("mem_v3", "local", 3, Some("mem_v2")),
    ])
    .await;

    let response = app.get("/capability_capsules/mem_v2?tenant=local").await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json()["capability_capsule"]["capability_capsule_id"],
        "mem_v2"
    );
    assert_eq!(
        response.json()["version_chain"],
        json!([
            {
                "capability_capsule_id": "mem_v3",
                "version": 3,
                "status": "active",
                "updated_at": "2026-03-21T00:05:03Z",
                "supersedes_capability_capsule_id": "mem_v2"
            },
            {
                "capability_capsule_id": "mem_v2",
                "version": 2,
                "status": "active",
                "updated_at": "2026-03-21T00:05:02Z",
                "supersedes_capability_capsule_id": "mem_v1"
            },
            {
                "capability_capsule_id": "mem_v1",
                "version": 1,
                "status": "active",
                "updated_at": "2026-03-21T00:05:01Z"
            }
        ])
    );
}

#[tokio::test]
async fn ingest_rejects_summary_equals_content() {
    let app = seeded_app(vec![]).await;
    let verbatim_text =
        "verbatim guard: content and summary are identical, which violates the rule";
    let response = app
        .post_json(
            "/capability_capsules",
            json!({
                "capability_capsule_type": "implementation",
                "content": verbatim_text,
                "summary": verbatim_text,
                "scope": "repo", "project": "test-project", "project": "test-project", "project": "test-project",
                "write_mode": "auto"
            }),
        )
        .await;

    assert_eq!(response.status(), 400);
    let body = response.json();
    let error_msg = body["error"]
        .as_str()
        .expect("error field should be a string");
    assert!(
        error_msg.contains("summary") || error_msg.contains("verbatim"),
        "error message should mention 'summary' or 'verbatim', got: {error_msg}"
    );
}

#[tokio::test]
async fn ingest_accepts_caller_summary_and_stores_it() {
    let app = seeded_app(vec![]).await;
    let content = "the cache must be invalidated whenever the DuckDB schema changes to avoid stale reads from the connection pool";
    let caller_summary = "invalidate cache on schema change";

    let created = app
        .post_json(
            "/capability_capsules",
            json!({
                "capability_capsule_type": "implementation",
                "content": content,
                "summary": caller_summary,
                "scope": "repo", "project": "test-project", "project": "test-project", "project": "test-project",
                "write_mode": "auto"
            }),
        )
        .await;

    assert_eq!(created.status(), 201);
    let capability_capsule_id = created.json()["capability_capsule_id"]
        .as_str()
        .expect("capability_capsule_id should be present")
        .to_string();

    let detail = app
        .get(&format!("/capability_capsules/{capability_capsule_id}"))
        .await;
    assert_eq!(detail.status(), 200);
    assert_eq!(
        detail.json()["capability_capsule"]["summary"],
        caller_summary,
        "stored summary should be the caller-supplied value, not the auto-derived one"
    );
}

#[tokio::test]
async fn batch_ingest_returns_per_item_results() {
    let app = test_app().await;
    let body = json!([
        {
            "capability_capsule_type": "implementation",
            "content": "alpha — first capsule of the batch",
            "scope": "repo", "project": "test-project", "project": "test-project",
            "write_mode": "auto",
            "idempotency_key": "batch-alpha"
        },
        {
            "capability_capsule_type": "implementation",
            "content": "beta — second capsule of the batch",
            "scope": "repo", "project": "test-project", "project": "test-project",
            "write_mode": "auto",
            "idempotency_key": "batch-beta"
        }
    ]);
    let resp = app.post_json("/capability_capsules/batch", body).await;
    assert_eq!(resp.status(), 201);
    let v = resp.json();
    let items = v["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    for item in items {
        assert_eq!(item["result"], "ok");
        assert!(item["capability_capsule_id"].is_string());
        assert_eq!(item["status"], "active");
    }
}

#[tokio::test]
async fn batch_ingest_dedupes_against_existing_idempotency_key() {
    let app = test_app().await;
    let single = json!({
        "capability_capsule_type": "implementation",
        "content": "shared — pre-seeded via single endpoint",
        "scope": "repo", "project": "test-project",
        "write_mode": "auto",
        "idempotency_key": "shared-key"
    });
    let pre = app.post_json("/capability_capsules", single).await;
    assert_eq!(pre.status(), 201);
    let pre_id = pre.json()["capability_capsule_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Same idempotency_key inside a batch → must return the same id
    // as the pre-seeded row, not a new one.
    let body = json!([{
        "capability_capsule_type": "implementation",
        "content": "shared — pre-seeded via single endpoint",
        "scope": "repo", "project": "test-project",
        "write_mode": "auto",
        "idempotency_key": "shared-key"
    }]);
    let resp = app.post_json("/capability_capsules/batch", body).await;
    assert_eq!(resp.status(), 201);
    let v = resp.json();
    let items = v["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["result"], "ok");
    assert_eq!(items[0]["capability_capsule_id"], pre_id);
}
