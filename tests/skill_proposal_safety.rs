use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use mem::{
    domain::{
        skill_bundle::{ResourceEntry, SkillId, SkillManifest, SKILL_MANIFEST_SCHEMA_VERSION},
        skill_candidate_serial_key, BlockType, ClaimedSkillCandidateJob, CompletedToolRound,
        CompletedToolRoundIndexBuild, ConversationMessage, LatestCompletedToolRounds, MessageRole,
        RoundIndexBuildStatus, RoundIntegrity, RoundSealKind, SkillBundleVersionRecord,
        SkillCandidateEnsureReport, SkillCandidateEvidence, SkillCandidateJob,
        SkillCandidateJobSpec, SkillCandidateJobStatus, SkillCandidateRoundRef,
        SkillCandidateTriggerReason, SkillHead, SkillProposalRecord, SkillProposalStatus,
        SkillResourceBlob, SourceAdapter, COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
        COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION, SKILL_CANDIDATE_TRIGGER_VERSION,
    },
    pipeline::skill_proposal_compiler::canonical_proposal_signature,
    service::{
        CapabilityCapsuleService, CompleteSkillDecisionRequest, PublishSkillProposalRequest,
        SkillProposalService,
    },
    storage::{CompletedToolRoundStore, SkillCandidateStore, SkillStore, StorageError, Store},
};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const TENANT: &str = "local";
const JOB_ID: &str = "job-publish-safety";
const LEASE: &str = "lease-publish-safety";
const SESSION: &str = "session-publish-safety";
const ROUND_ID: &str = "round-publish-safety";
const ROUND_FINGERPRINT: &str = "source-publish-safety";
const GENERATION: &str = "generation-publish-safety";
const NOW: &str = "00000001786000000000";

#[derive(Clone)]
struct CandidateState {
    job: Arc<Mutex<SkillCandidateJob>>,
}

impl CandidateState {
    fn new() -> Self {
        Self {
            job: Arc::new(Mutex::new(processing_job())),
        }
    }

    async fn invalidate_lease(&self) {
        let mut job = self.job.lock().await;
        job.lease_token = Some("replacement-lease".to_owned());
    }
}

#[async_trait]
impl SkillCandidateStore for CandidateState {
    async fn latest_skill_candidate_evidence(
        &self,
        _max_builds: usize,
        _max_rounds: usize,
    ) -> Result<Vec<SkillCandidateEvidence>, StorageError> {
        Ok(Vec::new())
    }

    async fn ensure_skill_candidate_jobs(
        &self,
        _specs: &[SkillCandidateJobSpec],
        _now: &str,
    ) -> Result<SkillCandidateEnsureReport, StorageError> {
        Ok(SkillCandidateEnsureReport::default())
    }

    async fn reconcile_skill_candidate_jobs(
        &self,
        _specs: &[SkillCandidateJobSpec],
        _active_evidence_keys: &HashSet<String>,
        _trigger_version: u32,
        _now: &str,
    ) -> Result<SkillCandidateEnsureReport, StorageError> {
        Ok(SkillCandidateEnsureReport::default())
    }

    async fn claim_skill_candidate_jobs(
        &self,
        _now: &str,
        _lease_expires_at: &str,
        _max_retries: u32,
        _limit: usize,
    ) -> Result<Vec<ClaimedSkillCandidateJob>, StorageError> {
        Ok(Vec::new())
    }

    async fn get_skill_candidate_job(
        &self,
        job_id: &str,
    ) -> Result<Option<SkillCandidateJob>, StorageError> {
        let job = self.job.lock().await.clone();
        Ok((job.job_id == job_id).then_some(job))
    }

    async fn list_skill_candidate_jobs(
        &self,
        _limit: usize,
    ) -> Result<Vec<SkillCandidateJob>, StorageError> {
        Ok(vec![self.job.lock().await.clone()])
    }

