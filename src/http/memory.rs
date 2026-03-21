use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::{
    app::AppState,
    domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode},
    error::AppError,
    service::IngestMemoryResponse,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/memories", post(ingest_memory))
        .route("/memories/:id", get(get_memory))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct HttpIngestMemoryRequest {
    #[serde(default = "default_tenant")]
    tenant: String,
    memory_type: MemoryType,
    content: String,
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
    #[serde(default = "default_source_agent")]
    source_agent: String,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    write_mode: WriteMode,
}

impl From<HttpIngestMemoryRequest> for IngestMemoryRequest {
    fn from(request: HttpIngestMemoryRequest) -> Self {
        Self {
            tenant: request.tenant,
            memory_type: request.memory_type,
            content: request.content,
            evidence: request.evidence,
            code_refs: request.code_refs,
            scope: request.scope,
            visibility: request.visibility,
            project: request.project,
            repo: request.repo,
            module: request.module,
            task_type: request.task_type,
            tags: request.tags,
            source_agent: request.source_agent,
            idempotency_key: request.idempotency_key,
            write_mode: request.write_mode,
        }
    }
}

async fn ingest_memory(
    State(app): State<AppState>,
    Json(request): Json<HttpIngestMemoryRequest>,
) -> Result<(StatusCode, Json<IngestMemoryResponse>), AppError> {
    let response = app.memory_service.ingest(request.into()).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn get_memory(
    State(app): State<AppState>,
    Path(memory_id): Path<String>,
) -> Result<Json<crate::domain::memory::MemoryDetailResponse>, AppError> {
    Ok(Json(app.memory_service.get_memory(&memory_id).await?))
}

fn default_tenant() -> String {
    "local".to_string()
}

fn default_source_agent() -> String {
    "api".to_string()
}
