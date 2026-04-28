use mem::config::EmbeddingSettings;
use mem::domain::memory::{
    IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode,
};
use mem::embedding::arc_embedding_provider;
use mem::service::{embedding_worker, MemoryService};
use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn delete_paths_mirror_into_vector_index() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("del.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());

    let svc = MemoryService::new(repo.clone());
    let req = |c: &str| IngestMemoryRequest {
        tenant: "t".into(),
        memory_type: MemoryType::Implementation,
        content: c.into(),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    };
    let r = svc.ingest(req("first")).await.unwrap();
    embedding_worker::tick(&repo, provider.as_ref(), &settings).await.unwrap();
    assert_eq!(idx.size(), 1);

    repo.delete_memory_embedding(&r.memory_id).await.unwrap();
    assert_eq!(
        repo.count_memory_embeddings_for_memory(&r.memory_id).await.unwrap(),
        0,
    );
    assert_eq!(
        idx.size(),
        0,
        "vector_index must mirror delete_memory_embedding"
    );

    // Carryover from Task 4 review: double-remove must be safe.
    repo.delete_memory_embedding(&r.memory_id).await.unwrap();
    assert_eq!(idx.size(), 0);
}
