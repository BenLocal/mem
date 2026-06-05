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
    /// Per-request relevance floor for the Facts / Patterns sections.
    /// `None` falls back to the process-wide `MEM_MIN_SCORE` (default 25).
    /// Lets a noisy auto-recall caller (e.g. the UserPromptSubmit hook)
    /// raise the bar without globally starving explicit searches.
    #[serde(default, skip_serializing_if = "skip_none")]
    pub min_score: Option<i64>,
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

impl SearchCapabilityCapsuleResponse {
    /// The distinct capability_capsule_ids actually *emitted* into this
    /// response (directives + facts + patterns). This is the "used" set
    /// the last-used worker stamps `last_used_at` on (roadmap O1) — the
    /// load-bearing capsules the agent received, not the wider candidate
    /// pool that was merely scanned during ranking. Order-preserving,
    /// first occurrence wins. `recent_conversations` carry no capsule id
    /// and are excluded.
    pub fn emitted_capsule_ids(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let ids = self
            .directives
            .iter()
            .map(|d| &d.capability_capsule_id)
            .chain(self.relevant_facts.iter().map(|f| &f.capability_capsule_id))
            .chain(
                self.reusable_patterns
                    .iter()
                    .map(|p| &p.capability_capsule_id),
            );
        for id in ids {
            if seen.insert(id.clone()) {
                out.push(id.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitted_capsule_ids_dedups_across_sections_order_preserving() {
        let resp = SearchCapabilityCapsuleResponse {
            directives: vec![DirectiveItem {
                capability_capsule_id: "a".into(),
                text: String::new(),
                source_summary: String::new(),
            }],
            relevant_facts: vec![
                FactItem {
                    capability_capsule_id: "b".into(),
                    text: String::new(),
                    code_refs: vec![],
                    source_summary: String::new(),
                },
                // duplicate of a directive → must collapse.
                FactItem {
                    capability_capsule_id: "a".into(),
                    text: String::new(),
                    code_refs: vec![],
                    source_summary: String::new(),
                },
            ],
            reusable_patterns: vec![PatternItem {
                capability_capsule_id: "c".into(),
                text: String::new(),
                applicability: None,
                source_summary: String::new(),
            }],
            ..Default::default()
        };
        // First-occurrence order across directives → facts → patterns,
        // each id once.
        assert_eq!(resp.emitted_capsule_ids(), vec!["a", "b", "c"]);
    }

    #[test]
    fn emitted_capsule_ids_empty_response_is_empty() {
        let resp = SearchCapabilityCapsuleResponse::default();
        assert!(resp.emitted_capsule_ids().is_empty());
    }
}
