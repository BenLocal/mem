// Integration tests for session auto-bucket (ROADMAP #10).
// Spec: docs/superpowers/specs/2026-04-29-sessions-design.md.
//
// These tests exercise the full HTTP path: POST /memories → storage → GET /memories/{id},
// verifying that session_id is assigned automatically and that the partitioning rules hold.
//
// Test 4 (idle eviction) is NOT an HTTP integration test here.  The pure logic (decide_session
// returning OpenNew when last_seen_at is stale) is already covered by the unit tests in
// src/pipeline/session.rs::tests.  Backdating a live session row would require either a
// test-only mutation API on DuckDbRepository or sleeping the full idle window — both add
// more friction than the value warrants for a path whose branching is already unit-tested.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mem::{
    app::AppState,
    http,
    storage::{DuckDbRepository, VectorIndex},
};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

// ---------------------------------------------------------------------------
// Test infrastructure (inline; mirrors the pattern in tests/ingest_api.rs)
// ---------------------------------------------------------------------------

struct TestApp {
    _temp_dir: Option<TempDir>,
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

/// Build an ephemeral-DB test app.  Each call produces an independent DuckDB file.
async fn fresh_app() -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("sessions-integration.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();

    let state = AppState {
        memory_service: mem::service::MemoryService::new(repo),
        config: mem::config::Config::local(),
        transcript_index: Arc::new(VectorIndex::new_in_memory(8, "fake", "fake", 8)),
    };

    TestApp {
        _temp_dir: Some(temp_dir),
        router: http::router().with_state(state),
    }
}

/// Minimal ingest request body.  Uses the same enum casings as existing ingest_api tests.
fn ingest_body(tenant: &str, agent: &str, content: &str) -> Value {
    json!({
        "tenant": tenant,
        "memory_type": "implementation",
        "content": content,
        "scope": "global",
        "write_mode": "auto",
        "source_agent": agent
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn post_memory(app: &TestApp, tenant: &str, agent: &str, content: &str) -> String {
    let resp = app
        .post_json("/memories", ingest_body(tenant, agent, content))
        .await;
    assert_eq!(resp.status.as_u16(), 201, "ingest failed: {}", resp.body);
    resp.json()["memory_id"]
        .as_str()
        .expect("memory_id in response")
        .to_string()
}

async fn get_memory(app: &TestApp, memory_id: &str) -> Value {
    let resp = app.get(&format!("/memories/{memory_id}")).await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "get_memory failed: {}",
        resp.body
    );
    resp.json()
}

fn session_id_of(memory_detail: &Value) -> String {
    memory_detail["memory"]["session_id"]
        .as_str()
        .expect("session_id should be a string in stored memory")
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The very first ingest for a (tenant, agent) pair must create a session and attach it.
#[tokio::test]
async fn ingest_creates_first_session() {
    let app = fresh_app().await;
    let mid = post_memory(&app, "t", "agent_a", "first memory").await;
    let detail = get_memory(&app, &mid).await;
    let sid = session_id_of(&detail);
    assert!(!sid.is_empty(), "session_id must not be empty");
}

/// Two consecutive ingests for the same (tenant, agent) within the default 30-minute idle
/// window must share a session.
#[tokio::test]
async fn ingest_continues_active_session_within_idle() {
    let app = fresh_app().await;
    let mid1 = post_memory(&app, "t", "agent_a", "first memory").await;
    let mid2 = post_memory(&app, "t", "agent_a", "second memory within idle").await;

    let sid1 = session_id_of(&get_memory(&app, &mid1).await);
    let sid2 = session_id_of(&get_memory(&app, &mid2).await);

    assert_eq!(
        sid1, sid2,
        "two ingests within default 30-min idle should share a session"
    );
}

/// Different `source_agent` values must produce independent sessions even within the same tenant.
#[tokio::test]
async fn ingest_independent_session_per_caller_agent() {
    let app = fresh_app().await;
    let mid_codex = post_memory(&app, "t", "codex", "from codex").await;
    let mid_cursor = post_memory(&app, "t", "cursor", "from cursor").await;

    let sid_codex = session_id_of(&get_memory(&app, &mid_codex).await);
    let sid_cursor = session_id_of(&get_memory(&app, &mid_cursor).await);

    assert_ne!(
        sid_codex, sid_cursor,
        "different caller_agents must get independent sessions"
    );
}

/// Memories in the same session share a session_id; memories in different sessions do not.
/// This verifies tenant-partitioning: same agent in two tenants gets two sessions.
#[tokio::test]
async fn ingest_independent_session_per_tenant() {
    let app = fresh_app().await;
    let mid_a = post_memory(&app, "tenant-alpha", "agent_x", "from alpha").await;
    let mid_b = post_memory(&app, "tenant-beta", "agent_x", "from beta").await;

    let sid_a = session_id_of(&get_memory(&app, &mid_a).await);
    let sid_b = session_id_of(&get_memory(&app, &mid_b).await);

    assert_ne!(
        sid_a, sid_b,
        "same agent in different tenants must get independent sessions"
    );
}
