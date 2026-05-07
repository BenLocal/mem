//! HTTP routes for the transcript-archive surface.
//!
//! Three routes, all mounted by [`router`]:
//!
//! - `POST /transcripts/messages` — ingest a single transcript block.
//! - `POST /transcripts/search`   — ranked search with optional filters.
//! - `GET  /transcripts`          — list every block for a session, ordered.
//!
//! Error mapping uses the shared [`AppError`] umbrella (same as
//! `http/memory.rs`): a bare `StorageError::InvalidInput` becomes 400,
//! everything else becomes 500. Validation also happens at the
//! deserialization boundary (axum returns 400 for malformed JSON / missing
//! query params). The query-embed failure path in
//! `transcript_service::search` is the natural 400-worthy case currently in
//! the wild — it returns `StorageError::InvalidInput` when the caller's
//! query text fails to embed.

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use std::collections::HashSet;

use crate::app::AppState;
use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::error::AppError;
use crate::pipeline::transcript_recall::MergedWindow;
use crate::service::{TranscriptSearchFilters, TranscriptSearchOpts};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/transcripts/messages", post(post_message))
        .route("/transcripts/search", post(post_search))
        .route("/transcripts/sessions", get(get_sessions))
        .route("/transcripts", get(get_by_session))
}

// ---------------------------------------------------------------------------
// GET /transcripts/sessions?tenant=… — admin web page transcript list
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SessionsQuery {
    #[serde(default = "default_tenant")]
    pub tenant: String,
}

fn default_tenant() -> String {
    "local".to_string()
}

async fn get_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionsQuery>,
) -> Result<Json<Vec<crate::storage::TranscriptSessionSummary>>, AppError> {
    let rows = state
        .transcript_service
        .list_sessions(&query.tenant)
        .await?;
    Ok(Json(rows))
}

// ---------------------------------------------------------------------------
// POST /transcripts/messages
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct IngestRequest {
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

#[derive(Debug, Serialize)]
pub struct IngestResponse {
    pub message_block_id: String,
}

async fn post_message(
    State(state): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> Result<Json<IngestResponse>, AppError> {
    // The HTTP boundary mints the id (UUID v7 keeps the surface ID convention
    // consistent with the memories pipeline — see commit 3100d49).
    let id = uuid::Uuid::now_v7().to_string();
    let msg = ConversationMessage {
        message_block_id: id.clone(),
        session_id: req.session_id,
        tenant: req.tenant,
        caller_agent: req.caller_agent,
        transcript_path: req.transcript_path,
        line_number: req.line_number,
        block_index: req.block_index,
        message_uuid: req.message_uuid,
        role: req.role,
        block_type: req.block_type,
        content: req.content,
        tool_name: req.tool_name,
        tool_use_id: req.tool_use_id,
        embed_eligible: req.embed_eligible,
        created_at: req.created_at,
    };
    state.transcript_service.ingest(msg).await?;
    Ok(Json(IngestResponse {
        message_block_id: id,
    }))
}

// ---------------------------------------------------------------------------
// GET /transcripts?session_id=…&tenant=…
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GetBySessionQuery {
    pub session_id: String,
    pub tenant: String,
}

#[derive(Debug, Serialize)]
pub struct GetBySessionResponse {
    pub messages: Vec<ConversationMessage>,
}

async fn get_by_session(
    State(state): State<AppState>,
    Query(q): Query<GetBySessionQuery>,
) -> Result<Json<GetBySessionResponse>, AppError> {
    let messages = state
        .transcript_service
        .get_by_session(&q.tenant, &q.session_id)
        .await?;
    Ok(Json(GetBySessionResponse { messages }))
}

// ---------------------------------------------------------------------------
// POST /transcripts/search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub tenant: String,
    pub session_id: Option<String>,
    pub role: Option<MessageRole>,
    pub block_type: Option<BlockType>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub anchor_session_id: Option<String>,
    pub context_window: Option<usize>,
    #[serde(default)]
    pub include_tool_blocks_in_context: bool,
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub windows: Vec<TranscriptWindow>,
}

#[derive(Debug, Serialize)]
pub struct TranscriptWindow {
    pub session_id: Option<String>,
    pub blocks: Vec<TranscriptWindowBlock>,
    pub primary_ids: Vec<String>,
    pub score: i64,
}

#[derive(Debug, Serialize)]
pub struct TranscriptWindowBlock {
    pub message_block_id: String,
    pub session_id: Option<String>,
    pub line_number: u64,
    pub block_index: u32,
    pub role: MessageRole,
    pub block_type: BlockType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub created_at: String,
    pub is_primary: bool,
    pub primary_score: Option<i64>,
}

async fn post_search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    let filters = TranscriptSearchFilters {
        session_id: req.session_id,
        role: req.role,
        block_type: req.block_type,
        time_from: req.time_from,
        time_to: req.time_to,
    };
    let opts = TranscriptSearchOpts {
        anchor_session_id: req.anchor_session_id,
        context_window: req.context_window,
        include_tool_blocks_in_context: req.include_tool_blocks_in_context,
    };

    let result = state
        .transcript_service
        .search(&req.tenant, &req.query, &filters, req.limit, &opts)
        .await?;

    let windows = result.windows.into_iter().map(window_to_dto).collect();
    Ok(Json(SearchResponse { windows }))
}

fn window_to_dto(w: MergedWindow) -> TranscriptWindow {
    let primary_set: HashSet<&str> = w.primary_ids.iter().map(String::as_str).collect();
    let blocks: Vec<TranscriptWindowBlock> = w
        .blocks
        .into_iter()
        .map(|m| {
            let id = m.message_block_id.clone();
            let is_primary = primary_set.contains(id.as_str());
            let primary_score = if is_primary {
                w.primary_scores.get(&id).copied()
            } else {
                None
            };
            TranscriptWindowBlock {
                message_block_id: id,
                session_id: m.session_id,
                line_number: m.line_number,
                block_index: m.block_index,
                role: m.role,
                block_type: m.block_type,
                content: m.content,
                tool_name: m.tool_name,
                tool_use_id: m.tool_use_id,
                created_at: m.created_at,
                is_primary,
                primary_score,
            }
        })
        .collect();
    TranscriptWindow {
        session_id: w.session_id,
        blocks,
        primary_ids: w.primary_ids,
        score: w.score,
    }
}
