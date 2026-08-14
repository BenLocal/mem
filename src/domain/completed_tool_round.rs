use serde::{Deserialize, Serialize};

pub const COMPLETED_TOOL_ROUND_PROJECTOR_VERSION: u32 = 3;
pub const COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceAdapter {
    ClaudeCode,
    Codex,
    Pi,
    Unknown,
}

impl SourceAdapter {
    pub fn from_caller_agent(caller_agent: &str) -> Self {
        let normalized = caller_agent.trim().to_ascii_lowercase();
        if normalized.contains("claude") {
            Self::ClaudeCode
        } else if normalized.contains("codex") {
            Self::Codex
        } else if normalized == "pi" || normalized.starts_with("pi-") {
            Self::Pi
        } else {
            Self::Unknown
        }
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "claude_code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "pi" => Some(Self::Pi),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoundSealKind {
    NextHuman,
    StreamEof,
}

impl RoundSealKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::NextHuman => "next_human",
            Self::StreamEof => "stream_eof",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "next_human" => Some(Self::NextHuman),
            "stream_eof" => Some(Self::StreamEof),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoundIntegrity {
    Clean,
    Gapped,
}

impl RoundIntegrity {
    pub fn is_clean(self) -> bool {
        matches!(self, Self::Clean)
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Gapped => "gapped",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "clean" => Some(Self::Clean),
            "gapped" => Some(Self::Gapped),
            _ => None,
        }
    }
}

/// Rebuildable locator and structural statistics for one completed human
/// round containing tool calls. Transcript content remains solely in
/// `conversation_messages`; this record never copies arguments, results, or
/// the final answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedToolRound {
    pub round_id: String,
    pub tenant: String,
    pub caller_agent: String,
    pub source_adapter: SourceAdapter,
    pub session_id: Option<String>,
    pub transcript_path: String,
    pub start_line_number: u64,
    pub start_block_index: u32,
    pub end_line_number: u64,
    pub end_block_index: u32,
    pub start_message_uuid: Option<String>,
    pub final_message_uuid: Option<String>,
    pub tool_call_ids: Vec<String>,
    pub tool_names: Vec<String>,
    pub tool_call_count: u32,
    pub matched_result_count: u32,
    pub missing_result_count: u32,
    pub orphan_result_count: u32,
    pub error_result_count: u32,
    pub unknown_result_status_count: u32,
    /// Timestamp of the final assistant answer from the immutable transcript.
    /// Candidate time windows use this, never the later index rebuild time.
    pub completed_at: Option<String>,
    pub seal_kind: RoundSealKind,
    pub integrity: RoundIntegrity,
    pub source_fingerprint: String,
    /// Versioned, content-free fingerprint of the opening human task after
    /// deterministic redaction and environment-literal normalization. Kept
    /// internal; the admin view deliberately does not expose it.
    pub task_fingerprint: Option<String>,
    pub task_signal_version: u32,
    /// Versioned fingerprint of the ordered canonical tool-family sequence.
    pub tool_pattern_fingerprint: String,
    pub projector_version: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedToolRoundProjection {
    pub rounds: Vec<CompletedToolRound>,
    pub source_block_count: u64,
    pub source_fingerprint: String,
    pub incomplete_segments: u64,
    pub skipped_without_tools: u64,
    pub auxiliary_tool_result_count: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoundIndexBuildStatus {
    Building,
    Completed,
}

impl RoundIndexBuildStatus {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Completed => "completed",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "building" => Some(Self::Building),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedToolRoundIndexBuild {
    pub generation_id: String,
    pub tenant: String,
    pub session_id: String,
    pub projector_version: u32,
    pub task_signal_version: u32,
    pub status: RoundIndexBuildStatus,
    pub source_block_count: u64,
    pub source_fingerprint: String,
    pub round_count: u64,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatestCompletedToolRounds {
    pub build: Option<CompletedToolRoundIndexBuild>,
    /// Exact generation row count from storage. `rounds` may be a bounded
    /// prefix for admin reads, so rebuild integrity checks must use this field.
    pub stored_round_count: u64,
    pub rounds: Vec<CompletedToolRound>,
}
