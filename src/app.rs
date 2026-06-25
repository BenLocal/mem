//! Wires `Store` (LanceDB, lance-native) into services + workers
//! + HTTP routes. Single entry point: `AppState::from_config(config)`.

use std::sync::Arc;

use axum::Router;
use tracing::{info, warn};

use crate::{
    http,
    service::{CapabilityCapsuleService, EntityService, FactCheckService, TranscriptService},
    storage::{Backend, Store},
};

#[derive(Clone)]
pub struct AppState {
    pub capability_capsule_service: CapabilityCapsuleService,
    pub config: crate::config::Config,
    /// Service façade backing the `/transcripts/*` HTTP routes.
    pub transcript_service: Arc<TranscriptService>,
    /// Service façade backing the `/entities/*` HTTP routes.
    pub entity_service: EntityService,
    /// Service façade backing `POST /fact_check` — pre-ingest
    /// entity-registry + KG sanity check (mempalace `fact_checker.py`
    /// analogue, minus the LLM). See `docs/mempalace-diff-v3.md` §5.
    pub fact_check_service: FactCheckService,
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
        // Privacy guard (v3 #33): warn loudly when the configured
        // provider sends content off the local machine. Opt out with
        // MEM_PRIVACY_WARN_SUPPRESS=1 once the operator has
        // acknowledged the data flow and doesn't want startup noise.
        if config.embedding.provider.sends_off_machine() && !privacy_warn_suppressed() {
            warn!(
                provider = provider.name(),
                model = provider.model(),
                "embedding provider sends content OFF this machine — set MEM_PRIVACY_WARN_SUPPRESS=1 to silence",
            );
        }

