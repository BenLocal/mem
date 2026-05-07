//! HTTP routes for the transcript-archive surface.
//!
//! Routes mounted by [`router`]:
//!
//! - `POST /transcripts/messages` — ingest a single transcript block.
//! - `POST /transcripts/search`   — ranked search with optional filters.
//! - `POST /transcripts`          — list blocks for a session (paged).
//! - `GET  /transcripts/sessions` — per-session aggregate.
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
        .route("/transcripts", post(post_get_by_session))
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
// POST /transcripts
//
// Body:
//   { "session_id": "...", "tenant": "...",
//     "limit": 200,                 // optional; omit → return everything
//     "cursor": { "created_at": "2026-04-30T07:59:06.792Z",
//                 "line_number": 309, "block_index": 0 },
//     "since": "...", "until": "..." }
//
// Was a `GET` with the same parameters as query strings — switched to POST
// because the cursor's `created_at` is ISO-8601 (`2026-04-30T07:59:06.792Z`)
// and the URL-encoding plus the colon-collision in any string-cursor
// scheme made the GET form fragile. JSON body sidesteps both.
//
// Pagination is opt-in: omitting `limit` returns every block (legacy
// behavior the integration tests still exercise). Setting `limit` switches
// to cursor-based scrolling — server returns up to `limit` rows plus
// `next_cursor` (null when exhausted) and `has_more`. The cursor is a
// structured `(created_at, line_number, block_index)` tuple so ties on
// `created_at` (multiple blocks ingested in the same millisecond) don't
// drop or double-count rows. `since`/`until` are passed through as-is
// (any string DuckDB can lexically compare against the column),
// independent of the cursor.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CursorTuple {
    pub created_at: String,
    pub line_number: i64,
    pub block_index: i64,
}

#[derive(Debug, Deserialize)]
pub struct GetBySessionRequest {
    pub session_id: String,
    pub tenant: String,
    pub limit: Option<usize>,
    pub cursor: Option<CursorTuple>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetBySessionResponse {
    pub messages: Vec<ConversationMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<CursorTuple>,
    pub has_more: bool,
}

impl Serialize for CursorTuple {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("CursorTuple", 3)?;
        st.serialize_field("created_at", &self.created_at)?;
        st.serialize_field("line_number", &self.line_number)?;
        st.serialize_field("block_index", &self.block_index)?;
        st.end()
    }
}

fn make_cursor(m: &ConversationMessage) -> CursorTuple {
    CursorTuple {
        created_at: m.created_at.clone(),
        line_number: m.line_number as i64,
        block_index: m.block_index as i64,
    }
}

async fn post_get_by_session(
    State(state): State<AppState>,
    Json(req): Json<GetBySessionRequest>,
) -> Result<Json<GetBySessionResponse>, AppError> {
    let Some(limit) = req.limit else {
        let messages = state
            .transcript_service
            .get_by_session(&req.tenant, &req.session_id)
            .await?;
        return Ok(Json(GetBySessionResponse {
            messages,
            next_cursor: None,
            has_more: false,
        }));
    };
    let cursor_ref = req
        .cursor
        .as_ref()
        .map(|c| (c.created_at.as_str(), c.line_number, c.block_index));
    let (messages, has_more) = state
        .transcript_service
        .get_by_session_paged(
            &req.tenant,
            &req.session_id,
            req.since.as_deref(),
            req.until.as_deref(),
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
