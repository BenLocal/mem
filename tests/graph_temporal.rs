use mem::domain::memory::GraphEdge;
use mem::storage::{DuckDbGraphStore, DuckDbRepository};
use std::sync::Arc;
use tempfile::tempdir;

async fn open_repo_and_graph(db_path: &std::path::Path)
    -> (Arc<DuckDbRepository>, DuckDbGraphStore)
{
    let repo = Arc::new(DuckDbRepository::open(db_path).await.unwrap());
    let graph = DuckDbGraphStore::new(repo.clone());
    (repo, graph)
}

#[tokio::test]
async fn duckdb_graph_store_constructs_against_fresh_db() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ctor.duckdb");
    let (_repo, graph) = open_repo_and_graph(&db).await;
    let edges = graph.neighbors("memory:does-not-exist").await.unwrap();
    assert!(edges.is_empty());
}

use mem::domain::memory::{
    IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode,
};
use mem::service::MemoryService;

async fn ingest_one(svc: &MemoryService, content: &str, project: Option<&str>, repo: Option<&str>)
    -> mem::service::IngestMemoryResponse
{
    svc.ingest(IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: project.map(String::from),
        repo: repo.map(String::from),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    }).await.unwrap()
}

#[tokio::test]
async fn sync_creates_active_edges_for_simple_memory() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sync.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "alpha", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();

    graph.sync_memory(&memory).await.unwrap();

    let edges = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    let relations: std::collections::HashSet<_> =
        edges.iter().map(|e| e.relation.as_str()).collect();
    assert!(relations.contains("applies_to"), "edges: {edges:?}");
    assert!(relations.contains("observed_in"), "edges: {edges:?}");
    for edge in &edges {
        assert_eq!(edge.valid_to, None);
        assert!(!edge.valid_from.is_empty(), "valid_from should be set");
    }
}

#[tokio::test]
async fn sync_is_idempotent_when_called_twice() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("idem.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "beta", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();

    graph.sync_memory(&memory).await.unwrap();
    let first = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    graph.sync_memory(&memory).await.unwrap();
    let second = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();

    assert_eq!(first.len(), second.len(), "edge count must not grow");
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.from_node_id, b.from_node_id);
        assert_eq!(a.relation, b.relation);
        assert_eq!(a.valid_from, b.valid_from, "valid_from must not be refreshed");
    }
}

#[test]
fn graph_edge_carries_valid_from_and_valid_to() {
    let edge = GraphEdge {
        from_node_id: "memory:abc".into(),
        to_node_id: "project:foo".into(),
        relation: "applies_to".into(),
        valid_from: "00000001761662918634".into(),
        valid_to: None,
    };
    assert_eq!(edge.valid_to, None);
    assert!(edge.valid_from.starts_with("000000"));

    let s = serde_json::to_string(&edge).unwrap();
    let back: GraphEdge = serde_json::from_str(&s).unwrap();
    assert_eq!(back.valid_to, None);
    assert_eq!(back.valid_from, "00000001761662918634");
}

#[tokio::test]
async fn close_edges_for_memory_sets_valid_to() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("close.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "gamma", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&memory).await.unwrap();

    let pre = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    assert!(!pre.is_empty(), "should have active edges before close");

    let closed = graph.close_edges_for_memory(&r.memory_id).await.unwrap();
    assert!(closed > 0, "should report at least one closed row");

    let post = graph.neighbors(&format!("memory:{}", r.memory_id)).await.unwrap();
    assert!(post.is_empty(), "no active edges after close");
}

fn current_ts_str() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{millis:020}")
}

fn bump_timestamp(s: &str, by_ms: u128) -> String {
    let n: u128 = s.parse().unwrap();
    format!("{:020}", n + by_ms)
}

#[tokio::test]
async fn neighbors_at_filters_by_timestamp() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("at.duckdb");
    let (repo, graph) = open_repo_and_graph(&db).await;
    let svc = MemoryService::new((*repo).clone());
    let r = ingest_one(&svc, "delta", Some("foo"), Some("mem")).await;
    let memory = repo.get_memory_for_tenant("t", &r.memory_id).await.unwrap().unwrap();
    graph.sync_memory(&memory).await.unwrap();
    let active = graph.neighbors("project:foo").await.unwrap();
    assert!(!active.is_empty());
    let valid_from_of_first = active[0].valid_from.clone();

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    graph.close_edges_for_memory(&r.memory_id).await.unwrap();
    let after_close = current_ts_str();

    // mid timestamp = valid_from + 1ms (still active before close)
    let mid = bump_timestamp(&valid_from_of_first, 1);
    let then = graph.neighbors_at("project:foo", &mid).await.unwrap();
    assert!(!then.is_empty(), "edge should be active at mid timestamp");

    let later = graph.neighbors_at("project:foo", &after_close).await.unwrap();
    assert!(later.is_empty(), "edge must be excluded at later timestamp");
}