    async fn renew_skill_candidate_job_lease(
        &self,
        _job_id: &str,
        _lease_token: &str,
        _now: &str,
        _lease_expires_at: &str,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn complete_skill_candidate_job(
        &self,
        _job_id: &str,
        lease_token: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        let mut job = self.job.lock().await;
        if job.lease_token.as_deref() == Some(lease_token) {
            job.status = SkillCandidateJobStatus::Completed;
            job.lease_token = None;
            job.lease_expires_at = None;
            Ok(())
        } else {
            Err(StorageError::Conflict("skill candidate lease lost"))
        }
    }

    async fn fail_skill_candidate_job(
        &self,
        _job_id: &str,
        _lease_token: &str,
        _error_code: &str,
        _retry_at: &str,
        _now: &str,
        _max_attempts: u32,
    ) -> Result<(), StorageError> {
        Ok(())
    }
}

struct RoundSource {
    candidate: CandidateState,
    invalidate_during_hydrate: bool,
}

#[async_trait]
impl CompletedToolRoundStore for RoundSource {
    async fn load_round_source_messages(
        &self,
        _tenant: &str,
        _session_id: &str,
        _max_blocks: usize,
        _max_bytes: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if self.invalidate_during_hydrate {
            self.candidate.invalidate_lease().await;
        }
        Ok(vec![evidence_message()])
    }

