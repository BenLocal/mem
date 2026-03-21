use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    app::AppState,
    domain::{
        memory::{MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility},
        query::{DirectiveItem, FactItem, PatternItem, SearchMemoryRequest, SearchMemoryResponse},
        workflow::WorkflowOutline,
    },
    http,
    service::MemoryService,
    storage::{DuckDbRepository, GraphStore, LocalGraphAdapter},
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
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

fn memory(
    tenant: &str,
    memory_id: &str,
    memory_type: MemoryType,
    scope: Scope,
    repo: Option<&str>,
    project: Option<&str>,
    module: Option<&str>,
    content: &str,
    summary: &str,
    updated_at: &str,
    decay_score: f32,
) -> MemoryRecord {
    MemoryRecord {
        memory_id: memory_id.into(),
        tenant: tenant.into(),
        memory_type,
        status: MemoryStatus::Active,
        scope,
        visibility: Visibility::Shared,
        version: 1,
        summary: summary.into(),
        content: content.into(),
        evidence: vec![format!("docs/{memory_id}.md")],
        code_refs: vec![format!("src/{memory_id}.rs")],
        project: project.map(str::to_string),
        repo: repo.map(str::to_string),
        module: module.map(str::to_string),
        task_type: Some("implementation".into()),
        tags: vec!["search".into()],
        confidence: 0.9,
        decay_score,
        content_hash: format!("hash-{memory_id}"),
        idempotency_key: None,
        supersedes_memory_id: None,
        source_agent: "codex-worker".into(),
        created_at: updated_at.into(),
        updated_at: updated_at.into(),
        last_validated_at: None,
    }
}

async fn seeded_search_app(memories: Vec<MemoryRecord>) -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("search-api.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    let graph = Arc::new(LocalGraphAdapter::new());

    for memory in memories {
        repo.insert_memory(memory.clone()).await.unwrap();
        graph.sync_memory(&memory).await.unwrap();
    }

    let state = AppState {
        memory_service: MemoryService::with_graph(repo, graph),
    };

    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
    }
}

#[test]
fn search_request_missing_required_field_fails_deserialization() {
    let value = json!({
        "intent": "debugging",
        "scope_filters": ["repo:billing"],
        "token_budget": 500,
        "caller_agent": "codex-worker",
        "expand_graph": true
    });

    let result = serde_json::from_value::<SearchMemoryRequest>(value);

    assert!(result.is_err());
}

#[test]
fn search_response_serializes_compressed_shapes() {
    let response = SearchMemoryResponse {
        directives: vec![DirectiveItem {
            memory_id: "mem_1".into(),
            text: "Use cache busting on schema changes".into(),
            source_summary: "Known rule from prior implementation".into(),
        }],
        relevant_facts: vec![FactItem {
            memory_id: "mem_2".into(),
            text: "DuckDB stores canonical memory records".into(),
            code_refs: vec!["src/storage/duckdb.rs".into()],
            source_summary: "Architecture note".into(),
        }],
        reusable_patterns: vec![PatternItem {
            memory_id: "mem_3".into(),
            text: "Check invariants before writing migrations".into(),
            applicability: None,
            source_summary: "Repeated successful workflow".into(),
        }],
        suggested_workflow: Some(WorkflowOutline {
            memory_id: "mem_4".into(),
            goal: "ship a safe schema change".into(),
            steps: vec!["write tests".into(), "implement".into(), "verify".into()],
            success_signals: vec!["tests pass".into()],
        }),
    };

    let value = serde_json::to_value(response).unwrap();

    assert_eq!(value["directives"][0]["memory_id"], "mem_1");
    assert_eq!(
        value["relevant_facts"][0]["code_refs"][0],
        "src/storage/duckdb.rs"
    );
    assert!(value["reusable_patterns"][0].get("applicability").is_none());
    assert_eq!(
        value["suggested_workflow"]["goal"],
        "ship a safe schema change"
    );
}

