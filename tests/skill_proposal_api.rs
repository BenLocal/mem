//! Public HTTP/storage coverage for the durable Skill proposal compiler lane.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use mem::{
    domain::{
        capability_capsule::{
            CapabilityCapsuleStatus, CapabilityCapsuleType, SKILL_PROPOSAL_SOURCE_AGENT,
        },
        skill_proposal::SkillProposalDraft,
    },
    domain::{
        skill_candidate_serial_key, BlockType, CompletedToolRound, CompletedToolRoundIndexBuild,
        ConversationMessage, MessageRole, RoundIndexBuildStatus, RoundIntegrity, RoundSealKind,
        SkillCandidateJobSpec, SkillCandidateJobStatus, SkillCandidateRoundRef,
        SkillCandidateTriggerReason, SkillProposalStatus, SourceAdapter,
        COMPLETED_TOOL_ROUND_PROJECTOR_VERSION, COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        SKILL_CANDIDATE_TRIGGER_VERSION,
    },
    http,
    pipeline::skill_proposal_compiler::canonical_proposal_signature,
    service::{
        CapabilityCapsuleService, PublishSkillProposalOutcome, PublishSkillProposalRequest,
        SkillCompileClaimBatch,
    },
    storage::{CompletedToolRoundStore, SkillCandidateStore, SkillStore, Store},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::util::ServiceExt;

mod common;

const TENANT: &str = "local";
const AGENT: &str = "codex";
const SESSION: &str = "skill-proposal-session";
const TRANSCRIPT: &str = "/workspace/mem/skill-proposal-session.jsonl";
const ROUND_ID: &str = "round-skill-proposal";
const GENERATION_ID: &str = "generation-skill-proposal";
const JOB_ID: &str = "job-skill-proposal";
const ROUND_FINGERPRINT: &str = "round-source-fingerprint";
const CREATED_AT: &str = "00000001786000000000";

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    store: Arc<Store>,
}

async fn test_app() -> TestApp {
    let (temp_dir, store) = common::test_store().await;
    let capsule_service = CapabilityCapsuleService::new(store.clone());
    let state = common::test_app_state(store.clone(), capsule_service);
    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        store,
    }
}

fn message(
    id: &str,
    line_number: u64,
    role: MessageRole,
    block_type: BlockType,
    content: String,
    tool_name: Option<&str>,
    tool_use_id: Option<&str>,
) -> ConversationMessage {
    ConversationMessage {
        message_block_id: id.to_owned(),
        session_id: Some(SESSION.to_owned()),
        tenant: TENANT.to_owned(),
        caller_agent: AGENT.to_owned(),
        transcript_path: TRANSCRIPT.to_owned(),
        line_number,
        block_index: 0,
        message_uuid: Some(format!("message-{line_number}")),
        role,
        block_type,
        content,
        tool_name: tool_name.map(ToOwned::to_owned),
        tool_use_id: tool_use_id.map(ToOwned::to_owned),
        embed_eligible: block_type.embed_eligible_default(),
        created_at: format!("{:020}", 1_786_000_000_000_u64 + line_number),
        meta_json: (line_number == 1).then(|| json!({"cwd": "/workspace/mem"}).to_string()),
    }
}

fn completed_round() -> CompletedToolRound {
    CompletedToolRound {
        round_id: ROUND_ID.to_owned(),
        tenant: TENANT.to_owned(),
        caller_agent: AGENT.to_owned(),
        source_adapter: SourceAdapter::Codex,
        session_id: Some(SESSION.to_owned()),
        transcript_path: TRANSCRIPT.to_owned(),
        start_line_number: 1,
        start_block_index: 0,
        end_line_number: 4,
        end_block_index: 0,
        start_message_uuid: Some("message-1".to_owned()),
        final_message_uuid: Some("message-4".to_owned()),
        tool_call_ids: vec!["call-1".to_owned()],
        tool_names: vec!["exec_command".to_owned()],
        tool_call_count: 1,
        matched_result_count: 1,
        missing_result_count: 0,
        orphan_result_count: 0,
        error_result_count: 0,
        unknown_result_status_count: 0,
        completed_at: Some(CREATED_AT.to_owned()),
        seal_kind: RoundSealKind::StreamEof,
        integrity: RoundIntegrity::Clean,
        source_fingerprint: ROUND_FINGERPRINT.to_owned(),
        task_fingerprint: Some("task-fingerprint".to_owned()),
        task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        tool_pattern_fingerprint: "tool-pattern-shell".to_owned(),
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
    }
}