    async fn publish_completed_tool_round_generation(
        &self,
        _build: &CompletedToolRoundIndexBuild,
        _rounds: &[CompletedToolRound],
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn latest_completed_tool_rounds(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> Result<LatestCompletedToolRounds, StorageError> {
        Ok(LatestCompletedToolRounds {
            build: Some(completed_build()),
            stored_round_count: 1,
            rounds: vec![completed_round()],
        })
    }
}

fn processing_job() -> SkillCandidateJob {
    SkillCandidateJob {
        job_id: JOB_ID.to_owned(),
        tenant: TENANT.to_owned(),
        caller_agent: "codex".to_owned(),
        serial_key: skill_candidate_serial_key(TENANT, "codex"),
        candidate_key: "candidate-publish-safety".to_owned(),
        input_fingerprint: "input-publish-safety".to_owned(),
        candidate_revision: 1,
        trigger_version: SKILL_CANDIDATE_TRIGGER_VERSION,
        trigger_reasons: vec![SkillCandidateTriggerReason::ToolVolume],
        round_refs: vec![SkillCandidateRoundRef {
            session_id: SESSION.to_owned(),
            round_id: ROUND_ID.to_owned(),
            source_fingerprint: ROUND_FINGERPRINT.to_owned(),
            projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
            task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
            generation_id: GENERATION.to_owned(),
        }],
        tool_call_count: 1,
        round_count: 1,
        distinct_session_count: 1,
        status: SkillCandidateJobStatus::Processing,
        attempt_count: 1,
        available_at: NOW.to_owned(),
        lease_token: Some(LEASE.to_owned()),
        lease_expires_at: Some("99999999999999999999".to_owned()),
        last_error_code: None,
        created_at: NOW.to_owned(),
        updated_at: NOW.to_owned(),
        completed_at: None,
    }
}

fn completed_build() -> CompletedToolRoundIndexBuild {
    CompletedToolRoundIndexBuild {
        generation_id: GENERATION.to_owned(),
        tenant: TENANT.to_owned(),
        session_id: SESSION.to_owned(),
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
        task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        status: RoundIndexBuildStatus::Completed,
        source_block_count: 1,
        source_fingerprint: "build-publish-safety".to_owned(),
        round_count: 1,
        started_at: NOW.to_owned(),
        completed_at: Some(NOW.to_owned()),
    }
}

fn completed_round() -> CompletedToolRound {
    CompletedToolRound {
        round_id: ROUND_ID.to_owned(),
        tenant: TENANT.to_owned(),
        caller_agent: "codex".to_owned(),
        source_adapter: SourceAdapter::Codex,
        session_id: Some(SESSION.to_owned()),
        transcript_path: "/workspace/publish-safety.jsonl".to_owned(),
        start_line_number: 1,
        start_block_index: 0,
        end_line_number: 1,
        end_block_index: 0,
        start_message_uuid: None,
        final_message_uuid: None,
        tool_call_ids: vec!["call-publish-safety".to_owned()],
        tool_names: vec!["exec_command".to_owned()],
        tool_call_count: 1,
        matched_result_count: 1,
        missing_result_count: 0,
        orphan_result_count: 0,
        error_result_count: 0,
        unknown_result_status_count: 0,
        completed_at: Some(NOW.to_owned()),
        seal_kind: RoundSealKind::StreamEof,
        integrity: RoundIntegrity::Clean,
        source_fingerprint: ROUND_FINGERPRINT.to_owned(),
        task_fingerprint: Some("task-publish-safety".to_owned()),
        task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        tool_pattern_fingerprint: "tool-pattern-shell".to_owned(),
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
    }
}

fn evidence_message() -> ConversationMessage {
    ConversationMessage {
        message_block_id: "block-publish-safety".to_owned(),
        session_id: Some(SESSION.to_owned()),
        tenant: TENANT.to_owned(),
        caller_agent: "codex".to_owned(),
        transcript_path: "/workspace/publish-safety.jsonl".to_owned(),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::User,
        block_type: BlockType::Text,
        content: "Inspect the service".to_owned(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: true,
        created_at: NOW.to_owned(),
        meta_json: None,
    }
}

fn publish_request() -> PublishSkillProposalRequest {
    let title = "Inspect service safely".to_owned();
    let steps = vec!["Run the status command".to_owned()];
    let parameters = Vec::new();
    PublishSkillProposalRequest {
        job_id: JOB_ID.to_owned(),
        lease_token: LEASE.to_owned(),
        draft: mem::domain::skill_proposal::SkillProposalDraft {
            canonical_signature: canonical_proposal_signature(&title, &steps, &parameters),
            title,
            steps,
            parameters,
        },
        model_id: "test-model".to_owned(),
        finish_reason: "stop".to_owned(),
        prompt_tokens: 10,
        completion_tokens: 10,
        target_skill_id: None,
        target_bundle_version_id: None,
        target_capability_capsule_id: None,
    }
}

fn expected_proposal_id(request: &PublishSkillProposalRequest) -> String {
    let job = processing_job();
    let mut key_hash = Sha256::new();
    key_hash.update(b"mem.skill_proposal.publish.v1");
    // Mirrors `proposal_idempotency_key`: the three update-target fields are
    // part of the key too, hashed as empty strings for a fresh proposal.
    let empty = String::new();
    for value in [
        &job.tenant,
        &job.job_id,
        &job.input_fingerprint,
        &request.draft.canonical_signature,
        request.target_skill_id.as_ref().unwrap_or(&empty),
        request.target_bundle_version_id.as_ref().unwrap_or(&empty),
        request
            .target_capability_capsule_id
            .as_ref()
            .unwrap_or(&empty),
    ] {
        key_hash.update((value.len() as u64).to_le_bytes());
        key_hash.update(value.as_bytes());
    }
    let key = format!("skill-proposal:{:x}", key_hash.finalize());
    format!("sp_{:x}", Sha256::digest(key.as_bytes()))
}

fn service(
    store: Arc<Store>,
    candidate: CandidateState,
    invalidate_during_hydrate: bool,
) -> SkillProposalService {
    let capsule_service = CapabilityCapsuleService::new(store.clone());
    SkillProposalService::new(
        Arc::new(candidate.clone()),
        Arc::new(RoundSource {
            candidate,
            invalidate_during_hydrate,
        }),
        capsule_service,
        store,
    )
}

#[tokio::test]
async fn lease_lost_after_hydrate_writes_no_capsule_or_proposal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(dir.path().join("lease-fence.lance"))
            .await
            .unwrap(),
    );
    let request = publish_request();
    let proposal_id = expected_proposal_id(&request);

    let result = service(store.clone(), CandidateState::new(), true)
        .publish(request)
        .await;

    assert!(result.is_err(), "lost lease must reject publish");
    assert!(store
        .list_capability_capsules_for_tenant(TENANT)
        .await
        .expect("capsule list")
        .is_empty());
    assert!(store
        .get_skill_proposal(TENANT, &proposal_id)
        .await
        .expect("proposal read")
        .is_none());
}

/// The Agent-as-Compiler path never runs the gateway lane's model-output
/// parser: the MCP tool forwards an agent-authored draft (with an empty
/// `canonical_signature`, since only the server can compute the real one)
/// straight to this route. So the route itself has to be the gate. Each of
/// these drafts is one the CLI compiler could not have produced, and none of
/// them may leave a capsule or a proposal behind.
#[tokio::test]
async fn publish_rejects_drafts_the_compiler_contract_forbids() {
    let cases: Vec<(
        &str,
        Vec<String>,
        Vec<mem::domain::skill_proposal::SkillParameter>,
    )> = vec![
        (
            "declared parameter never written into a step",
            vec!["Restart the service and wait for health".to_owned()],
            vec![mem::domain::skill_proposal::SkillParameter {
                name: "service_name".to_owned(),
                kind: mem::domain::skill_proposal::ParameterKind::String,
                required: true,
                default: None,
            }],
        ),
        (
            "placeholder with no declared parameter",
            vec!["Restart {{service_name}} and wait".to_owned()],
            Vec::new(),
        ),
        (
            "credential assignment inside a step",
            vec!["Deploy with password=hunter2hunter2".to_owned()],
            Vec::new(),
        ),
        (
            "step that is not a single line",
            vec!["Stop the service\nthen start it".to_owned()],
            Vec::new(),
        ),
    ];

    for (label, steps, parameters) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            Store::open(dir.path().join("publish-contract.lance"))
                .await
                .unwrap(),
        );
        let mut request = publish_request();
        request.draft.steps = steps;
        request.draft.parameters = parameters;
        // What the MCP compiler tool actually puts on the wire.
        request.draft.canonical_signature = String::new();

