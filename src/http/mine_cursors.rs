//! HTTP routes for the mine-cursor store (v3 #32).
//!
//! Two routes:
//!   - `GET  /mine/cursors?transcript_path=…` — read cursor; 200 with
//!     `{transcript_path, last_line_number, updated_at}` or 404 when
//!     the file has never been mined.
//!   - `POST /mine/cursors` body `{transcript_path, last_line_number}`
//!     — upsert the cursor; server stamps `updated_at`.
//!
//! Both are admin / client-tool endpoints — the `mem mine` CLI is the
//! primary caller. Operators can curl them directly to inspect or
//! reset cursors.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppState;
use crate::error::AppError;
use crate::storage::time::current_timestamp;

pub fn router() -> Router<AppState> {
    Router::new().route("/mine/cursors", get(get_cursor).post(upsert_cursor))
}

#[derive(Debug, Deserialize)]
struct GetCursorQuery {
    transcript_path: String,
}

#[derive(Debug, Serialize)]
struct CursorResponse {
    transcript_path: String,
    last_line_number: i64,
    updated_at: String,
}

async fn get_cursor(
    State(state): State<AppState>,
    Query(q): Query<GetCursorQuery>,
) -> Result<Response, AppError> {
    let store = &state.capability_capsule_service;
    match store.mine_cursor_get(&q.transcript_path).await? {
        Some(c) => Ok((
            StatusCode::OK,
            Json(CursorResponse {
                transcript_path: c.transcript_path,
                last_line_number: c.last_line_number,
                updated_at: c.updated_at,
            }),
        )
            .into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no cursor for this transcript_path" })),
        )
            .into_response()),
    }
}

#[derive(Debug, Deserialize)]
struct UpsertCursorRequest {
    transcript_path: String,
    last_line_number: i64,
}

async fn upsert_cursor(
    State(state): State<AppState>,
    Json(req): Json<UpsertCursorRequest>,
) -> Result<Json<CursorResponse>, AppError> {
    if req.last_line_number < 0 {
        return Err(crate::storage::StorageError::InvalidInput(
            "last_line_number must be non-negative".into(),
        )
        .into());
    }
    let now = current_timestamp();
    state
        .capability_capsule_service
        .mine_cursor_upsert(&req.transcript_path, req.last_line_number, &now)
        .await?;
    Ok(Json(CursorResponse {
        transcript_path: req.transcript_path,
        last_line_number: req.last_line_number,
        updated_at: now,
    }))
}
