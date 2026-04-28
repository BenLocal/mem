use std::sync::Arc;

use axum::Router;
use tracing::info;

use crate::{
    http,
    service::MemoryService,
    storage::{DuckDbGraphStore, DuckDbRepository, VectorIndex, VectorIndexFingerprint},
};

#[derive(Clone)]
pub struct AppState {
    pub memory_service: MemoryService,
    pub config: crate::config::Config,
}

impl AppState {
    pub async fn from_config(config: crate::config::Config) -> anyhow::Result<Self> {
        let repository = DuckDbRepository::open(&config.db_path).await?;
        info!(duckdb = %config.db_path.display(), "storage initialized");

        let fp = VectorIndexFingerprint {
            provider: config.embedding.job_provider_id().to_string(),
            model: config.embedding.model.clone(),
            dim: config.embedding.dim,
        };
        let vector_index =
            Arc::new(VectorIndex::open_or_rebuild(&repository, &config.db_path, &fp).await?);
        repository.attach_vector_index(vector_index.clone());
        info!(
            size = vector_index.size(),
            provider = %fp.provider,
            model = %fp.model,
            dim = fp.dim,
            "vector index ready"
        );

        let provider = crate::embedding::arc_embedding_provider(&config.embedding)
            .map_err(|e| anyhow::anyhow!("embedding provider: {e}"))?;
        info!(
            provider = provider.name(),
            model = provider.model(),
            dim = provider.dim(),
            "embedding provider initialized"
        );
        let provider_worker = provider.clone();
        let provider_search = provider.clone();
        let repo_worker = repository.clone();
        let worker_settings = config.embedding.clone();
        tokio::spawn(async move {
            crate::service::embedding_worker::run(repo_worker, provider_worker, worker_settings)
                .await;
        });

        let embedding_provider = config.embedding.job_provider_id().to_string();
        let graph = Arc::new(DuckDbGraphStore::new(Arc::new(repository.clone())));
        let memory_service = MemoryService::with_graph_and_embedding_providers(
            repository,
            graph,
            embedding_provider,
            Some(provider_search),
        );

        Ok(Self {
            memory_service,
            config,
        })
    }

    pub async fn local() -> anyhow::Result<Self> {
        Self::from_config(crate::config::Config::local()).await
    }
}

pub async fn router() -> anyhow::Result<Router> {
    router_with_config(crate::config::Config::local()).await
}

pub async fn router_with_config(config: crate::config::Config) -> anyhow::Result<Router> {
    let state = AppState::from_config(config).await?;
    Ok(http::router().with_state(state))
}