fn completed_build() -> CompletedToolRoundIndexBuild {
    CompletedToolRoundIndexBuild {
        generation_id: GENERATION_ID.to_owned(),
        tenant: TENANT.to_owned(),
        session_id: SESSION.to_owned(),
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
        task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        status: RoundIndexBuildStatus::Completed,
        source_block_count: 4,
        source_fingerprint: "generation-source-fingerprint".to_owned(),
        round_count: 1,
        started_at: CREATED_AT.to_owned(),
        completed_at: Some(CREATED_AT.to_owned()),
    }
}

fn candidate_spec() -> SkillCandidateJobSpec {
    SkillCandidateJobSpec {
        job_id: JOB_ID.to_owned(),
        tenant: TENANT.to_owned(),
        caller_agent: AGENT.to_owned(),
        serial_key: skill_candidate_serial_key(TENANT, AGENT),
        candidate_key: "candidate/skill-proposal".to_owned(),
        input_fingerprint: "candidate-input-fingerprint".to_owned(),
        candidate_revision: 1,
        trigger_version: SKILL_CANDIDATE_TRIGGER_VERSION,
        trigger_reasons: vec![SkillCandidateTriggerReason::ToolVolume],
        round_refs: vec![SkillCandidateRoundRef {
            session_id: SESSION.to_owned(),
            round_id: ROUND_ID.to_owned(),
            source_fingerprint: ROUND_FINGERPRINT.to_owned(),
            projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
            task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
            generation_id: GENERATION_ID.to_owned(),
        }],
        tool_call_count: 1,
        round_count: 1,
        distinct_session_count: 1,
    }
}

