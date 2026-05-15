//! Wires `Store` (LanceDB+DuckDB-via-extension) into services + workers
//! + HTTP routes. Single entry point: `AppState::from_config(config)`.

use std::sync::Arc;

use axum::Router;
use tracing::info;

use crate::{
    http,
    service::{CapabilityCapsuleService, EntityService, TranscriptService},
    storage::Store,
};

#[derive(Clone)]
pub struct AppState {
    pub capability_capsule_service: CapabilityCapsuleService,
    pub config: crate::config::Config,
    /// Service façade backing the `/transcripts/*` HTTP routes.
    pub transcript_service: Arc<TranscriptService>,
    /// Service façade backing the `/entities/*` HTTP routes.
    pub entity_service: EntityService,
}

impl AppState {
    pub async fn from_config(config: crate::config::Config) -> anyhow::Result<Self> {
        // Embedding provider — needed for both write-time auto-embed
        // (via the EmbeddingFunction adapter on LanceStore) and
        // search-time query embedding.
        let provider = crate::embedding::arc_embedding_provider(&config.embedding)
            .map_err(|e| anyhow::anyhow!("embedding provider: {e}"))?;
        info!(
            provider = provider.name(),
            model = provider.model(),
            dim = provider.dim(),
            "embedding provider initialized"
        );

        // Open the unified storage handle. LanceStore creates the
        // schema + FTS indexes; DuckDbQuery ATTACHes the lance dir.
        let store = Arc::new(
            Store::open_with_provider(&config.db_path, provider.clone())
                .await
                .map_err(|e| anyhow::anyhow!("storage open: {e}"))?,
        );
        info!(path = %config.db_path.display(), "storage initialized");

        // Configure the transcript embedding worker's job-provider id
        // before any transcript writes happen (writes that are
        // embed_eligible enqueue a transcript_embedding_jobs row, and
        // the row's provider column comes from this setter).
        store.set_transcript_job_provider(config.embedding.job_provider_id());

        // ── Workers ─────────────────────────────────────────────
        let provider_worker = provider.clone();
        let store_worker = store.clone();
        let worker_settings = config.embedding.clone();
        tokio::spawn(async move {
            crate::worker::embedding_worker::run(store_worker, provider_worker, worker_settings)
                .await;
        });

        let store_decay = store.clone();
        tokio::spawn(async move {
            crate::worker::decay_worker::start_decay_worker(store_decay).await;
        });

        if !config.vacuum.disabled {
            let store_vacuum = store.clone();
            let vacuum_settings = config.vacuum.clone();
            tokio::spawn(async move {
                crate::worker::vacuum_worker::run(store_vacuum, vacuum_settings).await;
            });
        }

        if config.auto_promote.enabled {
            let store_promote = store.clone();
            let promote_settings = config.auto_promote.clone();
            // MVP single-tenant scope. See worker docs for the
            // multi-tenant extension path.
            let tenant = std::env::var("MEM_TENANT").unwrap_or_else(|_| "local".to_string());
            tokio::spawn(async move {
                crate::worker::auto_promote_worker::run(store_promote, promote_settings, tenant)
                    .await;
            });
        }

        if !config.embedding.transcript_disabled {
            let provider_transcript = provider.clone();
            let store_transcript = store.clone();
            let mut transcript_settings = config.embedding.clone();
            transcript_settings.vector_index_flush_every =
                config.embedding.transcript_vector_index_flush_every;
            tokio::spawn(async move {
                crate::worker::transcript_embedding_worker::run(
                    store_transcript,
                    provider_transcript,
                    transcript_settings,
                )
                .await;
            });
        }

        // ── Services ────────────────────────────────────────────
        let embedding_provider_id = config.embedding.job_provider_id().to_string();
        let transcript_service = Arc::new(TranscriptService::new(
            store.clone(),
            Some(provider.clone()),
        ));
        let entity_service = EntityService::new(store.clone());
        let capability_capsule_service =
            CapabilityCapsuleService::with_providers(store, embedding_provider_id, Some(provider))
                .with_transcript_service(transcript_service.clone());

        Ok(Self {
            capability_capsule_service,
            config,
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
