use mem::{
    domain::{
        episode::EpisodeRecord,
        memory::{MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility},
    },
    storage::{duckdb::DuckDbRepository, FeedbackEvent},
};
use tempfile::tempdir;

fn sample_episode() -> EpisodeRecord {
    EpisodeRecord {
        episode_id: "ep_123".into(),
        tenant: "local".into(),
        goal: "debug invoice retries".into(),
        steps: vec!["inspect logs".into(), "trace job".into(), "verify fix".into()],
        outcome: "success".into(),
        evidence: vec!["docs/ops.md".into()],
        scope: Scope::Workspace,
        visibility: Visibility::Private,
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("runtime".into()),
        tags: vec!["debugging".into()],
        source_agent: "codex-worker".into(),
        idempotency_key: Some("episode-1".into()),
        created_at: "2026-03-21T00:00:00Z".into(),
        updated_at: "2026-03-21T00:10:00Z".into(),
        workflow_candidate: None,
    }
}

async fn test_duckdb_repo() -> DuckDbRepository {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("workflow-test.duckdb");
    DuckDbRepository::open(&db_path).await.unwrap()
}

#[tokio::test]
async fn storage_schema_bootstraps_feedback_and_episode_tables() {
    let repo = test_duckdb_repo().await;
    let episode = sample_episode();
    let feedback = FeedbackEvent {
        feedback_id: "fb_123".into(),
        memory_id: "mem_123".into(),
        feedback_kind: "useful".into(),
        created_at: "2026-03-21T00:15:00Z".into(),
    };

    repo.insert_feedback(feedback.clone()).await.unwrap();
    repo.insert_episode(episode.clone()).await.unwrap();

    let feedback_rows = repo.list_feedback_for_memory("mem_123").await.unwrap();
    let stored_episode = repo.get_episode(&episode.episode_id).await.unwrap().unwrap();

    assert_eq!(feedback_rows, vec![feedback]);
    assert_eq!(stored_episode, episode);
}

fn sample_versioned_memory(memory_id: &str, version: u64, supersedes: Option<&str>) -> MemoryRecord {
    MemoryRecord {
        memory_id: memory_id.into(),
        tenant: "local".into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version,
        summary: format!("summary-{memory_id}"),
        content: "stored content".into(),
        evidence: vec!["docs/review.md".into()],
        code_refs: vec!["src/review.rs".into()],
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("storage".into()),
        task_type: Some("review".into()),
        tags: vec!["version".into()],
        confidence: 0.7,
        decay_score: 0.2,
        content_hash: format!("hash-{memory_id}"),
        idempotency_key: None,
        supersedes_memory_id: supersedes.map(str::to_string),
        source_agent: "codex-worker".into(),
        created_at: format!("2026-03-21T00:00:0{version}Z"),
        updated_at: format!("2026-03-21T00:05:0{version}Z"),
        last_validated_at: None,
    }
}

#[tokio::test]
async fn duckdb_repository_lists_related_memory_versions() {
    let repo = test_duckdb_repo().await;
    let original = sample_versioned_memory("mem_001", 1, None);
    let replacement = sample_versioned_memory("mem_002", 2, Some("mem_001"));
    repo.insert_memory(original).await.unwrap();
    repo.insert_memory(replacement).await.unwrap();

    let versions = repo.list_memory_versions("mem_001").await.unwrap();

    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version, 2);
    assert_eq!(versions[1].version, 1);
}
