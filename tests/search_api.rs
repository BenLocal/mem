// `seeded_search_app` calls the deprecated `DuckDbGraphStore::sync_memory`
// method directly; the legacy `to_node_id` format it produces is fine for the
// search-pipeline assertions in this file.
#![allow(deprecated)]

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    domain::{
        capability_capsule::{
            CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope,
            Visibility,
        },
        query::{
            DirectiveItem, FactItem, PatternItem, SearchCapabilityCapsuleRequest,
            SearchCapabilityCapsuleResponse,
        },
        workflow::WorkflowOutline,
    },
    http,
    service::CapabilityCapsuleService,
    storage::Store,
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

mod common;

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
}

struct TestResponse {
    status: StatusCode,
    body: String,
}

struct CapabilityCapsuleSpec<'a> {
    tenant: &'a str,
    capability_capsule_id: &'a str,
    capability_capsule_type: CapabilityCapsuleType,
    scope: Scope,
    repo: Option<&'a str>,
    project: Option<&'a str>,
    module: Option<&'a str>,
    content: &'a str,
    summary: &'a str,
    updated_at: &'a str,
    decay_score: f32,
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

fn memory(spec: CapabilityCapsuleSpec<'_>) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: spec.capability_capsule_id.into(),
        tenant: spec.tenant.into(),
        capability_capsule_type: spec.capability_capsule_type,
        status: CapabilityCapsuleStatus::Active,
        scope: spec.scope,
        visibility: Visibility::Shared,
        version: 1,
        summary: spec.summary.into(),
        content: spec.content.into(),
        evidence: vec![format!("docs/{}.md", spec.capability_capsule_id)],
        code_refs: vec![format!("src/{}.rs", spec.capability_capsule_id)],
        project: spec.project.map(str::to_string),
        repo: spec.repo.map(str::to_string),
        module: spec.module.map(str::to_string),
        task_type: Some("implementation".into()),
        tags: vec!["search".into()],
        topics: vec![],
        confidence: 0.9,
        decay_score: spec.decay_score,
        content_hash: format!("hash-{}", spec.capability_capsule_id),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "codex-worker".into(),
        created_at: spec.updated_at.into(),
        updated_at: spec.updated_at.into(),
        last_validated_at: None,
        last_used_at: None,
    }
}

async fn seeded_search_app(memories: Vec<CapabilityCapsuleRecord>) -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("search-api.duckdb");
    let repo = Arc::new(Store::open(&db_path).await.unwrap());

    for memory in memories {
        repo.insert_capability_capsule(memory.clone())
            .await
            .unwrap();
    }

    let state = common::test_app_state(repo.clone(), CapabilityCapsuleService::new(repo));

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

    let result = serde_json::from_value::<SearchCapabilityCapsuleRequest>(value);

    assert!(result.is_err());
}

#[test]
fn search_response_serializes_compressed_shapes() {
    let response = SearchCapabilityCapsuleResponse {
        directives: vec![DirectiveItem {
            capability_capsule_id: "mem_1".into(),
            text: "Use cache busting on schema changes".into(),
            source_summary: "Known rule from prior implementation".into(),
        }],
        relevant_facts: vec![FactItem {
            capability_capsule_id: "mem_2".into(),
            text: "DuckDB stores canonical memory records".into(),
            code_refs: vec!["src/storage/duckdb.rs".into()],
            source_summary: "Architecture note".into(),
        }],
        reusable_patterns: vec![PatternItem {
            capability_capsule_id: "mem_3".into(),
            text: "Check invariants before writing migrations".into(),
            applicability: None,
            source_summary: "Repeated successful workflow".into(),
        }],
        suggested_workflow: Some(WorkflowOutline {
            capability_capsule_id: "mem_4".into(),
            goal: "ship a safe schema change".into(),
            steps: vec!["write tests".into(), "implement".into(), "verify".into()],
            success_signals: vec!["tests pass".into()],
        }),
        recent_conversations: Vec::new(),
    };

    let value = serde_json::to_value(response).unwrap();

    assert_eq!(value["directives"][0]["capability_capsule_id"], "mem_1");
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
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_pref",
            capability_capsule_type: CapabilityCapsuleType::Preference,
            scope: Scope::Global,
            repo: None,
            project: None,
            module: None,
            content: "Prefer concise answers and mention rollback risk",
            summary: "prefer concise answers",
            updated_at: "2026-03-21T00:00:01Z",
            decay_score: 0.0,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_fact",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "DuckDB stores canonical memory records and keeps indexes local",
            summary: "DuckDB storage layout",
            updated_at: "2026-03-21T00:00:02Z",
            decay_score: 0.0,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_pattern",
            capability_capsule_type: CapabilityCapsuleType::Experience,
            scope: Scope::Workspace,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "When retrying invoice jobs, check logs, isolate cache state, then verify the fix",
            summary: "retry debugging pattern",
            updated_at: "2026-03-21T00:00:03Z",
            decay_score: 0.0,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_workflow",
            capability_capsule_type: CapabilityCapsuleType::Workflow,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "inspect logs; trace the job; patch the retry path; rerun the scenario; confirm success",
            summary: "invoice retry workflow",
            updated_at: "2026-03-21T00:00:04Z",
            decay_score: 0.0,
        }),
    ])
    .await;

    let response = app
        .post_json(
            "/capability_capsules/search",
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
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_other",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("analytics"),
            project: Some("analytics"),
            module: Some("reports"),
            content: "Fix report rendering in the analytics pipeline",
            summary: "analytics fix",
            updated_at: "2026-03-21T00:00:01Z",
            decay_score: 0.0,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_billing",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "Invoice retry failures are caused by stale cache state",
            summary: "billing fix",
            updated_at: "2026-03-21T00:00:02Z",
            decay_score: 0.0,
        }),
    ])
    .await;

    let response = app
        .post_json(
            "/capability_capsules/search",
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
        response.json()["relevant_facts"][0]["capability_capsule_id"],
        "mem_billing"
    );
}

