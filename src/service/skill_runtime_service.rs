use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::capability_capsule::Visibility;
use crate::domain::{
    skill_candidate_serial_key, AgentLoadoutBinding, AgentLoadoutMode, SessionSkillPin,
    SkillBundleRevocation, SkillCandidateJobSpec, SkillCandidateTriggerReason, SkillFeedbackEvent,
    SkillManifest, SkillRevisionCandidate, SKILL_CANDIDATE_TRIGGER_VERSION,
    SKILL_SESSION_PIN_TTL_MS,
};
use crate::storage::{
    current_timestamp, timestamp_add_ms, SkillCandidateStore, SkillStore, StorageError,
};

use super::capability_capsule_service::ServiceError;
use super::CapabilityCapsuleService;

const MAX_LOADOUT_BINDINGS: usize = 64;
const MAX_FEEDBACK_EVENTS: usize = 1_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindSkillRequest {
    pub tenant: String,
    pub agent_id: String,
    pub skill_id: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveSkillLoadoutRequest {
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub skill_id: String,
    pub bundle_version_id: String,
    pub manifest: SkillManifest,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedSkillLoadout {
    pub tenant: String,
    pub agent_id: String,
    pub session_id: String,
    pub skills: Vec<ResolvedSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitSkillFeedbackRequest {
    pub tenant: String,
    pub feedback_id: String,
    pub skill_id: String,
    pub bundle_version_id: String,
    pub feedback_kind: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmitSkillFeedbackResponse {
    pub feedback: SkillFeedbackEvent,
    pub negative_feedback_count: usize,
    pub revision_candidate_ready: bool,
    pub revision_job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevokeSkillBundleRequest {
    pub tenant: String,
    pub skill_id: String,
    pub bundle_version_id: String,
    pub reason_code: String,
    #[serde(skip)]
    pub revoked_by_role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillResourceContent {
    pub media_type: String,
    pub content: Vec<u8>,
}

#[derive(Clone)]
pub struct SkillRuntimeService {
    store: Arc<dyn SkillStore>,
    candidate_store: Arc<dyn SkillCandidateStore>,
    capsule_service: CapabilityCapsuleService,
    feedback_gate: Arc<tokio::sync::Mutex<()>>,
}

impl SkillRuntimeService {
    pub fn new(
        store: Arc<dyn SkillStore>,
        candidate_store: Arc<dyn SkillCandidateStore>,
        capsule_service: CapabilityCapsuleService,
    ) -> Self {
        Self {
            store,
            candidate_store,
            capsule_service,
            feedback_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn bind(
        &self,
        request: BindSkillRequest,
    ) -> Result<AgentLoadoutBinding, ServiceError> {
        validate_key(&request.tenant, "tenant")?;
        validate_key(&request.agent_id, "agent_id")?;
        validate_key(&request.skill_id, "skill_id")?;
        if request.visibility == Visibility::Private {
            return Err(ServiceError::Storage(StorageError::InvalidInput(
                "Private Skill loadout requires owner/principal ACL".into(),
            )));
        }
        let head = self
            .store
            .get_skill_head(&request.tenant, &request.skill_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let bundle = self
            .store
            .get_skill_bundle_version(&request.tenant, &request.skill_id, &head.bundle_version_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        self.require_not_revoked(&request.tenant, &request.skill_id, &head.bundle_version_id)
            .await?;
        self.require_active_anchor(&request.tenant, &bundle.workflow_capsule_id)
            .await?;
        Ok(self
            .store
            .bind_agent_loadout(AgentLoadoutBinding {
                tenant: request.tenant,
                agent_id: request.agent_id,
                skill_id: request.skill_id,
                mode: AgentLoadoutMode::FollowHead,
                priority: request.priority,
                enabled: request.enabled,
                visibility: request.visibility,
                updated_at: current_timestamp(),
            })
            .await?)
    }

    pub async fn resolve(
        &self,
        request: ResolveSkillLoadoutRequest,
    ) -> Result<ResolvedSkillLoadout, ServiceError> {
        validate_key(&request.tenant, "tenant")?;
        validate_key(&request.agent_id, "agent_id")?;
        validate_key(&request.session_id, "session_id")?;
        let bindings = self
            .store
            .list_agent_loadout(&request.tenant, &request.agent_id, MAX_LOADOUT_BINDINGS)
            .await?;
        let mut skills = Vec::new();
        for binding in bindings.into_iter().filter(|binding| binding.enabled) {
            if binding.visibility == Visibility::Private {
                continue;
            }
            let now = current_timestamp();
            let existing = self
                .store
                .get_session_skill_pin(
                    &request.tenant,
                    &request.session_id,
                    &request.agent_id,
                    &binding.skill_id,
                )
                .await?;
            let pin = if let Some(pin) = existing.filter(|pin| pin.expires_at > now) {
                pin
            } else {
                let head = self
                    .store
                    .get_skill_head(&request.tenant, &binding.skill_id)
                    .await?
                    .ok_or(StorageError::NotFound("Skill head"))?;
                let head_bundle = self
                    .store
                    .get_skill_bundle_version(
                        &request.tenant,
                        &binding.skill_id,
                        &head.bundle_version_id,
                    )
                    .await?
                    .ok_or(StorageError::NotFound("head Skill bundle"))?;
                self.require_not_revoked(
                    &request.tenant,
                    &binding.skill_id,
                    &head.bundle_version_id,
                )
                .await?;
                self.require_active_anchor(&request.tenant, &head_bundle.workflow_capsule_id)
                    .await?;
                self.store
                    .get_or_pin_session_skill(new_session_pin(
                        &request.tenant,
                        &request.session_id,
                        &request.agent_id,
                        &binding.skill_id,
                        &head.bundle_version_id,
                        &now,
                    ))
                    .await?
            };
            let bundle = self
                .store
                .get_skill_bundle_version(
                    &request.tenant,
                    &binding.skill_id,
                    &pin.bundle_version_id,
                )
                .await?
                .ok_or(StorageError::NotFound("pinned Skill bundle"))?;
            self.require_not_revoked(&request.tenant, &binding.skill_id, &pin.bundle_version_id)
                .await?;
            self.require_active_anchor(&request.tenant, &bundle.workflow_capsule_id)
                .await?;
            skills.push(ResolvedSkill {
                skill_id: binding.skill_id,
                bundle_version_id: bundle.bundle_version_id,
                manifest: bundle.manifest,
                priority: binding.priority,
            });
        }
        skills.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.skill_id.cmp(&right.skill_id))
        });
        Ok(ResolvedSkillLoadout {
            tenant: request.tenant,
            agent_id: request.agent_id,
            session_id: request.session_id,
            skills,
        })
    }

    pub async fn feedback(
        &self,
        request: SubmitSkillFeedbackRequest,
    ) -> Result<SubmitSkillFeedbackResponse, ServiceError> {
        let _feedback_guard = self.feedback_gate.try_lock().map_err(|_| {
            StorageError::RateLimited("Skill feedback operation is already in progress".into())
        })?;
        validate_key(&request.tenant, "tenant")?;
        validate_key(&request.feedback_id, "feedback_id")?;
        validate_key(&request.skill_id, "skill_id")?;
        validate_key(&request.bundle_version_id, "bundle_version_id")?;
        validate_feedback_kind(&request.feedback_kind)?;
        if self
            .store
            .get_skill_bundle_version(
                &request.tenant,
                &request.skill_id,
                &request.bundle_version_id,
            )
            .await?
            .is_none()
        {
            return Err(ServiceError::NotFound);
        }
        let existing_feedbacks = self
            .store
            .list_skill_feedback(
                &request.tenant,
                &request.skill_id,
                &request.bundle_version_id,
                MAX_FEEDBACK_EVENTS,
            )
            .await?;
        if is_negative_feedback(&request.feedback_kind)
            && !existing_feedbacks
                .iter()
                .any(|event| event.feedback_id == request.feedback_id)
        {
            self.require_feedback_lane_capacity(
                &request.tenant,
                &request.skill_id,
                &request.bundle_version_id,
                &existing_feedbacks,
            )
            .await?;
        }
        let feedback = self
            .store
            .append_skill_feedback(SkillFeedbackEvent {
                tenant: request.tenant.clone(),
                feedback_id: request.feedback_id,
                skill_id: request.skill_id.clone(),
                bundle_version_id: request.bundle_version_id.clone(),
                feedback_kind: request.feedback_kind,
                note: request.note.filter(|note| !note.is_empty()),
                created_at: current_timestamp(),
            })
            .await?;
        let feedbacks = self
            .store
            .list_skill_feedback(
                &request.tenant,
                &request.skill_id,
                &request.bundle_version_id,
                MAX_FEEDBACK_EVENTS,
            )
            .await?;
        let negative_feedback_count = feedbacks
            .iter()
            .filter(|event| {
                matches!(
                    event.feedback_kind.as_str(),
                    "outdated" | "does_not_apply_here" | "incorrect"
                )
            })
            .count();
        let revision_job_id = self
            .enqueue_feedback_revision(
                &request.tenant,
                &request.skill_id,
                &request.bundle_version_id,
                &feedbacks,
            )
            .await?;
        Ok(SubmitSkillFeedbackResponse {
            feedback,
            negative_feedback_count,
            revision_candidate_ready: negative_feedback_count >= 3,
            revision_job_id,
        })
    }

    pub async fn get_resource(
        &self,
        tenant: &str,
        agent_id: &str,
        session_id: &str,
        skill_id: &str,
        bundle_version_id: &str,
        sha256: &str,
    ) -> Result<SkillResourceContent, ServiceError> {
        for (value, label) in [
            (tenant, "tenant"),
            (agent_id, "agent_id"),
            (session_id, "session_id"),
            (skill_id, "skill_id"),
            (bundle_version_id, "bundle_version_id"),
            (sha256, "sha256"),
        ] {
            validate_key(value, label)?;
        }
        self.store
            .get_agent_loadout_binding(tenant, agent_id, skill_id)
            .await?
            .filter(|binding| binding.enabled && binding.visibility != Visibility::Private)
            .ok_or(ServiceError::NotFound)?;
        let pin = self
            .store
            .get_session_skill_pin(tenant, session_id, agent_id, skill_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if pin.expires_at <= current_timestamp() || pin.bundle_version_id != bundle_version_id {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "resource request does not match a live session Skill pin",
            )));
        }
        let bundle = self
            .store
            .get_skill_bundle_version(tenant, skill_id, bundle_version_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        self.require_not_revoked(tenant, skill_id, bundle_version_id)
            .await?;
        self.require_active_anchor(tenant, &bundle.workflow_capsule_id)
            .await?;
        if !bundle
            .manifest
            .resources
            .iter()
            .any(|resource| resource.sha256 == sha256)
        {
            return Err(ServiceError::NotFound);
        }
        let blob = self
            .store
            .get_skill_resource_blob(tenant, sha256)
            .await?
            .ok_or(ServiceError::NotFound)?;
        Ok(SkillResourceContent {
            media_type: blob.media_type,
            content: blob.content,
        })
    }

    pub async fn revoke(
        &self,
        request: RevokeSkillBundleRequest,
    ) -> Result<SkillBundleRevocation, ServiceError> {
        validate_key(&request.tenant, "tenant")?;
        validate_key(&request.skill_id, "skill_id")?;
        validate_key(&request.bundle_version_id, "bundle_version_id")?;
        if request.reason_code.is_empty()
            || request.reason_code.len() > 64
            || !request
                .reason_code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(ServiceError::Storage(StorageError::InvalidInput(
                "invalid Skill revocation reason code".into(),
            )));
        }
        let revocation_id = revocation_id(
            &request.tenant,
            &request.skill_id,
            &request.bundle_version_id,
        );
        let revoked_by_role = request
            .revoked_by_role
            .filter(|role| matches!(role.as_str(), "reviewer" | "admin"))
            .ok_or_else(|| {
                ServiceError::Storage(StorageError::InvalidInput(
                    "missing Skill revocation actor role".into(),
                ))
            })?;
        Ok(self
            .store
            .revoke_skill_bundle(SkillBundleRevocation {
                revocation_id,
                tenant: request.tenant,
                skill_id: request.skill_id,
                bundle_version_id: request.bundle_version_id,
                reason_code: request.reason_code,
                revoked_by_role,
                revoked_at: current_timestamp(),
            })
            .await?)
    }

    async fn require_active_anchor(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), ServiceError> {
        let capsule = self
            .capsule_service
            .get_capability_capsule(Some(tenant), capability_capsule_id)
            .await?
            .capability_capsule;
        if capsule.status != crate::domain::capability_capsule::CapabilityCapsuleStatus::Active {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill bundle anchor is not active",
            )));
        }
        Ok(())
    }

    async fn require_not_revoked(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
    ) -> Result<(), ServiceError> {
        if self
            .store
            .get_skill_bundle_revocation(tenant, skill_id, bundle_version_id)
            .await?
            .is_some()
        {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill bundle has been revoked",
            )));
        }
        Ok(())
    }

    async fn enqueue_feedback_revision(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
        feedbacks: &[SkillFeedbackEvent],
    ) -> Result<Option<String>, ServiceError> {
        let negatives: Vec<_> = feedbacks
            .iter()
            .filter(|event| {
                matches!(
                    event.feedback_kind.as_str(),
                    "outdated" | "does_not_apply_here" | "incorrect"
                )
            })
            .collect();
        let revision = negatives.len() / 3;
        if revision == 0 {
            return Ok(None);
        }
        let head = self.store.get_skill_head(tenant, skill_id).await?;
        if head.as_ref().map(|head| head.bundle_version_id.as_str()) != Some(bundle_version_id) {
            return Ok(None);
        }
        let bundle = self
            .store
            .get_skill_bundle_version(tenant, skill_id, bundle_version_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if self
            .store
            .get_skill_bundle_revocation(tenant, skill_id, bundle_version_id)
            .await?
            .is_some()
        {
            return Ok(None);
        }
        let start = (revision - 1) * 3;
        let feedback_event_ids: Vec<_> = negatives[start..start + 3]
            .iter()
            .map(|event| event.feedback_id.clone())
            .collect();
        let job_id =
            feedback_revision_job_id(tenant, skill_id, bundle_version_id, &feedback_event_ids);
        let now = current_timestamp();
        let caller_agent = feedback_revision_agent(skill_id);
        let serial_key = skill_candidate_serial_key(tenant, &caller_agent);
        self.store
            .insert_skill_revision_candidate(SkillRevisionCandidate {
                job_id: job_id.clone(),
                tenant: tenant.to_string(),
                skill_id: skill_id.to_string(),
                base_bundle_version_id: bundle_version_id.to_string(),
                base_capability_capsule_id: bundle.workflow_capsule_id,
                feedback_event_ids: feedback_event_ids.clone(),
                created_at: now.clone(),
            })
            .await?;
        let input_fingerprint = feedback_revision_input_fingerprint(&feedback_event_ids);
        self.candidate_store
            .ensure_skill_candidate_jobs(
                &[SkillCandidateJobSpec {
                    job_id: job_id.clone(),
                    tenant: tenant.to_string(),
                    caller_agent: caller_agent.clone(),
                    serial_key,
                    candidate_key: format!("skill-revision-{input_fingerprint}"),
                    input_fingerprint,
                    candidate_revision: revision as u32,
                    trigger_version: SKILL_CANDIDATE_TRIGGER_VERSION,
                    trigger_reasons: vec![SkillCandidateTriggerReason::NegativeFeedback],
                    round_refs: Vec::new(),
                    tool_call_count: 0,
                    round_count: 0,
                    distinct_session_count: 0,
                }],
                &now,
            )
            .await?;
        Ok(Some(job_id))
    }

    async fn require_feedback_lane_capacity(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
        feedbacks: &[SkillFeedbackEvent],
    ) -> Result<(), ServiceError> {
        if self
            .store
            .get_skill_head(tenant, skill_id)
            .await?
            .as_ref()
            .map(|head| head.bundle_version_id.as_str())
            != Some(bundle_version_id)
        {
            return Ok(());
        }
        let negatives: Vec<_> = feedbacks
            .iter()
            .filter(|event| is_negative_feedback(&event.feedback_kind))
            .collect();
        let revision = negatives.len() / 3;
        if revision == 0 {
            return Ok(());
        }
        let start = (revision - 1) * 3;
        let feedback_event_ids: Vec<_> = negatives[start..start + 3]
            .iter()
            .map(|event| event.feedback_id.clone())
            .collect();
        let job_id =
            feedback_revision_job_id(tenant, skill_id, bundle_version_id, &feedback_event_ids);
        let nonterminal = self
            .candidate_store
            .get_skill_candidate_job(&job_id)
            .await?
            .is_some_and(|job| {
                matches!(
                    job.status,
                    crate::domain::SkillCandidateJobStatus::Pending
                        | crate::domain::SkillCandidateJobStatus::Processing
                        | crate::domain::SkillCandidateJobStatus::RetryWait
                )
            });
        if nonterminal {
            return Err(ServiceError::Storage(StorageError::RateLimited(
                "current Skill feedback revision is still pending".into(),
            )));
        }
        Ok(())
    }
}

