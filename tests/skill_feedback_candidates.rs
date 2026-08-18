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
        ResourceEntry, SkillBundleVersionRecord, SkillCandidateJobStatus,
        SkillCandidateTriggerReason, SkillHead, SkillId, SkillManifest, SkillProposalRecord,
        SkillProposalStatus, SkillResourceBlob, SKILL_DOCUMENT_PATH, SKILL_MANIFEST_SCHEMA_VERSION,
    },
    http,
    pipeline::skill_proposal_compiler::canonical_proposal_signature,
    service::{CapabilityCapsuleService, PublishSkillProposalOutcome, SkillCompileClaimBatch},
    storage::{SkillCandidateStore, SkillStore, Store},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::util::ServiceExt;

mod common;

const TENANT: &str = "local";
const SKILL_ID: &str = "skill-feedback-loop";
const V1: &str = "bundle-feedback-v1";
const V2: &str = "bundle-feedback-v2";
const NOW: &str = "00000001786000000000";

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

async fn install_bundle(
    app: &TestApp,
    version: &str,
    previous: Option<&str>,
    advance_from: Option<&str>,
) {
    let anchor_id = format!("capsule-{version}");
    app.store
        .insert_capability_capsule(CapabilityCapsuleRecord {
            capability_capsule_id: anchor_id.clone(),
            tenant: TENANT.to_owned(),
            capability_capsule_type: CapabilityCapsuleType::Workflow,
            status: CapabilityCapsuleStatus::Active,
            scope: Scope::Workspace,
            visibility: Visibility::Shared,
            version: 1,
            summary: format!("Feedback base Skill {version}"),
            content: "1. Inspect the base service\n2. Record the result".to_owned(),
            confidence: 0.9,
            content_hash: format!("{:0>64}", version),
            source_agent: SKILL_PROPOSAL_SOURCE_AGENT.to_owned(),
            created_at: NOW.to_owned(),
            updated_at: NOW.to_owned(),
            ..Default::default()
        })
        .await
        .expect("insert active Skill anchor");
    let document = format!(
        "---\nname: feedback-loop\ndescription: Base Skill {version}\n---\n\n# Feedback base Skill\n\n## Steps\n\n1. Inspect the base service\n"
    )
    .into_bytes();
    let document_sha = format!("{:x}", Sha256::digest(&document));
    let blob = app
        .store
        .put_skill_resource_blob(SkillResourceBlob {
            tenant: TENANT.to_owned(),
            sha256: document_sha.clone(),
            media_type: "text/markdown".to_owned(),
            size_bytes: document.len() as u64,
            content: document,
            created_at: NOW.to_owned(),
        })
        .await
        .expect("insert Skill document");
    let manifest = SkillManifest {
        schema_version: SKILL_MANIFEST_SCHEMA_VERSION,
        skill_id: SkillId(SKILL_ID.to_owned()),
        resources: vec![ResourceEntry {
            path: SKILL_DOCUMENT_PATH.to_owned(),
            media_type: blob.media_type,
            sha256: blob.sha256,
            size_bytes: blob.size_bytes,
            executable: false,
        }],
    };
    app.store
        .settle_skill_proposal(SkillProposalRecord {
            proposal_id: format!("proposal-{version}"),
            tenant: TENANT.to_owned(),
            job_id: format!("job-{version}"),
            capsule_id: anchor_id.clone(),
            draft_json: json!({
                "title": format!("Feedback base Skill {version}"),
                "steps": ["Inspect the base service"],
                "parameters": [],
                "canonical_signature": "feedback-loop-fixture",
            })
            .to_string(),
            provenance_json: json!({"compiler_version": "test-v1"}).to_string(),
            target_skill_id: Some(SKILL_ID.to_owned()),
            expected_head_version: previous.map(ToOwned::to_owned),
            status: SkillProposalStatus::Accepted,
            created_at: NOW.to_owned(),
            updated_at: NOW.to_owned(),
        })
        .await
        .expect("insert accepted Skill proposal receipt");
    app.store
        .append_skill_bundle_version(SkillBundleVersionRecord {
            tenant: TENANT.to_owned(),
            skill_id: SKILL_ID.to_owned(),
            bundle_version_id: version.to_owned(),
            proposal_id: format!("proposal-{version}"),
            workflow_capsule_id: anchor_id,
            previous_bundle_version_id: previous.map(ToOwned::to_owned),
            manifest_sha256: manifest.digest().expect("manifest digest"),
            manifest,
            created_at: NOW.to_owned(),
        })
        .await
        .expect("insert Skill bundle");
    app.store
        .compare_and_set_skill_head(
            advance_from,
            SkillHead {
                tenant: TENANT.to_owned(),
                skill_id: SKILL_ID.to_owned(),
                bundle_version_id: version.to_owned(),
                updated_at: NOW.to_owned(),
            },
        )
        .await
        .expect("advance Skill head");
}

