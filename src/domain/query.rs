use serde::{Deserialize, Serialize};

use super::workflow::WorkflowOutline;

fn skip_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SearchCapabilityCapsuleRequest {
    pub query: String,
    pub intent: String,
    pub scope_filters: Vec<String>,
    pub token_budget: usize,
    pub caller_agent: String,
    pub expand_graph: bool,
    #[serde(skip_serializing_if = "skip_none")]
    pub tenant: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct DirectiveItem {
    pub capability_capsule_id: String,
    pub text: String,
    pub source_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct FactItem {
    pub capability_capsule_id: String,
    pub text: String,
    pub code_refs: Vec<String>,
    pub source_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct PatternItem {
    pub capability_capsule_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "skip_none")]
    pub applicability: Option<String>,
    pub source_summary: String,
}

/// One block from a recent transcript session, surfaced as part of
/// the wake-up response so the agent can recover prior conversational
/// context. The block_id can be passed back to the transcript-archive
/// API for full-fidelity reload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ConversationHighlight {
    pub message_block_id: String,
    /// Lowercase: "user" | "assistant" | "system".
    pub role: String,
    /// snake_case: "text" | "thinking" (only embed_eligible blocks
    /// surface here — tool_use / tool_result are excluded).
    pub block_type: String,
    /// Token-budgeted excerpt of the block content. Compressed via
    /// the same tiktoken-backed `compress_text` the directives /
    /// facts / patterns sections use, so the wake-up response stays
    /// inside `token_budget`.
    pub text: String,
    pub created_at: String,
}

/// One recent transcript session, surfaced on the wake-up fast path
/// so an agent can both (a) read the highlight blocks inline and
/// (b) reverse-look up the full session via `session_id` (e.g. via
/// the `mcp__mem__transcript_session_get` MCP tool or
/// `POST /transcripts {session_id}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ConversationSnippet {
    /// Claude Code session id from the transcript JSONL — what
    /// `/transcripts/sessions` and `POST /transcripts` filter on.
    pub session_id: String,
    /// Newest block timestamp in this session.
    pub last_at: String,
    /// Total blocks (text + thinking + tool_use + tool_result) in
    /// the session — useful for the agent to gauge session size
    /// before deciding to reverse-look up.
    pub block_count: i64,
    #[serde(skip_serializing_if = "skip_none")]
    pub caller_agent: Option<String>,
    /// Top-N highlight blocks for this session, newest first.
    /// Empty when the session has no embed_eligible content.
    pub highlights: Vec<ConversationHighlight>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub struct SearchCapabilityCapsuleResponse {
    pub directives: Vec<DirectiveItem>,
    pub relevant_facts: Vec<FactItem>,
    pub reusable_patterns: Vec<PatternItem>,
    #[serde(skip_serializing_if = "skip_none")]
    pub suggested_workflow: Option<WorkflowOutline>,
    /// Recent transcript sessions, populated only on the wake-up
    /// fast path (intent=wake_up + empty query). Empty for normal
    /// search calls. Each snippet carries a `session_id` the caller
    /// can reverse-look up to recover the full conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_conversations: Vec<ConversationSnippet>,
}
