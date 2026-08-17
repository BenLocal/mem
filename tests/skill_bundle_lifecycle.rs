//! Public-seam RED coverage for accepted Skill bundles, loadout pins, and feedback.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
};
use mem::{
    domain::{
        capability_capsule::{
            CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope,
            Visibility, SKILL_PROPOSAL_SOURCE_AGENT,
        },
        skill_proposal::SkillProposalDraft,
        CompletedToolRound, CompletedToolRoundIndexBuild, RoundIndexBuildStatus, RoundIntegrity,
        RoundSealKind, SkillCandidateJobSpec, SkillCandidateRoundRef, SkillCandidateTriggerReason,
        SkillProposalRecord, SkillProposalStatus, SourceAdapter,
        COMPLETED_TOOL_ROUND_PROJECTOR_VERSION, COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        SKILL_CANDIDATE_TRIGGER_VERSION, SKILL_DOCUMENT_PATH,
    },
    http,
    pipeline::skill_proposal_compiler::canonical_proposal_signature,
    service::{AcceptSkillProposalResponse, CapabilityCapsuleService, ResolvedSkillLoadout},
    storage::{CompletedToolRoundStore, SkillCandidateStore, SkillStore, Store},
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::util::ServiceExt;

mod common;

const TENANT: &str = "local";
const AGENT: &str = "codex-test";
const SKILL_ID: &str = "skill_lifecycle_contract";
const NOW: &str = "00000001790000000000";

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

fn draft(label: &str) -> SkillProposalDraft {
    let title = format!("Inspect service {label}");
    let steps = vec![format!("Collect status for {label}")];
    let parameters = Vec::new();
    let canonical_signature = canonical_proposal_signature(&title, &steps, &parameters);
    SkillProposalDraft {
        title,
        steps,
        parameters,
        canonical_signature,
    }
}

fn anchor(capsule_id: &str, label: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: capsule_id.to_owned(),
        tenant: TENANT.to_owned(),
        capability_capsule_type: CapabilityCapsuleType::Workflow,
        status: CapabilityCapsuleStatus::PendingConfirmation,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("Pending Skill proposal {label}"),
        content: format!("Review the generated workflow for {label}"),
        project: Some("mem".to_owned()),
        repo: Some("mem".to_owned()),
        confidence: 0.6,
        content_hash: format!("{:0>64}", label),
        source_agent: SKILL_PROPOSAL_SOURCE_AGENT.to_owned(),
        created_at: NOW.to_owned(),
        updated_at: NOW.to_owned(),
        ..Default::default()
    }
}

