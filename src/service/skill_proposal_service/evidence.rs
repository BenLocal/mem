use std::collections::HashMap;

use crate::domain::capability_capsule::{CapabilityCapsuleStatus, CapabilityCapsuleType};
use crate::domain::skill_proposal::{
    DedupCandidateStatus, EnvironmentContext, SkillProposalDraft, WorkflowDedupCandidate,
};
use crate::domain::{ClaimedSkillCandidateJob, ConversationMessage, SkillProposalStatus};
use crate::pipeline::environment_parameterizer;
use crate::pipeline::hard_secret_redaction;
use crate::pipeline::skill_proposal_compiler::canonical_proposal_signature;
use crate::storage::StorageError;

use super::{
    render_steps, SkillCompileClaim, SkillProposalService, MAX_CATALOG_STEPS,
    MAX_CATALOG_STEP_CHARS, MAX_CATALOG_TITLE_CHARS, MAX_DEDUP_CANDIDATES, MAX_EVIDENCE_BLOCKS,
    MAX_EVIDENCE_BYTES, MAX_SESSION_BLOCKS, MAX_SESSION_BYTES,
};
use crate::service::capability_capsule_service::ServiceError;

pub(super) struct HydratedEvidence {
    pub(super) sanitized_evidence: String,
    pub(super) environment: EnvironmentContext,
    pub(super) dedup_candidates: Vec<WorkflowDedupCandidate>,
    pub(super) required_update_target: Option<WorkflowDedupCandidate>,
}

impl SkillProposalService {
    pub(super) async fn hydrate_claim(
        &self,
        claim: &ClaimedSkillCandidateJob,
    ) -> Result<SkillCompileClaim, StorageError> {
        let evidence = self.hydrate_job(&claim.job).await?;
        Ok(SkillCompileClaim {
            claim: claim.clone(),
            sanitized_evidence: evidence.sanitized_evidence,
            environment: evidence.environment,
            dedup_candidates: evidence.dedup_candidates,
        })
    }

    pub(super) async fn hydrate_job(
        &self,
        job: &crate::domain::SkillCandidateJob,
    ) -> Result<HydratedEvidence, StorageError> {
        if let Some(revision) = self
            .skill_store
            .get_skill_revision_candidate(&job.tenant, &job.job_id)
            .await?
        {
            return self.hydrate_revision_job(job, revision).await;
        }
        if job
            .trigger_reasons
            .contains(&crate::domain::SkillCandidateTriggerReason::NegativeFeedback)
        {
            return Err(StorageError::Conflict(
                "Skill feedback revision evidence is missing",
            ));
        }
        let mut messages_by_session: HashMap<String, Vec<ConversationMessage>> = HashMap::new();
        let mut evidence_blocks = Vec::new();
        let mut environment = EnvironmentContext::default();
        for reference in &job.round_refs {
            let latest = self
                .round_store
                .latest_completed_tool_rounds(&job.tenant, &reference.session_id)
                .await?;
            let round = latest
                .rounds
                .iter()
                .find(|round| {
                    round.round_id == reference.round_id
                        && round.source_fingerprint == reference.source_fingerprint
                        && round.projector_version == reference.projector_version
                        && round.task_signal_version == reference.task_signal_version
                })
                .ok_or(StorageError::Conflict(
                    "skill candidate evidence is no longer current",
                ))?;
            if !messages_by_session.contains_key(&reference.session_id) {
                let messages = self
                    .round_store
                    .load_round_source_messages(
                        &job.tenant,
                        &reference.session_id,
                        MAX_SESSION_BLOCKS,
                        MAX_SESSION_BYTES,
                    )
                    .await?;
                messages_by_session.insert(reference.session_id.clone(), messages);
            }
            let messages = messages_by_session
                .get(&reference.session_id)
                .expect("inserted session messages");
            for message in messages
                .iter()
                .filter(|message| within_round(message, round))
            {
                if evidence_blocks.len() >= MAX_EVIDENCE_BLOCKS {
                    return Err(StorageError::InvalidInput(
                        "skill candidate evidence block limit exceeded".into(),
                    ));
                }
                if environment.workspace_root.is_none() {
                    environment.workspace_root =
                        message.meta_json.as_deref().and_then(parse_cwd_from_meta);
                }
                evidence_blocks.push(render_evidence_block(message));
            }
        }
        let raw = evidence_blocks.join("\n");
        if raw.len() > MAX_EVIDENCE_BYTES {
            return Err(StorageError::InvalidInput(
                "skill candidate evidence byte limit exceeded".into(),
            ));
        }
        let sanitized = hard_secret_redaction::hard_scrub(&raw);
        let parameterized =
            environment_parameterizer::parameterize(sanitized.as_str(), &environment);
        Ok(HydratedEvidence {
            sanitized_evidence: parameterized,
            // Environment literals have already been replaced inside the
            // single writer; never serialize their raw values to the CLI.
            environment: EnvironmentContext::default(),
            dedup_candidates: self.dedup_candidates(&job.tenant, None).await?,
            required_update_target: None,
        })
    }

