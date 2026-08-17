//! Domain contract for compiling review-gated Skill proposals.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactClass {
    Skill,
    Memory,
    Wiki,
    CodeGraph,
    Ephemeral,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    Path,
    Url,
    Host,
    Port,
    Repo,
    Branch,
    ResourceId,
    SecretRef,
    String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct SkillParameter {
    pub name: String,
    pub kind: ParameterKind,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillProposalDraft {
    pub title: String,
    pub steps: Vec<String>,
    pub parameters: Vec<SkillParameter>,
    pub canonical_signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DedupCandidateStatus {
    Active,
    PendingConfirmation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowDedupCandidate {
    pub capability_capsule_id: String,
    pub status: DedupCandidateStatus,
    pub title: String,
    pub steps: Vec<String>,
    pub parameters: Vec<SkillParameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_skill_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bundle_version_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DedupTarget {
    pub capability_capsule_id: String,
    pub status: DedupCandidateStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompileDecision {
    Propose(SkillProposalDraft),
    ProposeUpdate {
        target: DedupTarget,
        target_skill_id: String,
        target_bundle_version_id: String,
        draft: SkillProposalDraft,
    },
    Duplicate {
        target: DedupTarget,
        canonical_signature: String,
    },
    Classified {
        artifact_class: ArtifactClass,
        reason: String,
    },
    NothingToSave {
        reason: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvironmentContext {
    pub workspace_root: Option<String>,
    pub home_dir: Option<String>,
    pub temp_dir: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RawSkillEvidence {
    content: String,
    environment: EnvironmentContext,
}

impl RawSkillEvidence {
    pub fn new(content: impl Into<String>, environment: EnvironmentContext) -> Self {
        Self {
            content: content.into(),
            environment,
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn environment(&self) -> &EnvironmentContext {
        &self.environment
    }
}

impl std::fmt::Debug for RawSkillEvidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawSkillEvidence")
            .field("content_bytes", &self.content.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedModelInput(String);

impl PreparedModelInput {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PreparedModelInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("PreparedModelInput")
            .field(&format_args!("{} bytes", self.0.len()))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompileError {
    #[error("model output is invalid")]
    InvalidModelOutput,
    #[error("generated output failed the hard secret gate")]
    UnsafeGeneratedOutput { finding_count: usize },
    #[error("placeholder is not declared: {name}")]
    UndeclaredPlaceholder { name: String },
    #[error("declared parameter is unused: {name}")]
    UnusedParameter { name: String },
    #[error("secret reference cannot have a default: {name}")]
    SecretDefaultNotAllowed { name: String },
}
