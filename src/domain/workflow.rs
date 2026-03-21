use serde::{Deserialize, Serialize};

use super::memory::Scope;

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct WorkflowOutline {
    pub memory_id: String,
    pub goal: String,
    pub steps: Vec<String>,
    pub success_signals: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

impl Default for WorkflowCandidate {
    fn default() -> Self {
        Self {
            memory_id: None,
            goal: String::new(),
            preconditions: Vec::new(),
            steps: Vec::new(),
            decision_points: Vec::new(),
            success_signals: Vec::new(),
            failure_signals: Vec::new(),
            evidence: Vec::new(),
            scope: Scope::default(),
        }
    }
}
