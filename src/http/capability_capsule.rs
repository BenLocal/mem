use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::{
    app::AppState,
    domain::capability_capsule::{
        CapabilityCapsuleType, FeedbackKind, IngestCapabilityCapsuleRequest, Scope, Visibility,
        WriteMode,
    },
    domain::episode::{EpisodeResponse, IngestEpisodeRequest},
    domain::query::SearchCapabilityCapsuleRequest,
    error::AppError,
    service::{BatchIngestItem, IngestCapabilityCapsuleResponse},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/capability_capsules",
            post(ingest_capability_capsule).get(list_capability_capsules),
        )
        .route(
            "/capability_capsules/batch",
            post(ingest_capability_capsules_batch),
        )
        .route("/episodes", post(ingest_episode))
        .route(
            "/capability_capsules/search",
            post(search_capability_capsule),
        )
        .route(
            "/capability_capsules/list",
            post(list_capability_capsules_in_scope),
        )
        .route("/capability_capsules/profile", post(get_profile))
        .route("/capability_capsules/wings", get(list_wings))
        .route("/capability_capsules/taxonomy", get(get_taxonomy))
        .route("/capability_capsules/stats", get(capsule_stats))
        .route("/capability_capsules/feedback", post(submit_feedback))
        .route(
            "/capability_capsules/{id}",
            get(get_capability_capsule).delete(delete_capability_capsule),
        )
}

async fn delete_capability_capsule(
    State(app): State<AppState>,
    Path(capability_capsule_id): Path<String>,
    Query(query): Query<CapabilityCapsuleDetailQuery>,
) -> Result<StatusCode, AppError> {
    let tenant = query.tenant.as_deref().unwrap_or("local");
    app.capability_capsule_service
        .delete_capability_capsule_hard(tenant, &capability_capsule_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CapabilityCapsuleListQuery {
    #[serde(default = "default_tenant")]
    tenant: String,
}

async fn list_capability_capsules(
    State(app): State<AppState>,
    Query(query): Query<CapabilityCapsuleListQuery>,
) -> Result<Json<Vec<crate::domain::capability_capsule::CapabilityCapsuleRecord>>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .list_capability_capsules(&query.tenant)
            .await?,
    ))
}

async fn list_wings(
    State(app): State<AppState>,
    Query(query): Query<CapabilityCapsuleListQuery>,
) -> Result<Json<Vec<String>>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .list_wings(&query.tenant)
            .await?,
    ))
}

async fn capsule_stats(
    State(app): State<AppState>,
    Query(query): Query<CapabilityCapsuleListQuery>,
) -> Result<Json<crate::domain::capability_capsule::CapsuleStats>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .capsule_stats(&query.tenant)
            .await?,
    ))
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct TaxonomyWing {
    project: String,
    repos: Vec<String>,
}

async fn get_taxonomy(
    State(app): State<AppState>,
    Query(query): Query<CapabilityCapsuleListQuery>,
) -> Result<Json<Vec<TaxonomyWing>>, AppError> {
    let raw = app
        .capability_capsule_service
        .get_taxonomy(&query.tenant)
        .await?;
    let wings: Vec<TaxonomyWing> = raw
        .into_iter()
        .map(|(project, repos)| TaxonomyWing { project, repos })
        .collect();
    Ok(Json(wings))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ListInScopeRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    capability_capsule_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
    /// Restrict to one writer agent (the field stored as `source_agent`
    /// on the capsule). Combined with `capability_capsule_type=diary`
    /// this is the path `capability_capsule_agent_diary_read` uses.
    #[serde(default)]
    source_agent: Option<String>,
    #[serde(default)]
    cursor: Option<ListInScopeCursor>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct ListInScopeCursor {
    updated_at: String,
    capability_capsule_id: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct ListInScopeResponse {
    capability_capsules: Vec<crate::domain::capability_capsule::CapabilityCapsuleRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<ListInScopeCursor>,
    has_more: bool,
}

async fn list_capability_capsules_in_scope(
    State(app): State<AppState>,
    Json(req): Json<ListInScopeRequest>,
) -> Result<Json<ListInScopeResponse>, AppError> {
    let cursor_ref = req
        .cursor
        .as_ref()
        .map(|c| (c.updated_at.as_str(), c.capability_capsule_id.as_str()));
    let (capability_capsules, has_more) = app
        .capability_capsule_service
        .list_capability_capsules_in_scope(
            &req.tenant,
            req.project.as_deref(),
            req.repo.as_deref(),
            req.module.as_deref(),
            req.capability_capsule_type.as_deref(),
            req.status.as_deref(),
            req.source_agent.as_deref(),
            cursor_ref,
            req.limit.unwrap_or(50),
        )
        .await?;
    let next_cursor = if has_more {
        capability_capsules.last().map(|r| ListInScopeCursor {
            updated_at: r.updated_at.clone(),
            capability_capsule_id: r.capability_capsule_id.clone(),
        })
    } else {
        None
    };
    Ok(Json(ListInScopeResponse {
        capability_capsules,
        next_cursor,
        has_more,
    }))
}

/// G5 — `POST /capability_capsules/profile`. Aggregates the in-scope
/// `Preference` + `Workflow` capsules and the tenant's entities into one
/// queryable "developer / project conventions" view. Read-only; mirrors the
/// `list` endpoint's POST-with-body shape.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ProfileRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn get_profile(
    State(app): State<AppState>,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<crate::service::capability_capsule_service::ProfileResponse>, AppError> {
    let profile = app
        .capability_capsule_service
        .build_profile(
            &req.tenant,
            req.project.as_deref(),
            req.repo.as_deref(),
            req.limit.unwrap_or(100),
        )
        .await?;
    Ok(Json(profile))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct HttpIngestMemoryRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    capability_capsule_type: CapabilityCapsuleType,
    content: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    code_refs: Vec<String>,
    scope: Scope,
    #[serde(default)]
    visibility: Visibility,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    task_type: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default = "default_source_agent")]
    source_agent: String,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    write_mode: WriteMode,
    /// Optional supersession link (caller passes the prior capsule's
    /// id). The new row keeps `supersedes_capability_capsule_id`
    /// pointing back at the original; audit / version-chain reads
    /// surface both. Edge closure is not automatic — use
    /// `kg_invalidate_edge` if you also want to close edges from the
    /// previous version.
    #[serde(default)]
    supersedes_capability_capsule_id: Option<String>,
    /// Optional hard expiry — a 20-digit zero-padded ms-since-epoch string.
    /// When set, the capsule is recalled until this time, then skipped by
    /// retrieve and archived by the decay worker. Omit for no deadline.
    #[serde(default)]
    expires_at: Option<String>,
}