async fn post_json(app: &TestApp, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(Body::from(body.to_string()))
        .expect("request build");
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("request runs");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn unauthenticated_malformed_claim_is_401_before_json_parsing() {
    let app = test_app().await;
    let request = Request::builder()
        .method("POST")
        .uri("/admin/skill-proposals/claim")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{malformed-json"))
        .expect("request build");

    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("request runs");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn claim_scrubs_evidence_and_publish_persists_content_free_pending_provenance() {
    let app = test_app().await;
    let canary = format!("{}{}", ["s", "k", "-"].concat(), "q".repeat(20));
    let raw_user_evidence = format!("请检查服务，密钥是{canary}");
    let messages = vec![
        message(
            "block-user",
            1,
            MessageRole::User,
            BlockType::Text,
            raw_user_evidence.clone(),
            None,
            None,
        ),
        message(
            "block-tool-use",
            2,
            MessageRole::Assistant,
            BlockType::ToolUse,
            json!({"cmd": "service status"}).to_string(),
            Some("exec_command"),
            Some("call-1"),
        ),
        message(
            "block-tool-result",
            3,
            MessageRole::User,
            BlockType::ToolResult,
            "service is running".to_owned(),
            Some("exec_command"),
            Some("call-1"),
        ),
        message(
            "block-final",
            4,
            MessageRole::Assistant,
            BlockType::Text,
            "The service is healthy".to_owned(),
            None,
            None,
        ),
    ];
    assert_eq!(
        app.store
            .create_conversation_messages(&messages)
            .await
            .expect("insert normalized transcript"),
        4,
    );
    app.store
        .publish_completed_tool_round_generation(&completed_build(), &[completed_round()])
        .await
        .expect("publish completed round generation");
    let ensure = app
        .store
        .ensure_skill_candidate_jobs(&[candidate_spec()], CREATED_AT)
        .await
        .expect("ensure candidate job");
    assert_eq!(ensure.inserted, 1);

    let (claim_status, claim_body) =
        post_json(&app, "/admin/skill-proposals/claim", json!({"limit": 1})).await;
    assert_eq!(claim_status, StatusCode::OK, "claim response: {claim_body}");
    let batch: SkillCompileClaimBatch =
        serde_json::from_value(claim_body).expect("claim response shape");
    assert!(batch.degraded_job_ids.is_empty());
    assert_eq!(batch.claims.len(), 1);
    let claimed = &batch.claims[0];
    assert_eq!(
        claimed.claim.job.status,
        SkillCandidateJobStatus::Processing
    );
    assert!(!claimed.sanitized_evidence.contains(&canary));
    assert!(claimed.sanitized_evidence.contains("[redacted:"));

    let title = "Inspect service status".to_owned();
    let steps = vec!["Run the service status command".to_owned()];
    let parameters = Vec::new();
    let draft = SkillProposalDraft {
        canonical_signature: canonical_proposal_signature(&title, &steps, &parameters),
        title,
        steps,
        parameters,
    };
    let publish_request = PublishSkillProposalRequest {
        job_id: claimed.claim.job.job_id.clone(),
        lease_token: claimed.claim.lease_token.clone(),
        draft,
        model_id: "test-model".to_owned(),
        finish_reason: "stop".to_owned(),
        prompt_tokens: 120,
        completion_tokens: 40,
        target_skill_id: None,
        target_bundle_version_id: None,
        target_capability_capsule_id: None,
    };
    let (publish_status, publish_body) = post_json(
        &app,
        "/admin/skill-proposals/publish",
        serde_json::to_value(&publish_request).expect("publish request JSON"),
    )
    .await;
    assert_eq!(
        publish_status,
        StatusCode::OK,
        "publish response: {publish_body}"
    );
    let outcome: PublishSkillProposalOutcome =
        serde_json::from_value(publish_body).expect("publish response shape");
    let capsule_id = match outcome {
        PublishSkillProposalOutcome::Proposed {
            capability_capsule_id,
        } => capability_capsule_id,
        other => panic!("expected proposed outcome, got {other:?}"),
    };
    let (replay_status, replay_body) = post_json(
        &app,
        "/admin/skill-proposals/publish",
        serde_json::to_value(&publish_request).expect("publish replay JSON"),
    )
    .await;
    assert_eq!(
        replay_status,
        StatusCode::OK,
        "publish replay: {replay_body}"
    );
    assert_eq!(
        serde_json::from_value::<PublishSkillProposalOutcome>(replay_body)
            .expect("publish replay shape"),
        PublishSkillProposalOutcome::Proposed {
            capability_capsule_id: capsule_id.clone(),
        }
    );

    let capsule = app
        .store
        .get_capability_capsule_for_tenant(TENANT, &capsule_id)
        .await
        .expect("capsule read")
        .expect("proposal capsule exists");
    assert_eq!(
        capsule.capability_capsule_type,
        CapabilityCapsuleType::Workflow
    );
    assert_eq!(capsule.status, CapabilityCapsuleStatus::PendingConfirmation);
    assert_eq!(capsule.source_agent, SKILL_PROPOSAL_SOURCE_AGENT);
    let job = app
        .store
        .get_skill_candidate_job(JOB_ID)
        .await
        .expect("job read")
        .expect("job exists");
    assert_eq!(job.status, SkillCandidateJobStatus::Completed);

    let idempotency_key = capsule
        .idempotency_key
        .as_deref()
        .expect("compiler proposal idempotency key");
    let proposal_id = format!("sp_{:x}", Sha256::digest(idempotency_key.as_bytes()));
    let proposal = app
        .store
        .get_skill_proposal(TENANT, &proposal_id)
        .await
        .expect("proposal receipt read")
        .expect("proposal receipt exists");
    assert_eq!(proposal.capsule_id, capsule_id);
    assert_eq!(proposal.status, SkillProposalStatus::PendingConfirmation);
    assert!(!proposal.provenance_json.contains(&canary));
    assert!(!proposal.provenance_json.contains(&raw_user_evidence));
    assert!(!proposal.provenance_json.contains("请检查服务"));
}
