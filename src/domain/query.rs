#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::workflow::WorkflowOutline;

fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SearchMemoryRequest {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub scope_filters: Vec<String>,
    #[serde(default)]
    pub token_budget: usize,
    #[serde(default)]
    pub caller_agent: String,
    #[serde(default)]
    pub expand_graph: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct DirectiveItem {
    pub memory_id: String,
    pub text: String,
    pub source_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct FactItem {
    pub memory_id: String,
    pub text: String,
    pub code_refs: Vec<String>,
    pub source_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct PatternItem {
    pub memory_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "skip_none")]
    pub applicability: Option<String>,
    pub source_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SearchMemoryResponse {
    pub directives: Vec<DirectiveItem>,
    pub relevant_facts: Vec<FactItem>,
    pub reusable_patterns: Vec<PatternItem>,
    #[serde(default, skip_serializing_if = "skip_none")]
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