#[tokio::test]
async fn stale_memory_penalty_prefers_recent_memory() {
    let app = seeded_search_app(vec![
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_old",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "Retry failures happen when cache metadata is stale",
            summary: "retry failure note",
            updated_at: "2025-03-21T00:00:01Z",
            decay_score: 0.8,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_new",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "Retry failures happen when cache metadata is stale",
            summary: "retry failure note",
            updated_at: "2026-03-21T00:00:01Z",
            decay_score: 0.0,
        }),
    ])
    .await;

    let response = app
        .post_json(
            "/capability_capsules/search",
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
    assert_eq!(
        response.json()["relevant_facts"][0]["capability_capsule_id"],
        "mem_new"
    );
}

#[tokio::test]
async fn search_respects_tenant_scope() {
    let app = seeded_search_app(vec![
        memory(CapabilityCapsuleSpec {
            tenant: "tenant-a",
            capability_capsule_id: "mem_a",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "Invoice retry failures are caused by stale cache state",
            summary: "billing fix tenant a",
            updated_at: "2026-03-21T00:00:02Z",
            decay_score: 0.0,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "tenant-b",
            capability_capsule_id: "mem_b",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "Invoice retry failures are caused by stale cache state",
            summary: "billing fix tenant b",
            updated_at: "2026-03-21T00:00:03Z",
            decay_score: 0.0,
        }),
    ])
    .await;

    let response = app
        .post_json(
            "/capability_capsules/search",
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
    assert_eq!(
        response.json()["relevant_facts"][0]["capability_capsule_id"],
        "mem_b"
    );
}

#[tokio::test]
async fn negative_feedback_penalizes_future_recall() {
    let app = seeded_search_app(vec![
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_target",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("billing"),
            project: Some("billing"),
            module: Some("invoice"),
            content: "Invoice retry failures are caused by stale cache state",
            summary: "invoice retry failure note",
            updated_at: "2026-03-21T00:00:02Z",
            decay_score: 0.0,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_backup",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("analytics"),
            project: Some("analytics"),
            module: Some("reports"),
            content: "Invoice retry failures are caused by stale cache state",
            summary: "invoice retry failure note",
            updated_at: "2026-03-21T00:00:01Z",
            decay_score: 0.0,
        }),
    ])
    .await;

    let before = app
        .post_json(
            "/capability_capsules/search",
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
    assert_eq!(
        before.json()["relevant_facts"][0]["capability_capsule_id"],
        "mem_target"
    );

    let feedback = app
        .post_json(
            "/capability_capsules/feedback",
            json!({
                "tenant": "local",
                "capability_capsule_id": "mem_target",
                "feedback_kind": "incorrect"
            }),
        )
        .await;

    assert_eq!(feedback.status(), 200);
    assert_eq!(feedback.json()["status"], "archived");

    let after = app
        .post_json(
            "/capability_capsules/search",
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
    assert_eq!(
        after.json()["relevant_facts"][0]["capability_capsule_id"],
        "mem_backup"
    );
}

// Regression for the FTS rebuild "stopwords has been deleted" failure mode
// (docs/api-data-flow.md §5.1, observed 2026-05-06). The bundled DuckDB FTS
// 1.x dependency tracker keeps a stale edge to the just-dropped `stopwords`
// macro, so the second-and-onward rebuild on a long-lived connection used to
// SessionStart-hook wake-up call (`intent="wake_up"`, empty `query`) takes
// the fast path in `CapabilityCapsuleService::search`: it fetches a bounded slice of
// recent active memories from DuckDB, skips embedding / HNSW / BM25 / graph
// ranking, and hands the slice straight to `compress`. The full pipeline
// took 11–200 s in production on a moderately-loaded DB; this test pins the
// fast-path contract — recent active memories surface, archived rows are
// filtered out at the SQL layer, and the response shape matches a normal
// search response.
#[tokio::test]
async fn wake_up_fast_path_returns_recent_active_memories() {
    let archived_memory = {
        let mut m = memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_archived",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("mem"),
            project: Some("mem"),
            module: Some("storage"),
            content: "Archived note that the wake-up fast path must filter out.",
            summary: "archived note",
            updated_at: "2026-05-07T00:00:03Z",
            decay_score: 0.0,
        });
        m.status = CapabilityCapsuleStatus::Archived;
        m
    };
    let app = seeded_search_app(vec![
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_pref",
            capability_capsule_type: CapabilityCapsuleType::Preference,
            scope: Scope::Global,
            repo: None,
            project: None,
            module: None,
            content: "Prefer concise answers and mention rollback risk.",
            summary: "prefer concise answers",
            updated_at: "2026-05-07T00:00:02Z",
            decay_score: 0.0,
        }),
        memory(CapabilityCapsuleSpec {
            tenant: "local",
            capability_capsule_id: "mem_recent_fact",
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            scope: Scope::Repo,
            repo: Some("mem"),
            project: Some("mem"),
            module: Some("storage"),
            content: "DuckDB stores canonical memory records and keeps indexes local.",
            summary: "duckdb storage layout",
            updated_at: "2026-05-07T00:00:01Z",
            decay_score: 0.0,
        }),
        archived_memory,
    ])
    .await;

    let response = app
        .post_json(
            "/capability_capsules/search",
            json!({
                "query": "",
                "intent": "wake_up",
                "scope_filters": [],
                "token_budget": 800,
                "caller_agent": "claude-code",
                "expand_graph": false,
                "tenant": "local",
            }),
        )
        .await;
    assert_eq!(response.status(), 200);

    let body = response.json();
    let surfaced: std::collections::HashSet<String> = body["directives"]
        .as_array()
        .unwrap()
        .iter()
        .chain(body["relevant_facts"].as_array().unwrap().iter())
        .chain(body["reusable_patterns"].as_array().unwrap().iter())
        .filter_map(|item| item["capability_capsule_id"].as_str().map(str::to_string))
        .collect();

    assert!(
        surfaced.contains("mem_pref"),
        "preference must surface as directive on wake-up"
    );
    assert!(
        surfaced.contains("mem_recent_fact"),
        "recent active fact must surface on wake-up (no relevance floor on this path)"
    );
    assert!(
        !surfaced.contains("mem_archived"),
        "archived rows must be filtered at the SQL layer"
    );
}

