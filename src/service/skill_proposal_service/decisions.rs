use crate::domain::{SkillCandidateJobStatus, SkillCompileDecisionRecord};
use crate::pipeline::hard_secret_redaction;
use crate::pipeline::skill_proposal_compiler::canonical_proposal_signature;
use crate::storage::{current_timestamp, StorageError};

use super::{
    require_tenant_scope, validate_model_metadata, CompleteSkillDecisionRequest, ServiceError,
    SkillProposalService,
};

impl SkillProposalService {
    pub async fn complete_decision_for_tenant(
        &self,
        request: CompleteSkillDecisionRequest,
        tenant_scope: Option<&str>,
    ) -> Result<SkillCompileDecisionRecord, ServiceError> {
        let _settlement_guard = self
            .settlement_gate
            .try_lock()
            .map_err(|_| StorageError::Conflict("Skill compiler settlement already in progress"))?;
        validate_decision_request(&request)?;
        let job = self
            .candidate_store
            .get_skill_candidate_job(&request.job_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        require_tenant_scope(&job.tenant, tenant_scope)?;
        let now = current_timestamp();
        let desired = decision_record(&job, &request, now.clone());
        if let Some(existing) = self
            .skill_store
            .get_skill_compile_decision(&job.tenant, &job.job_id)
            .await?
        {
            if !same_decision_request(&existing, &desired) {
                return Err(ServiceError::Storage(StorageError::Conflict(
                    "Skill candidate decision replay changed",
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
            return Ok(existing);
        }
        self.require_live_claim(&job.job_id, &request.lease_token)
            .await?;
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
        if request.decision_kind == "duplicate"
            && !hydrated.dedup_candidates.iter().any(|candidate| {
                request.target_capability_capsule_id.as_deref()
                    == Some(candidate.capability_capsule_id.as_str())
                    && request.canonical_signature.as_deref()
                        == Some(
                            canonical_proposal_signature(
                                &candidate.title,
                                &candidate.steps,
                                &candidate.parameters,
                            )
                            .as_str(),
                        )
            })
        {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "duplicate decision target is outside the compiled shortlist",
            )));
        }
        self.require_live_claim(&job.job_id, &request.lease_token)
            .await?;
        let stored = self
            .skill_store
            .settle_skill_compile_decision(desired)
            .await?;
        self.candidate_store
            .complete_skill_candidate_job(&job.job_id, &request.lease_token, &now)
            .await?;
        Ok(stored)
    }
}

fn validate_decision_request(request: &CompleteSkillDecisionRequest) -> Result<(), ServiceError> {
    validate_model_metadata(&request.model_id, &request.finish_reason)?;
    let valid = match request.decision_kind.as_str() {
        "duplicate" => {
            request
                .canonical_signature
                .as_deref()
                .is_some_and(is_lower_hex_sha256)
                && request.target_capability_capsule_id.is_some()
                && request.artifact_class.is_none()
                && request.reason.is_none()
        }
        "classified" => {
            request.canonical_signature.is_none()
                && request.target_capability_capsule_id.is_none()
                && request.artifact_class.as_deref().is_some_and(|class| {
                    matches!(class, "memory" | "wiki" | "code_graph" | "ephemeral")
                })
                && request
                    .reason
                    .as_deref()
                    .is_some_and(|reason| !reason.is_empty())
        }
        "nothing_to_save" => {
            request.canonical_signature.is_none()
                && request.target_capability_capsule_id.is_none()
                && request.artifact_class.is_none()
                && request
                    .reason
                    .as_deref()
                    .is_some_and(|reason| !reason.is_empty())
        }
        _ => false,
    };
    if !valid {
        return Err(ServiceError::Storage(StorageError::InvalidInput(
            "invalid terminal Skill compiler decision".into(),
        )));
    }
    for value in request
        .reason
        .iter()
        .chain(request.artifact_class.iter())
        .chain(request.target_capability_capsule_id.iter())
    {
        if value.len() > 4_096
            || value.chars().any(char::is_control)
            || hard_secret_redaction::hard_scan(value).is_err()
        {
            return Err(ServiceError::Storage(StorageError::InvalidInput(
                "unsafe terminal Skill compiler decision".into(),
            )));
        }
    }
    Ok(())
}

fn decision_record(
    job: &crate::domain::SkillCandidateJob,
    request: &CompleteSkillDecisionRequest,
    created_at: String,
) -> SkillCompileDecisionRecord {
    SkillCompileDecisionRecord {
        job_id: job.job_id.clone(),
        tenant: job.tenant.clone(),
        input_fingerprint: job.input_fingerprint.clone(),
        decision_kind: request.decision_kind.clone(),
        canonical_signature: request.canonical_signature.clone(),
        target_capability_capsule_id: request.target_capability_capsule_id.clone(),
        artifact_class: request.artifact_class.clone(),
        reason: request.reason.clone(),
        model_id: request.model_id.clone(),
        finish_reason: request.finish_reason.clone(),
        prompt_tokens: request.prompt_tokens,
        completion_tokens: request.completion_tokens,
        created_at,
    }
}

fn same_decision_request(
    left: &SkillCompileDecisionRecord,
    right: &SkillCompileDecisionRecord,
) -> bool {
    left.job_id == right.job_id
        && left.tenant == right.tenant
        && left.input_fingerprint == right.input_fingerprint
        && left.decision_kind == right.decision_kind
        && left.canonical_signature == right.canonical_signature
        && left.target_capability_capsule_id == right.target_capability_capsule_id
        && left.artifact_class == right.artifact_class
        && left.reason == right.reason
        && left.model_id == right.model_id
        && left.finish_reason == right.finish_reason
        && left.prompt_tokens == right.prompt_tokens
        && left.completion_tokens == right.completion_tokens
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