        let result = service(store.clone(), CandidateState::new(), false)
            .publish(request)
            .await;

        assert!(result.is_err(), "{label} must be rejected");
        assert!(
            store
                .list_capability_capsules_for_tenant(TENANT)
                .await
                .expect("capsule list")
                .is_empty(),
            "{label} must not write a capsule"
        );
    }
}

/// A valid draft still publishes when its signature is blank or wrong: the
/// server recomputes it, and exact-duplicate detection compares against that
/// recomputed value rather than anything the compiler claimed.
#[tokio::test]
async fn publish_recomputes_a_caller_supplied_canonical_signature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(dir.path().join("publish-signature.lance"))
            .await
            .unwrap(),
    );
    let honest = publish_request();
    let proposal_id = expected_proposal_id(&honest);
    let mut request = publish_request();
    request.draft.canonical_signature = "not-a-signature".to_owned();

    service(store.clone(), CandidateState::new(), false)
        .publish(request)
        .await
        .expect("valid draft publishes");

    let stored = store
        .get_skill_proposal(TENANT, &proposal_id)
        .await
        .expect("proposal read")
        .expect("proposal is stored under the recomputed signature");
    let stored_draft: mem::domain::skill_proposal::SkillProposalDraft =
        serde_json::from_str(&stored.draft_json).expect("stored draft json");
    assert_eq!(
        stored_draft.canonical_signature, honest.draft.canonical_signature,
        "the stored signature is the recomputed one"
    );
}