#[tokio::test]
async fn search_returns_compressed_memory_pack() {
    let app = seeded_search_app(vec![
        memory(
            "local",
            "mem_pref",
            MemoryType::Preference,
            Scope::Global,
            None,
            None,
            None,
            "Prefer concise answers and mention rollback risk",
            "prefer concise answers",
            "2026-03-21T00:00:01Z",
            0.0,
        ),
        memory(
            "local",
            "mem_fact",
            MemoryType::Implementation,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "DuckDB stores canonical memory records and keeps indexes local",
            "DuckDB storage layout",
            "2026-03-21T00:00:02Z",
            0.0,
        ),
        memory(
            "local",
            "mem_pattern",
            MemoryType::Experience,
            Scope::Workspace,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "When retrying invoice jobs, check logs, isolate cache state, then verify the fix",
            "retry debugging pattern",
            "2026-03-21T00:00:03Z",
            0.0,
        ),
        memory(
            "local",
            "mem_workflow",
            MemoryType::Workflow,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "inspect logs; trace the job; patch the retry path; rerun the scenario; confirm success",
            "invoice retry workflow",
            "2026-03-21T00:00:04Z",
            0.0,
        ),
    ])
    .await;

    let response = app
        .post_json(
            "/memories/search",
            json!({
                "query": "how should I debug invoice retry failures",
                "intent": "debugging",
                "scope_filters": ["repo:billing"],
                "token_budget": 500,
                "caller_agent": "codex-worker",
                "expand_graph": true,
                "tenant": "local"
            }),
        )
        .await;

    let body = response.json();
    assert_eq!(response.status(), 200);
    assert!(body["directives"].is_array());
    assert!(body["relevant_facts"].is_array());
    assert!(body["reusable_patterns"].is_array());
    assert!(body["suggested_workflow"].is_object());
    assert!(!body["directives"].as_array().unwrap().is_empty());
    assert!(!body["relevant_facts"].as_array().unwrap().is_empty());
    assert!(!body["reusable_patterns"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn scope_bias_prefers_matching_repo_memory() {
    let app = seeded_search_app(vec![
        memory(
            "local",
            "mem_other",
            MemoryType::Implementation,
            Scope::Repo,
            Some("analytics"),
            Some("analytics"),
            Some("reports"),
            "Fix report rendering in the analytics pipeline",
            "analytics fix",
            "2026-03-21T00:00:01Z",
            0.0,
        ),
        memory(
            "local",
            "mem_billing",
            MemoryType::Implementation,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "Invoice retry failures are caused by stale cache state",
            "billing fix",
            "2026-03-21T00:00:02Z",
            0.0,
        ),
    ])
    .await;

    let response = app
        .post_json(
            "/memories/search",
            json!({
                "query": "invoice retry failure",
                "intent": "debugging",
                "scope_filters": ["repo:billing"],
                "token_budget": 300,
                "caller_agent": "codex-worker",
                "expand_graph": true,
                "tenant": "local"
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json()["relevant_facts"][0]["memory_id"],
        "mem_billing"
    );
}

#[tokio::test]
async fn stale_memory_penalty_prefers_recent_memory() {
    let app = seeded_search_app(vec![
        memory(
            "local",
            "mem_old",
            MemoryType::Implementation,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "Retry failures happen when cache metadata is stale",
            "retry failure note",
            "2025-03-21T00:00:01Z",
            0.8,
        ),
        memory(
            "local",
            "mem_new",
            MemoryType::Implementation,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "Retry failures happen when cache metadata is stale",
            "retry failure note",
            "2026-03-21T00:00:01Z",
            0.0,
        ),
    ])
    .await;

    let response = app
        .post_json(
            "/memories/search",
            json!({
                "query": "retry failures stale cache metadata",
                "intent": "debugging",
                "scope_filters": ["repo:billing"],
                "token_budget": 300,
                "caller_agent": "codex-worker",
                "expand_graph": true,
                "tenant": "local"
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["relevant_facts"][0]["memory_id"], "mem_new");
}

#[tokio::test]
async fn search_respects_tenant_scope() {
    let app = seeded_search_app(vec![
        memory(
            "tenant-a",
            "mem_a",
            MemoryType::Implementation,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "Invoice retry failures are caused by stale cache state",
            "billing fix tenant a",
            "2026-03-21T00:00:02Z",
            0.0,
        ),
        memory(
            "tenant-b",
            "mem_b",
            MemoryType::Implementation,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "Invoice retry failures are caused by stale cache state",
            "billing fix tenant b",
            "2026-03-21T00:00:03Z",
            0.0,
        ),
    ])
    .await;

    let response = app
        .post_json(
            "/memories/search",
            json!({
                "query": "invoice retry failure",
                "intent": "debugging",
                "scope_filters": ["repo:billing"],
                "token_budget": 300,
                "caller_agent": "codex-worker",
                "expand_graph": true,
                "tenant": "tenant-b"
            }),
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["relevant_facts"][0]["memory_id"], "mem_b");
}

#[tokio::test]
async fn negative_feedback_penalizes_future_recall() {
    let app = seeded_search_app(vec![
        memory(
            "local",
            "mem_target",
            MemoryType::Implementation,
            Scope::Repo,
            Some("billing"),
            Some("billing"),
            Some("invoice"),
            "Invoice retry failures are caused by stale cache state",
            "invoice retry failure note",
            "2026-03-21T00:00:02Z",
            0.0,
        ),
        memory(
            "local",
            "mem_backup",
            MemoryType::Implementation,
            Scope::Repo,
            Some("analytics"),
            Some("analytics"),
            Some("reports"),
            "Invoice retry failures are caused by stale cache state",
            "invoice retry failure note",
            "2026-03-21T00:00:01Z",
            0.0,
        ),
    ])
    .await;

    let before = app
        .post_json(
            "/memories/search",
            json!({
                "query": "invoice retry failure stale cache state",
                "intent": "debugging",
                "scope_filters": ["repo:billing"],
                "token_budget": 300,
                "caller_agent": "codex-worker",
                "expand_graph": true,
                "tenant": "local"
            }),
        )
        .await;

    assert_eq!(before.status(), 200);
    assert_eq!(before.json()["relevant_facts"][0]["memory_id"], "mem_target");

    let feedback = app
        .post_json(
            "/memories/feedback",
            json!({
                "tenant": "local",
                "memory_id": "mem_target",
                "feedback_kind": "incorrect"
            }),
        )
        .await;

    assert_eq!(feedback.status(), 200);
    assert_eq!(feedback.json()["status"], "archived");

    let after = app
        .post_json(
            "/memories/search",
            json!({
                "query": "invoice retry failure stale cache state",
                "intent": "debugging",
                "scope_filters": ["repo:billing"],
                "token_budget": 300,
                "caller_agent": "codex-worker",
                "expand_graph": true,
                "tenant": "local"
            }),
        )
        .await;

    assert_eq!(after.status(), 200);
    assert_eq!(after.json()["relevant_facts"][0]["memory_id"], "mem_backup");
}
