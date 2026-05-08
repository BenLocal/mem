//! Concurrency tests for the dedicated worker-write `Connection` split.
//!
//! These exercise the architecture introduced by the
//! `feat(storage): dedicated worker write connection via try_clone`
//! change: HTTP write traffic locks `repo.conn`, worker tick traffic
//! locks `repo.worker_conn`, and the two no longer serialize against
//! each other for non-overlapping table writes.

use mem::{
    config::{EmbeddingProviderKind, EmbeddingSettings},
    domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode},
    embedding::arc_embedding_provider,
    service::MemoryService,
    storage::DuckDbRepository,
    worker::embedding_worker,
};
use std::sync::Arc;
use tempfile::tempdir;

fn fake_settings() -> EmbeddingSettings {
    let mut s = EmbeddingSettings::development_defaults();
    s.provider = EmbeddingProviderKind::Fake;
    s.model = "fake".to_string();
    s.dim = 64;
    s
}

fn ingest_request(tenant: &str, content: &str) -> IngestMemoryRequest {
    IngestMemoryRequest {
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
    }
}

/// HTTP-side ingests and worker tick run concurrently to completion.
///
/// With a single shared Mutex they would have serialized; with the
/// split they overlap freely. The assertion is just that nothing
/// deadlocks and every queued job reaches `completed` — the timing win
/// is not asserted here (CI variance), only the absence of a stall.
#[tokio::test]
async fn http_ingest_overlaps_with_worker_tick() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("worker-iso.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let settings = fake_settings();
    let provider = arc_embedding_provider(&settings).unwrap();
    let service = Arc::new(MemoryService::new_with_settings(repo.clone(), &settings));

    let tenant = "tenant-iso";
    let n_ingest: usize = 20;

    // Spawn the HTTP ingest task: queues n_ingest jobs in rapid
    // succession via the HTTP write conn.
    let svc = service.clone();
    let ingest_task = tokio::spawn(async move {
        let mut ids = Vec::with_capacity(n_ingest);
        for i in 0..n_ingest {
            let resp = svc
                .ingest(ingest_request(tenant, &format!("worker-iso fact {i}")))
                .await
                .expect("ingest");
            ids.push(resp.memory_id);
        }
        ids
    });

    // Spawn the worker task: ticks until all jobs are drained or 15 s
    // elapses. Each tick claims via worker_conn → does NOT block on
    // the HTTP-side INSERTs running concurrently. The generous
    // deadline accommodates loaded CI hardware where parallel test
    // files compete for cores.
    let repo_w = repo.clone();
    let worker_task = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            embedding_worker::tick(&repo_w, provider.as_ref(), &settings)
                .await
                .expect("worker tick");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if std::time::Instant::now() > deadline {
                break;
            }
        }
    });

    let ids = ingest_task.await.expect("ingest task");
    worker_task.await.expect("worker task");

    // Every job should have been claimed and completed by the worker.
    let mut completed = 0;
    for id in &ids {
        let count = repo.count_memory_embeddings_for_memory(id).await.unwrap();
        if count == 1 {
            completed += 1;
        }
    }
    assert_eq!(
        completed, n_ingest,
        "expected all {n_ingest} ingested memories to have an embedding row written by the worker",
    );
}