#[tokio::test]
async fn update_target_must_match_hydrated_shortlist_capsule_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(dir.path().join("update-target.lance"))
            .await
            .unwrap(),
    );
    let manifest = SkillManifest {
        schema_version: SKILL_MANIFEST_SCHEMA_VERSION,
        skill_id: SkillId("skill-existing".to_owned()),
        resources: vec![ResourceEntry {
            path: "SKILL.md".to_owned(),
            media_type: "text/markdown".to_owned(),
            sha256: format!("{:x}", Sha256::digest([])),
            size_bytes: 0,
            executable: false,
        }],
    };
    store
        .settle_skill_proposal(SkillProposalRecord {
            proposal_id: "proposal-existing".to_owned(),
            tenant: TENANT.to_owned(),
            job_id: "job-existing".to_owned(),
            capsule_id: "capsule-not-in-shortlist".to_owned(),
            draft_json: "{}".to_owned(),
            provenance_json: "{}".to_owned(),
            target_skill_id: None,
            expected_head_version: None,
            status: SkillProposalStatus::Accepted,
            created_at: NOW.to_owned(),
            updated_at: NOW.to_owned(),
        })
        .await
        .unwrap();
    store
        .put_skill_resource_blob(SkillResourceBlob {
            tenant: TENANT.to_owned(),
            sha256: format!("{:x}", Sha256::digest([])),
            media_type: "text/markdown".to_owned(),
            content: Vec::new(),
            size_bytes: 0,
            created_at: NOW.to_owned(),
        })
        .await
        .unwrap();
    let bundle = SkillBundleVersionRecord {
        tenant: TENANT.to_owned(),
        skill_id: "skill-existing".to_owned(),
        bundle_version_id: "bundle-existing-v1".to_owned(),
        proposal_id: "proposal-existing".to_owned(),
        workflow_capsule_id: "capsule-not-in-shortlist".to_owned(),
        previous_bundle_version_id: None,
        manifest_sha256: manifest.digest().unwrap(),
        manifest,
        created_at: NOW.to_owned(),
    };
    store.append_skill_bundle_version(bundle).await.unwrap();
    store
        .compare_and_set_skill_head(
            None,
            SkillHead {
                tenant: TENANT.to_owned(),
                skill_id: "skill-existing".to_owned(),
                bundle_version_id: "bundle-existing-v1".to_owned(),
                updated_at: NOW.to_owned(),
            },
        )
        .await
        .unwrap();
    let mut request = publish_request();
    request.target_skill_id = Some("skill-existing".to_owned());
    request.target_bundle_version_id = Some("bundle-existing-v1".to_owned());
    request.target_capability_capsule_id = Some("capsule-not-in-shortlist".to_owned());

    let result = service(store.clone(), CandidateState::new(), false)
        .publish(request)
        .await;

    assert!(
        result.is_err(),
        "non-shortlisted update target must be rejected"
    );
    assert!(store
        .list_capability_capsules_for_tenant(TENANT)
        .await
        .expect("capsule list")
        .is_empty());
}

