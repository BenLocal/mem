use mem::{
    domain::memory::{FeedbackSummary, MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility},
    storage::duckdb::DuckDbRepository,
};
use tempfile::tempdir;

fn sample_memory(memory_id: &str, status: MemoryStatus) -> MemoryRecord {
    MemoryRecord {
        memory_id: memory_id.into(),
        tenant: "local".into(),
        memory_type: MemoryType::Preference,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{memory_id}"),
        content: "stored content".into(),
        evidence: vec!["docs/review.md".into()],
        code_refs: vec!["src/review.rs".into()],
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("review".into()),
        task_type: Some("review".into()),
        tags: vec!["review".into()],
        confidence: 0.7,
        decay_score: 0.2,
        content_hash: format!("hash-{memory_id}"),
        idempotency_key: None,
        supersedes_memory_id: None,
        source_agent: "codex-worker".into(),
        created_at: format!("2026-03-21T00:00:{memory_id}Z"),
        updated_at: format!("2026-03-21T00:05:{memory_id}Z"),
        last_validated_at: None,
    }
}

async fn test_duckdb_repo() -> DuckDbRepository {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("review-test.duckdb");
    DuckDbRepository::open(&db_path).await.unwrap()
}

#[tokio::test]
async fn duckdb_repository_lists_pending_review_rows() {
    let repo = test_duckdb_repo().await;
    repo.insert_memory(sample_memory("001", MemoryStatus::PendingConfirmation))
        .await
        .unwrap();
    repo.insert_memory(sample_memory("002", MemoryStatus::Active))
        .await
        .unwrap();

    let pending = repo.list_pending_review().await.unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].memory_id, "001");
    assert_eq!(pending[0].status, MemoryStatus::PendingConfirmation);
}

#[tokio::test]
async fn duckdb_repository_summarizes_feedback_by_kind() {
    let repo = test_duckdb_repo().await;
    repo.insert_feedback(mem::storage::FeedbackEvent {
        feedback_id: "fb_001".into(),
        memory_id: "mem_123".into(),
        feedback_kind: "useful".into(),
        created_at: "2026-03-21T00:00:01Z".into(),
    })
    .await
    .unwrap();
    repo.insert_feedback(mem::storage::FeedbackEvent {
        feedback_id: "fb_002".into(),
        memory_id: "mem_123".into(),
        feedback_kind: "outdated".into(),
        created_at: "2026-03-21T00:00:02Z".into(),
    })
    .await
    .unwrap();

    let summary = repo.feedback_summary("mem_123").await.unwrap();

    assert_eq!(
        summary,
        FeedbackSummary {
            total: 2,
            useful: 1,
            outdated: 1,
            incorrect: 0,
            applies_here: 0,
            does_not_apply_here: 0,
        }
    );
}