        // Open the unified storage handle, dispatching on the configured
        // backend. Each arm produces the same three bindings the rest of
        // `from_config` consumes:
        //   * `store: Arc<dyn Backend>`        — the erased storage handle
        //   * `edge_access_tx: Option<…>`      — K9 potentiation sender
        //   * `capsule_used_tx: UnboundedSender<…>` — O1 last-used sender
        // so everything below this `match` is backend-agnostic and stays
        // byte-for-byte identical to the pre-Postgres single-backend path.
        let (store, edge_access_tx, capsule_used_tx): (
            Arc<dyn Backend>,
            Option<
                tokio::sync::mpsc::UnboundedSender<crate::worker::potentiation_worker::EdgeAccess>,
            >,
            tokio::sync::mpsc::UnboundedSender<crate::worker::last_used_worker::CapsuleUsed>,
        ) = match config.backend {
            crate::config::BackendKind::Lance => {
                // LanceStore creates the schema + FTS indexes and serves
                // both reads and writes. We hold a concrete `Arc<Store>`
                // here so we can call `set_transcript_job_provider`
                // (Lance-only configuration — not on any sub-trait) and
                // spawn the two Store-glue workers, then upcast to
                // `Arc<dyn Backend>` so the concrete type never appears
                // below this `match`.
                let store_concrete = Arc::new(
                    Store::open_with_provider(&config.db_path, provider.clone())
                        .await
                        .map_err(|e| anyhow::anyhow!("storage open: {e}"))?,
                );
                info!(path = %config.db_path.display(), "storage initialized");

                // Configure the transcript embedding worker's job-provider
                // id before any transcript writes happen (writes that are
                // embed_eligible enqueue a transcript_embedding_jobs row,
                // and the row's provider column comes from this setter).
                store_concrete.set_transcript_job_provider(config.embedding.job_provider_id());

                // ── K9 potentiation worker (strategy B, in-mem channel) ──
                // Spawned BEFORE the upcast so the worker keeps a concrete
                // `Arc<Store>` (it calls `Store::potentiate_edge`, a
                // Store-level composition). The sender goes to the capsule
                // service, which enqueues graph-edge co-access events
                // during search. Default OFF (`MEM_EDGE_DYNAMICS_ENABLED`).
                let edge_access_tx = if config.edge_dynamics.enabled {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    let store_pot = store_concrete.clone();
                    let ed_settings = config.edge_dynamics.clone();
                    tokio::spawn(async move {
                        crate::worker::potentiation_worker::run(store_pot, rx, ed_settings).await;
                    });
                    Some(tx)
                } else {
                    None
                };

                // ── O1 last-used worker (retrieval reinforcement, on) ──
                // Spawned BEFORE the upcast so the worker keeps a concrete
                // `Arc<Store>` (it calls `Store::bump_last_used_at`). The
                // sender goes to the capsule service, which enqueues a
                // capsule-used event for every capsule emitted into a
                // search response; the worker coalesces them off the read
                // path and stamps `last_used_at`, anchoring the decay clock.
                let (capsule_used_tx, capsule_used_rx) = tokio::sync::mpsc::unbounded_channel();
                {
                    let store_lu = store_concrete.clone();
                    // Drain cadence; coalescing within a window bounds
                    // write pressure to one batched UPDATE per tenant per
                    // tick.
                    let flush_secs = std::env::var("MEM_LAST_USED_FLUSH_SECS")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                        .filter(|&v| v > 0)
                        .unwrap_or(5);
                    tokio::spawn(async move {
                        crate::worker::last_used_worker::run(store_lu, capsule_used_rx, flush_secs)
                            .await;
                    });
                }

                let store: Arc<dyn Backend> = store_concrete;
                (store, edge_access_tx, capsule_used_tx)
            }
            crate::config::BackendKind::Postgres => {
                #[cfg(feature = "postgres")]
                {
                    // Connect + idempotently migrate. The Store-glue
                    // workers (last_used / potentiation) are skipped: they
                    // are optimizations, not correctness, and call
                    // Store-level compositions absent on the Postgres
                    // backend. The transcript embedding-job provider IS
                    // configured (P5 wired the transcript fan-out). The
                    // capsule service still gets a live `capsule_used_tx`;
                    // its receiver is dropped, so the events it emits are
                    // silently discarded (acceptable until the PG last-used
                    // worker lands).
                    let url = config
                        .postgres_url
                        .as_deref()
                        .expect("postgres_url present when backend=Postgres");
                    let pg_concrete = crate::storage::PostgresCapsuleStore::connect(url)
                        .await
                        .map_err(|e| anyhow::anyhow!("postgres connect: {e}"))?;
                    // Stamp the transcript embedding-job provider so
                    // embed-eligible transcript inserts can enqueue jobs
                    // (P5: TranscriptStore::create_conversation_message
                    // fans out into transcript_embedding_jobs).
                    pg_concrete.set_transcript_job_provider(config.embedding.job_provider_id());
                    let pg = Arc::new(pg_concrete);
                    info!(backend = "postgres", "storage initialized");
                    let store: Arc<dyn Backend> = pg;
                    let (capsule_used_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                    (store, None, capsule_used_tx)
                }
                #[cfg(not(feature = "postgres"))]
                {
                    return Err(anyhow::anyhow!(
                        "MEM_BACKEND=postgres requires building mem with --features postgres"
                    ));
                }
            }
            crate::config::BackendKind::Clickhouse => {
                // clickhouse-backend P1 is a scaffold: `ClickHouseBackend`
                // implements `CapsuleStore` only, so it can't yet be erased
                // to `Arc<dyn Backend>`. Both arms return — wiring it as a
                // full backend lands in P2 (the other 10 sub-traits).
                #[cfg(feature = "clickhouse")]
                {
                    let _url = config
                        .clickhouse_url
                        .as_deref()
                        .expect("clickhouse_url present when backend=Clickhouse");
                    return Err(anyhow::anyhow!(
                        "clickhouse backend is a P1 scaffold (CapsuleStore only) — not yet a \
                         complete Backend; tracked in clickhouse-backend P2"
                    ));
                }
                #[cfg(not(feature = "clickhouse"))]
                {
                    return Err(anyhow::anyhow!(
                        "MEM_BACKEND=clickhouse requires building mem with --features clickhouse"
                    ));
                }
            }
        };

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

        if config.dedup.enabled {
            let store_dedup = store.clone();
            let dedup_settings = config.dedup.clone();
            // Same single-tenant MVP scope as auto_promote.
            let tenant = std::env::var("MEM_TENANT").unwrap_or_else(|_| "local".to_string());
            tokio::spawn(async move {
                crate::worker::dedup_worker::run(store_dedup, dedup_settings, tenant).await;
            });
        }

        if config.idle_archive.enabled {
            let store_idle = store.clone();
            let idle_settings = config.idle_archive.clone();
            // Same single-tenant MVP scope as dedup / auto_promote.
            let tenant = std::env::var("MEM_TENANT").unwrap_or_else(|_| "local".to_string());
            tokio::spawn(async move {
                crate::worker::idle_archive_worker::run(store_idle, idle_settings, tenant).await;
            });
        }

        if config.topic_tunnel.enabled {
            let store_tt = store.clone();
            let tt_settings = config.topic_tunnel.clone();
            let tenant = std::env::var("MEM_TENANT").unwrap_or_else(|_| "local".to_string());
            tokio::spawn(async move {
                crate::worker::topic_tunnel_worker::run(store_tt, tt_settings, tenant).await;
            });
        }

        if config.cooccurrence.enabled {
            let store_co = store.clone();
            let co_settings = config.cooccurrence.clone();
            let tenant = std::env::var("MEM_TENANT").unwrap_or_else(|_| "local".to_string());
            tokio::spawn(async move {
                crate::worker::cooccurrence_worker::run(store_co, co_settings, tenant).await;
            });
        }

        if config.evolution.enabled {
            let store_evo = store.clone();
            let evo_settings = config.evolution.clone();
            // Same single-tenant MVP scope as dedup / auto_promote.
            let tenant = std::env::var("MEM_TENANT").unwrap_or_else(|_| "local".to_string());
            tokio::spawn(async move {
                crate::worker::evolution_worker::run(store_evo, evo_settings, tenant).await;
            });
        }

        if !config.embedding.transcript_disabled {
            let provider_transcript = provider.clone();
            let store_transcript = store.clone();
            let transcript_settings = config.embedding.clone();
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
        let fact_check_service = FactCheckService::new(store.clone());
        let mut capability_capsule_service =
            CapabilityCapsuleService::with_providers(store, embedding_provider_id, Some(provider))
                .with_transcript_service(transcript_service.clone())
                .with_ingest_settings(config.ingest.clone());
        if let Some(tx) = edge_access_tx {
            capability_capsule_service = capability_capsule_service.with_potentiation_sender(tx);
        }
        capability_capsule_service =
            capability_capsule_service.with_last_used_sender(capsule_used_tx);

        Ok(Self {
            capability_capsule_service,
            config,
            transcript_service,
            entity_service,
            fact_check_service,
        })
    }

    pub async fn local() -> anyhow::Result<Self> {
        Self::from_config(crate::config::Config::local()).await
    }
}

/// Read the privacy-warning suppression env var. Truthy
/// (`1`/`true`/`yes`, case-insensitive) silences the off-machine
/// embedding warning emitted by `from_config`. Default: unset →
/// warning fires whenever a hosted provider is configured.
fn privacy_warn_suppressed() -> bool {
    matches!(
        std::env::var("MEM_PRIVACY_WARN_SUPPRESS")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

pub async fn router() -> anyhow::Result<Router> {
    router_with_config(crate::config::Config::local()).await
}

pub async fn router_with_config(config: crate::config::Config) -> anyhow::Result<Router> {
    let state = AppState::from_config(config).await?;
    Ok(http::router().with_state(state))
}
