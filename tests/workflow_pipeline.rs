use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    app::AppState,
    domain::{
        episode::EpisodeRecord,
        memory::{MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility},
    },
    http,
    storage::{duckdb::DuckDbRepository, FeedbackEvent},
};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    repo: DuckDbRepository,
}

struct TestResponse {
    status: StatusCode,
    body: String,
}

impl TestResponse {
    fn status(&self) -> u16 {
        self.status.as_u16()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("body should be valid json")
    }
}

impl TestApp {
    async fn get(&self, path: &str) -> TestResponse {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("request should build");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("request should succeed");
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        TestResponse {
            status,
            body: String::from_utf8(body.to_vec()).expect("body should be utf-8"),
        }
    }

    async fn post_json(&self, path: &str, body: Value) -> TestResponse {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request should build");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("request should succeed");
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        TestResponse {
            status,
            body: String::from_utf8(body.to_vec()).expect("body should be utf-8"),
        }
    }
}

async fn test_app() -> TestApp {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("workflow-test.duckdb");
    let repo = DuckDbRepository::open(&db_path).await.unwrap();
    let state = AppState {
        memory_service: mem::service::MemoryService::new(repo.clone()),
    };

    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        repo,
    }
}

fn sample_episode() -> EpisodeRecord {
    EpisodeRecord {
        episode_id: "ep_123".into(),
        tenant: "local".into(),
        goal: "debug invoice retries".into(),
        steps: vec![
            "inspect logs".into(),
            "trace job".into(),
            "verify fix".into(),
        ],
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
    let stored_episode = repo
        .get_episode(&episode.episode_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(feedback_rows, vec![feedback]);
    assert_eq!(stored_episode, episode);
}

fn sample_versioned_memory(
    memory_id: &str,
    version: u64,
    supersedes: Option<&str>,
) -> MemoryRecord {
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

#[tokio::test]
async fn ingest_episode_persists_successful_run() {
    let app = test_app().await;
    let response = app
        .post_json(
            "/episodes",
            json!({
                "goal": "debug invoice retries",
                "steps": ["inspect logs", "trace job", "verify fix"],
                "outcome": "success"
            }),
        )
        .await;

    assert_eq!(response.status(), 201);
    assert_eq!(response.json()["status"], "created");

    let response_json = response.json();
    let episode_id = response_json["episode_id"]
        .as_str()
        .expect("episode id should be present");
    let stored_episode = app
        .repo
        .get_episode(episode_id)
        .await
        .unwrap()
        .expect("episode should be persisted");

    assert_eq!(stored_episode.goal, "debug invoice retries");
    assert_eq!(stored_episode.outcome, "success");
}

#[tokio::test]
async fn repeated_successful_episodes_produce_workflow_candidate() {
    let app = test_app().await;

    for _ in 0..2 {
        let response = app
            .post_json(
                "/episodes",
                json!({
                    "goal": "debug invoice retries",
                    "steps": ["inspect logs", "trace job", "verify fix"],
                    "outcome": "success"
                }),
            )
            .await;
        assert_eq!(response.status(), 201);
    }

    let pending = app.get("/reviews/pending?tenant=local").await;
    assert_eq!(pending.status(), 200);
    let pending_json = pending.json();
    let pending_rows = pending_json
        .as_array()
        .expect("pending rows should be an array");
    assert!(pending_rows
        .iter()
        .any(|row| row["memory_type"] == "workflow" && row["status"] == "pending_confirmation"));
}