fn is_negative_feedback(kind: &str) -> bool {
    matches!(kind, "outdated" | "does_not_apply_here" | "incorrect")
}

fn new_session_pin(
    tenant: &str,
    session_id: &str,
    agent_id: &str,
    skill_id: &str,
    bundle_version_id: &str,
    now: &str,
) -> SessionSkillPin {
    SessionSkillPin {
        tenant: tenant.to_string(),
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        skill_id: skill_id.to_string(),
        bundle_version_id: bundle_version_id.to_string(),
        pinned_at: now.to_string(),
        expires_at: timestamp_add_ms(now, SKILL_SESSION_PIN_TTL_MS),
        revision: 1,
    }
}

fn revocation_id(tenant: &str, skill_id: &str, bundle_version_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mem.skill_bundle.revocation.v1");
    for value in [tenant, skill_id, bundle_version_id] {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    format!("sbr_{:x}", hash.finalize())
}

fn feedback_revision_job_id(
    tenant: &str,
    skill_id: &str,
    bundle_version_id: &str,
    feedback_event_ids: &[String],
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mem.skill_feedback.revision_job.v1");
    hash.update(SKILL_CANDIDATE_TRIGGER_VERSION.to_le_bytes());
    for value in [tenant, skill_id, bundle_version_id]
        .into_iter()
        .chain(feedback_event_ids.iter().map(String::as_str))
    {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    format!("scr_{:x}", hash.finalize())
}

fn feedback_revision_agent(skill_id: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(skill_id.as_bytes()));
    format!("skill-feedback-{}", &digest[..24])
}

fn feedback_revision_input_fingerprint(feedback_event_ids: &[String]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mem.skill_feedback.input.v1");
    hash.update(SKILL_CANDIDATE_TRIGGER_VERSION.to_le_bytes());
    for feedback_id in feedback_event_ids {
        hash.update((feedback_id.len() as u64).to_le_bytes());
        hash.update(feedback_id.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn validate_feedback_kind(kind: &str) -> Result<(), ServiceError> {
    if matches!(
        kind,
        "useful" | "applies_here" | "outdated" | "does_not_apply_here" | "incorrect"
    ) {
        Ok(())
    } else {
        Err(ServiceError::Storage(StorageError::InvalidInput(
            "invalid Skill feedback kind".into(),
        )))
    }
}

fn validate_key(value: &str, label: &str) -> Result<(), ServiceError> {
    if value.trim().is_empty() || value.len() > 1_024 || value.chars().any(char::is_control) {
        Err(ServiceError::Storage(StorageError::InvalidInput(format!(
            "invalid {label}"
        ))))
    } else {
        Ok(())
    }
}

fn default_true() -> bool {
    true
}
