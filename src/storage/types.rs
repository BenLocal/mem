//! Shared storage data types — row payloads, error types, summaries.
//! Previously lived under `storage/duckdb/{mod,graph_store,transcript_repo}.rs`
//! when that module was the storage layer; now centralised here so
//! `LanceStore` / `DuckDbQuery` / `Store` share a single home.

use serde::Serialize;
use thiserror::Error;

use crate::domain::ConversationMessage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackEvent {
    pub feedback_id: String,
    pub capability_capsule_id: String,
    pub feedback_kind: String,
    pub created_at: String,
}

/// Row claimed by the embedding worker (`status = processing`).
#[derive(Debug, Clone)]
pub struct ClaimedEmbeddingJob {
    pub job_id: String,
    pub tenant: String,
    pub capability_capsule_id: String,
    pub target_content_hash: String,
    pub provider: String,
    pub attempt_count: i64,
}

/// Insert payload for `embedding_jobs` (worker and APIs use the same
/// row shape).
#[derive(Debug, Clone)]
pub struct EmbeddingJobInsert {
    pub job_id: String,
    pub tenant: String,
    pub capability_capsule_id: String,
    pub target_content_hash: String,
    pub provider: String,
    pub available_at: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Aggregate row used by the admin web page's transcripts list view.
/// One per `(tenant, session_id)`. `caller_agent` is whatever
/// `max(caller_agent)` returned — typical sessions have a single
/// agent so this is unambiguous; in mixed-agent edge cases it picks
/// one deterministically rather than blocking the listing.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptSessionSummary {
    pub session_id: String,
    pub block_count: i64,
    pub first_at: String,
    pub last_at: String,
    pub caller_agent: Option<String>,
}

/// Row claimed by the transcript embedding worker
/// (`status = processing`). Mirrors `ClaimedEmbeddingJob` for the
/// memories side, with `capability_capsule_id` renamed to `message_block_id` and
/// `target_content_hash` dropped (transcript blocks are immutable on
/// insert, so the hash is implicit in the row id).
#[derive(Debug, Clone)]
pub struct ClaimedTranscriptEmbeddingJob {
    pub job_id: String,
    pub tenant: String,
    pub message_block_id: String,
    pub provider: String,
    pub attempt_count: i64,
}

/// Result of `Store::context_window_for_block`. The `primary` is the
/// requested block; `before` and `after` are temporally adjacent
/// same-session blocks (filtered per `include_tool_blocks`).
#[derive(Debug, Clone)]
pub struct ContextWindow {
    pub primary: ConversationMessage,
    pub before: Vec<ConversationMessage>,
    pub after: Vec<ConversationMessage>,
}

/// Top-level storage error. Carries the underlying I/O / serde /
/// data-validation flavors plus a `NotFound(&'static str)` for
/// internal-consistency lookup misses.
///
/// Removed in this round: the `DuckDb(duckdb::Error)` variant — the
/// legacy DuckDB-as-storage backend was deleted in this commit. The
/// remaining `DuckDbQuery` SQL layer surfaces `duckdb::Error` only
/// at row-decode boundaries, where it's converted to
/// `InvalidInput`.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("duckdb error: {0}")]
    DuckDb(#[from] duckdb::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid data: {0}")]
    InvalidData(&'static str),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("vector index error: {0}")]
    VectorIndex(String),
    /// Internal-consistency lookup miss (e.g. an id returned by a
    /// sibling index moments ago is no longer present). Carries only
    /// a `&'static str` category so HTTP error bodies cannot leak
    /// runtime ids.
    #[error("not found: {0}")]
    NotFound(&'static str),
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("graph backend error: {0}")]
    Backend(String),
}

impl From<StorageError> for GraphError {
    fn from(e: StorageError) -> Self {
        GraphError::Backend(e.to_string())
    }
}

impl From<duckdb::Error> for GraphError {
    fn from(e: duckdb::Error) -> Self {
        GraphError::Backend(e.to_string())
    }
}
