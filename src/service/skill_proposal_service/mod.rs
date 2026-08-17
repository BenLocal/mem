mod decisions;
mod evidence;

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::capability_capsule::{
    CapabilityCapsuleStatus, CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope,
    Visibility, WriteMode, SKILL_PROPOSAL_SOURCE_AGENT,
};
use crate::domain::skill_proposal::{
    EnvironmentContext, SkillProposalDraft, WorkflowDedupCandidate,
};
use crate::domain::{
    ClaimedSkillCandidateJob, SkillCandidateJobStatus, SkillCompileDecisionRecord,
};
use crate::domain::{SkillProposalRecord, SkillProposalStatus};
use crate::pipeline::hard_secret_redaction;
use crate::pipeline::skill_proposal_compiler::validate_proposal_draft;
use crate::storage::{
    current_timestamp, timestamp_add_ms, CompletedToolRoundStore, SkillCandidateStore, SkillStore,
    StorageError,
};

use super::{capability_capsule_service::ServiceError, CapabilityCapsuleService};

const LEASE_MS: u128 = 5 * 60 * 1_000;
const RETRY_MS: u128 = 60 * 1_000;
const MAX_ATTEMPTS: u32 = 3;
const MAX_CLAIMS: usize = 8;
const MAX_SESSION_BLOCKS: usize = 20_000;
const MAX_SESSION_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVIDENCE_BLOCKS: usize = 512;
const MAX_EVIDENCE_BYTES: usize = 512 * 1024;
const MAX_DEDUP_CANDIDATES: usize = 5;
const MAX_CATALOG_TITLE_CHARS: usize = 200;
const MAX_CATALOG_STEPS: usize = 32;
const MAX_CATALOG_STEP_CHARS: usize = 1_000;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCompileClaim {
    pub claim: ClaimedSkillCandidateJob,
    pub sanitized_evidence: String,
    pub environment: EnvironmentContext,
    pub dedup_candidates: Vec<WorkflowDedupCandidate>,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCompileClaimBatch {
    pub claims: Vec<SkillCompileClaim>,
    pub degraded_job_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCompilePreview {
    pub job: crate::domain::SkillCandidateJob,
    pub sanitized_evidence: String,
    pub environment: EnvironmentContext,
    pub dedup_candidates: Vec<WorkflowDedupCandidate>,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCompilePreviewBatch {
    pub candidates: Vec<SkillCompilePreview>,
    pub degraded_job_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishSkillProposalRequest {
    pub job_id: String,
    pub lease_token: String,
    pub draft: SkillProposalDraft,
    pub model_id: String,
    pub finish_reason: String,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub target_skill_id: Option<String>,
    #[serde(default)]
    pub target_bundle_version_id: Option<String>,
    #[serde(default)]
    pub target_capability_capsule_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublishSkillProposalOutcome {
    Proposed { capability_capsule_id: String },
    Duplicate { capability_capsule_id: String },
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompleteSkillDecisionRequest {
    pub job_id: String,
    pub lease_token: String,
    pub decision_kind: String,
    #[serde(default)]
    pub canonical_signature: Option<String>,
    #[serde(default)]
    pub target_capability_capsule_id: Option<String>,
    #[serde(default)]
    pub artifact_class: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    pub model_id: String,
    pub finish_reason: String,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[derive(Clone)]
pub struct SkillProposalService {
    candidate_store: Arc<dyn SkillCandidateStore>,
    round_store: Arc<dyn CompletedToolRoundStore>,
    capsule_service: CapabilityCapsuleService,
    skill_store: Arc<dyn SkillStore>,
    admin_gate: Arc<tokio::sync::Mutex<()>>,
    settlement_gate: Arc<tokio::sync::Mutex<()>>,
}

impl SkillProposalService {
    pub fn new(
        candidate_store: Arc<dyn SkillCandidateStore>,
        round_store: Arc<dyn CompletedToolRoundStore>,
        capsule_service: CapabilityCapsuleService,
        skill_store: Arc<dyn SkillStore>,
    ) -> Self {
        Self {
            candidate_store,
            round_store,
            capsule_service,
            skill_store,
            admin_gate: Arc::new(tokio::sync::Mutex::new(())),
            settlement_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Claim and hydrate a bounded batch. Per-job transcript failures are
    /// recorded as retryable queue failures and returned as degraded IDs, so a
    /// recoverable Lance read never turns the whole admin request into 500.
    pub async fn claim(&self, limit: usize) -> Result<SkillCompileClaimBatch, ServiceError> {
        self.claim_for_tenant(limit, None).await
    }

    pub async fn claim_for_tenant(
        &self,
        limit: usize,
        tenant_scope: Option<&str>,
    ) -> Result<SkillCompileClaimBatch, ServiceError> {
        let _guard = self
            .admin_gate
            .try_lock()
            .map_err(|_| StorageError::Conflict("Skill compiler admin operation is busy"))?;
        let limit = limit.clamp(1, MAX_CLAIMS);
        let now = current_timestamp();
        let lease_expires_at = timestamp_add_ms(&now, LEASE_MS);
        let claimed = match tenant_scope {
            Some(tenant) => {
                self.candidate_store
                    .claim_skill_candidate_jobs_for_tenant(
                        tenant,
                        &now,
                        &lease_expires_at,
                        MAX_ATTEMPTS,
                        limit,
                    )
                    .await?
            }
            None => {
                self.candidate_store
                    .claim_skill_candidate_jobs(&now, &lease_expires_at, MAX_ATTEMPTS, limit)
                    .await?
            }
        };
        let mut report = SkillCompileClaimBatch::default();
        for claim in claimed {
            let has_terminal_receipt = self
                .skill_store
                .get_skill_compile_decision(&claim.job.tenant, &claim.job.job_id)
                .await?
                .is_some()
                || self
                    .skill_store
                    .get_skill_proposal_by_job(&claim.job.tenant, &claim.job.job_id)
                    .await?
                    .is_some();
            if has_terminal_receipt {
                self.candidate_store
                    .complete_skill_candidate_job(&claim.job.job_id, &claim.lease_token, &now)
                    .await?;
                continue;
            }
            match self.hydrate_claim(&claim).await {
                Ok(hydrated) => report.claims.push(hydrated),
                Err(error) => {
                    if matches!(error, StorageError::Conflict(_)) {
                        self.candidate_store
                            .stale_claimed_skill_candidate_job(
                                &claim.job.job_id,
                                &claim.lease_token,
                                &now,
                            )
                            .await?;
                    } else {
                        let retry_at = timestamp_add_ms(&now, RETRY_MS);
                        self.candidate_store
                            .fail_skill_candidate_job(
                                &claim.job.job_id,
                                &claim.lease_token,
                                error_code(&error),
                                &retry_at,
                                &now,
                                MAX_ATTEMPTS,
                            )
                            .await?;
                    }
                    report.degraded_job_ids.push(claim.job.job_id);
                }
            }
        }
        Ok(report)
    }

    pub async fn preview(&self, limit: usize) -> Result<SkillCompilePreviewBatch, ServiceError> {
        self.preview_for_tenant(limit, None).await
    }

    pub async fn preview_for_tenant(
        &self,
        limit: usize,
        tenant_scope: Option<&str>,
    ) -> Result<SkillCompilePreviewBatch, ServiceError> {
        let _guard = self
            .admin_gate
            .try_lock()
            .map_err(|_| StorageError::Conflict("Skill compiler admin operation is busy"))?;
        let limit = limit.clamp(1, MAX_CLAIMS);
        let now = current_timestamp();
        let mut jobs = match tenant_scope {
            Some(tenant) => {
                self.candidate_store
                    .preview_skill_candidate_jobs_for_tenant(tenant, 1_000)
                    .await?
            }
            None => {
                self.candidate_store
                    .list_skill_candidate_jobs(1_000)
                    .await?
            }
        };
        jobs.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        let mut lanes = HashSet::new();
        let selected: Vec<_> = jobs
            .into_iter()
            .filter(|job| {
                tenant_scope.is_none_or(|tenant| job.tenant == tenant)
                    && matches!(
                        job.status,
                        SkillCandidateJobStatus::Pending | SkillCandidateJobStatus::RetryWait
                    )
                    && job.available_at <= now
                    && lanes.insert(job.serial_key.clone())
            })
            .take(limit)
            .collect();
        let mut report = SkillCompilePreviewBatch::default();
        for job in selected {
            match self.hydrate_job(&job).await {
                Ok(evidence) => report.candidates.push(SkillCompilePreview {
                    job,
                    sanitized_evidence: evidence.sanitized_evidence,
                    environment: evidence.environment,
                    dedup_candidates: evidence.dedup_candidates,
                }),
                Err(_) => report.degraded_job_ids.push(job.job_id),
            }
        }
        Ok(report)
    }

    pub async fn renew(&self, job_id: &str, lease_token: &str) -> Result<(), ServiceError> {
        self.renew_for_tenant(job_id, lease_token, None).await
    }

    pub async fn renew_for_tenant(
        &self,
        job_id: &str,
        lease_token: &str,
        tenant_scope: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.require_job_scope(job_id, tenant_scope).await?;
        let now = current_timestamp();
        let lease_expires_at = timestamp_add_ms(&now, LEASE_MS);
        self.candidate_store
            .renew_skill_candidate_job_lease(job_id, lease_token, &now, &lease_expires_at)
            .await?;
        Ok(())
    }

    pub async fn revalidate_job_evidence(&self, job_id: &str) -> Result<(), ServiceError> {
        let job = self
            .candidate_store
            .get_skill_candidate_job(job_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if job.trigger_version != crate::domain::SKILL_CANDIDATE_TRIGGER_VERSION {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill proposal trigger policy is obsolete",
            )));
        }
        self.hydrate_job(&job).await?;
        Ok(())
    }

    pub async fn fail(
        &self,
        job_id: &str,
        lease_token: &str,
        error_code: &str,
    ) -> Result<(), ServiceError> {
        self.fail_for_tenant(job_id, lease_token, error_code, None)
            .await
    }

    pub async fn fail_for_tenant(
        &self,
        job_id: &str,
        lease_token: &str,
        error_code: &str,
        tenant_scope: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.require_job_scope(job_id, tenant_scope).await?;
        let now = current_timestamp();
        let retry_at = timestamp_add_ms(&now, RETRY_MS);
        self.candidate_store
            .fail_skill_candidate_job(
                job_id,
                lease_token,
                error_code,
                &retry_at,
                &now,
                MAX_ATTEMPTS,
            )
            .await?;
        Ok(())
    }

    pub async fn publish(
        &self,
        request: PublishSkillProposalRequest,
    ) -> Result<PublishSkillProposalOutcome, ServiceError> {
        self.publish_for_tenant(request, None).await
    }

    pub async fn publish_for_tenant(
        &self,
        request: PublishSkillProposalRequest,
        tenant_scope: Option<&str>,
    ) -> Result<PublishSkillProposalOutcome, ServiceError> {
        let _settlement_guard = self
            .settlement_gate
            .try_lock()
            .map_err(|_| StorageError::Conflict("Skill compiler settlement already in progress"))?;
        validate_model_receipt(&request)?;
        let draft = validate_proposal_draft(request.draft)
            .map_err(|_| StorageError::InvalidInput("invalid Skill proposal draft".into()))?;
        let now = current_timestamp();
        let job = self
            .candidate_store
            .get_skill_candidate_job(&request.job_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        require_tenant_scope(&job.tenant, tenant_scope)?;
        match (
            request.target_skill_id.as_deref(),
            request.target_bundle_version_id.as_deref(),
            request.target_capability_capsule_id.as_deref(),
        ) {
            (Some(_), Some(_), Some(_)) | (None, None, None) => {}
            _ => {
                return Err(ServiceError::Storage(StorageError::InvalidInput(
                    "Skill update target is incomplete".into(),
                )))
            }
        }
        let idempotency_key = proposal_idempotency_key(
            &job,
            &draft,
            request.target_skill_id.as_deref(),
            request.target_bundle_version_id.as_deref(),
            request.target_capability_capsule_id.as_deref(),
        );
        let proposal_id = proposal_id(&idempotency_key);
        if let Some(existing) = self
            .skill_store
            .get_skill_proposal(&job.tenant, &proposal_id)
            .await?
        {
            let stored_draft: SkillProposalDraft =
                serde_json::from_str(&existing.draft_json).map_err(StorageError::from)?;
            let stored_target_capsule =
                serde_json::from_str::<serde_json::Value>(&existing.provenance_json)
                    .map_err(StorageError::from)?
                    .get("target_capability_capsule_id")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned);
            if stored_draft != draft
                || existing.target_skill_id != request.target_skill_id
                || existing.expected_head_version != request.target_bundle_version_id
                || stored_target_capsule != request.target_capability_capsule_id
            {
                return Err(ServiceError::Storage(StorageError::Conflict(
                    "Skill proposal replay payload changed",
                )));
            }
            if job.status == SkillCandidateJobStatus::Processing
                && job.lease_token.as_deref() == Some(request.lease_token.as_str())
                && job
                    .lease_expires_at
                    .as_deref()
                    .is_some_and(|expiry| expiry > now.as_str())
            {
                self.candidate_store
                    .complete_skill_candidate_job(&job.job_id, &request.lease_token, &now)
                    .await?;
            }
            return Ok(PublishSkillProposalOutcome::Proposed {
                capability_capsule_id: existing.capsule_id,
            });
        }
        if let Some(existing) = self
            .skill_store
            .get_skill_compile_decision(&job.tenant, &job.job_id)
            .await?
        {
            if existing.decision_kind == "duplicate"
                && existing.canonical_signature.as_deref()
                    == Some(draft.canonical_signature.as_str())
            {
                if job.status == SkillCandidateJobStatus::Processing
                    && job.lease_token.as_deref() == Some(request.lease_token.as_str())
                    && job
                        .lease_expires_at
                        .as_deref()
                        .is_some_and(|expiry| expiry > now.as_str())
                {
                    self.candidate_store
                        .complete_skill_candidate_job(&job.job_id, &request.lease_token, &now)
                        .await?;
                }
                return Ok(PublishSkillProposalOutcome::Duplicate {
                    capability_capsule_id: existing.target_capability_capsule_id.ok_or(
                        StorageError::InvalidData("duplicate decision is missing its target"),
                    )?,
                });
            }
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill candidate already has a different terminal decision",
            )));
        }
        if job.status != SkillCandidateJobStatus::Processing
            || job.lease_token.as_deref() != Some(request.lease_token.as_str())
            || job
                .lease_expires_at
                .as_deref()
                .is_none_or(|expiry| expiry <= now.as_str())
        {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "skill candidate lease lost",
            )));
        }
        // Load again immediately before the first side effect. This is the
        // authoritative head/evidence fence; the earlier claim-time load is
        // only prompt material.
        let hydrated = match self.hydrate_job(&job).await {
            Ok(hydrated) => hydrated,
            Err(error @ StorageError::Conflict(_)) => {
                self.candidate_store
                    .stale_claimed_skill_candidate_job(
                        &job.job_id,
                        &request.lease_token,
                        &current_timestamp(),
                    )
                    .await?;
                return Err(ServiceError::Storage(error));
            }
            Err(error) => return Err(ServiceError::Storage(error)),
        };
        self.require_live_claim(&request.job_id, &request.lease_token)
            .await?;
        if let Some(required) = hydrated.required_update_target.as_ref() {
            if request.target_skill_id.as_deref() != required.target_skill_id.as_deref()
                || request.target_bundle_version_id.as_deref()
                    != required.target_bundle_version_id.as_deref()
                || request.target_capability_capsule_id.as_deref()
                    != Some(required.capability_capsule_id.as_str())
            {
                return Err(ServiceError::Storage(StorageError::Conflict(
                    "feedback revision must update its reviewed base Skill",
                )));
            }
        }
        match (
            request.target_skill_id.as_deref(),
            request.target_bundle_version_id.as_deref(),
            request.target_capability_capsule_id.as_deref(),
        ) {
            (Some(skill_id), Some(version_id), Some(capsule_id)) => {
                let allowed = hydrated.dedup_candidates.iter().any(|candidate| {
                    candidate.target_skill_id.as_deref() == Some(skill_id)
                        && candidate.target_bundle_version_id.as_deref() == Some(version_id)
                        && candidate.capability_capsule_id == capsule_id
                });
                if !allowed {
                    return Err(ServiceError::Storage(StorageError::Conflict(
                        "Skill update target is outside the compiled shortlist",
                    )));
                }
                let head = self
                    .skill_store
                    .get_skill_head(&job.tenant, skill_id)
                    .await?
                    .ok_or(ServiceError::NotFound)?;
                if head.bundle_version_id != version_id {
                    return Err(ServiceError::Storage(StorageError::Conflict(
                        "Skill update target is no longer current",
                    )));
                }
            }
            (None, None, None) => {}
            _ => unreachable!("validated complete update target"),
        }
        if let Some(duplicate) = self
            .find_exact_duplicate(&job.tenant, &draft.canonical_signature)
            .await?
        {
            self.require_live_claim(&request.job_id, &request.lease_token)
                .await?;
            self.skill_store
                .settle_skill_compile_decision(SkillCompileDecisionRecord {
                    job_id: job.job_id.clone(),
                    tenant: job.tenant.clone(),
                    input_fingerprint: job.input_fingerprint.clone(),
                    decision_kind: "duplicate".to_string(),
                    canonical_signature: Some(draft.canonical_signature.clone()),
                    target_capability_capsule_id: Some(duplicate.capability_capsule_id.clone()),
                    artifact_class: None,
                    reason: None,
                    model_id: request.model_id.clone(),
                    finish_reason: request.finish_reason.clone(),
                    prompt_tokens: request.prompt_tokens,
                    completion_tokens: request.completion_tokens,
                    created_at: now.clone(),
                })
                .await?;
            self.candidate_store
                .complete_skill_candidate_job(&job.job_id, &request.lease_token, &now)
                .await?;
            return Ok(PublishSkillProposalOutcome::Duplicate {
                capability_capsule_id: duplicate.capability_capsule_id.clone(),
            });
        }

        self.require_live_claim(&request.job_id, &request.lease_token)
            .await?;

        let evidence = proposal_evidence(&job);
        let stored = self
            .capsule_service
            .ingest(IngestCapabilityCapsuleRequest {
                tenant: job.tenant.clone(),
                capability_capsule_type: CapabilityCapsuleType::Workflow,
                content: render_steps(&draft.steps),
                summary: Some(draft.title.clone()),
                evidence,
                code_refs: Vec::new(),
                scope: Scope::Workspace,
                visibility: Visibility::Shared,
                project: None,
                repo: None,
                module: Some("skill-proposal".to_string()),
                task_type: Some("skill".to_string()),
                tags: vec![
                    "skill-proposal".to_string(),
                    "compiler:v1".to_string(),
                    "hard-redaction:v1".to_string(),
                    "parameterizer:v1".to_string(),
                ],
                topics: Vec::new(),
                source_agent: SKILL_PROPOSAL_SOURCE_AGENT.to_string(),
                idempotency_key: Some(idempotency_key),
                write_mode: WriteMode::Propose,
                supersedes_capability_capsule_id: None,
                expires_at: None,
            })
            .await?;
        if stored.status != CapabilityCapsuleStatus::PendingConfirmation {
            return Err(ServiceError::Storage(StorageError::InvalidData(
                "Skill proposal bypassed pending review",
            )));
        }
        let provenance_json = serde_json::to_string(&serde_json::json!({
            "receipt_version": 1,
            "job_id": &job.job_id,
            "input_fingerprint": &job.input_fingerprint,
            "round_refs": &job.round_refs,
            "compiler_version": "v1",
            "prompt_version": "v1",
            "redaction_version": "v1",
            "parameterizer_version": "v1",
            "classifier_version": "v1",
            "dedup_version": "v1",
            "model_id": &request.model_id,
            "finish_reason": &request.finish_reason,
            "prompt_tokens": request.prompt_tokens,
            "completion_tokens": request.completion_tokens,
            "canonical_signature": &draft.canonical_signature,
            "target_skill_id": &request.target_skill_id,
            "target_bundle_version_id": &request.target_bundle_version_id,
            "target_capability_capsule_id": &request.target_capability_capsule_id,
        }))
        .map_err(StorageError::from)?;
        self.skill_store
            .settle_skill_proposal(SkillProposalRecord {
                proposal_id,
                tenant: job.tenant.clone(),
                job_id: job.job_id.clone(),
                capsule_id: stored.capability_capsule_id.clone(),
                draft_json: serde_json::to_string(&draft).map_err(StorageError::from)?,
                provenance_json,
                target_skill_id: request.target_skill_id,
                expected_head_version: request.target_bundle_version_id,
                status: SkillProposalStatus::PendingConfirmation,
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .await?;
        self.candidate_store
            .complete_skill_candidate_job(&job.job_id, &request.lease_token, &now)
            .await?;
        Ok(PublishSkillProposalOutcome::Proposed {
            capability_capsule_id: stored.capability_capsule_id,
        })
    }

    async fn require_live_claim(
        &self,
        job_id: &str,
        lease_token: &str,
    ) -> Result<crate::domain::SkillCandidateJob, ServiceError> {
        let now = current_timestamp();
        let job = self
            .candidate_store
            .get_skill_candidate_job(job_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if job.status != SkillCandidateJobStatus::Processing
            || job.lease_token.as_deref() != Some(lease_token)
            || job
                .lease_expires_at
                .as_deref()
                .is_none_or(|expiry| expiry <= now.as_str())
        {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "skill candidate lease lost",
            )));
        }
        Ok(job)
    }

    async fn require_job_scope(
        &self,
        job_id: &str,
        tenant_scope: Option<&str>,
    ) -> Result<(), ServiceError> {
        let job = self
            .candidate_store
            .get_skill_candidate_job(job_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        require_tenant_scope(&job.tenant, tenant_scope)
    }
}

pub(super) fn render_steps(steps: &[String]) -> String {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| format!("{}. {step}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn proposal_evidence(job: &crate::domain::SkillCandidateJob) -> Vec<String> {
    let mut evidence = vec![format!("skill_candidate_job:{}", job.job_id)];
    evidence.extend(job.round_refs.iter().map(|reference| {
        format!(
            "completed_tool_round:{}:{}:p{}:t{}",
            reference.round_id,
            reference.source_fingerprint,
            reference.projector_version,
            reference.task_signal_version,
        )
    }));
    evidence
}

fn proposal_idempotency_key(
    job: &crate::domain::SkillCandidateJob,
    draft: &SkillProposalDraft,
    target_skill_id: Option<&str>,
    target_bundle_version_id: Option<&str>,
    target_capability_capsule_id: Option<&str>,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mem.skill_proposal.publish.v1");
    for value in [
        &job.tenant,
        &job.job_id,
        &job.input_fingerprint,
        &draft.canonical_signature,
        target_skill_id.unwrap_or(""),
        target_bundle_version_id.unwrap_or(""),
        target_capability_capsule_id.unwrap_or(""),
    ] {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    format!("skill-proposal:{:x}", hash.finalize())
}

fn proposal_id(idempotency_key: &str) -> String {
    format!("sp_{:x}", Sha256::digest(idempotency_key.as_bytes()))
}

fn validate_model_receipt(request: &PublishSkillProposalRequest) -> Result<(), ServiceError> {
    validate_model_metadata(&request.model_id, &request.finish_reason)
}

fn validate_model_metadata(model_id: &str, finish_reason: &str) -> Result<(), ServiceError> {
    for value in [model_id, finish_reason] {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(ServiceError::Storage(StorageError::InvalidInput(
                "invalid Skill compiler receipt metadata".into(),
            )));
        }
        if hard_secret_redaction::hard_scan(value).is_err() {
            return Err(ServiceError::Storage(StorageError::InvalidInput(
                "unsafe Skill compiler receipt metadata".into(),
            )));
        }
    }
    if !matches!(finish_reason, "stop" | "agent_tool_call") {
        return Err(ServiceError::Storage(StorageError::InvalidInput(
            "Skill compiler output did not finish cleanly".into(),
        )));
    }
    Ok(())
}

fn error_code(error: &StorageError) -> &'static str {
    match error {
        StorageError::Conflict(_) => "evidence_stale",
        StorageError::InvalidInput(_) => "evidence_invalid",
        StorageError::Unsupported(_) => "unsupported_backend",
        _ => "evidence_read_failed",
    }
}

fn require_tenant_scope(tenant: &str, tenant_scope: Option<&str>) -> Result<(), ServiceError> {
    if tenant_scope.is_none_or(|allowed| allowed == tenant) {
        Ok(())
    } else {
        Err(ServiceError::NotFound)
    }
}