    async fn hydrate_revision_job(
        &self,
        job: &crate::domain::SkillCandidateJob,
        revision: crate::domain::SkillRevisionCandidate,
    ) -> Result<HydratedEvidence, StorageError> {
        if job.trigger_reasons != [crate::domain::SkillCandidateTriggerReason::NegativeFeedback]
            || revision.tenant != job.tenant
        {
            return Err(StorageError::Conflict(
                "Skill feedback revision receipt does not match its job",
            ));
        }
        let head = self
            .skill_store
            .get_skill_head(&job.tenant, &revision.skill_id)
            .await?
            .ok_or(StorageError::Conflict(
                "Skill feedback revision base is no longer current",
            ))?;
        if head.bundle_version_id != revision.base_bundle_version_id {
            return Err(StorageError::Conflict(
                "Skill feedback revision base is no longer current",
            ));
        }
        if self
            .skill_store
            .get_skill_bundle_revocation(
                &job.tenant,
                &revision.skill_id,
                &revision.base_bundle_version_id,
            )
            .await?
            .is_some()
        {
            return Err(StorageError::Conflict(
                "Skill feedback revision base has been revoked",
            ));
        }
        let bundle = self
            .skill_store
            .get_skill_bundle_version(
                &job.tenant,
                &revision.skill_id,
                &revision.base_bundle_version_id,
            )
            .await?
            .ok_or(StorageError::Conflict(
                "Skill feedback revision bundle is missing",
            ))?;
        if bundle.workflow_capsule_id != revision.base_capability_capsule_id {
            return Err(StorageError::Conflict(
                "Skill feedback revision capsule changed",
            ));
        }
        let proposal = self
            .skill_store
            .get_skill_proposal(&job.tenant, &bundle.proposal_id)
            .await?
            .ok_or(StorageError::Conflict(
                "Skill feedback revision proposal is missing",
            ))?;
        if proposal.status != SkillProposalStatus::Accepted {
            return Err(StorageError::Conflict(
                "Skill feedback revision proposal is not accepted",
            ));
        }
        let draft: SkillProposalDraft = serde_json::from_str(&proposal.draft_json)?;
        let feedbacks = self
            .skill_store
            .list_skill_feedback(
                &job.tenant,
                &revision.skill_id,
                &revision.base_bundle_version_id,
                1_000,
            )
            .await?;
        let by_id: HashMap<_, _> = feedbacks
            .into_iter()
            .map(|feedback| (feedback.feedback_id.clone(), feedback))
            .collect();
        let mut evidence = format!(
            "PUBLISHED SKILL BASE (quoted data):\ntitle: {}\nsteps:\n{}\n\nNEGATIVE FEEDBACK (quoted data):\n",
            draft.title,
            render_steps(&draft.steps),
        );
        for feedback_id in &revision.feedback_event_ids {
            let feedback = by_id.get(feedback_id).ok_or(StorageError::Conflict(
                "Skill feedback revision event is missing",
            ))?;
            evidence.push_str(&format!(
                "- kind={} note={}\n",
                feedback.feedback_kind,
                feedback.note.as_deref().unwrap_or("none"),
            ));
        }
        let sanitized = hard_secret_redaction::hard_scrub(&evidence);
        let parameterized = environment_parameterizer::parameterize(
            sanitized.as_str(),
            &EnvironmentContext::default(),
        );
        let candidates = self
            .dedup_candidates(
                &job.tenant,
                Some(revision.base_capability_capsule_id.as_str()),
            )
            .await?;
        let required_update_target = candidates
            .iter()
            .find(|candidate| {
                candidate.capability_capsule_id == revision.base_capability_capsule_id
                    && candidate.target_skill_id.as_deref() == Some(revision.skill_id.as_str())
                    && candidate.target_bundle_version_id.as_deref()
                        == Some(revision.base_bundle_version_id.as_str())
            })
            .cloned()
            .ok_or(StorageError::Conflict(
                "Skill feedback revision base is outside the active catalog",
            ))?;
        Ok(HydratedEvidence {
            sanitized_evidence: parameterized,
            environment: EnvironmentContext::default(),
            dedup_candidates: candidates,
            required_update_target: Some(required_update_target),
        })
    }

