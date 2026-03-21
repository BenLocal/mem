use serde::{Deserialize, Serialize};

fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    PendingConfirmation,
    Provisional,
    Active,
    Archived,
    Rejected,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct IngestMemoryRequest {
    pub tenant: String,
    pub memory_type: MemoryType,
    pub content: String,
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
    pub source_agent: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    pub write_mode: WriteMode,
}

impl Default for IngestMemoryRequest {
    fn default() -> Self {
        Self {
            tenant: String::new(),
            memory_type: MemoryType::default(),
            content: String::new(),
            evidence: Vec::new(),
            code_refs: Vec::new(),
            scope: Scope::default(),
            visibility: Visibility::default(),
            project: None,
            repo: None,
            module: None,
            task_type: None,
            tags: Vec::new(),
            source_agent: String::new(),
            idempotency_key: None,
            write_mode: WriteMode::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct MemoryRecord {
    pub memory_id: String,
    pub tenant: String,
    pub memory_type: MemoryType,
    pub status: MemoryStatus,
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
    pub confidence: f32,
    pub decay_score: f32,
    pub content_hash: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub supersedes_memory_id: Option<String>,
    pub source_agent: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub last_validated_at: Option<String>,
}

impl Default for MemoryRecord {
    fn default() -> Self {
        Self {
            memory_id: String::new(),
            tenant: String::new(),
            memory_type: MemoryType::default(),
            status: MemoryStatus::default(),
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
            confidence: 0.0,
            decay_score: 0.0,
            content_hash: String::new(),
            idempotency_key: None,
            supersedes_memory_id: None,
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
    pub memory_id: String,
    pub summary: String,
    pub content: String,
    pub evidence: Vec<String>,
    pub code_refs: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct EditPendingResponse {
    pub original_memory_id: String,
    pub memory: MemoryRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct MemoryVersionLink {
    pub memory_id: String,
    pub version: u64,
    pub status: MemoryStatus,
    pub updated_at: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub supersedes_memory_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct GraphEdge {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
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
pub struct MemoryDetailResponse {
    pub memory: MemoryRecord,
    pub version_chain: Vec<MemoryVersionLink>,
    pub graph_links: Vec<GraphEdge>,
    pub feedback_summary: FeedbackSummary,
}