async fn feedback(app: &TestApp, id: &str, version: &str, note: &str) -> Value {
    let (status, body) = post_json(
        app,
        "/admin/skills/feedback",
        json!({
            "tenant": TENANT,
            "feedback_id": id,
            "skill_id": SKILL_ID,
            "bundle_version_id": version,
            "feedback_kind": "outdated",
            "note": note,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "feedback {id} response: {body}");
    body
}

#[tokio::test]
async fn negative_feedback_thresholds_create_stable_sanitized_revision_candidates_only_for_head() {
    let app = test_app().await;
    install_bundle(&app, V1, None, None).await;
    let canary = format!("{}{}", ["s", "k", "-"].concat(), "r".repeat(20));

    feedback(&app, "feedback-1", V1, "status step was stale").await;
    feedback(&app, "feedback-2", V1, "result guidance did not apply").await;
    let third_note = format!("密钥是{canary}，the base Skill needs revision");
    let third = feedback(&app, "feedback-3", V1, &third_note).await;
    assert_eq!(third["revision_candidate_ready"], true);

    let jobs_after_three = app
        .store
        .list_skill_candidate_jobs(20)
        .await
        .expect("candidate jobs after feedback threshold");
    assert_eq!(jobs_after_three.len(), 1);
    assert_eq!(jobs_after_three[0].status, SkillCandidateJobStatus::Pending);
    assert!(jobs_after_three[0]
        .trigger_reasons
        .contains(&SkillCandidateTriggerReason::NegativeFeedback));
    let first_job_id = jobs_after_three[0].job_id.clone();

    let (backlog_status, _) = post_json(
        &app,
        "/admin/skills/feedback",
        json!({
            "tenant": TENANT,
            "feedback_id": "feedback-4",
            "skill_id": SKILL_ID,
            "bundle_version_id": V1,
            "feedback_kind": "outdated",
            "note": "must wait for the current revision job",
        }),
    )
    .await;
    assert_eq!(backlog_status, StatusCode::TOO_MANY_REQUESTS);

    feedback(&app, "feedback-3", V1, &third_note).await;
    let jobs_after_replay = app
        .store
        .list_skill_candidate_jobs(20)
        .await
        .expect("candidate jobs after feedback replay");
    assert_eq!(jobs_after_replay.len(), 1);
    assert_eq!(jobs_after_replay[0].job_id, first_job_id);

    let (claim_status, claim_body) =
        post_json(&app, "/admin/skill-proposals/claim", json!({"limit": 1})).await;
    assert_eq!(claim_status, StatusCode::OK, "claim response: {claim_body}");
    let claims: SkillCompileClaimBatch =
        serde_json::from_value(claim_body).expect("claim response shape");
    assert_eq!(claims.claims.len(), 1);
    let claim = &claims.claims[0];
    assert_eq!(claim.claim.job.job_id, first_job_id);
    assert!(claim.sanitized_evidence.contains("Feedback base Skill"));
    assert!(claim.sanitized_evidence.contains("status step was stale"));
    assert!(claim.sanitized_evidence.contains("needs revision"));
    assert!(!claim.sanitized_evidence.contains(&canary));
    assert!(claim.sanitized_evidence.contains("[redacted:"));

    let title = "Feedback revised Skill".to_owned();
    let steps = vec!["Inspect the base service and verify freshness".to_owned()];
    let parameters: Vec<mem::domain::skill_proposal::SkillParameter> = Vec::new();
    let draft = json!({
        "title": title,
        "steps": steps,
        "parameters": parameters,
        "canonical_signature": canonical_proposal_signature(
            "Feedback revised Skill",
            &["Inspect the base service and verify freshness".to_owned()],
            &[],
        ),
    });
    let publish_base = json!({
        "job_id": claim.claim.job.job_id,
        "lease_token": claim.claim.lease_token,
        "draft": draft,
        "model_id": "test-feedback-compiler",
        "finish_reason": "stop",
        "prompt_tokens": 100,
        "completion_tokens": 20,
    });
    let (missing_target_status, _) =
        post_json(&app, "/admin/skill-proposals/publish", publish_base.clone()).await;
    assert_eq!(missing_target_status, StatusCode::CONFLICT);
    let target = claim
        .dedup_candidates
        .iter()
        .find(|candidate| candidate.target_skill_id.as_deref() == Some(SKILL_ID))
        .expect("base Skill is in compiler catalog");
    let mut valid_publish = publish_base;
    valid_publish["target_skill_id"] = json!(target.target_skill_id);
    valid_publish["target_bundle_version_id"] = json!(target.target_bundle_version_id);
    valid_publish["target_capability_capsule_id"] = json!(target.capability_capsule_id);
    let (complete_status, complete_body) =
        post_json(&app, "/admin/skill-proposals/publish", valid_publish).await;
    assert_eq!(
        complete_status,
        StatusCode::OK,
        "feedback publish response: {complete_body}"
    );
    let capsule_id = match serde_json::from_value::<PublishSkillProposalOutcome>(complete_body)
        .expect("feedback proposal outcome")
    {
        PublishSkillProposalOutcome::Proposed {
            capability_capsule_id,
        } => capability_capsule_id,
        PublishSkillProposalOutcome::Duplicate { .. } => {
            panic!("feedback revision should produce a changed proposal")
        }
    };
    let capsule = app
        .store
        .get_capability_capsule_for_tenant(TENANT, &capsule_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(capsule.status, CapabilityCapsuleStatus::PendingConfirmation);
    let proposal_id = format!(
        "sp_{:x}",
        Sha256::digest(capsule.idempotency_key.as_deref().unwrap().as_bytes())
    );
    let proposal = app
        .store
        .get_skill_proposal(TENANT, &proposal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(proposal.target_skill_id.as_deref(), Some(SKILL_ID));
    assert_eq!(proposal.expected_head_version.as_deref(), Some(V1));

    for index in 4..=6 {
        feedback(
            &app,
            &format!("feedback-{index}"),
            V1,
            &format!("negative revision signal {index}"),
        )
        .await;
    }
    let jobs_after_six = app
        .store
        .list_skill_candidate_jobs(20)
        .await
        .expect("candidate jobs after second threshold");
    assert_eq!(jobs_after_six.len(), 2);
    assert!(jobs_after_six.iter().all(|job| job
        .trigger_reasons
        .contains(&SkillCandidateTriggerReason::NegativeFeedback)));
    let second_job = jobs_after_six
        .iter()
        .find(|job| job.job_id != first_job_id)
        .expect("second stable feedback candidate");
    assert_eq!(second_job.status, SkillCandidateJobStatus::Pending);
    let second_job_id = second_job.job_id.clone();

    feedback(&app, "feedback-6", V1, "negative revision signal 6").await;
    let jobs_after_six_replay = app
        .store
        .list_skill_candidate_jobs(20)
        .await
        .expect("candidate jobs after second threshold replay");
    assert_eq!(jobs_after_six_replay.len(), 2);
    assert!(jobs_after_six_replay
        .iter()
        .any(|job| job.job_id == second_job_id));

    install_bundle(&app, V2, Some(V1), Some(V1)).await;
    for index in 7..=9 {
        feedback(
            &app,
            &format!("feedback-{index}"),
            V1,
            &format!("stale bundle signal {index}"),
        )
        .await;
    }
    let jobs_after_head_advance = app
        .store
        .list_skill_candidate_jobs(20)
        .await
        .expect("candidate jobs after stale-bundle feedback");
    assert_eq!(
        jobs_after_head_advance.len(),
        2,
        "feedback for a non-head bundle must not create a publishable update job",
    );
}

/// A Skill anchor answers only to Skill governance: `/admin/skills/feedback`
/// counts negatives toward a review-gated revision, while ordinary capsule
/// feedback must not touch it at all. `incorrect` is the sharp case — it
/// archives its target, and an archived anchor fails `require_active_anchor`,
/// so admitting it would let one call retire a published Skill from outside
/// governance. `validate_feedback_target` enforces this for every kind in all
/// three backends and had no test; the rule is scoped to anchors, so an
/// ordinary capsule still archives on `incorrect`.
#[tokio::test]
async fn ordinary_capsule_feedback_cannot_touch_a_bundle_managed_skill_anchor() {
    let app = test_app().await;
    install_bundle(&app, V1, None, None).await;
    let anchor_id = format!("capsule-{V1}");

    for kind in ["incorrect", "applies_here", "useful", "outdated"] {
        let (status, body) = post_json(
            &app,
            "/capability_capsules/feedback",
            json!({
                "tenant": TENANT,
                "capability_capsule_id": anchor_id,
                "feedback_kind": kind,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{kind}: {body:?}");
        assert_eq!(
            app.store
                .get_capability_capsule_for_tenant(TENANT, &anchor_id)
                .await
                .expect("anchor read")
                .expect("anchor row")
                .status,
            CapabilityCapsuleStatus::Active,
            "{kind} must leave the anchor active"
        );
    }

    let ordinary_id = "capsule-ordinary-feedback";
    app.store
        .insert_capability_capsule(CapabilityCapsuleRecord {
            capability_capsule_id: ordinary_id.to_owned(),
            tenant: TENANT.to_owned(),
            capability_capsule_type: CapabilityCapsuleType::Experience,
            status: CapabilityCapsuleStatus::Active,
            scope: Scope::Workspace,
            visibility: Visibility::Shared,
            version: 1,
            summary: "Ordinary capsule".to_owned(),
            content: "An ordinary experience capsule".to_owned(),
            confidence: 0.9,
            content_hash: format!("{:0>64}", "ordinary"),
            source_agent: "test".to_owned(),
            created_at: NOW.to_owned(),
            updated_at: NOW.to_owned(),
            ..Default::default()
        })
        .await
        .expect("insert ordinary capsule");
    let (status, _) = post_json(
        &app,
        "/capability_capsules/feedback",
        json!({
            "tenant": TENANT,
            "capability_capsule_id": ordinary_id,
            "feedback_kind": "incorrect",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        app.store
            .get_capability_capsule_for_tenant(TENANT, ordinary_id)
            .await
            .expect("ordinary read")
            .expect("ordinary row")
            .status,
        CapabilityCapsuleStatus::Archived,
        "the rule is scoped to Skill anchors"
    );
}
