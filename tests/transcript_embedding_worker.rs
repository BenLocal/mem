//! Integration tests for the transcript embedding worker.
//!
//! Mirrors `tests/embedding_worker.rs` but exercises the parallel transcript
//! pipeline introduced in Task 7 of the conversation-archive plan.

use std::sync::Arc;

use async_trait::async_trait;
use mem::config::{EmbeddingProviderKind, EmbeddingSettings};
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::{EmbeddingError, EmbeddingProvider, FakeEmbeddingProvider};
use mem::service::transcript_embedding_worker;
use mem::storage::{DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use tempfile::TempDir;

/// Build an `EmbeddingSettings` with `provider = EmbedAnything` so the worker's
/// `job.provider != settings.job_provider_id()` sanity check passes — the
/// repository hardcodes `"embedanything"` on every transcript job (see Task 8
/// TODO in `transcript_repo.rs::create_conversation_message`). The actual
/// embedder we hand to `tick` is a fake; the settings only gate the provider
/// id check, not the embedding work.
fn test_settings(dim: usize, max_retries: u32) -> EmbeddingSettings {
    let mut s = EmbeddingSettings::development_defaults();
    s.provider = EmbeddingProviderKind::EmbedAnything;
    s.dim = dim;
    s.max_retries = max_retries;
    s.worker_poll_interval_ms = 50;
    s.vector_index_flush_every = 1;
    s
}

fn sample_message(suffix: &str, content: &str) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("mb-{suffix}"),
        session_id: Some("sess".to_string()),
        tenant: "local".to_string(),
        caller_agent: "claude-code".to_string(),
        transcript_path: format!("/tmp/{suffix}.jsonl"),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type: BlockType::Text,
        content: content.to_string(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: true,
        created_at: "2026-04-30T00:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn worker_processes_pending_transcript_jobs_and_writes_to_index() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let dim = 8;
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", dim));
    let fp = VectorIndexFingerprint {
        provider: "embedanything".to_string(),
        model: provider.model().to_string(),
        dim,
    };
    let index = Arc::new(
        VectorIndex::open_or_rebuild_transcripts(&repo, &db, &fp)
            .await
            .unwrap(),
    );

    let msg = sample_message("ok", "hello world");
    repo.create_conversation_message(&msg).await.unwrap();

    let settings = test_settings(dim, 5);
    transcript_embedding_worker::tick(&repo, provider.as_ref(), &settings, &index)
        .await
        .unwrap();

    // Job is completed and index has 1 row.
    let conn = duckdb::Connection::open(&db).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM transcript_embedding_jobs LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(index.size(), 1);

    let emb_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM conversation_message_embeddings",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(emb_count, 1);
}

/// Provider that always returns an error from `embed_text` — used to verify
/// that a transcript provider failure does not interfere with the memories
/// pipeline (its errors are confined to the transcript queue).
struct AlwaysFailingProvider {
    dim: usize,
}

#[async_trait]
impl EmbeddingProvider for AlwaysFailingProvider {
    fn name(&self) -> &'static str {
        "embedanything"
    }
    fn model(&self) -> &str {
        "fail-model"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::Internal(
            "simulated transcript failure".into(),
        ))
    }
}

#[tokio::test]
async fn worker_failure_does_not_affect_memories_pipeline() {
    use mem::domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode};
    use mem::embedding::arc_embedding_provider;
    use mem::service::{embedding_worker, MemoryService};

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let dim = 8;
    // Memories provider: a real fake (succeeds).
    let mut memory_settings = EmbeddingSettings::development_defaults();
    memory_settings.provider = EmbeddingProviderKind::Fake;
    memory_settings.model = "fake".to_string();
    memory_settings.dim = dim;
    memory_settings.max_retries = 2;
    let memory_provider = arc_embedding_provider(&memory_settings).unwrap();

    // Transcript provider: always fails.
    let transcript_provider: Arc<dyn EmbeddingProvider> = Arc::new(AlwaysFailingProvider { dim });
    let transcript_settings = test_settings(dim, 2);

    // Memories pipeline ingest.
    let service = MemoryService::new(repo.clone());
    let response = service
        .ingest(IngestMemoryRequest {
            tenant: "tenant".into(),
            memory_type: MemoryType::Implementation,
            content: "memory content body".into(),
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

    // Transcript ingest.
    let msg = sample_message("isolation", "transcript content");
    repo.create_conversation_message(&msg).await.unwrap();

    // Transcript index (separate from memories index, which we don't attach
    // here — memories test never needs ANN to assert job state).
    let fp = VectorIndexFingerprint {
        provider: "embedanything".to_string(),
        model: transcript_provider.model().to_string(),
        dim,
    };
    let transcript_index = Arc::new(
        VectorIndex::open_or_rebuild_transcripts(&repo, &db, &fp)
            .await
            .unwrap(),
    );

    // Run one tick of each worker. The memories tick must succeed, the
    // transcript tick must record a failure (without panicking).
    embedding_worker::tick(&repo, memory_provider.as_ref(), &memory_settings)
        .await
        .unwrap();
    transcript_embedding_worker::tick(
        &repo,
        transcript_provider.as_ref(),
        &transcript_settings,
        &transcript_index,
    )
    .await
    .unwrap();

    // Memories job should be completed.
    let mem_job_id = repo
        .first_embedding_job_id_for_memory(&response.memory_id)
        .await
        .unwrap()
        .expect("memory job row");
    assert_eq!(
        repo.get_embedding_job_status(&mem_job_id)
            .await
            .unwrap()
            .as_deref(),
        Some("completed"),
        "memories worker should have completed independently of the transcript failure"
    );
    assert_eq!(
        repo.count_memory_embeddings_for_memory(&response.memory_id)
            .await
            .unwrap(),
        1
    );

    // Transcript job should be failed (rescheduled or permanently) — never
    // completed, never panicking.
    let conn = duckdb::Connection::open(&db).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM transcript_embedding_jobs LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "failed",
        "transcript worker error path should mark the job 'failed'"
    );
    let attempts: i64 = conn
        .query_row(
            "SELECT attempt_count FROM transcript_embedding_jobs LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 1, "first failure increments attempt_count to 1");

    // No transcript embedding row was written.
    let emb_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM conversation_message_embeddings",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(emb_count, 0);
}