impl From<HttpIngestMemoryRequest> for IngestCapabilityCapsuleRequest {
    fn from(request: HttpIngestMemoryRequest) -> Self {
        Self {
            tenant: request.tenant,
            capability_capsule_type: request.capability_capsule_type,
            content: request.content,
            summary: request.summary,
            evidence: request.evidence,
            code_refs: request.code_refs,
            scope: request.scope,
            visibility: request.visibility,
            project: request.project,
            repo: request.repo,
            module: request.module,
            task_type: request.task_type,
            tags: request.tags,
            topics: request.topics,
            source_agent: request.source_agent,
            idempotency_key: request.idempotency_key,
            write_mode: request.write_mode,
            supersedes_capability_capsule_id: request.supersedes_capability_capsule_id,
            expires_at: request.expires_at,
        }
    }
}

async fn ingest_capability_capsule(
    State(app): State<AppState>,
    Json(request): Json<HttpIngestMemoryRequest>,
) -> Result<(StatusCode, Json<IngestCapabilityCapsuleResponse>), AppError> {
    let response = app
        .capability_capsule_service
        .ingest(request.into())
        .await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// Bulk capsule ingest. Body is a JSON array of the same shape as the
/// single endpoint accepts. Response is `{ "items": [<per-item>] }`,
/// where each item is `{"result":"ok",…}` or `{"result":"err","error":"…"}`,
/// preserving input order. Returns 207 (Multi-Status) when any item
/// failed; 201 otherwise. Service-level (non-per-item) errors still
/// return 5xx via `AppError`.
async fn ingest_capability_capsules_batch(
    State(app): State<AppState>,
    Json(requests): Json<Vec<HttpIngestMemoryRequest>>,
) -> Result<(StatusCode, Json<BatchIngestResponse>), AppError> {
    let domain_requests: Vec<IngestCapabilityCapsuleRequest> =
        requests.into_iter().map(Into::into).collect();
    let items = app
        .capability_capsule_service
        .ingest_batch(domain_requests)
        .await?;
    let any_err = items
        .iter()
        .any(|i| matches!(i, BatchIngestItem::Err { .. }));
    let status = if any_err {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(BatchIngestResponse { items })))
}

#[derive(Debug, serde::Serialize)]
struct BatchIngestResponse {
    items: Vec<BatchIngestItem>,
}

async fn ingest_episode(
    State(app): State<AppState>,
    Json(request): Json<IngestEpisodeRequest>,
) -> Result<(StatusCode, Json<EpisodeResponse>), AppError> {
    let response = app
        .capability_capsule_service
        .ingest_episode(request)
        .await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn get_capability_capsule(
    State(app): State<AppState>,
    Path(capability_capsule_id): Path<String>,
    Query(query): Query<CapabilityCapsuleDetailQuery>,
) -> Result<Json<crate::domain::capability_capsule::CapabilityCapsuleDetailResponse>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .get_capability_capsule(query.tenant.as_deref(), &capability_capsule_id)
            .await?,
    ))
}

async fn search_capability_capsule(
    State(app): State<AppState>,
    Json(request): Json<SearchCapabilityCapsuleRequest>,
) -> Result<Json<crate::domain::query::SearchCapabilityCapsuleResponse>, AppError> {
    Ok(Json(app.capability_capsule_service.search(request).await?))
}

async fn submit_feedback(
    State(app): State<AppState>,
    Json(request): Json<HttpFeedbackRequest>,
) -> Result<Json<crate::domain::capability_capsule::CapabilityCapsuleRecord>, AppError> {
    Ok(Json(
        app.capability_capsule_service
            .submit_feedback(
                &request.tenant,
                &request.capability_capsule_id,
                request.feedback_kind,
                request.note,
            )
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CapabilityCapsuleDetailQuery {
    #[serde(default)]
    tenant: Option<String>,
}

fn default_tenant() -> String {
    "local".to_string()
}

fn default_source_agent() -> String {
    "api".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct HttpFeedbackRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    capability_capsule_id: String,
    feedback_kind: FeedbackKind,
    /// Optional caller-supplied note (verbatim text). Persisted on
    /// the resulting `feedback_events` row; not consumed by ranking.
    #[serde(default)]
    note: Option<String>,
}