#[tokio::test]
async fn duplicate_decision_replays_after_job_completion_without_a_second_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(dir.path().join("duplicate-replay.lance"))
            .await
            .unwrap(),
    );
    let request = publish_request();
    store
        .insert_capability_capsule(mem::domain::capability_capsule::CapabilityCapsuleRecord {
            capability_capsule_id: "existing-workflow".to_owned(),
            tenant: TENANT.to_owned(),
            capability_capsule_type:
                mem::domain::capability_capsule::CapabilityCapsuleType::Workflow,
            status: mem::domain::capability_capsule::CapabilityCapsuleStatus::Active,
            summary: request.draft.title.clone(),
            content: "1. Run the status command".to_owned(),
            content_hash: "d".repeat(64),
            source_agent: "reviewed-human-workflow".to_owned(),
            created_at: NOW.to_owned(),
            updated_at: NOW.to_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let candidate = CandidateState::new();
    let proposal_service = service(store.clone(), candidate.clone(), false);

    let first = proposal_service.publish(request.clone()).await.unwrap();
    let replay = proposal_service.publish(request).await.unwrap();

    assert_eq!(first, replay);
    assert_eq!(
        first,
        mem::service::PublishSkillProposalOutcome::Duplicate {
            capability_capsule_id: "existing-workflow".to_owned(),
        }
    );
    assert_eq!(
        candidate.job.lock().await.status,
        SkillCandidateJobStatus::Completed
    );
    assert_eq!(
        store
            .list_capability_capsules_for_tenant(TENANT)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn explicit_duplicate_decision_requires_the_catalog_target_and_signature() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(dir.path().join("explicit-duplicate.lance"))
            .await
            .unwrap(),
    );
    let publish = publish_request();
    store
        .insert_capability_capsule(mem::domain::capability_capsule::CapabilityCapsuleRecord {
            capability_capsule_id: "existing-workflow".to_owned(),
            tenant: TENANT.to_owned(),
            capability_capsule_type:
                mem::domain::capability_capsule::CapabilityCapsuleType::Workflow,
            status: mem::domain::capability_capsule::CapabilityCapsuleStatus::Active,
            summary: publish.draft.title.clone(),
            content: "1. Run the status command".to_owned(),
            content_hash: "e".repeat(64),
            source_agent: "reviewed-human-workflow".to_owned(),
            created_at: NOW.to_owned(),
            updated_at: NOW.to_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
    let candidate = CandidateState::new();
    let proposal_service = service(store.clone(), candidate.clone(), false);
    let request = CompleteSkillDecisionRequest {
        job_id: JOB_ID.to_owned(),
        lease_token: LEASE.to_owned(),
        decision_kind: "duplicate".to_owned(),
        canonical_signature: Some(publish.draft.canonical_signature),
        target_capability_capsule_id: Some("outside-catalog".to_owned()),
        artifact_class: None,
        reason: None,
        model_id: "test-model".to_owned(),
        finish_reason: "stop".to_owned(),
        prompt_tokens: 10,
        completion_tokens: 10,
    };
    assert!(proposal_service
        .complete_decision_for_tenant(request.clone(), None)
        .await
        .is_err());
    assert!(store
        .get_skill_compile_decision(TENANT, JOB_ID)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        candidate.job.lock().await.status,
        SkillCandidateJobStatus::Processing
    );

    let valid = CompleteSkillDecisionRequest {
        target_capability_capsule_id: Some("existing-workflow".to_owned()),
        ..request
    };
    let first = proposal_service
        .complete_decision_for_tenant(valid.clone(), None)
        .await
        .unwrap();
    let replay = proposal_service
        .complete_decision_for_tenant(valid, None)
        .await
        .unwrap();
    assert_eq!(first, replay);
    assert_eq!(first.decision_kind, "duplicate");
    assert_eq!(
        candidate.job.lock().await.status,
        SkillCandidateJobStatus::Completed
    );
}

#[tokio::test]
async fn terminal_decision_rejects_a_job_outside_the_role_tenant_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(dir.path().join("decision-tenant-scope.lance"))
            .await
            .unwrap(),
    );
    let candidate = CandidateState::new();
    candidate.job.lock().await.tenant = "other-tenant".to_owned();
    let proposal_service = service(store.clone(), candidate.clone(), false);
    let result = proposal_service
        .complete_decision_for_tenant(
            CompleteSkillDecisionRequest {
                job_id: JOB_ID.to_owned(),
                lease_token: LEASE.to_owned(),
                decision_kind: "nothing_to_save".to_owned(),
                canonical_signature: None,
                target_capability_capsule_id: None,
                artifact_class: None,
                reason: Some("nothing durable".to_owned()),
                model_id: "test-model".to_owned(),
                finish_reason: "stop".to_owned(),
                prompt_tokens: 10,
                completion_tokens: 10,
            },
            Some(TENANT),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        candidate.job.lock().await.status,
        SkillCandidateJobStatus::Processing
    );
    assert!(store
        .get_skill_compile_decision("other-tenant", JOB_ID)
        .await
        .unwrap()
        .is_none());
}
