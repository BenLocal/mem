use serde_json::json;
use mem::domain::{
    episode::{EpisodeRecord, EpisodeResponse, IngestEpisodeRequest},
    memory::{
        EditPendingRequest, EditPendingResponse, FeedbackSummary, GraphEdge, IngestMemoryRequest,
        MemoryDetailResponse, MemoryRecord, MemoryStatus, MemoryType, MemoryVersionLink, Scope,
        Visibility, WriteMode,
    },
    workflow::WorkflowCandidate,
};

fn sample_memory_record() -> MemoryRecord {
    MemoryRecord {
        memory_id: "mem_123".into(),
        tenant: "local".into(),
        memory_type: MemoryType::Implementation,
        status: MemoryStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 2,
        summary: "cache invalidation rule".into(),
        content: "Invalidate cache when schema version changes".into(),
        evidence: vec!["docs/notes.md".into()],
        code_refs: vec!["src/cache.rs".into()],
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("cache".into()),
        task_type: Some("bugfix".into()),
        tags: vec!["cache".into(), "schema".into()],
        confidence: 0.95,
        decay_score: 0.05,
        content_hash: "abc123".into(),
        idempotency_key: Some("idem-1".into()),
        supersedes_memory_id: Some("mem_122".into()),
        source_agent: "codex-worker".into(),
        created_at: "2026-03-21T00:00:00Z".into(),
        updated_at: "2026-03-21T00:05:00Z".into(),
        last_validated_at: Some("2026-03-21T00:06:00Z".into()),
    }
}

#[test]
fn ingest_request_serializes_expected_shape() {
    let request = IngestMemoryRequest {
        tenant: "local".into(),
        memory_type: MemoryType::Implementation,
        content: "cache invalidation rule".into(),
        evidence: vec!["docs/notes.md".into()],
        code_refs: vec!["src/cache.rs".into()],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: Some("memory-service".into()),
        repo: Some("mem".into()),
        module: Some("cache".into()),
        task_type: Some("bugfix".into()),
        tags: vec!["cache".into(), "schema".into()],
        source_agent: "codex-worker".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    };

    let value = serde_json::to_value(request).unwrap();

    assert_eq!(value["scope"], "repo");
    assert_eq!(value["write_mode"], "auto");
    assert_eq!(value["memory_type"], "implementation");
    assert!(value.get("idempotency_key").is_none());
}

#[test]
fn ingest_request_missing_required_field_fails_deserialization() {
    let value = json!({
        "tenant": "local",
        "memory_type": "implementation",
        "evidence": ["docs/notes.md"],
        "code_refs": ["src/cache.rs"],
        "scope": "repo",
        "visibility": "shared",
        "tags": ["cache"],
        "source_agent": "codex-worker",
        "write_mode": "auto"
    });

    let result = serde_json::from_value::<IngestMemoryRequest>(value);

    assert!(result.is_err());
}

#[test]
fn edit_pending_request_serializes_required_fields() {
    let request = EditPendingRequest {
        memory_id: "mem_123".into(),
        summary: "cache invalidation rule".into(),
        content: "Invalidate cache when schema version changes".into(),
        evidence: vec!["docs/notes.md".into()],
        code_refs: vec!["src/cache.rs".into()],
        tags: vec!["cache".into(), "schema".into()],
    };

    let value = serde_json::to_value(request).unwrap();

    assert_eq!(value["memory_id"], "mem_123");
    assert_eq!(value["summary"], "cache invalidation rule");
    assert_eq!(value["tags"][0], "cache");
}

#[test]
fn write_mode_propose_serializes_expected_shape() {
    let request = IngestMemoryRequest {
        tenant: "local".into(),
        memory_type: MemoryType::Preference,
        content: "prefer concise answers".into(),
        evidence: vec!["user request".into()],
        code_refs: vec![],
        scope: Scope::Global,
        visibility: Visibility::Private,
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec!["preference".into()],
        source_agent: "codex-worker".into(),
        idempotency_key: None,
        write_mode: WriteMode::Propose,
    };

    let value = serde_json::to_value(request).unwrap();

    assert_eq!(value["write_mode"], "propose");
}

#[test]
fn memory_detail_response_serializes_full_shape() {
    let response = MemoryDetailResponse {
        memory: sample_memory_record(),
        version_chain: vec![MemoryVersionLink {
            memory_id: "mem_122".into(),
            version: 1,
            status: MemoryStatus::Archived,
            updated_at: "2026-03-20T23:59:00Z".into(),
            supersedes_memory_id: None,
        }],
        graph_links: vec![GraphEdge {
            from_node_id: "memory:mem_123".into(),
            to_node_id: "repo:mem".into(),
            relation: "observed_in".into(),
        }],
        feedback_summary: FeedbackSummary {
            total: 3,
            useful: 2,
            outdated: 1,
            incorrect: 0,
            applies_here: 0,
            does_not_apply_here: 0,
        },
    };

    let value = serde_json::to_value(response).unwrap();

    assert_eq!(value["memory"]["memory_id"], "mem_123");
    assert_eq!(value["version_chain"][0]["status"], "archived");
    assert_eq!(value["graph_links"][0]["relation"], "observed_in");
    assert_eq!(value["feedback_summary"]["useful"], 2);
}

#[test]
fn edit_pending_response_serializes_updated_memory() {
    let response = EditPendingResponse {
        original_memory_id: "mem_123".into(),
        memory: sample_memory_record(),
    };

    let value = serde_json::to_value(response).unwrap();

    assert_eq!(value["original_memory_id"], "mem_123");
    assert_eq!(value["memory"]["status"], "active");
}

#[test]
fn episode_request_and_response_round_trip_shapes() {
    let request = IngestEpisodeRequest {
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
    };

    let request_value = serde_json::to_value(request).unwrap();
    assert_eq!(request_value["goal"], "debug invoice retries");
    assert!(request_value.get("idempotency_key").is_some());

    let response = EpisodeResponse {
        episode_id: "episode_1".into(),
        status: "recorded".into(),
        workflow_candidate: Some(WorkflowCandidate {
            memory_id: None,
            goal: "debug invoice retries".into(),
            preconditions: vec!["start with logs".into()],
            steps: vec!["inspect logs".into(), "trace job".into(), "verify fix".into()],
            decision_points: vec!["check retry count".into()],
            success_signals: vec!["fixed".into()],
            failure_signals: vec!["still failing".into()],
            evidence: vec!["docs/ops.md".into()],
            scope: Scope::Workspace,
        }),
    };

    let response_value = serde_json::to_value(response).unwrap();
    assert_eq!(response_value["episode_id"], "episode_1");
    assert_eq!(response_value["workflow_candidate"]["goal"], "debug invoice retries");
}

#[test]
fn episode_record_serializes_expected_shape() {
    let record = EpisodeRecord {
        episode_id: "episode_1".into(),
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
        updated_at: "2026-03-21T00:01:00Z".into(),
        workflow_candidate: None,
    };

    let value = serde_json::to_value(record).unwrap();

    assert_eq!(value["episode_id"], "episode_1");
    assert_eq!(value["scope"], "workspace");
    assert!(value.get("workflow_candidate").is_none());
}
