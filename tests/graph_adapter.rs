use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    app::{self, AppState},
    domain::memory::{MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility},
    http,
    service::MemoryService,
    storage::{GraphStore, IndraDbGraphAdapter, LocalGraphAdapter},
};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::tempdir;
use tower::util::ServiceExt;

fn sample_impl_memory(memory_id: &str, supersedes_memory_id: Option<&str>) -> MemoryRecord {
    MemoryRecord {
        memory_id: memory_id.into(),
        tenant: "local".into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{memory_id}"),
        content: "cache invoices after successful sync".into(),
        evidence: vec!["docs/invoice.md".into()],
        code_refs: vec!["src/billing/invoice.rs".into()],
        project: Some("billing".into()),
        repo: Some("mem".into()),
        module: Some("invoice".into()),
        task_type: Some("implementation".into()),
        tags: vec!["graph".into(), "contradicts:mem_legacy".into()],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: format!("hash-{memory_id}"),
        idempotency_key: None,
        supersedes_memory_id: supersedes_memory_id.map(str::to_string),
        source_agent: "codex-worker".into(),
        created_at: "2026-03-21T00:00:01Z".into(),
        updated_at: "2026-03-21T00:05:01Z".into(),
        last_validated_at: None,
    }
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

struct TestApp {
    router: axum::Router,
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
        router: app::router().await.expect("app router should build"),
    }
}

async fn test_app_with_unavailable_graph() -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("graph-unavailable.duckdb");
    let repository = mem::storage::DuckDbRepository::open(&db_path)
        .await
        .unwrap();
    let state = AppState {
        memory_service: MemoryService::with_graph(repository, Arc::new(IndraDbGraphAdapter::new())),
        config: mem::config::Config::local(),
    };

    TestApp {
        router: http::router().with_state(state),
    }
}

#[tokio::test]
async fn ingest_extracts_project_and_module_nodes() {
    let graph = LocalGraphAdapter::new();
    let memory = sample_impl_memory("mem_001", None);

    graph.sync_memory(&memory).await.unwrap();

    let project_neighbors = graph.neighbors("project:billing").await.unwrap();
    let module_neighbors = graph.neighbors("module:mem:invoice").await.unwrap();

    assert!(project_neighbors.iter().any(|edge| {
        edge.from_node_id == "memory:mem_001"
            && edge.to_node_id == "project:billing"
            && edge.relation == "applies_to"
    }));
    assert!(module_neighbors.iter().any(|edge| {
        edge.from_node_id == "memory:mem_001"
            && edge.to_node_id == "module:mem:invoice"
            && edge.relation == "relevant_to"
    }));
}

#[tokio::test]
async fn ingest_extracts_workflow_and_supersedes_edges() {
    let graph = LocalGraphAdapter::new();
    let memory = sample_impl_memory("mem_002", Some("mem_001"));

    graph.sync_memory(&memory).await.unwrap();

    let workflow_neighbors = graph.neighbors("workflow:implementation").await.unwrap();
    let supersedes_neighbors = graph.neighbors("memory:mem_002").await.unwrap();
    let contradiction_neighbors = graph.neighbors("memory:mem_002").await.unwrap();

    assert!(workflow_neighbors.iter().any(|edge| {
        edge.from_node_id == "memory:mem_002"
            && edge.to_node_id == "workflow:implementation"
            && edge.relation == "uses_workflow"
    }));
    assert!(supersedes_neighbors.iter().any(|edge| {
        edge.from_node_id == "memory:mem_002"
            && edge.to_node_id == "memory:mem_001"
            && edge.relation == "supersedes"
    }));
    assert!(contradiction_neighbors.iter().any(|edge| {
        edge.from_node_id == "memory:mem_002"
            && edge.to_node_id == "memory:mem_legacy"
            && edge.relation == "contradicts"
    }));
}

#[tokio::test]
async fn http_neighbors_returns_graph_edges_after_ingest() {
    let app = test_app().await;

    let response = app
        .post_json(
            "/memories",
            json!({
                "memory_type": "implementation",
                "content": "cache invoices after successful sync",
                "scope": "repo",
                "project": "billing",
                "repo": "mem",
                "module": "invoice",
                "task_type": "implementation",
                "write_mode": "auto"
            }),
        )
        .await;

    assert_eq!(response.status(), 201);

    let neighbors = app.get("/graph/neighbors/module:mem:invoice").await;

    assert_eq!(neighbors.status(), 200);
    assert!(neighbors.json().as_array().unwrap().iter().any(|edge| {
        edge["relation"] == "relevant_to" && edge["to_node_id"] == "module:mem:invoice"
    }));
}

#[tokio::test]
async fn memory_routes_degrade_when_graph_backend_is_unavailable() {
    let app = test_app_with_unavailable_graph().await;

    let created = app
        .post_json(
            "/memories",
            json!({
                "memory_type": "implementation",
                "content": "cache invoices after successful sync",
                "scope": "repo",
                "project": "billing",
                "repo": "mem",
                "module": "invoice",
                "task_type": "implementation",
                "write_mode": "auto"
            }),
        )
        .await;

    assert_eq!(created.status(), 201);
    let created_json = created.json();
    let memory_id = created_json["memory_id"].as_str().unwrap();

    let detail = app.get(&format!("/memories/{memory_id}")).await;
    assert_eq!(detail.status(), 200);
    assert_eq!(detail.json()["graph_links"], json!([]));

    let neighbors = app.get("/graph/neighbors/module:mem:invoice").await;
    assert_eq!(neighbors.status(), 200);
    assert_eq!(neighbors.json(), json!([]));
}
