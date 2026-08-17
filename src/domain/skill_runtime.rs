use serde::{Deserialize, Serialize};

use super::{capability_capsule::Visibility, skill_bundle::SkillManifest};

pub const SKILL_SESSION_PIN_TTL_MS: u128 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillProposalStatus {
    PendingConfirmation,
    Accepted,
    Rejected,
    NeedsRebase,
}

impl SkillProposalStatus {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::PendingConfirmation => "pending_confirmation",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::NeedsRebase => "needs_rebase",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "pending_confirmation" => Some(Self::PendingConfirmation),
            "accepted" => Some(Self::Accepted),
            "rejected" => Some(Self::Rejected),
            "needs_rebase" => Some(Self::NeedsRebase),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillProposalRecord {
    pub proposal_id: String,
    pub tenant: String,
    pub job_id: String,
    pub capsule_id: String,
    pub draft_json: String,
    pub provenance_json: String,
    pub target_skill_id: Option<String>,
    pub expected_head_version: Option<String>,
    pub status: SkillProposalStatus,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillResourceBlob {
    pub tenant: String,
    pub sha256: String,
    pub media_type: String,
    pub content: Vec<u8>,
    pub size_bytes: u64,
    pub created_at: String,
}

impl std::fmt::Debug for SkillResourceBlob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SkillResourceBlob")
            .field("tenant", &self.tenant)
            .field("sha256", &self.sha256)
            .field("media_type", &self.media_type)
            .field("content_bytes", &self.content.len())
            .field("size_bytes", &self.size_bytes)
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillBundleVersionRecord {
    pub tenant: String,
    pub skill_id: String,
    pub bundle_version_id: String,
    pub proposal_id: String,
    pub workflow_capsule_id: String,
    pub previous_bundle_version_id: Option<String>,
    pub manifest: SkillManifest,
    pub manifest_sha256: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillHead {
    pub tenant: String,
    pub skill_id: String,
    pub bundle_version_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentLoadoutMode {
    FollowHead,
}

impl AgentLoadoutMode {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::FollowHead => "follow_head",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "follow_head" => Some(Self::FollowHead),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLoadoutBinding {
    pub tenant: String,
    pub agent_id: String,
    pub skill_id: String,
    pub mode: AgentLoadoutMode,
    pub priority: i32,
    pub enabled: bool,
    pub visibility: Visibility,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSkillPin {
    pub tenant: String,
    pub session_id: String,
    pub agent_id: String,
    pub skill_id: String,
    pub bundle_version_id: String,
    pub pinned_at: String,
    pub expires_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillBundleRevocation {
    pub revocation_id: String,
    pub tenant: String,
    pub skill_id: String,
    pub bundle_version_id: String,
    pub reason_code: String,
    pub revoked_by_role: String,
    pub revoked_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillRevisionCandidate {
    pub job_id: String,
    pub tenant: String,
    pub skill_id: String,
    pub base_bundle_version_id: String,
    pub base_capability_capsule_id: String,
    pub feedback_event_ids: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCompileDecisionRecord {
    pub job_id: String,
    pub tenant: String,
    pub input_fingerprint: String,
    pub decision_kind: String,
    pub canonical_signature: Option<String>,
    pub target_capability_capsule_id: Option<String>,
    pub artifact_class: Option<String>,
    pub reason: Option<String>,
    pub model_id: String,
    pub finish_reason: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillFeedbackEvent {
    pub tenant: String,
    pub feedback_id: String,
    pub skill_id: String,
    pub bundle_version_id: String,
    pub feedback_kind: String,
    pub note: Option<String>,
    pub created_at: String,
}
