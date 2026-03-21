#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::{
    memory::{Scope, Visibility},
    workflow::WorkflowCandidate,
};

fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestEpisodeRequest {
    #[serde(default)]
    pub tenant: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub evidence: Vec<String>,
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
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_agent: String,
    #[serde(default, skip_serializing_if = "skip_none")]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct EpisodeRecord {
    pub episode_id: String,
    #[serde(default)]
    pub tenant: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub evidence: Vec<String>,
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
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_agent: String,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "skip_none")]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct EpisodeResponse {
    pub episode_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub workflow_candidate: Option<WorkflowCandidate>,
}
