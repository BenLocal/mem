use std::sync::Arc;

use axum::Router;
use tracing::info;

use crate::{
    http,
    service::{EntityService, MemoryService, TranscriptService},
    storage::{DuckDbGraphStore, DuckDbRepository, VectorIndex, VectorIndexFingerprint},
};

#[derive(Clone)]
pub struct AppState {
    pub memory_service: MemoryService,
    pub config: crate::config::Config,
    /// Transcript-archive HNSW sidecar. Held on `AppState` (not on the
    /// repository like the memories index) so [`TranscriptService`] can take
    /// an explicit `Arc<VectorIndex>` rather than reaching through the
    /// repository for it.
    pub transcript_index: Arc<VectorIndex>,
    /// Service façade backing the `/transcripts/*` HTTP routes. Cheap to
    /// clone (wraps `Clone`/`Arc` collaborators) so it can sit on `AppState`.
    pub transcript_service: TranscriptService,
    /// Service façade backing the `/entities/*` HTTP routes. Wraps the
    /// shared `DuckDbRepository` and exposes the `EntityRegistry` trait
    /// behind an HTTP-friendly surface.
    pub entity_service: EntityService,
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
        repository.set_transcript_job_provider(config.embedding.job_provider_id());
        info!(
            size = vector_index.size(),
            provider = %fp.provider,
            model = %fp.model,
            dim = fp.dim,
            "vector index ready"
        );

        let transcript_index = Arc::new(
            VectorIndex::open_or_rebuild_transcripts(&repository, &config.db_path, &fp).await?,
        );
        info!(
            size = transcript_index.size(),
            provider = %fp.provider,
            model = %fp.model,
            dim = fp.dim,
            "transcript vector index ready"
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
        let repo_decay = repository.clone();
        let worker_settings = config.embedding.clone();
        tokio::spawn(async move {
            crate::service::embedding_worker::run(repo_worker, provider_worker, worker_settings)
                .await;
        });
        tokio::spawn(async move {
            crate::service::decay_worker::start_decay_worker(Arc::new(repo_decay)).await;
        });

        if !config.embedding.transcript_disabled {
            let provider_transcript = provider.clone();
            let repo_transcript = repository.clone();
            let mut transcript_settings = config.embedding.clone();
            // Transcript pipeline has its own flush cadence — the memories
            // value is intentionally *not* reused.
            transcript_settings.vector_index_flush_every =
                config.embedding.transcript_vector_index_flush_every;
            let transcript_index_for_worker = transcript_index.clone();
            tokio::spawn(async move {
                crate::service::transcript_embedding_worker::run(
                    repo_transcript,
                    provider_transcript,
                    transcript_settings,
                    transcript_index_for_worker,
                )
                .await;
            });
        }

        let embedding_provider = config.embedding.job_provider_id().to_string();
        let graph = Arc::new(DuckDbGraphStore::new(Arc::new(repository.clone())));
        let transcript_service = TranscriptService::new(
            repository.clone(),
            transcript_index.clone(),
            Some(provider.clone()),
        );
        let entity_service = EntityService::new(repository.clone());
        let memory_service = MemoryService::with_graph_and_embedding_providers(
            repository,
            graph,
            embedding_provider,
            Some(provider_search),
        );

        Ok(Self {
            memory_service,
            config,
            transcript_index,
            transcript_service,
            entity_service,
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
