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
    service::IngestCapabilityCapsuleResponse,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/capability_capsules",
            post(ingest_capability_capsule).get(list_capability_capsules),
        )
        .route("/episodes", post(ingest_episode))
        .route(
            "/capability_capsules/search",
            post(search_capability_capsule),
        )
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
}
