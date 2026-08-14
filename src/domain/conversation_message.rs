use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationMessage {
    pub message_block_id: String,
    pub session_id: Option<String>,
    pub tenant: String,
    pub caller_agent: String,
    pub transcript_path: String,
    pub line_number: u64,
    pub block_index: u32,
    pub message_uuid: Option<String>,
    pub role: MessageRole,
    pub block_type: BlockType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub embed_eligible: bool,
    pub created_at: String,
    /// Optional JSON-serialized envelope/per-block metadata that
    /// `mine.rs` extracts from the source JSONL but doesn't have a
    /// first-class column for. Convention (all keys optional):
    ///
    ///   - `cwd`: working directory at message time (envelope-level)
    ///   - `git_branch`: branch at message time (envelope-level)
    ///   - `parent_uuid`: thread-topology link (envelope-level)
    ///   - `is_error`: tool_result-only flag indicating tool failure
    ///
    /// Stored as a JSON string so additional fields can be added
    /// without schema migration. `None` (NULL on disk) when the
    /// caller didn't supply any metadata — typical for clients
    /// POSTing to `/transcripts/messages` directly without the
    /// JSONL envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_json: Option<String>,
}

impl ConversationMessage {
    /// Approximate heap bytes retained by this parsed row's owned strings.
    /// Used to keep derived-index rebuilds bounded after Lance materializes a
    /// record; numeric/enumerated fields are inline and intentionally omitted.
    pub(crate) fn owned_string_bytes(&self) -> usize {
        self.message_block_id
            .len()
            .saturating_add(self.session_id.as_ref().map_or(0, String::len))
            .saturating_add(self.tenant.len())
            .saturating_add(self.caller_agent.len())
            .saturating_add(self.transcript_path.len())
            .saturating_add(self.message_uuid.as_ref().map_or(0, String::len))
            .saturating_add(self.content.len())
            .saturating_add(self.tool_name.as_ref().map_or(0, String::len))
            .saturating_add(self.tool_use_id.as_ref().map_or(0, String::len))
            .saturating_add(self.created_at.len())
            .saturating_add(self.meta_json.as_ref().map_or(0, String::len))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockType {
    Text,
    ToolUse,
    ToolResult,
    Thinking,
}

impl BlockType {
    pub fn embed_eligible_default(self) -> bool {
        matches!(self, BlockType::Text | BlockType::Thinking)
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            BlockType::Text => "text",
            BlockType::ToolUse => "tool_use",
            BlockType::ToolResult => "tool_result",
            BlockType::Thinking => "thinking",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "text" => Some(BlockType::Text),
            "tool_use" => Some(BlockType::ToolUse),
            "tool_result" => Some(BlockType::ToolResult),
            "thinking" => Some(BlockType::Thinking),
            _ => None,
        }
    }
}

impl MessageRole {
    pub fn as_db_str(self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "user" => Some(MessageRole::User),
            "assistant" => Some(MessageRole::Assistant),
            "system" => Some(MessageRole::System),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_string_bytes_counts_every_materialized_string_field() {
        let message = ConversationMessage {
            message_block_id: "block".into(),
            session_id: Some("session".into()),
            tenant: "tenant".into(),
            caller_agent: "agent".into(),
            transcript_path: "path".into(),
            line_number: 1,
            block_index: 0,
            message_uuid: Some("uuid".into()),
            role: MessageRole::Assistant,
            block_type: BlockType::ToolUse,
            content: "content".into(),
            tool_name: Some("tool".into()),
            tool_use_id: Some("call".into()),
            embed_eligible: false,
            created_at: "created".into(),
            meta_json: Some("meta".into()),
        };

        let expected: usize = [
            "block", "session", "tenant", "agent", "path", "uuid", "content", "tool", "call",
            "created", "meta",
        ]
        .iter()
        .map(|value| value.len())
        .sum();

        assert_eq!(message.owned_string_bytes(), expected);
    }

    #[test]
    fn embed_eligible_default_truth_table() {
        assert!(BlockType::Text.embed_eligible_default());
        assert!(BlockType::Thinking.embed_eligible_default());
        assert!(!BlockType::ToolUse.embed_eligible_default());
        assert!(!BlockType::ToolResult.embed_eligible_default());
    }

    #[test]
    fn role_serializes_lowercase() {
        let s = serde_json::to_string(&MessageRole::User).unwrap();
        assert_eq!(s, "\"user\"");
    }

    #[test]
    fn block_type_serializes_snake_case() {
        let s = serde_json::to_string(&BlockType::ToolUse).unwrap();
        assert_eq!(s, "\"tool_use\"");
    }
}
