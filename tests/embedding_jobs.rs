use mem::{
    app::AppState,
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
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: content_hash.into(),
        idempotency_key: None,
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

    assert!(repo.try_enqueue_embedding_job(insert("ej_a")).await.unwrap());
    assert!(!repo.try_enqueue_embedding_job(insert("ej_b")).await.unwrap());

    let n = repo
        .stale_live_embedding_jobs_for_memory("t1", "mem_ej_stale", "fake", &now)
        .await
        .unwrap();
    assert_eq!(n, 1);

    assert!(repo.try_enqueue_embedding_job(insert("ej_c")).await.unwrap());
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
    let state = AppState {
        memory_service: MemoryService::new(repo.clone()),
        config: mem::config::Config::local(),
    };
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
