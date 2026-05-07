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
//   &limit=200&cursor=<created_at>:<line>:<block>&since=<ts>&until=<ts>
//
// Pagination is opt-in: omitting `limit` returns every block (legacy
// behavior the integration tests still exercise). Setting `limit` switches
// to cursor-based scrolling — server returns up to `limit` rows plus
// `next_cursor` (null when exhausted) and `has_more`. Cursor encodes the
// last returned `(created_at, line_number, block_index)` tuple so ties on
// `created_at` (multiple blocks ingested in the same millisecond) don't
// drop or double-count rows. `since`/`until` are 20-digit ms strings,
// independent of the cursor — a date-range filter on top of the scroll.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GetBySessionQuery {
    pub session_id: String,
    pub tenant: String,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetBySessionResponse {
    pub messages: Vec<ConversationMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

fn parse_cursor(s: &str) -> Result<(String, i64, i64), AppError> {
    fn bad(msg: &str) -> AppError {
        crate::storage::StorageError::InvalidInput(msg.to_string()).into()
    }
    let mut parts = s.splitn(3, ':');
    let at = parts
        .next()
        .ok_or_else(|| bad("cursor missing created_at"))?;
    let line: i64 = parts
        .next()
        .ok_or_else(|| bad("cursor missing line_number"))?
        .parse()
        .map_err(|_| bad("cursor line_number not int"))?;
    let idx: i64 = parts
        .next()
        .ok_or_else(|| bad("cursor missing block_index"))?
        .parse()
        .map_err(|_| bad("cursor block_index not int"))?;
    Ok((at.to_string(), line, idx))
}

fn make_cursor(m: &ConversationMessage) -> String {
    format!("{}:{}:{}", m.created_at, m.line_number, m.block_index)
}

async fn get_by_session(
    State(state): State<AppState>,
    Query(q): Query<GetBySessionQuery>,
) -> Result<Json<GetBySessionResponse>, AppError> {
    let Some(limit) = q.limit else {
        let messages = state
            .transcript_service
            .get_by_session(&q.tenant, &q.session_id)
            .await?;
        return Ok(Json(GetBySessionResponse {
            messages,
            next_cursor: None,
            has_more: false,
        }));
    };
    let cursor_owned = match q.cursor.as_deref() {
        Some(s) if !s.is_empty() => Some(parse_cursor(s)?),
        _ => None,
    };
    let cursor_ref = cursor_owned.as_ref().map(|(a, l, i)| (a.as_str(), *l, *i));
    let (messages, has_more) = state
        .transcript_service
        .get_by_session_paged(
            &q.tenant,
            &q.session_id,
            q.since.as_deref(),
            q.until.as_deref(),
            cursor_ref,
            limit,
        )
        .await?;
    let next_cursor = if has_more {
        messages.last().map(make_cursor)
    } else {
        None
    };
    Ok(Json(GetBySessionResponse {
        messages,
        next_cursor,
        has_more,
    }))
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
