#![allow(dead_code)]

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
    ProposeOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct IngestMemoryRequest {
    #[serde(default)]
    pub tenant: String,
    #[serde(default)]
    pub memory_type: MemoryType,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub code_refs: Vec<String>,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub task_type: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_agent: String,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
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
    #[serde(default)]
    pub tenant: String,
    #[serde(default)]
    pub memory_type: MemoryType,
    #[serde(default)]
    pub status: MemoryStatus,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub code_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub task_type: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub decay_score: f32,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub supersedes_memory_id: Option<String>,
    #[serde(default)]
    pub source_agent: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "skip_none")]
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
