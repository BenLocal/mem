use mem::{
    domain::memory::{
        IngestMemoryRequest, MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility, WriteMode,
    },
    http,
    service::MemoryService,
    storage::{DuckDbRepository, EmbeddingJobInsert},
};
use serde_json::json;
use tempfile::tempdir;
use tower::util::ServiceExt;

mod common;

fn sample_active_memory(memory_id: &str, tenant: &str, content_hash: &str) -> MemoryRecord {
    MemoryRecord {
        memory_id: memory_id.into(),
        tenant: tenant.into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "s".into(),
        content: "c".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: content_hash.into(),
        idempotency_key: None,
        session_id: None,
        supersedes_memory_id: None,
        source_agent: "test".into(),
        created_at: "1".into(),
        updated_at: "1".into(),
        last_validated_at: None,
    }
}

#[tokio::test]
async fn try_enqueue_dedupes_live_jobs_same_fingerprint() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ej.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.insert_memory(sample_active_memory("mem_ej_1", "t1", "hash_a"))
        .await
        .unwrap();

    let now = "20260000000000000001".to_string();
    let base = |job_id: &str| EmbeddingJobInsert {
        job_id: job_id.into(),
        tenant: "t1".into(),
        memory_id: "mem_ej_1".into(),
        target_content_hash: "hash_a".into(),
        provider: "fake".into(),
        available_at: now.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
    };

    assert!(repo.try_enqueue_embedding_job(base("ej_1")).await.unwrap());
    assert!(!repo.try_enqueue_embedding_job(base("ej_2")).await.unwrap());
    assert_eq!(
        repo.count_embedding_jobs_for_memory("mem_ej_1")
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn stale_live_embedding_jobs_allow_fresh_enqueue_same_fingerprint() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ej-stale.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.insert_memory(sample_active_memory("mem_ej_stale", "t1", "hash_a"))
        .await
        .unwrap();

    let now = "20260000000000000001".to_string();
    let insert = |job_id: &str| EmbeddingJobInsert {
        job_id: job_id.into(),
        tenant: "t1".into(),
        memory_id: "mem_ej_stale".into(),
        target_content_hash: "hash_a".into(),
        provider: "fake".into(),
        available_at: now.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
    };

    assert!(repo
        .try_enqueue_embedding_job(insert("ej_a"))
        .await
        .unwrap());
    assert!(!repo
        .try_enqueue_embedding_job(insert("ej_b"))
        .await
        .unwrap());

    let n = repo
        .stale_live_embedding_jobs_for_memory("t1", "mem_ej_stale", "fake", &now)
        .await
        .unwrap();
    assert_eq!(n, 1);

    assert!(repo
        .try_enqueue_embedding_job(insert("ej_c"))
        .await
        .unwrap());
    assert_eq!(
        repo.count_embedding_jobs_for_memory("mem_ej_stale")
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn ingest_creates_one_embedding_job() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ingest-ej.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let service = MemoryService::new(repo.clone());

    let request = IngestMemoryRequest {
        tenant: "tenant-embed".into(),
        memory_type: MemoryType::Implementation,
        content: "use tokio for async boundaries".into(),
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

    let response = service.ingest(request).await.unwrap();
    assert_eq!(
        repo.count_embedding_jobs_for_memory(&response.memory_id)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn http_ingest_creates_embedding_job() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("http-ej.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();
    let state = common::test_app_state(repo.clone(), MemoryService::new(repo.clone()));
    let router = http::router().with_state(state);

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/memories")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({
                "memory_type": "implementation",
                "content": "document public API contracts",
                "scope": "repo",
                "visibility": "shared",
                "repo": "mem",
            })
            .to_string(),
        ))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let memory_id = value["memory_id"].as_str().unwrap();
    assert_eq!(
        repo.count_embedding_jobs_for_memory(memory_id)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn count_total_memory_embeddings_returns_zero_for_empty_db() {
    use tempfile::tempdir;
    let dir = tempdir().unwrap();
    let db = dir.path().join("count-empty.duckdb");
    let repo = mem::storage::DuckDbRepository::open(&db).await.unwrap();
    assert_eq!(repo.count_total_memory_embeddings().await.unwrap(), 0);
}

#[tokio::test]
async fn iter_memory_embeddings_visits_each_row() {
    use mem::storage::EmbeddingRowSource;
    use tempfile::tempdir;
    let dir = tempdir().unwrap();
    let db = dir.path().join("iter.duckdb");
    let repo = mem::storage::DuckDbRepository::open(&db).await.unwrap();
    repo.seed_memory_embedding_for_test("mem_a", "tenant-x", &[1.0, 0.0])
        .await
        .unwrap();
    repo.seed_memory_embedding_for_test("mem_b", "tenant-x", &[0.0, 1.0])
        .await
        .unwrap();

    let mut seen = Vec::new();
    repo.for_each_embedding(100, &mut |id, blob| {
        seen.push((id.to_string(), blob.to_vec()));
        Ok(())
    })
    .unwrap();
    assert_eq!(seen.len(), 2);
    let ids: std::collections::HashSet<_> = seen.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains("mem_a"));
    assert!(ids.contains("mem_b"));
}

#[tokio::test]
async fn migrate_content_hash_handles_legacy_row_with_children() {
    // Regression for incident TODO #2 (see mem incident memory
    // mem_019dfba4-9e08-71b2-a676-f0218c01f9b6). Pre-fix the legacy
    // content_hash → sha256 migration ran a naive `UPDATE memories SET
    // content_hash = …`, which DuckDB implements as DELETE+INSERT on the
    // parent — and the DELETE half raises FK RESTRICT whenever any
    // `embedding_jobs` / `memory_embeddings` row references the memory.
    //
    // Result: any legacy DB that already had children failed to bootstrap
    // (`mem serve` could not start). The fix lifts the children out, runs
    // the parent UPDATE, then restores the children with the new sha256
    // propagated to `memory_embeddings.content_hash`.
    let dir = tempdir().unwrap();
    let db = dir.path().join("legacy-hash.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    // 16 hex chars = the legacy DefaultHasher digest size, triggers
    // `length(content_hash) != CONTENT_HASH_LEN` in the migration.
    let legacy_hash = "abcdef0123456789";
    repo.insert_memory(sample_active_memory("mem_legacy", "tL", legacy_hash))
        .await
        .unwrap();

    let now = "20260000000000000001".to_string();
    assert!(repo
        .try_enqueue_embedding_job(EmbeddingJobInsert {
            job_id: "ej_legacy".into(),
            tenant: "tL".into(),
            memory_id: "mem_legacy".into(),
            target_content_hash: legacy_hash.into(),
            provider: "fake".into(),
            available_at: now.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .await
        .unwrap());

    // Seeds a `memory_embeddings` row (the parent insert is skipped via
    // `INSERT OR IGNORE` because mem_legacy already exists).
    repo.seed_memory_embedding_for_test("mem_legacy", "tL", &[1.0, 0.0, 0.0])
        .await
        .unwrap();
    drop(repo);

    // Re-open. Pre-fix this would propagate
    //   "Constraint Error: \"memories\" is still referenced by a foreign key"
    // out of `bootstrap()` and the open call would fail.
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let migrated = repo
        .get_memory_for_tenant("tL", "mem_legacy")
        .await
        .unwrap()
        .expect("memory must survive migration");
    assert_eq!(
        migrated.content_hash.len(),
        64,
        "content_hash must be sha256 (64 hex chars) after migration"
    );
    assert_ne!(
        migrated.content_hash, legacy_hash,
        "content_hash must have changed from legacy form"
    );

    assert_eq!(
        repo.count_embedding_jobs_for_memory("mem_legacy")
            .await
            .unwrap(),
        1,
        "embedding_jobs child must survive the legacy-hash migration"
    );
    assert_eq!(
        repo.count_total_memory_embeddings().await.unwrap(),
        1,
        "memory_embeddings child must survive the legacy-hash migration"
    );
}

#[tokio::test]
async fn open_time_sweep_is_idempotent_on_healthy_db() {
    // Sanity check: the open-time orphan sweep (added in 4ca5a75 to break the
    // FK retry loop) must not delete anything on a healthy DB. Regression guard
    // confirming we don't accidentally delete legitimate embedding_jobs rows.
    //
    // The actual orphan-deletion path can't be unit-tested without bypassing
    // DuckDB FK (PRAGMA foreign_keys=OFF is unsupported by the bundled version),
    // so the destructive path is exercised manually via `mem repair
    // --prune-embedding-orphans`.
    let dir = tempdir().unwrap();
    let db = dir.path().join("healthy-open.duckdb");
    // Use a 64-char SHA-256-shaped hash so re-open does NOT trigger
    // migrate_content_hash_to_sha256, whose `UPDATE memories SET content_hash`
    // would otherwise FK-error (DuckDB implements UPDATE-on-parent as
    // DELETE+INSERT internally; the DELETE half fires FK when children exist).
    // That migration path is orthogonal to the open-time orphan sweep we want
    // to exercise here.
    let sha = "0".repeat(64);
    let repo = DuckDbRepository::open(&db).await.unwrap();
    repo.insert_memory(sample_active_memory("mem_h_1", "tA", &sha))
        .await
        .unwrap();
    let now = "20260000000000000001".to_string();
    let insert = EmbeddingJobInsert {
        job_id: "ej_h_1".into(),
        tenant: "tA".into(),
        memory_id: "mem_h_1".into(),
        target_content_hash: sha.clone(),
        provider: "fake".into(),
        available_at: now.clone(),
        created_at: now.clone(),
        updated_at: now,
    };
    assert!(repo.try_enqueue_embedding_job(insert).await.unwrap());
    drop(repo);

    // Re-open: the open-time sweep runs. The healthy job must survive.
    let repo = DuckDbRepository::open(&db).await.unwrap();
    assert_eq!(
        repo.count_embedding_jobs_for_memory("mem_h_1")
            .await
            .unwrap(),
        1,
        "open-time sweep must not touch healthy embedding_jobs rows"
    );
}
