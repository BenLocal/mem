use serde::{Deserialize, Serialize};

use super::{
    memory::{Scope, Visibility},
    workflow::WorkflowCandidate,
};

fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

fn default_tenant() -> String {
    "local".to_string()
}

fn default_scope() -> Scope {
    Scope::Workspace
}

fn default_visibility() -> Visibility {
    Visibility::Private
}

fn default_source_agent() -> String {
    "api".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestEpisodeRequest {
    #[serde(default = "default_tenant")]
    pub tenant: String,
    pub goal: String,
    pub steps: Vec<String>,
    pub outcome: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default = "default_visibility")]
    pub visibility: Visibility,
    #[serde(skip_serializing_if = "skip_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub module: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_source_agent")]
    pub source_agent: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
}

impl Default for IngestEpisodeRequest {
    fn default() -> Self {
        Self {
            tenant: default_tenant(),
            goal: String::new(),
            steps: Vec::new(),
            outcome: String::new(),
            evidence: Vec::new(),
            scope: default_scope(),
            visibility: default_visibility(),
            project: None,
            repo: None,
            module: None,
            tags: Vec::new(),
            source_agent: default_source_agent(),
            idempotency_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub struct EpisodeRecord {
    pub episode_id: String,
    pub tenant: String,
    pub goal: String,
    pub steps: Vec<String>,
    pub outcome: String,
    pub evidence: Vec<String>,
    pub scope: Scope,
    pub visibility: Visibility,
    #[serde(skip_serializing_if = "skip_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "skip_none")]
    pub module: Option<String>,
    pub tags: Vec<String>,
    pub source_agent: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub workflow_candidate: Option<WorkflowCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct EpisodeResponse {
    pub episode_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub workflow_candidate: Option<WorkflowCandidate>,
}