async fn seed_proposal(
    app: &TestApp,
    proposal_id: &str,
    label: &str,
    expected_head_version: Option<String>,
) -> SkillProposalDraft {
    let capsule_id = format!("capsule-{proposal_id}");
    let job_id = format!("job-{proposal_id}");
    let session_id = format!("session-{proposal_id}");
    let round_id = format!("round-{proposal_id}");
    let generation_id = format!("generation-{proposal_id}");
    let source_fingerprint = format!("source-{proposal_id}");
    app.store
        .publish_completed_tool_round_generation(
            &CompletedToolRoundIndexBuild {
                generation_id: generation_id.clone(),
                tenant: TENANT.to_owned(),
                session_id: session_id.clone(),
                projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
                task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
                status: RoundIndexBuildStatus::Completed,
                source_block_count: 0,
                source_fingerprint: format!("build-{proposal_id}"),
                round_count: 1,
                started_at: NOW.to_owned(),
                completed_at: Some(NOW.to_owned()),
            },
            &[CompletedToolRound {
                round_id: round_id.clone(),
                tenant: TENANT.to_owned(),
                caller_agent: AGENT.to_owned(),
                source_adapter: SourceAdapter::Codex,
                session_id: Some(session_id.clone()),
                transcript_path: format!("/tmp/{proposal_id}.jsonl"),
                start_line_number: 1,
                start_block_index: 0,
                end_line_number: 1,
                end_block_index: 0,
                start_message_uuid: None,
                final_message_uuid: None,
                tool_call_ids: vec![format!("call-{proposal_id}")],
                tool_names: vec!["shell".to_owned()],
                tool_call_count: 1,
                matched_result_count: 1,
                missing_result_count: 0,
                orphan_result_count: 0,
                error_result_count: 0,
                unknown_result_status_count: 0,
                completed_at: Some(NOW.to_owned()),
                seal_kind: RoundSealKind::StreamEof,
                integrity: RoundIntegrity::Clean,
                source_fingerprint: source_fingerprint.clone(),
                task_fingerprint: Some(format!("task-{proposal_id}")),
                task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
                tool_pattern_fingerprint: "tool-shell".to_owned(),
                projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
            }],
        )
        .await
        .expect("publish proposal evidence");
    app.store
        .ensure_skill_candidate_jobs(
            &[SkillCandidateJobSpec {
                job_id: job_id.clone(),
                tenant: TENANT.to_owned(),
                caller_agent: AGENT.to_owned(),
                serial_key: mem::domain::skill_candidate_serial_key(TENANT, AGENT),
                candidate_key: format!("candidate-{proposal_id}"),
                input_fingerprint: format!("input-{proposal_id}"),
                candidate_revision: 1,
                trigger_version: SKILL_CANDIDATE_TRIGGER_VERSION,
                trigger_reasons: vec![SkillCandidateTriggerReason::ToolVolume],
                round_refs: vec![SkillCandidateRoundRef {
                    session_id,
                    round_id,
                    source_fingerprint,
                    projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
                    task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
                    generation_id,
                }],
                tool_call_count: 1,
                round_count: 1,
                distinct_session_count: 1,
            }],
            NOW,
        )
        .await
        .expect("insert proposal candidate receipt");
    let proposal_draft = draft(label);
    app.store
        .insert_capability_capsule(anchor(&capsule_id, label))
        .await
        .expect("insert proposal anchor");
    app.store
        .settle_skill_proposal(SkillProposalRecord {
            proposal_id: proposal_id.to_owned(),
            tenant: TENANT.to_owned(),
            job_id,
            capsule_id,
            draft_json: serde_json::to_string(&proposal_draft).expect("serialize draft"),
            provenance_json: json!({
                "schema_version": 1,
                "compiler_version": "integration-test-v1",
                "source_fingerprints": [format!("round-{label}")],
            })
            .to_string(),
            target_skill_id: Some(SKILL_ID.to_owned()),
            expected_head_version,
            status: SkillProposalStatus::PendingConfirmation,
            created_at: NOW.to_owned(),
            updated_at: NOW.to_owned(),
        })
        .await
        .expect("insert Skill proposal receipt");
    proposal_draft
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

async fn get_bytes(app: &TestApp, path: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", common::TEST_ADMIN_TOKEN),
        )
        .body(Body::empty())
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
        .expect("response body")
        .to_vec();
    (status, bytes)
}

