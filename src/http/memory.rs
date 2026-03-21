use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    domain::memory::{IngestMemoryRequest, MemoryType, Scope, Visibility, WriteMode},
    service::{IngestMemoryResponse, MemoryService},
    storage::StorageError,
};

static APP_DB_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct AppState {
    pub memory_service: MemoryService,
}

impl AppState {
    pub fn local() -> Self {
        Self {
            memory_service: MemoryService::new(default_db_path()),
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/memories", post(ingest_memory))
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

#[derive(Debug)]
struct AppError(anyhow::Error);

impl From<StorageError> for AppError {
    fn from(error: StorageError) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

fn default_tenant() -> String {
    "local".to_string()
}

fn default_source_agent() -> String {
    "api".to_string()
}

fn default_db_path() -> PathBuf {
    if let Ok(path) = std::env::var("MEM_DB_PATH") {
        return PathBuf::from(path);
    }

    let sequence = APP_DB_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mem-app-{}-{sequence}.duckdb", std::process::id()))
}
