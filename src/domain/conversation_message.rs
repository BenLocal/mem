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