    async fn dedup_candidates(
        &self,
        tenant: &str,
        required_capsule_id: Option<&str>,
    ) -> Result<Vec<WorkflowDedupCandidate>, StorageError> {
        let mut candidates = self.all_dedup_candidates(tenant, true).await?;
        candidates.sort_by(|left, right| {
            let left_required = Some(left.capability_capsule_id.as_str()) == required_capsule_id;
            let right_required = Some(right.capability_capsule_id.as_str()) == required_capsule_id;
            right_required
                .cmp(&left_required)
                .then_with(|| left.capability_capsule_id.cmp(&right.capability_capsule_id))
        });
        candidates.truncate(MAX_DEDUP_CANDIDATES);
        Ok(candidates)
    }

    pub(super) async fn find_exact_duplicate(
        &self,
        tenant: &str,
        canonical_signature: &str,
    ) -> Result<Option<WorkflowDedupCandidate>, StorageError> {
        Ok(self
            .all_dedup_candidates(tenant, false)
            .await?
            .into_iter()
            .find(|candidate| {
                canonical_proposal_signature(
                    &candidate.title,
                    &candidate.steps,
                    &candidate.parameters,
                ) == canonical_signature
            }))
    }

    async fn all_dedup_candidates(
        &self,
        tenant: &str,
        bounded_for_prompt: bool,
    ) -> Result<Vec<WorkflowDedupCandidate>, StorageError> {
        let capsules = self
            .capsule_service
            .list_capability_capsules(tenant)
            .await
            .map_err(service_to_storage)?;
        let mut candidates = Vec::new();
        for capsule in capsules
            .into_iter()
            .filter(|capsule| capsule.capability_capsule_type == CapabilityCapsuleType::Workflow)
        {
            let status = match capsule.status {
                CapabilityCapsuleStatus::Active => DedupCandidateStatus::Active,
                CapabilityCapsuleStatus::PendingConfirmation => {
                    DedupCandidateStatus::PendingConfirmation
                }
                _ => continue,
            };
            let bundle = self
                .skill_store
                .find_skill_bundle_by_workflow_capsule(tenant, &capsule.capability_capsule_id)
                .await?;
            let mut title = hard_secret_redaction::hard_scrub(&capsule.summary)
                .as_str()
                .to_string();
            let mut steps: Vec<_> = parse_steps(&capsule.content)
                .into_iter()
                .map(|step| {
                    hard_secret_redaction::hard_scrub(&step)
                        .as_str()
                        .to_string()
                })
                .collect();
            if bounded_for_prompt {
                title = bounded_catalog_text(&title, MAX_CATALOG_TITLE_CHARS);
                steps.truncate(MAX_CATALOG_STEPS);
                steps = steps
                    .into_iter()
                    .map(|step| bounded_catalog_text(&step, MAX_CATALOG_STEP_CHARS))
                    .collect();
            }
            candidates.push(WorkflowDedupCandidate {
                capability_capsule_id: capsule.capability_capsule_id,
                status,
                title,
                steps,
                parameters: Vec::new(),
                target_skill_id: bundle.as_ref().map(|bundle| bundle.skill_id.clone()),
                target_bundle_version_id: bundle.map(|bundle| bundle.bundle_version_id),
            });
        }
        Ok(candidates)
    }
}

fn bounded_catalog_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        value.chars().take(max_chars).collect()
    }
}

fn within_round(message: &ConversationMessage, round: &crate::domain::CompletedToolRound) -> bool {
    let position = (message.line_number, message.block_index);
    position >= (round.start_line_number, round.start_block_index)
        && position <= (round.end_line_number, round.end_block_index)
}

fn render_evidence_block(message: &ConversationMessage) -> String {
    format!(
        "[role={:?} block={:?} tool={}]\n{}",
        message.role,
        message.block_type,
        message.tool_name.as_deref().unwrap_or("none"),
        message.content
    )
}

fn parse_cwd_from_meta(meta: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(meta)
        .ok()?
        .get("cwd")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_steps(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once(". ")
                .filter(|(prefix, _)| prefix.bytes().all(|byte| byte.is_ascii_digit()))
                .map_or(line, |(_, step)| step)
                .to_string()
        })
        .collect()
}

fn service_to_storage(error: ServiceError) -> StorageError {
    match error {
        ServiceError::Storage(error) => error,
        ServiceError::NotFound => StorageError::NotFound("capsule"),
        ServiceError::Graph(_) => StorageError::InvalidData("graph error during Skill dedup"),
    }
}
