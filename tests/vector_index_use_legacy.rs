use mem::config::{EmbeddingProviderKind, EmbeddingSettings};
use mem::domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode};
use mem::embedding::arc_embedding_provider;
use mem::service::{embedding_worker, MemoryService};
use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn use_legacy_env_skips_vector_index() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("legacy.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    // Pin to Fake provider so the test runs deterministically without an
    // EmbedAnything model present in the test environment.
    let mut settings = EmbeddingSettings::development_defaults();
    settings.provider = EmbeddingProviderKind::Fake;
    settings.model = "fake".to_string();
    settings.dim = 64;
    let provider = arc_embedding_provider(&settings).unwrap();
    let fp = VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(VectorIndex::open_or_rebuild(&repo, &db, &fp).await.unwrap());
    repo.attach_vector_index(idx.clone());

    let svc = MemoryService::new_with_settings(repo.clone(), &settings);
    let _ = svc
        .ingest(IngestMemoryRequest {
            tenant: "t".into(),
            memory_type: MemoryType::Implementation,
            content: "legacy-target".into(),
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
        })
        .await
        .unwrap();
    embedding_worker::tick(&repo, provider.as_ref(), &settings)
        .await
        .unwrap();

    let q = provider.embed_text("legacy-target").await.unwrap();

    // Default path (ANN)
    unsafe {
        std::env::remove_var("MEM_VECTOR_INDEX_USE_LEGACY");
    }
    let ann_hits = repo.semantic_search_memories("t", &q, 1).await.unwrap();

    // Legacy path
    unsafe {
        std::env::set_var("MEM_VECTOR_INDEX_USE_LEGACY", "1");
    }
    let legacy_hits = repo.semantic_search_memories("t", &q, 1).await.unwrap();
    unsafe {
        std::env::remove_var("MEM_VECTOR_INDEX_USE_LEGACY");
    }

    assert_eq!(ann_hits.len(), 1);
    assert_eq!(legacy_hits.len(), 1);
    assert_eq!(ann_hits[0].0.memory_id, legacy_hits[0].0.memory_id);
}
