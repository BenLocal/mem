use mem::domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode};
use mem::service::MemoryService;
use mem::storage::DuckDbRepository;
use tempfile::tempdir;

#[tokio::test]
async fn fetch_by_ids_filters_tenant_and_status() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("fetch.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let svc = MemoryService::new(repo.clone());

    let make = |tenant: &str, content: &str| IngestMemoryRequest {
        tenant: tenant.into(),
        memory_type: MemoryType::Implementation,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    };
    let a = svc.ingest(make("ten-a", "alpha")).await.unwrap();
    let b = svc.ingest(make("ten-b", "beta")).await.unwrap();
    let c = svc.ingest(make("ten-a", "gamma")).await.unwrap();

    let ids = vec![
        a.memory_id.as_str(),
        b.memory_id.as_str(),
        c.memory_id.as_str(),
    ];
    let rows = repo.fetch_memories_by_ids("ten-a", &ids).await.unwrap();

    let returned: std::collections::HashSet<_> =
        rows.iter().map(|m| m.memory_id.as_str()).collect();
    assert!(returned.contains(a.memory_id.as_str()));
    assert!(returned.contains(c.memory_id.as_str()));
    assert!(!returned.contains(b.memory_id.as_str())); // wrong tenant
}
