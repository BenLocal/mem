use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::{
    app::AppState,
    config::EmbeddingProviderKind,
    domain::embeddings::{
        EmbeddingProviderInfo, EmbeddingsRebuildRequest, EmbeddingsRebuildResponse,
    },
    error::AppError,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/embeddings/jobs", get(list_jobs))
        .route("/embeddings/rebuild", post(rebuild))
        .route("/embeddings/providers", get(providers))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ListJobsQuery {
    #[serde(default = "default_tenant")]
    tenant: String,
    status: Option<String>,
    memory_id: Option<String>,
    #[serde(default = "default_jobs_limit")]
    limit: usize,
}

fn default_tenant() -> String {
    "local".to_string()
}

fn default_jobs_limit() -> usize {
    200
}

async fn list_jobs(
    State(app): State<AppState>,
    Query(q): Query<ListJobsQuery>,
) -> Result<Json<Vec<crate::domain::embeddings::EmbeddingJobInfo>>, AppError> {
    let limit = q.limit.clamp(1, 10_000);
    let jobs = app
        .memory_service
        .list_embedding_jobs(
            &q.tenant,
            q.status.as_deref(),
            q.memory_id.as_deref(),
            limit,
        )
        .await?;
    Ok(Json(jobs))
}

async fn rebuild(
    State(app): State<AppState>,
    Json(body): Json<EmbeddingsRebuildRequest>,
) -> Result<Json<EmbeddingsRebuildResponse>, AppError> {
    let res = app
        .memory_service
        .rebuild_embeddings(&body.tenant, &body.memory_ids, body.force)
        .await?;
    Ok(Json(res))
}

async fn providers(State(app): State<AppState>) -> Json<EmbeddingProviderInfo> {
    let provider = match app.config.embedding.provider {
        EmbeddingProviderKind::Fake => "fake",
        EmbeddingProviderKind::Real => "openai",
    };
    Json(EmbeddingProviderInfo {
        provider: provider.to_string(),
        model: app.config.embedding.model.clone(),
        dimension: app.config.embedding.dim,
    })
}
