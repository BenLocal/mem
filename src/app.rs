use std::sync::Arc;

use axum::Router;

use crate::{
    config::Config,
    http,
    service::MemoryService,
    storage::{DuckDbRepository, LocalGraphAdapter},
};

#[derive(Clone)]
pub struct AppState {
    pub memory_service: MemoryService,
    pub config: Config,
}

impl AppState {
    pub async fn from_config(config: Config) -> anyhow::Result<Self> {
        let repository = DuckDbRepository::open(&config.db_path).await?;
        let provider = crate::embedding::arc_embedding_provider(&config.embedding)
            .map_err(|e| anyhow::anyhow!("embedding provider: {e}"))?;
        let provider_worker = provider.clone();
        let provider_search = provider.clone();
        let repo_worker = repository.clone();
        let worker_settings = config.embedding.clone();
        tokio::spawn(async move {
            crate::service::embedding_worker::run(repo_worker, provider_worker, worker_settings)
                .await;
        });

        let embedding_provider = config.embedding.job_provider_id().to_string();
        let memory_service = MemoryService::with_graph_and_embedding_providers(
            repository,
            Arc::new(LocalGraphAdapter::default()),
            embedding_provider,
            Some(provider_search),
        );

        Ok(Self {
            memory_service,
            config,
        })
    }

    pub async fn local() -> anyhow::Result<Self> {
        Self::from_config(Config::local()).await
    }
}

pub async fn router() -> anyhow::Result<Router> {
    router_with_config(Config::local()).await
}

pub async fn router_with_config(config: Config) -> anyhow::Result<Router> {
    let state = AppState::from_config(config).await?;
    Ok(http::router().with_state(state))
}
