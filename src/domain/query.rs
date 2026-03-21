use serde::{Deserialize, Serialize};

use super::workflow::WorkflowOutline;

#[allow(dead_code)]
fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SearchMemoryRequest {
    pub query: String,
    pub intent: String,
    pub scope_filters: Vec<String>,
    pub token_budget: usize,
    pub caller_agent: String,
    pub expand_graph: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct DirectiveItem {
    pub memory_id: String,
    pub text: String,
    pub source_summary: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct FactItem {
    pub memory_id: String,
    pub text: String,
    pub code_refs: Vec<String>,
    pub source_summary: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct PatternItem {
    pub memory_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub applicability: Option<String>,
    pub source_summary: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SearchMemoryResponse {
    pub directives: Vec<DirectiveItem>,
    pub relevant_facts: Vec<FactItem>,
    pub reusable_patterns: Vec<PatternItem>,
    #[serde(skip_serializing_if = "skip_none")]
    pub suggested_workflow: Option<WorkflowOutline>,
}

impl Default for SearchMemoryResponse {
    fn default() -> Self {
        Self {
            directives: Vec::new(),
            relevant_facts: Vec::new(),
            reusable_patterns: Vec::new(),
            suggested_workflow: None,
        }
    }
}