// 500 with the dependency-commit error. Fix: detect that error class and
// retry once after `INSTALL fts; LOAD fts;` to reset extension state. This
// test exercises the repeat-rebuild scenario via the public HTTP surface —
// two dirtying writes followed by two searches against the same `AppState`
// (and therefore the same long-lived DuckDB connection).
#[tokio::test]
async fn fts_rebuild_survives_repeat_dirty_cycles() {
    let app = seeded_search_app(vec![memory(CapabilityCapsuleSpec {
        tenant: "local",
        capability_capsule_id: "mem_first",
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        scope: Scope::Repo,
        repo: Some("mem"),
        project: Some("mem"),
        module: Some("storage"),
        content: "first round payload for the FTS rebuild probe",
        summary: "first round summary",
        updated_at: "2026-05-06T00:00:01Z",
        decay_score: 0.0,
    })])
    .await;

    // First search: triggers the initial drop+create cycle inside
    // `ensure_fts_index_fresh`.
    let first = app
        .post_json(
            "/capability_capsules/search",
            json!({
                "query": "rebuild probe",
                "intent": "debugging",
                "scope_filters": [],
                "token_budget": 300,
                "caller_agent": "codex-worker",
                "expand_graph": false,
                "tenant": "local",
            }),
        )
        .await;
    assert_eq!(first.status(), 200, "first search must succeed");

    // Insert another memory through the public API — this writes a row into
    // `memories` and flips the `fts_dirty` flag back to true.
    let ingest = app
        .post_json(
            "/capability_capsules",
            json!({
                "capability_capsule_type": "implementation",
                "content": "second round payload for the FTS rebuild probe",
                "scope": "repo",
                "visibility": "shared",
                "project": "mem",
                "repo": "mem",
                "module": "storage",
                "source_agent": "test",
                "tenant": "local",
            }),
        )
        .await;
    assert_eq!(ingest.status(), 201, "ingest must succeed");

    // Second search: triggers `ensure_fts_index_fresh` a second time on the
    // same connection. Pre-fix this would 500 with
    //   "Could not commit creation of dependency, subject \"stopwords\" has
    //    been deleted".
    let second = app
        .post_json(
            "/capability_capsules/search",
            json!({
                "query": "rebuild probe",
                "intent": "debugging",
                "scope_filters": [],
                "token_budget": 300,
                "caller_agent": "codex-worker",
                "expand_graph": false,
                "tenant": "local",
            }),
        )
        .await;
    assert_eq!(
        second.status(),
        200,
        "second search must succeed (FTS dependency-tracker workaround)"
    );
}
