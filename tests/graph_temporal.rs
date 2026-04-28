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
