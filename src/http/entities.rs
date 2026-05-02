//! HTTP routes for the entity registry. See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.
//!
//! Four routes, all mounted by [`router`]:
//!
//! - `POST /entities`                       — create / resolve canonical entity (+ optional aliases).
//! - `GET  /entities/{entity_id}?tenant=…`  — fetch entity + aliases, or 404.
//! - `POST /entities/{entity_id}/aliases`   — declare an explicit synonym; 200 / 409.
//! - `GET  /entities?tenant=…&kind=…&q=……` — list entities (created_at desc).
//!
//! Status mapping:
//! - `POST /entities` → 201 (always — `resolve_or_create` is idempotent on
//!   alias hit, so re-POSTing the same canonical name still resolves to the
//!   same entity_id and returns 201 + the existing record).
//! - `GET /entities/{id}` → 200 / 404 (handler-level, since `Option::None` is
//!   not an error per `EntityRegistry::get_entity`).
//! - `POST /entities/{id}/aliases` → 200 (Inserted / AlreadyOnSameEntity) /
//!   409 (ConflictWithDifferentEntity) — encoded by the handler from
//!   [`AddAliasOutcome`].
//! - `GET /entities` → 200, with `limit` clamped to 100 server-side.
//!
//! Storage errors flow through [`AppError`]; `StorageError::InvalidInput` →
//! 400, everything else → 500.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppState;
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::error::AppError;
use crate::storage::time::current_timestamp;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/entities", post(post_entity).get(list_entities))
        .route("/entities/{entity_id}", get(get_entity))
        .route("/entities/{entity_id}/aliases", post(post_alias))
}

// ---------------------------------------------------------------------------
// POST /entities
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateEntityRequest {
    tenant: String,
    canonical_name: String,
    kind: EntityKind,
    #[serde(default)]
    aliases: Vec<String>,
}

async fn post_entity(
    State(state): State<AppState>,
    Json(req): Json<CreateEntityRequest>,
) -> Result<(StatusCode, Json<EntityWithAliases>), AppError> {
    let now = current_timestamp();
    let result = state
        .entity_service
        .create_with_aliases(
            &req.tenant,
            &req.canonical_name,
            req.kind,
            &req.aliases,
            &now,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(result)))
}

// ---------------------------------------------------------------------------
// GET /entities/{entity_id}?tenant=…
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GetEntityQuery {
    tenant: String,
}

async fn get_entity(
    State(state): State<AppState>,
    Path(entity_id): Path<String>,
    Query(q): Query<GetEntityQuery>,
) -> Result<Response, AppError> {
    match state.entity_service.get(&q.tenant, &entity_id).await? {
        Some(e) => Ok((StatusCode::OK, Json(e)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("entity not found: {entity_id}") })),
        )
            .into_response()),
    }
}

// ---------------------------------------------------------------------------
// POST /entities/{entity_id}/aliases
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AddAliasRequest {
    tenant: String,
    alias: String,
}

#[derive(Debug, Serialize)]
struct AddAliasResponse {
    outcome: &'static str,
    existing_entity_id: Option<String>,
}

async fn post_alias(
    State(state): State<AppState>,
    Path(entity_id): Path<String>,
    Json(req): Json<AddAliasRequest>,
) -> Result<(StatusCode, Json<AddAliasResponse>), AppError> {
    let now = current_timestamp();
    let outcome = state
        .entity_service
        .add_alias(&req.tenant, &entity_id, &req.alias, &now)
        .await?;
    let (status, payload) = match outcome {
        AddAliasOutcome::Inserted => (
            StatusCode::OK,
            AddAliasResponse {
                outcome: "inserted",
                existing_entity_id: None,
            },
        ),
        AddAliasOutcome::AlreadyOnSameEntity => (
            StatusCode::OK,
            AddAliasResponse {
                outcome: "already_on_same_entity",
                existing_entity_id: Some(entity_id.clone()),
            },
        ),
        AddAliasOutcome::ConflictWithDifferentEntity(other) => (
            StatusCode::CONFLICT,
            AddAliasResponse {
                outcome: "conflict_with_different_entity",
                existing_entity_id: Some(other),
            },
        ),
    };
    Ok((status, Json(payload)))
}

// ---------------------------------------------------------------------------
// GET /entities?tenant=…&kind=…&q=…&limit=…
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListEntitiesQuery {
    tenant: String,
    #[serde(default)]
    kind: Option<EntityKind>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Debug, Serialize)]
struct ListEntitiesResponse {
    entities: Vec<Entity>,
}

async fn list_entities(
    State(state): State<AppState>,
    Query(q): Query<ListEntitiesQuery>,
) -> Result<Json<ListEntitiesResponse>, AppError> {
    let entities = state
        .entity_service
        .list(&q.tenant, q.kind, q.q.as_deref(), q.limit.min(100))
        .await?;
    Ok(Json(ListEntitiesResponse { entities }))
}