async fn accept(
    app: &TestApp,
    proposal_id: &str,
    expected_head_version: Option<&str>,
) -> AcceptSkillProposalResponse {
    let (status, body) = post_json(
        app,
        "/admin/skill-proposals/accept",
        json!({
            "tenant": TENANT,
            "proposal_id": proposal_id,
            "expected_head_version": expected_head_version,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "accept response: {body}");
    serde_json::from_value(body).expect("accept response shape")
}

async fn bind_shared(app: &TestApp) {
    let (status, body) = post_json(
        app,
        "/admin/agent-loadouts/bind",
        json!({
            "tenant": TENANT,
            "agent_id": AGENT,
            "skill_id": SKILL_ID,
            "priority": 10,
            "enabled": true,
            "visibility": "shared",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bind response: {body}");
}

async fn resolve(app: &TestApp, session_id: &str) -> ResolvedSkillLoadout {
    let (status, body) = post_json(
        app,
        "/admin/agent-loadouts/resolve",
        json!({"tenant": TENANT, "agent_id": AGENT, "session_id": session_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve response: {body}");
    serde_json::from_value(body).expect("resolve response shape")
}

#[tokio::test]
async fn accept_creates_one_immutable_bundle_and_head_then_replays_same_version() {
    let app = test_app().await;
    seed_proposal(&app, "proposal-v1", "v1", None).await;

    let first = accept(&app, "proposal-v1", None).await;
    let bundle_before = app
        .store
        .get_skill_bundle_version(TENANT, &first.skill_id, &first.bundle_version_id)
        .await
        .expect("bundle read")
        .expect("accepted bundle exists");
    let head_before = app
        .store
        .get_skill_head(TENANT, &first.skill_id)
        .await
        .expect("head read")
        .expect("head exists");
    let proposal = app
        .store
        .get_skill_proposal(TENANT, "proposal-v1")
        .await
        .expect("proposal read")
        .expect("proposal exists");
    let anchor = app
        .store
        .get_capability_capsule_for_tenant(TENANT, &proposal.capsule_id)
        .await
        .expect("anchor read")
        .expect("anchor exists");

    assert_eq!(proposal.status, SkillProposalStatus::Accepted);
    assert_eq!(anchor.status, CapabilityCapsuleStatus::Active);
    assert_eq!(head_before.bundle_version_id, first.bundle_version_id);
    assert_eq!(bundle_before.proposal_id, "proposal-v1");

    for (query, intent) in [("Inspect service v1", "debugging"), ("", "wake_up")] {
        let (search_status, search_body) = post_json(
            &app,
            "/capability_capsules/search",
            json!({
                "query": query,
                "intent": intent,
                "scope_filters": [],
                "token_budget": 500,
                "caller_agent": AGENT,
                "expand_graph": false,
                "tenant": TENANT,
            }),
        )
        .await;
        assert_eq!(
            search_status,
            StatusCode::OK,
            "search response: {search_body}"
        );
        assert!(
            !search_body.to_string().contains(&first.workflow_capsule_id),
            "runtime-managed Skill anchor leaked through ordinary {intent} recall",
        );
    }

    let replay = accept(&app, "proposal-v1", None).await;
    let bundle_after = app
        .store
        .get_skill_bundle_version(TENANT, &replay.skill_id, &replay.bundle_version_id)
        .await
        .expect("bundle replay read")
        .expect("replayed bundle exists");
    let head_after = app
        .store
        .get_skill_head(TENANT, &replay.skill_id)
        .await
        .expect("head replay read")
        .expect("head still exists");

    assert_eq!(replay, first);
    assert_eq!(bundle_after, bundle_before);
    assert_eq!(head_after, head_before);
}

#[tokio::test]
async fn specialized_reject_records_the_verdict_without_publishing_a_head() {
    let app = test_app().await;
    seed_proposal(&app, "proposal-rejected", "rejected", None).await;
    let (status, body) = post_json(
        &app,
        "/admin/skill-proposals/reject",
        json!({"tenant": TENANT, "proposal_id": "proposal-rejected"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reject response: {body}");
    let proposal = app
        .store
        .get_skill_proposal(TENANT, "proposal-rejected")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(proposal.status, SkillProposalStatus::Rejected);
    assert!(app
        .store
        .get_skill_head(TENANT, SKILL_ID)
        .await
        .unwrap()
        .is_none());
    let capsule = app
        .store
        .get_capability_capsule_for_tenant(TENANT, &proposal.capsule_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(capsule.status, CapabilityCapsuleStatus::Rejected);
}

#[tokio::test]
async fn first_seen_session_pin_does_not_drift_when_targeted_v2_advances_head() {
    let app = test_app().await;
    seed_proposal(&app, "proposal-v1", "v1", None).await;
    let v1 = accept(&app, "proposal-v1", None).await;
    bind_shared(&app).await;
    let s1_before = resolve(&app, "session-s1").await;
    assert_eq!(s1_before.skills[0].bundle_version_id, v1.bundle_version_id);

    seed_proposal(
        &app,
        "proposal-v2",
        "v2",
        Some(v1.bundle_version_id.clone()),
    )
    .await;
    let v2 = accept(&app, "proposal-v2", Some(&v1.bundle_version_id)).await;
    assert_ne!(v2.bundle_version_id, v1.bundle_version_id);

    let s1_after = resolve(&app, "session-s1").await;
    let s2 = resolve(&app, "session-s2").await;
    assert_eq!(s1_after.skills[0].bundle_version_id, v1.bundle_version_id);
    assert_eq!(s2.skills[0].bundle_version_id, v2.bundle_version_id);
    app.store
        .get_or_pin_session_skill(mem::domain::SessionSkillPin {
            tenant: TENANT.to_owned(),
            session_id: "session-expired".to_owned(),
            agent_id: AGENT.to_owned(),
            skill_id: SKILL_ID.to_owned(),
            bundle_version_id: v1.bundle_version_id.clone(),
            pinned_at: "00000000000000000001".to_owned(),
            expires_at: "00000000000000000002".to_owned(),
            revision: 1,
        })
        .await
        .unwrap();
    assert_eq!(
        resolve(&app, "session-expired").await.skills[0].bundle_version_id,
        v2.bundle_version_id
    );
    let repinned = app
        .store
        .get_session_skill_pin(TENANT, "session-expired", AGENT, SKILL_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repinned.revision, 2);
    let expiry_after_repin = repinned.expires_at.clone();
    resolve(&app, "session-expired").await;
    let stable_pin = app
        .store
        .get_session_skill_pin(TENANT, "session-expired", AGENT, SKILL_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stable_pin.revision, 2);
    assert_eq!(stable_pin.expires_at, expiry_after_repin);
    let (historical_adopt_status, _) = post_json(
        &app,
        "/admin/skill-proposals/accept",
        json!({
            "tenant": TENANT,
            "proposal_id": "proposal-v1",
            "expected_head_version": null,
            "adopt_session": {"session_id": "session-downgrade", "agent_id": AGENT},
        }),
    )
    .await;
    assert_eq!(historical_adopt_status, StatusCode::CONFLICT);

    let (revoke_status, revoke_body) = post_json(
        &app,
        "/admin/skills/revoke",
        json!({
            "tenant": TENANT,
            "skill_id": SKILL_ID,
            "bundle_version_id": v1.bundle_version_id,
            "reason_code": "unsafe_historical_version",
        }),
    )
    .await;
    assert_eq!(
        revoke_status,
        StatusCode::OK,
        "revoke response: {revoke_body}"
    );
    let (old_session_status, _) = post_json(
        &app,
        "/admin/agent-loadouts/resolve",
        json!({"tenant": TENANT, "agent_id": AGENT, "session_id": "session-s1"}),
    )
    .await;
    assert_eq!(old_session_status, StatusCode::CONFLICT);
    let v1_bundle = app
        .store
        .get_skill_bundle_version(TENANT, SKILL_ID, &v1.bundle_version_id)
        .await
        .unwrap()
        .unwrap();
    let v1_sha = &v1_bundle.manifest.resources[0].sha256;
    let v1_resource = format!(
        "/admin/skills/{SKILL_ID}/versions/{}/resources/{v1_sha}?tenant={TENANT}&agent_id={AGENT}&session_id=session-s1",
        v1.bundle_version_id
    );
    assert_eq!(get_bytes(&app, &v1_resource).await.0, StatusCode::CONFLICT);
    assert_eq!(
        resolve(&app, "session-s2").await.skills[0].bundle_version_id,
        v2.bundle_version_id
    );
}

#[tokio::test]
async fn three_negative_feedback_events_ready_revision_without_mutating_runtime_state() {
    let app = test_app().await;
    seed_proposal(&app, "proposal-v1", "v1", None).await;
    let accepted = accept(&app, "proposal-v1", None).await;
    bind_shared(&app).await;
    let pinned_before = resolve(&app, "session-s1").await;
    let head_before = app
        .store
        .get_skill_head(TENANT, SKILL_ID)
        .await
        .expect("head read")
        .expect("head exists");
    let bundle_before = app
        .store
        .get_skill_bundle_version(TENANT, SKILL_ID, &accepted.bundle_version_id)
        .await
        .expect("bundle read")
        .expect("bundle exists");

    let mut final_response = Value::Null;
    for (index, kind) in ["outdated", "does_not_apply_here", "incorrect"]
        .into_iter()
        .enumerate()
    {
        let (status, body) = post_json(
            &app,
            "/admin/skills/feedback",
            json!({
                "tenant": TENANT,
                "feedback_id": format!("skill-feedback-{index}"),
                "skill_id": SKILL_ID,
                "bundle_version_id": accepted.bundle_version_id,
                "feedback_kind": kind,
                "note": format!("negative signal {index}"),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "feedback response: {body}");
        final_response = body;
    }

    assert_eq!(final_response["negative_feedback_count"], 3);
    assert_eq!(final_response["revision_candidate_ready"], true);
    assert_eq!(
        app.store
            .list_skill_feedback(TENANT, SKILL_ID, &accepted.bundle_version_id, 10)
            .await
            .expect("feedback read")
            .len(),
        3,
    );
    assert_eq!(
        app.store
            .get_skill_head(TENANT, SKILL_ID)
            .await
            .expect("head read")
            .expect("head exists"),
        head_before,
    );
    assert_eq!(
        app.store
            .get_skill_bundle_version(TENANT, SKILL_ID, &accepted.bundle_version_id)
            .await
            .expect("bundle read")
            .expect("bundle exists"),
        bundle_before,
    );
    assert_eq!(resolve(&app, "session-s1").await, pinned_before);
}

#[tokio::test]
async fn generated_skill_document_has_valid_agent_skill_yaml_frontmatter() {
    let app = test_app().await;
    seed_proposal(&app, "proposal-frontmatter", "frontmatter", None).await;
    let accepted = accept(&app, "proposal-frontmatter", None).await;
    let bundle = app
        .store
        .get_skill_bundle_version(TENANT, SKILL_ID, &accepted.bundle_version_id)
        .await
        .expect("bundle read")
        .expect("bundle exists");
    let document = bundle
        .manifest
        .resources
        .iter()
        .find(|resource| resource.path == SKILL_DOCUMENT_PATH)
        .expect("SKILL.md manifest entry");
    let blob = app
        .store
        .get_skill_resource_blob(TENANT, &document.sha256)
        .await
        .expect("resource read")
        .expect("SKILL.md blob exists");
    let text = std::str::from_utf8(&blob.content).expect("SKILL.md is UTF-8");
    let frontmatter_and_body = text
        .strip_prefix("---\n")
        .expect("SKILL.md starts with YAML frontmatter");
    let (frontmatter, body) = frontmatter_and_body
        .split_once("\n---\n")
        .expect("SKILL.md closes YAML frontmatter");
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(frontmatter).expect("frontmatter is valid YAML");
    let mapping = yaml.as_mapping().expect("frontmatter is a YAML mapping");
    for required in ["name", "description"] {
        let value = mapping
            .get(serde_yaml_ng::Value::String(required.to_owned()))
            .and_then(serde_yaml_ng::Value::as_str)
            .filter(|value| !value.trim().is_empty());
        assert!(value.is_some(), "frontmatter has non-empty {required}");
    }
    assert!(body.contains("## Steps"));
}

#[tokio::test]
async fn accept_replay_repairs_each_staged_crash_suffix() {
    for (label, publish_head, mark_accepted) in [
        ("bundle-only", false, false),
        ("head-published", true, false),
        ("receipt-accepted", true, true),
    ] {
        let app = test_app().await;
        seed_proposal(&app, "proposal-base", "base", None).await;
        let base = accept(&app, "proposal-base", None).await;
        let proposal_id = format!("proposal-{label}");
        seed_proposal(
            &app,
            &proposal_id,
            label,
            Some(base.bundle_version_id.clone()),
        )
        .await;
        let proposal = app
            .store
            .get_skill_proposal(TENANT, &proposal_id)
            .await
            .unwrap()
            .unwrap();
        let base_bundle = app
            .store
            .get_skill_bundle_version(TENANT, SKILL_ID, &base.bundle_version_id)
            .await
            .unwrap()
            .unwrap();
        let staged_version = format!("staged-{label}");
        app.store
            .append_skill_bundle_version(mem::domain::SkillBundleVersionRecord {
                tenant: TENANT.to_owned(),
                skill_id: SKILL_ID.to_owned(),
                bundle_version_id: staged_version.clone(),
                proposal_id: proposal_id.clone(),
                workflow_capsule_id: proposal.capsule_id.clone(),
                previous_bundle_version_id: Some(base.bundle_version_id.clone()),
                manifest: base_bundle.manifest.clone(),
                manifest_sha256: base_bundle.manifest_sha256.clone(),
                created_at: NOW.to_owned(),
            })
            .await
            .unwrap();
        if publish_head {
            app.store
                .compare_and_set_skill_head(
                    Some(&base.bundle_version_id),
                    mem::domain::SkillHead {
                        tenant: TENANT.to_owned(),
                        skill_id: SKILL_ID.to_owned(),
                        bundle_version_id: staged_version.clone(),
                        updated_at: NOW.to_owned(),
                    },
                )
                .await
                .unwrap();
        }
        if mark_accepted {
            app.store
                .update_skill_proposal_outcome(
                    TENANT,
                    &proposal_id,
                    SkillProposalStatus::PendingConfirmation,
                    SkillProposalStatus::Accepted,
                    NOW,
                )
                .await
                .unwrap();
        }

        let repaired = accept(&app, &proposal_id, Some(&base.bundle_version_id)).await;
        assert_eq!(repaired.bundle_version_id, staged_version);
        assert_eq!(
            app.store
                .get_skill_head(TENANT, SKILL_ID)
                .await
                .unwrap()
                .unwrap()
                .bundle_version_id,
            staged_version
        );
        assert_eq!(
            app.store
                .get_skill_proposal(TENANT, &proposal_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SkillProposalStatus::Accepted
        );
        assert_eq!(
            app.store
                .get_capability_capsule_for_tenant(TENANT, &proposal.capsule_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            CapabilityCapsuleStatus::Active
        );
        assert_eq!(
            accept(&app, &proposal_id, Some(&base.bundle_version_id))
                .await
                .bundle_version_id,
            staged_version
        );
    }
}

#[tokio::test]
async fn resource_requires_live_exact_session_pin_and_revocation_fails_closed() {
    let app = test_app().await;
    seed_proposal(&app, "proposal-resource", "resource", None).await;
    let accepted = accept(&app, "proposal-resource", None).await;
    bind_shared(&app).await;
    resolve(&app, "session-resource").await;
    let bundle = app
        .store
        .get_skill_bundle_version(TENANT, SKILL_ID, &accepted.bundle_version_id)
        .await
        .unwrap()
        .unwrap();
    let sha256 = bundle.manifest.resources[0].sha256.clone();
    let path = format!(
        "/admin/skills/{SKILL_ID}/versions/{}/resources/{sha256}?tenant={TENANT}&agent_id={AGENT}&session_id=session-resource",
        accepted.bundle_version_id
    );
    assert_eq!(get_bytes(&app, &path).await.0, StatusCode::OK);
    let unpinned = path.replace("session-resource", "session-never-pinned");
    assert_eq!(get_bytes(&app, &unpinned).await.0, StatusCode::NOT_FOUND);

    let (status, body) = post_json(
        &app,
        "/admin/skills/revoke",
        json!({
            "tenant": TENANT,
            "skill_id": SKILL_ID,
            "bundle_version_id": accepted.bundle_version_id,
            "reason_code": "unsafe_output",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revoke response: {body}");
    assert_eq!(get_bytes(&app, &path).await.0, StatusCode::CONFLICT);
    let (resolve_status, _) = post_json(
        &app,
        "/admin/agent-loadouts/resolve",
        json!({"tenant": TENANT, "agent_id": AGENT, "session_id": "session-resource"}),
    )
    .await;
    assert_eq!(resolve_status, StatusCode::CONFLICT);
}
