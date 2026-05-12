use serde::{Deserialize, Serialize};

fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityCapsuleStatus {
    #[default]
    PendingConfirmation,
    Provisional,
    Active,
    Archived,
    Rejected,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityCapsuleType {
    #[default]
    Implementation,
    Experience,
    Preference,
    Episode,
    Workflow,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Global,
    Project,
    Repo,
    Workspace,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    Shared,
    System,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    #[default]
    Auto,
    #[serde(rename = "propose")]
    Propose,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    Useful,
    Outdated,
    Incorrect,
    AppliesHere,
    DoesNotApplyHere,
}

impl FeedbackKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Outdated => "outdated",
            Self::Incorrect => "incorrect",
            Self::AppliesHere => "applies_here",
            Self::DoesNotApplyHere => "does_not_apply_here",
        }
    }

    pub fn confidence_delta(&self) -> f32 {
        match self {
            Self::Useful => 0.1,
            Self::AppliesHere => 0.05,
            _ => 0.0,
        }
    }

    pub fn decay_delta(&self) -> f32 {
        match self {
            Self::Outdated => 0.2,
            Self::DoesNotApplyHere => 0.1,
            _ => 0.0,
        }
    }

    pub fn marks_validated(&self) -> bool {
        matches!(self, Self::Useful | Self::AppliesHere)
    }

    pub fn archived_status(&self) -> bool {
        matches!(self, Self::Incorrect)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub struct IngestCapabilityCapsuleRequest {
    pub tenant: String,
    pub capability_capsule_type: CapabilityCapsuleType,
    pub content: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub summary: Option<String>,
    pub evidence: Vec<String>,
    pub code_refs: Vec<String>,
    pub scope: Scope,
    pub visibility: Visibility,
    #[serde(skip_serializing_if = "skip_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub task_type: Option<String>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    pub source_agent: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    pub write_mode: WriteMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct CapabilityCapsuleRecord {
    pub capability_capsule_id: String,
    pub tenant: String,
    pub capability_capsule_type: CapabilityCapsuleType,
    pub status: CapabilityCapsuleStatus,
    pub scope: Scope,
    pub visibility: Visibility,
    pub version: u64,
    pub summary: String,
    pub content: String,
    pub evidence: Vec<String>,
    pub code_refs: Vec<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub task_type: Option<String>,
    pub tags: Vec<String>,
    pub topics: Vec<String>,
    pub confidence: f32,
    pub decay_score: f32,
    pub content_hash: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub supersedes_capability_capsule_id: Option<String>,
    pub source_agent: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub last_validated_at: Option<String>,
}

impl Default for CapabilityCapsuleRecord {
    fn default() -> Self {
        Self {
            capability_capsule_id: String::new(),
            tenant: String::new(),
            capability_capsule_type: CapabilityCapsuleType::default(),
            status: CapabilityCapsuleStatus::default(),
            scope: Scope::default(),
            visibility: Visibility::default(),
            version: 0,
            summary: String::new(),
            content: String::new(),
            evidence: Vec::new(),
            code_refs: Vec::new(),
            project: None,
            repo: None,
            module: None,
            task_type: None,
            tags: Vec::new(),
            topics: Vec::new(),
            confidence: 0.0,
            decay_score: 0.0,
            content_hash: String::new(),
            idempotency_key: None,
            session_id: None,
            supersedes_capability_capsule_id: None,
            source_agent: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            last_validated_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct EditPendingRequest {
    pub capability_capsule_id: String,
    pub summary: String,
    pub content: String,
    pub evidence: Vec<String>,
    pub code_refs: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct EditPendingResponse {
    pub original_capability_capsule_id: String,
    pub capability_capsule: CapabilityCapsuleRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct CapabilityCapsuleVersionLink {
    pub capability_capsule_id: String,
    pub version: u64,
    pub status: CapabilityCapsuleStatus,
    pub updated_at: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub supersedes_capability_capsule_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct GraphEdge {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    pub valid_from: String,
    pub valid_to: Option<String>,
}

/// Aggregate counts over the whole `graph_edges` table. Tenant-less
/// because the schema has no tenant column — all tenants share one
/// graph. `top_relations` is the top-N `(relation, count)` pairs
/// (currently N=16) for at-a-glance distribution; the full
/// breakdown can be obtained via a dedicated SQL query if needed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub struct GraphStats {
    pub node_count: i64,
    pub total_edges: i64,
    pub active_edges: i64,
    pub closed_edges: i64,
    pub top_relations: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct FeedbackSummary {
    pub total: u64,
    pub useful: u64,
    pub outdated: u64,
    pub incorrect: u64,
    pub applies_here: u64,
    pub does_not_apply_here: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct CapabilityCapsuleDetailResponse {
    pub capability_capsule: CapabilityCapsuleRecord,
    pub version_chain: Vec<CapabilityCapsuleVersionLink>,
    pub graph_links: Vec<GraphEdge>,
    pub feedback_summary: FeedbackSummary,
    #[serde(default)]
    pub embedding: super::embeddings::CapabilityCapsuleEmbeddingMeta,
}
