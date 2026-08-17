use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::capability_capsule::CapabilityCapsuleStatus;
use crate::domain::skill_bundle::{
    ResourceEntry, SkillId, SkillManifest, SKILL_DOCUMENT_PATH, SKILL_MANIFEST_SCHEMA_VERSION,
};
use crate::domain::skill_proposal::SkillProposalDraft;
use crate::domain::{
    SessionSkillPin, SkillBundleVersionRecord, SkillHead, SkillProposalStatus, SkillResourceBlob,
    SKILL_SESSION_PIN_TTL_MS,
};
use crate::pipeline::hard_secret_redaction;
use crate::pipeline::skill_proposal_compiler::validate_proposal_draft;
use crate::storage::{current_timestamp, timestamp_add_ms, SkillStore, StorageError};

use super::SkillProposalService;

use super::{capability_capsule_service::ServiceError, CapabilityCapsuleService};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptSkillProposalRequest {
    pub tenant: String,
    pub proposal_id: String,
    #[serde(default)]
    pub expected_head_version: Option<String>,
    #[serde(default)]
    pub adopt_session: Option<AdoptSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdoptSession {
    pub session_id: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptSkillProposalResponse {
    pub skill_id: String,
    pub bundle_version_id: String,
    pub workflow_capsule_id: String,
    pub session_pin: Option<SessionSkillPin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RejectSkillProposalRequest {
    pub tenant: String,
    pub proposal_id: String,
}

#[derive(Clone)]
pub struct SkillGovernanceService {
    store: Arc<dyn SkillStore>,
    capsule_service: CapabilityCapsuleService,
    proposal_service: Arc<SkillProposalService>,
    accept_gate: Arc<tokio::sync::Mutex<()>>,
}

impl SkillGovernanceService {
    pub fn new(
        store: Arc<dyn SkillStore>,
        capsule_service: CapabilityCapsuleService,
        proposal_service: Arc<SkillProposalService>,
    ) -> Self {
        Self {
            store,
            capsule_service,
            proposal_service,
            accept_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn accept(
        &self,
        request: AcceptSkillProposalRequest,
    ) -> Result<AcceptSkillProposalResponse, ServiceError> {
        validate_identifier(&request.tenant, "tenant")?;
        validate_identifier(&request.proposal_id, "proposal_id")?;
        let _guard = self.accept_gate.lock().await;
        let mut proposal = self
            .store
            .get_skill_proposal(&request.tenant, &request.proposal_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if !matches!(
            proposal.status,
            SkillProposalStatus::PendingConfirmation | SkillProposalStatus::Accepted
        ) {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill proposal is not reviewable",
            )));
        }
        if proposal.expected_head_version != request.expected_head_version {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "review expectation does not match compiled Skill base",
            )));
        }
        let skill_id = proposal
            .target_skill_id
            .clone()
            .unwrap_or_else(|| deterministic_skill_id(&proposal.proposal_id));
        if let Some(bundle) = self
            .store
            .find_skill_bundle_by_workflow_capsule(&request.tenant, &proposal.capsule_id)
            .await?
        {
            if bundle.proposal_id != proposal.proposal_id
                || bundle.skill_id != skill_id
                || bundle.previous_bundle_version_id != proposal.expected_head_version
            {
                return Err(ServiceError::Storage(StorageError::Conflict(
                    "staged Skill bundle does not match its proposal",
                )));
            }
            return self.resume_bundle(request, proposal, bundle).await;
        }
        if proposal.status == SkillProposalStatus::Accepted {
            return Err(ServiceError::Storage(StorageError::InvalidData(
                "accepted Skill proposal is missing its immutable bundle",
            )));
        }
        self.proposal_service
            .revalidate_job_evidence(&proposal.job_id)
            .await?;
        let draft: SkillProposalDraft = serde_json::from_str(&proposal.draft_json)
            .map_err(|_| StorageError::InvalidData("invalid Skill proposal draft"))?;
        let draft = validate_proposal_draft(draft)
            .map_err(|_| StorageError::InvalidData("invalid Skill proposal draft"))?;
        let current_head = self
            .store
            .get_skill_head(&request.tenant, &skill_id)
            .await?;
        if current_head
            .as_ref()
            .map(|head| head.bundle_version_id.as_str())
            != request.expected_head_version.as_deref()
        {
            if proposal.status == SkillProposalStatus::PendingConfirmation {
                proposal = self
                    .store
                    .update_skill_proposal_outcome(
                        &request.tenant,
                        &request.proposal_id,
                        SkillProposalStatus::PendingConfirmation,
                        SkillProposalStatus::NeedsRebase,
                        &current_timestamp(),
                    )
                    .await?;
            }
            let _ = proposal;
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill head changed; proposal needs rebase",
            )));
        }

        // Validate and persist only an unreferenced content-addressed blob
        // before changing lifecycle state. If this phase fails, the proposal
        // remains PendingConfirmation and no runtime-visible head exists.
        let skill_document = render_skill_document(&skill_id, &draft)?;
        hard_secret_redaction::hard_scan(&skill_document)
            .map_err(|_| StorageError::InvalidData("unsafe Skill bundle content"))?;
        let document_sha = format!("{:x}", Sha256::digest(skill_document.as_bytes()));
        let document_size = skill_document.len() as u64;
        let now = current_timestamp();
        self.store
            .put_skill_resource_blob(SkillResourceBlob {
                tenant: request.tenant.clone(),
                sha256: document_sha.clone(),
                media_type: "text/markdown".to_string(),
                content: skill_document.into_bytes(),
                size_bytes: document_size,
                created_at: now.clone(),
            })
            .await?;
        let actual_blob = self
            .store
            .get_skill_resource_blob(&request.tenant, &document_sha)
            .await?
            .ok_or(StorageError::NotFound("skill resource blob"))?;
        let manifest = SkillManifest {
            schema_version: SKILL_MANIFEST_SCHEMA_VERSION,
            skill_id: SkillId(skill_id.clone()),
            resources: vec![ResourceEntry {
                path: SKILL_DOCUMENT_PATH.to_string(),
                media_type: actual_blob.media_type,
                sha256: actual_blob.sha256,
                size_bytes: actual_blob.size_bytes,
                executable: false,
            }],
        };
        let manifest_sha256 = manifest
            .digest()
            .map_err(|_| StorageError::InvalidData("invalid Skill manifest"))?;
        let bundle_version_id =
            deterministic_bundle_version_id(&skill_id, &proposal.proposal_id, &manifest_sha256);

        let capsule = self
            .capsule_service
            .get_capability_capsule(Some(&request.tenant), &proposal.capsule_id)
            .await?
            .capability_capsule;
        if !matches!(
            capsule.status,
            CapabilityCapsuleStatus::PendingConfirmation | CapabilityCapsuleStatus::Active
        ) {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill proposal capsule is not pending",
            )));
        }
        let workflow_capsule_id = capsule.capability_capsule_id;

        self.store
            .append_skill_bundle_version(SkillBundleVersionRecord {
                tenant: request.tenant.clone(),
                skill_id: skill_id.clone(),
                bundle_version_id: bundle_version_id.clone(),
                proposal_id: proposal.proposal_id.clone(),
                workflow_capsule_id: workflow_capsule_id.clone(),
                previous_bundle_version_id: current_head
                    .as_ref()
                    .map(|head| head.bundle_version_id.clone()),
                manifest,
                manifest_sha256,
                created_at: now.clone(),
            })
            .await?;
        self.store
            .compare_and_set_skill_head(
                request.expected_head_version.as_deref(),
                SkillHead {
                    tenant: request.tenant.clone(),
                    skill_id: skill_id.clone(),
                    bundle_version_id: bundle_version_id.clone(),
                    updated_at: now.clone(),
                },
            )
            .await?;
        proposal = self
            .store
            .update_skill_proposal_outcome(
                &request.tenant,
                &request.proposal_id,
                SkillProposalStatus::PendingConfirmation,
                SkillProposalStatus::Accepted,
                &current_timestamp(),
            )
            .await?;
        debug_assert_eq!(proposal.status, SkillProposalStatus::Accepted);
        if capsule.status == CapabilityCapsuleStatus::PendingConfirmation {
            self.capsule_service
                .accept_skill_proposal_capsule(&request.tenant, &workflow_capsule_id)
                .await?;
        }
        let session_pin = if let Some(adopt) = request.adopt_session {
            Some(
                self.store
                    .get_or_pin_session_skill(SessionSkillPin {
                        tenant: request.tenant,
                        session_id: adopt.session_id,
                        agent_id: adopt.agent_id,
                        skill_id: skill_id.clone(),
                        bundle_version_id: bundle_version_id.clone(),
                        pinned_at: now,
                        expires_at: timestamp_add_ms(
                            &current_timestamp(),
                            SKILL_SESSION_PIN_TTL_MS,
                        ),
                        revision: 1,
                    })
                    .await?,
            )
        } else {
            None
        };
        Ok(AcceptSkillProposalResponse {
            skill_id,
            bundle_version_id,
            workflow_capsule_id,
            session_pin,
        })
    }

    async fn resume_bundle(
        &self,
        request: AcceptSkillProposalRequest,
        proposal: crate::domain::SkillProposalRecord,
        bundle: SkillBundleVersionRecord,
    ) -> Result<AcceptSkillProposalResponse, ServiceError> {
        let current_head = self
            .store
            .get_skill_head(&request.tenant, &bundle.skill_id)
            .await?;
        let current_version = current_head
            .as_ref()
            .map(|head| head.bundle_version_id.as_str());
        let mut is_current_head = current_version == Some(bundle.bundle_version_id.as_str());
        if current_version != Some(bundle.bundle_version_id.as_str()) {
            if current_version == proposal.expected_head_version.as_deref() {
                self.store
                    .compare_and_set_skill_head(
                        proposal.expected_head_version.as_deref(),
                        SkillHead {
                            tenant: request.tenant.clone(),
                            skill_id: bundle.skill_id.clone(),
                            bundle_version_id: bundle.bundle_version_id.clone(),
                            updated_at: current_timestamp(),
                        },
                    )
                    .await?;
                is_current_head = true;
            } else if proposal.status != SkillProposalStatus::Accepted {
                self.store
                    .update_skill_proposal_outcome(
                        &request.tenant,
                        &request.proposal_id,
                        SkillProposalStatus::PendingConfirmation,
                        SkillProposalStatus::NeedsRebase,
                        &current_timestamp(),
                    )
                    .await?;
                return Err(ServiceError::Storage(StorageError::Conflict(
                    "Skill head changed; staged proposal needs rebase",
                )));
            }
        }
        if request.adopt_session.is_some() && !is_current_head {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "historical Skill replay cannot adopt a new session pin",
            )));
        }
        if request.adopt_session.is_some()
            && self
                .store
                .get_skill_bundle_revocation(
                    &request.tenant,
                    &bundle.skill_id,
                    &bundle.bundle_version_id,
                )
                .await?
                .is_some()
        {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "revoked Skill bundle cannot be pinned",
            )));
        }
        if proposal.status == SkillProposalStatus::PendingConfirmation {
            self.store
                .update_skill_proposal_outcome(
                    &request.tenant,
                    &request.proposal_id,
                    SkillProposalStatus::PendingConfirmation,
                    SkillProposalStatus::Accepted,
                    &current_timestamp(),
                )
                .await?;
        }
        let capsule = self
            .capsule_service
            .get_capability_capsule(Some(&request.tenant), &bundle.workflow_capsule_id)
            .await?
            .capability_capsule;
        let workflow_capsule_id = match capsule.status {
            CapabilityCapsuleStatus::PendingConfirmation => {
                self.capsule_service
                    .accept_skill_proposal_capsule(&request.tenant, &bundle.workflow_capsule_id)
                    .await?
                    .capability_capsule_id
            }
            CapabilityCapsuleStatus::Active => capsule.capability_capsule_id,
            _ => {
                return Err(ServiceError::Storage(StorageError::Conflict(
                    "published Skill anchor is not reviewable",
                )))
            }
        };
        let session_pin = if let Some(adopt) = request.adopt_session {
            Some(
                self.store
                    .get_or_pin_session_skill(SessionSkillPin {
                        tenant: request.tenant,
                        session_id: adopt.session_id,
                        agent_id: adopt.agent_id,
                        skill_id: bundle.skill_id.clone(),
                        bundle_version_id: bundle.bundle_version_id.clone(),
                        pinned_at: current_timestamp(),
                        expires_at: timestamp_add_ms(
                            &current_timestamp(),
                            SKILL_SESSION_PIN_TTL_MS,
                        ),
                        revision: 1,
                    })
                    .await?,
            )
        } else {
            None
        };
        Ok(AcceptSkillProposalResponse {
            skill_id: bundle.skill_id,
            bundle_version_id: bundle.bundle_version_id,
            workflow_capsule_id,
            session_pin,
        })
    }

    pub async fn reject(&self, request: RejectSkillProposalRequest) -> Result<(), ServiceError> {
        let _guard = self.accept_gate.lock().await;
        let proposal = self
            .store
            .get_skill_proposal(&request.tenant, &request.proposal_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if proposal.status == SkillProposalStatus::Rejected {
            return Ok(());
        }
        if !matches!(
            proposal.status,
            SkillProposalStatus::PendingConfirmation | SkillProposalStatus::NeedsRebase
        ) {
            return Err(ServiceError::Storage(StorageError::Conflict(
                "Skill proposal is not reviewable",
            )));
        }
        let capsule = self
            .capsule_service
            .get_capability_capsule(Some(&request.tenant), &proposal.capsule_id)
            .await?
            .capability_capsule;
        match capsule.status {
            CapabilityCapsuleStatus::PendingConfirmation => {
                self.capsule_service
                    .reject_skill_proposal_capsule(&request.tenant, &proposal.capsule_id)
                    .await?;
            }
            CapabilityCapsuleStatus::Rejected => {}
            _ => {
                return Err(ServiceError::Storage(StorageError::Conflict(
                    "Skill proposal capsule is not rejectable",
                )))
            }
        }
        self.store
            .update_skill_proposal_outcome(
                &request.tenant,
                &request.proposal_id,
                proposal.status,
                SkillProposalStatus::Rejected,
                &current_timestamp(),
            )
            .await?;
        Ok(())
    }
}

