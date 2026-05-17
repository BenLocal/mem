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
    /// Caller-private notebook: each agent's own scratchpad. Stored
    /// in `capability_capsules` like any other row but **excluded
    /// from `capability_capsule_search` results by default** unless
    /// the caller passes `include_diary=true`. Use via
    /// `capability_capsule_agent_diary_write` /
    /// `capability_capsule_agent_diary_read` MCP tools — those
    /// enforce the `source_agent` round-trip so one agent's diary
    /// can't leak into another's reads. Filter convention at the
    /// search layer: `WHERE capability_capsule_type != 'diary'`.
    Diary,
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
    /// System-emitted: the auto-promote sweep moved a long-idle
    /// `PendingConfirmation` row to `Active`. Carries no confidence /
    /// decay delta — promotion alone is the signal — and produces a
    /// `feedback_events` audit row so the source of the transition
    /// stays queryable. Not a user-driven judgment; never sent by
    /// `submit_feedback`.
    AutoPromoted,
}

impl FeedbackKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::Outdated => "outdated",
            Self::Incorrect => "incorrect",
            Self::AppliesHere => "applies_here",
            Self::DoesNotApplyHere => "does_not_apply_here",
            Self::AutoPromoted => "auto_promoted",
        }
    }

    /// Parse the wire / DB form (snake_case string) back into the
    /// enum. Inverse of [`Self::as_str`]. Returns `None` for any
    /// string that doesn't match a known kind — callers in storage
    /// backends typically surface that as
    /// `StorageError::InvalidData("invalid feedback kind")`.
    ///
    /// Matches the `EntityKind::from_db_str` naming convention.
    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "useful" => Self::Useful,
            "outdated" => Self::Outdated,
            "incorrect" => Self::Incorrect,
            "applies_here" => Self::AppliesHere,
            "does_not_apply_here" => Self::DoesNotApplyHere,
            "auto_promoted" => Self::AutoPromoted,
            _ => return None,
        })
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

    /// Status transition the event triggers on its parent capsule, if any.
    /// `Incorrect` archives; `AutoPromoted` activates; all others leave
    /// status alone.
    pub fn status_after(&self) -> Option<CapabilityCapsuleStatus> {
        match self {
            Self::Incorrect => Some(CapabilityCapsuleStatus::Archived),
            Self::AutoPromoted => Some(CapabilityCapsuleStatus::Active),
            _ => None,
        }
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
    /// Optional supersession link. When set, the new row is written
    /// with `supersedes_capability_capsule_id` pointing at the
    /// caller-supplied id; the original row stays in place for audit
    /// (the canonical "update by writing a new version" path).
    /// Caller is responsible for separately invalidating any edges
    /// that the supersession should close — the ingest pipeline does
    /// not auto-close them.
    #[serde(default, skip_serializing_if = "skip_none")]
    pub supersedes_capability_capsule_id: Option<String>,
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
    /// Monotonic chain version (1-based). Stored as `i64` so every
    /// backend with a signed-integer column type (Postgres BIGINT,
    /// DuckDB BIGINT, sqlite INTEGER) maps cleanly without a
    /// per-call `try_from` guard. `u64` would have been wider than
    /// any realistic version chain — see backend-coupling §6.5 pain #1.
    pub version: i64,
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
    pub version: i64,
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

/// Per-tenant capsule-pool snapshot: total row count plus a count per
/// `CapabilityCapsuleStatus` variant. The five status fields cover the
/// full enum so the caller can always sum to `total`; an unknown
/// status string from a future enum addition would be dropped silently
/// (caller can detect via `pending_confirmation + provisional + active
/// + archived + rejected != total`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub struct CapsuleStats {
    pub total: i64,
    pub pending_confirmation: i64,
    pub provisional: i64,
    pub active: i64,
    pub archived: i64,
    pub rejected: i64,
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
