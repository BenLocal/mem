use serde::{Deserialize, Serialize};

use super::memory::Scope;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct WorkflowOutline {
    pub memory_id: String,
    pub goal: String,
    pub steps: Vec<String>,
    pub success_signals: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub struct WorkflowCandidate {
    pub memory_id: Option<String>,
    pub goal: String,
    pub preconditions: Vec<String>,
    pub steps: Vec<String>,
    pub decision_points: Vec<String>,
    pub success_signals: Vec<String>,
    pub failure_signals: Vec<String>,
    pub evidence: Vec<String>,
    pub scope: Scope,
}