fn render_skill_document(
    skill_id: &str,
    draft: &SkillProposalDraft,
) -> Result<String, ServiceError> {
    let name_hash = format!("{:x}", Sha256::digest(skill_id.as_bytes()));
    let description = serde_json::to_string(&format!(
        "Use this skill when the task requires: {}",
        draft.title
    ))
    .map_err(StorageError::from)?;
    let mut output = format!(
        "---\nname: mem-{}\ndescription: {}\n---\n\n# {}\n\n",
        &name_hash[..24],
        description,
        draft.title
    );
    if !draft.parameters.is_empty() {
        output.push_str("## Parameters\n\n");
        for parameter in &draft.parameters {
            output.push_str(&format!(
                "- `{{{{{}}}}}`: {:?}{}\n",
                parameter.name,
                parameter.kind,
                if parameter.required {
                    " (required)"
                } else {
                    ""
                }
            ));
        }
        output.push('\n');
    }
    output.push_str("## Steps\n\n");
    for (index, step) in draft.steps.iter().enumerate() {
        output.push_str(&format!("{}. {step}\n", index + 1));
    }
    Ok(output)
}

fn deterministic_skill_id(proposal_id: &str) -> String {
    format!("skill_{:x}", Sha256::digest(proposal_id.as_bytes()))
}

fn deterministic_bundle_version_id(
    skill_id: &str,
    proposal_id: &str,
    manifest_sha256: &str,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mem.skill_bundle.version.v1");
    for value in [skill_id, proposal_id, manifest_sha256] {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    format!("sbv_{:x}", hash.finalize())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), ServiceError> {
    if value.trim().is_empty() || value.len() > 1_024 || value.chars().any(char::is_control) {
        return Err(ServiceError::Storage(StorageError::InvalidInput(format!(
            "invalid {label}"
        ))));
    }
    Ok(())
}
