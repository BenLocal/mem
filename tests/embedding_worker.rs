use mem::{
    config::EmbeddingSettings,
    domain::memory::{
        IngestMemoryRequest, MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility, WriteMode,
    },
    embedding::arc_embedding_provider,
    service::{embedding_worker, MemoryService},
    storage::{DuckDbRepository, EmbeddingJobInsert},
};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
async fn worker_completes_job_and_writes_embedding_row() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("worker-ok.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();

    let service = MemoryService::new(repo.clone());
    let request = IngestMemoryRequest {
        tenant: "tenant-w".into(),
        memory_type: MemoryType::Implementation,
        content: "pin async_trait to 0.1 for MSRV".into(),
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
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    };
    let response = service.ingest(request).await.unwrap();

    let job_id = repo
        .first_embedding_job_id_for_memory(&response.memory_id)
        .await
        .unwrap()
        .expect("job row");

    embedding_worker::tick(&repo, provider.as_ref(), &settings)
        .await
        .unwrap();

    assert_eq!(
        repo.get_embedding_job_status(&job_id)
            .await
            .unwrap()
            .as_deref(),
        Some("completed")
    );
    assert_eq!(
        repo.count_memory_embeddings_for_memory(&response.memory_id)
            .await
            .unwrap(),
        1
    );

    // Vector index reflects the new row when one is attached.
    let fp = mem::storage::VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(
        mem::storage::VectorIndex::open_or_rebuild(&repo, &db, &fp)
            .await
            .unwrap(),
    );
    repo.attach_vector_index(idx.clone());
    // open_or_rebuild's rebuild path populates from existing memory_embeddings
    assert_eq!(idx.size(), 1);
}

#[tokio::test]
async fn worker_writes_to_attached_vector_index() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("worker-vec.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();

    let fp = mem::storage::VectorIndexFingerprint {
        provider: settings.job_provider_id().to_string(),
        model: settings.model.clone(),
        dim: settings.dim,
    };
    let idx = Arc::new(
        mem::storage::VectorIndex::open_or_rebuild(&repo, &db, &fp)
            .await
            .unwrap(),
    );
    repo.attach_vector_index(idx.clone());
    assert_eq!(idx.size(), 0);

    let service = MemoryService::new(repo.clone());
    let response = service
        .ingest(IngestMemoryRequest {
            tenant: "t".into(),
            memory_type: MemoryType::Implementation,
            content: "wire-up content".into(),
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
            source_agent: "test".into(),
            idempotency_key: None,
            write_mode: WriteMode::Auto,
        })
        .await
        .unwrap();

    embedding_worker::tick(&repo, provider.as_ref(), &settings)
        .await
        .unwrap();

    assert_eq!(idx.size(), 1);
    let q = provider.embed_text("wire-up content").await.unwrap();
    let hits = idx.search(&q, 1).await.unwrap();
    assert_eq!(hits[0].0, response.memory_id);
}

#[tokio::test]
async fn worker_marks_stale_when_job_target_hash_mismatches_memory() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("worker-stale.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = EmbeddingSettings::development_defaults();
    let provider = arc_embedding_provider(&settings).unwrap();

    let memory = MemoryRecord {
        memory_id: "mem_stale_1".into(),
        tenant: "tenant-s".into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "s".into(),
        content: "body".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: "actual-hash".into(),
        idempotency_key: None,
        session_id: None,
        supersedes_memory_id: None,
        source_agent: "test".into(),
        created_at: "1".into(),
        updated_at: "1".into(),
        last_validated_at: None,
    };
    repo.insert_memory(memory).await.unwrap();

    let ts = "00000000000000000000".to_string();
    let job_id = "ej_stale_manual".to_string();
    assert!(repo
        .try_enqueue_embedding_job(EmbeddingJobInsert {
            job_id: job_id.clone(),
            tenant: "tenant-s".into(),
            memory_id: "mem_stale_1".into(),
            target_content_hash: "outdated-job-hash".into(),
            provider: "fake".into(),
            available_at: ts.clone(),
            created_at: ts.clone(),
            updated_at: ts,
        })
        .await
        .unwrap());

    embedding_worker::tick(&repo, provider.as_ref(), &settings)
        .await
        .unwrap();

    assert_eq!(
        repo.get_embedding_job_status(&job_id)
            .await
            .unwrap()
            .as_deref(),
        Some("stale")
    );
    assert_eq!(
        repo.count_memory_embeddings_for_memory("mem_stale_1")
            .await
            .unwrap(),
        0
    );
}
