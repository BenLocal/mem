use serde::{Deserialize, Serialize};

use super::{
    memory::{Scope, Visibility},
    workflow::WorkflowCandidate,
};

#[allow(dead_code)]
fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestEpisodeRequest {
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
}

impl Default for IngestEpisodeRequest {
    fn default() -> Self {
        Self {
            tenant: String::new(),
            goal: String::new(),
            steps: Vec::new(),
            outcome: String::new(),
            evidence: Vec::new(),
            scope: Scope::default(),
            visibility: Visibility::default(),
            project: None,
            repo: None,
            module: None,
            tags: Vec::new(),
            source_agent: String::new(),
            idempotency_key: None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

impl Default for EpisodeRecord {
    fn default() -> Self {
        Self {
            episode_id: String::new(),
            tenant: String::new(),
            goal: String::new(),
            steps: Vec::new(),
            outcome: String::new(),
            evidence: Vec::new(),
            scope: Scope::default(),
            visibility: Visibility::default(),
            project: None,
            repo: None,
            module: None,
            tags: Vec::new(),
            source_agent: String::new(),
            idempotency_key: None,
            created_at: String::new(),
            updated_at: String::new(),
            workflow_candidate: None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct EpisodeResponse {
    pub episode_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub workflow_candidate: Option<WorkflowCandidate>,
}
