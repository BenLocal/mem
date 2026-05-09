use serde::Serialize;
use std::sync::Arc;
use thiserror::Error;

use crate::domain::{
    capability_capsule::CapabilityCapsuleDetailResponse,
    embeddings::{CapabilityCapsuleEmbeddingMeta, EmbeddingJobInfo, EmbeddingsRebuildResponse},
    episode::EpisodeResponse,
};
use crate::embedding::EmbeddingProvider;
use crate::{
    domain::{
        capability_capsule::{
            CapabilityCapsuleRecord, CapabilityCapsuleStatus, EditPendingRequest,
            EditPendingResponse, FeedbackKind, GraphEdge, IngestCapabilityCapsuleRequest,
        },
        episode::{EpisodeRecord, IngestEpisodeRequest},
        query::{SearchCapabilityCapsuleRequest, SearchCapabilityCapsuleResponse},
    },
    pipeline::ingest::{
        compute_content_hash, initial_status, memory_node_id, GraphEdgeDraft, ToNodeKind,
    },
    pipeline::workflow,
    pipeline::{compress, retrieve},
    storage::{current_timestamp, EmbeddingJobInsert, GraphError, StorageError, Store},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestCapabilityCapsuleResponse {
    pub capability_capsule_id: String,
    pub status: CapabilityCapsuleStatus,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("memory not found")]
    NotFound,
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Graph(#[from] GraphError),
}

impl From<CapabilityCapsuleRecord> for IngestCapabilityCapsuleResponse {
    fn from(memory: CapabilityCapsuleRecord) -> Self {
        Self {
            capability_capsule_id: memory.capability_capsule_id,
            status: memory.status,
        }
    }
}

#[derive(Clone)]
pub struct CapabilityCapsuleService {
    /// Shared storage handle. Writes flow to LanceStore; reads
    /// (incl. graph reads via `pipeline::retrieve::rank_with_graph_*`)
    /// flow to DuckDbQuery. The graph surface lives on `Store`'s
    /// `GraphStore` trait impl — passed as `&dyn GraphStore` to the
    /// pipeline.
    store: Arc<Store>,
    /// Value stored on `embedding_jobs.provider` (e.g. `fake`, `openai`).
    embedding_job_provider: String,
    /// When set, search runs hybrid lexical + semantic retrieval.
    embedding_search_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Optional handle to the transcript-archive service. Only used
    /// by the wake-up fast path to populate
    /// `SearchCapabilityCapsuleResponse.recent_conversations`. When
    /// `None`, wake-up still works — it just omits the section.
    /// Tests / unit fixtures that don't need transcript enrichment
    /// pass `None` via [`Self::new`] / [`Self::with_providers`].
    transcript_service: Option<Arc<crate::service::TranscriptService>>,
}

impl CapabilityCapsuleService {
    /// Primary constructor. `embedding_job_provider` defaults to
    /// `"fake"` (legacy compat); search provider is `None` (BM25-only
    /// recall). Use [`Self::with_providers`] to override.
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            embedding_job_provider: "fake".to_string(),
            embedding_search_provider: None,
            transcript_service: None,
        }
    }

    /// Constructor that derives `embedding_jobs.provider` from
    /// `settings.job_provider_id()` so the worker's runtime provider
    /// check (`embedding_worker::tick`) succeeds against jobs this
    /// service enqueues.
    pub fn new_with_settings(
        store: Arc<Store>,
        settings: &crate::config::EmbeddingSettings,
    ) -> Self {
        Self {
            store,
            embedding_job_provider: settings.job_provider_id().to_string(),
            embedding_search_provider: None,
            transcript_service: None,
        }
    }

    /// Full constructor used by `app.rs`. Wires both the
    /// `embedding_jobs.provider` stamp and the search-time embedding
    /// provider (so semantic recall is enabled).
    pub fn with_providers(
        store: Arc<Store>,
        embedding_job_provider: String,
        embedding_search_provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self {
            store,
            embedding_job_provider,
            embedding_search_provider,
            transcript_service: None,
        }
    }

    /// Attach a transcript service so the wake-up fast path can
    /// surface `recent_conversations`. Builder-style — no-op when
    /// not called (typical test path).
    pub fn with_transcript_service(
        mut self,
        transcript_service: Arc<crate::service::TranscriptService>,
    ) -> Self {
        self.transcript_service = Some(transcript_service);
        self
    }

    pub async fn ingest(
        &self,
        request: IngestCapabilityCapsuleRequest,
    ) -> Result<IngestCapabilityCapsuleResponse, ServiceError> {
        let content_hash = compute_content_hash(&request);

        if let Some(existing) = self
            .store
            .find_by_idempotency_or_hash(&request.tenant, &request.idempotency_key, &content_hash)
            .await?
        {
            return Ok(existing.into());
        }

        let status = initial_status(&request.capability_capsule_type, &request.write_mode);
        let now = current_timestamp();

        crate::pipeline::ingest::validate_verbatim(&request.content, request.summary.as_deref())
            .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;

        let summary = request
            .summary
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| summarize(&request.content));

        let session_id = crate::pipeline::session::resolve_session(
            &self.store,
            &request.tenant,
            &request.source_agent,
            &now,
            crate::pipeline::session::idle_minutes_from_env(),
        )
        .await
        .map_err(ServiceError::Storage)?;

        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: next_memory_id(),
            tenant: request.tenant,
            capability_capsule_type: request.capability_capsule_type,
            status: status.clone(),
            scope: request.scope,
            visibility: request.visibility,
            version: 1,
            summary,
            content: request.content,
            evidence: request.evidence,
            code_refs: request.code_refs,
            project: request.project,
            repo: request.repo,
            module: request.module,
            task_type: request.task_type,
            tags: request.tags,
            topics: request.topics,
            confidence: default_confidence(&status),
            decay_score: 0.0,
            content_hash,
            idempotency_key: request.idempotency_key,
            session_id: Some(session_id.clone()),
            supersedes_capability_capsule_id: None,
            source_agent: request.source_agent,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_validated_at: None,
        };

        let stored = self.store.insert_capability_capsule(memory).await?;
        let drafts = crate::pipeline::ingest::extract_graph_edge_drafts(&stored);
        let edges = resolve_drafts_to_edges(drafts, &self.store, &stored.tenant, &now).await?;
        self.store.sync_memory_edges(&edges, &now).await?;
        self.enqueue_embedding_job_for_memory(&stored).await?;
        self.store
            .touch_session(&session_id, &now)
            .await
            .map_err(ServiceError::Storage)?;
        Ok(stored.into())
    }

    pub async fn ingest_episode(
        &self,
        request: IngestEpisodeRequest,
    ) -> Result<EpisodeResponse, ServiceError> {
        let episode_id = next_episode_id();
        let now = current_timestamp();
        let mut episode = EpisodeRecord {
            episode_id: episode_id.clone(),
            tenant: request.tenant.clone(),
            goal: request.goal.clone(),
            steps: request.steps.clone(),
            outcome: request.outcome.clone(),
            evidence: request.evidence.clone(),
            scope: request.scope.clone(),
            visibility: request.visibility.clone(),
            project: request.project.clone(),
            repo: request.repo.clone(),
            module: request.module.clone(),
            tags: request.tags.clone(),
            source_agent: request.source_agent.clone(),
            idempotency_key: request.idempotency_key.clone(),
            created_at: now.clone(),
            updated_at: now,
            workflow_candidate: None,
        };

        let mut workflow_candidate = {
            let mut episodes = self
                .store
                .list_successful_episodes_for_tenant(&episode.tenant)
                .await?;
            episodes.push(episode.clone());
            workflow::maybe_extract_workflow(&episodes)
        };

        if let Some(candidate) = workflow_candidate.as_mut() {
            let workflow_memory = self
                .ingest(workflow::workflow_memory_request(&episode, candidate))
                .await?;
            candidate.capability_capsule_id = Some(workflow_memory.capability_capsule_id);
            episode.workflow_candidate = Some(candidate.clone());
        }

        self.store.insert_episode(episode).await?;

        Ok(EpisodeResponse {
            episode_id,
            status: "created".to_string(),
            workflow_candidate,
        })
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, ServiceError> {
        Ok(self.store.list_pending_review(tenant).await?)
    }

    /// All memories for a tenant, regardless of status, ordered by created_at
    /// ascending. Backs the admin web page (`GET /memories?tenant=…`).
    pub async fn list_capability_capsules(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, ServiceError> {
        Ok(self
            .store
            .list_capability_capsules_for_tenant(tenant)
            .await?)
    }

    /// Hard-delete a memory and all its references. **Irreversible.**
    /// Backs `DELETE /capability_capsules/{id}` from the admin web page.
    ///
    /// Order:
    ///   1. Verify the row exists for this tenant (clean 404 if not).
    ///   2. Transactional DuckDB cascade
    ///      (`repository::delete_capability_capsule_hard`).
    ///   3. Best-effort HNSW sidecar removal — if the sidecar is missing or
    ///      the remove fails, the DB delete still wins; an orphan vector
    ///      gets cleaned by the next `mem repair --rebuild`.
    pub async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), ServiceError> {
        self.store
            .get_capability_capsule_for_tenant(tenant, capability_capsule_id)
            .await?
            .ok_or(ServiceError::NotFound)?;

        self.store
            .delete_capability_capsule_hard(tenant, capability_capsule_id)
            .await?;
        // Vector-index sidecar removal happens inside
        // `DuckDbRepository::delete_capability_capsule_hard` itself; service code no
        // longer needs to know the backend uses HNSW.
        Ok(())
    }

    pub async fn get_capability_capsule(
        &self,
        tenant: Option<&str>,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleDetailResponse, ServiceError> {
        let memory = match tenant {
            Some(tenant) => {
                self.store
                    .get_capability_capsule_for_tenant(tenant, capability_capsule_id)
                    .await?
            }
            None => {
                self.store
                    .get_capability_capsule(capability_capsule_id.to_string())
                    .await?
            }
        }
        .ok_or(ServiceError::NotFound)?;

        let embedding = self.embedding_meta_for_memory(&memory).await?;

        let graph_links = self
            .store
            .neighbors(&memory_node_id(capability_capsule_id))
            .await
            .unwrap_or_default();

        Ok(CapabilityCapsuleDetailResponse {
            version_chain: self
                .store
                .list_capability_capsule_versions_for_tenant(&memory.tenant, capability_capsule_id)
                .await?,
            graph_links,
            feedback_summary: self.store.feedback_summary(capability_capsule_id).await?,
            capability_capsule: memory,
            embedding,
        })
    }

    pub async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status: Option<&str>,
        capability_capsule_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, ServiceError> {
        Ok(self
            .store
            .list_embedding_jobs(tenant, status, capability_capsule_id, limit)
            .await?)
    }

    pub async fn rebuild_embeddings(
        &self,
        tenant: &str,
        capability_capsule_ids: &[String],
        force: bool,
    ) -> Result<EmbeddingsRebuildResponse, ServiceError> {
        let ids: Vec<String> = if capability_capsule_ids.is_empty() {
            self.store
                .list_capability_capsule_ids_for_tenant(tenant)
                .await?
        } else {
            capability_capsule_ids.to_vec()
        };

        let mut enqueued: u32 = 0;
        for mid in ids {
            let memory = self
                .store
                .get_capability_capsule_for_tenant(tenant, &mid)
                .await?
                .ok_or(ServiceError::NotFound)?;

            let now = current_timestamp();
            if force {
                self.store.delete_capability_capsule_embedding(&mid).await?;
                self.store
                    .stale_live_embedding_jobs_for_capability_capsule(
                        tenant,
                        &mid,
                        &self.embedding_job_provider,
                        &now,
                    )
                    .await?;
            }

            let insert = EmbeddingJobInsert {
                job_id: next_embedding_job_id(),
                tenant: memory.tenant.clone(),
                capability_capsule_id: memory.capability_capsule_id.clone(),
                target_content_hash: memory.content_hash.clone(),
                provider: self.embedding_job_provider.clone(),
                available_at: now.clone(),
                created_at: now.clone(),
                updated_at: now,
            };
            if self.store.try_enqueue_embedding_job(insert).await? {
                enqueued += 1;
            }
        }

        Ok(EmbeddingsRebuildResponse { enqueued })
    }

    async fn embedding_meta_for_memory(
        &self,
        memory: &CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleEmbeddingMeta, ServiceError> {
        if let Some((model, hash, updated_at)) = self
            .store
            .get_capability_capsule_embedding_row(&memory.capability_capsule_id)
            .await?
        {
            if hash == memory.content_hash {
                return Ok(CapabilityCapsuleEmbeddingMeta {
                    status: "indexed".to_string(),
                    model: Some(model),
                    updated_at: Some(updated_at),
                    content_hash: Some(hash),
                });
            }
            return Ok(CapabilityCapsuleEmbeddingMeta {
                status: "stale".to_string(),
                model: Some(model),
                updated_at: Some(updated_at),
                content_hash: Some(hash),
            });
        }

        let job_status = self
            .store
            .latest_embedding_job_status_for_hash(
                &memory.tenant,
                &memory.capability_capsule_id,
                &memory.content_hash,
            )
            .await?;

        let status_label = match job_status.as_deref() {
            None => "none",
            Some("pending") => "pending",
            Some("processing") => "processing",
            Some("failed") => "failed",
            Some("completed") | Some("stale") => "none",
            Some(_) => "none",
        };

        Ok(CapabilityCapsuleEmbeddingMeta {
            status: status_label.to_string(),
            ..Default::default()
        })
    }

    pub async fn accept_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, ServiceError> {
        self.store
            .get_pending(tenant, capability_capsule_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        Ok(self
            .store
            .accept_pending(tenant, capability_capsule_id)
            .await?)
    }

    pub async fn reject_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<CapabilityCapsuleRecord, ServiceError> {
        self.store
            .get_pending(tenant, capability_capsule_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        Ok(self
            .store
            .reject_pending(tenant, capability_capsule_id)
            .await?)
    }

    /// Supersede flow: accept a pending memory by replacing it with an edited active version.
    ///
    /// After storage is updated, the graph is kept consistent:
    /// 1. v1's edges are closed (`close_edges_for_capability_capsule`)
    /// 2. v2's edges are opened via the new draft + registry-resolve + `sync_memory_edges` path
    pub async fn edit_and_accept_pending(
        &self,
        tenant: &str,
        patch: EditPendingRequest,
    ) -> Result<EditPendingResponse, ServiceError> {
        let original_memory_id = patch.capability_capsule_id.clone();
        let original = self
            .store
            .get_pending(tenant, &original_memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;

        let superseding = self
            .store
            .replace_pending_with_successor(
                tenant,
                &original_memory_id,
                self.superseding_active_version(&original, patch),
            )
            .await?;

        // Close v1's graph edges, then open v2's — order matters.
        self.store
            .close_edges_for_capability_capsule(&original.capability_capsule_id)
            .await?;
        let now = current_timestamp();
        let drafts = crate::pipeline::ingest::extract_graph_edge_drafts(&superseding);
        let edges = resolve_drafts_to_edges(drafts, &self.store, &superseding.tenant, &now).await?;
        self.store.sync_memory_edges(&edges, &now).await?;

        self.enqueue_embedding_job_for_memory(&superseding).await?;

        Ok(EditPendingResponse {
            original_capability_capsule_id: original.capability_capsule_id,
            capability_capsule: superseding,
        })
    }

    pub async fn submit_feedback(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        feedback_kind: FeedbackKind,
    ) -> Result<CapabilityCapsuleRecord, ServiceError> {
        let memory = self
            .store
            .get_capability_capsule_for_tenant(tenant, capability_capsule_id)
            .await?
            .ok_or(ServiceError::NotFound)?;

        let feedback = crate::storage::FeedbackEvent {
            feedback_id: next_feedback_id(),
            capability_capsule_id: memory.capability_capsule_id.clone(),
            feedback_kind: feedback_kind.as_str().to_string(),
            created_at: current_timestamp(),
        };

        Ok(self.store.apply_feedback(&memory, feedback).await?)
    }

    fn superseding_active_version(
        &self,
        original: &CapabilityCapsuleRecord,
        patch: EditPendingRequest,
    ) -> CapabilityCapsuleRecord {
        let request = IngestCapabilityCapsuleRequest {
            tenant: original.tenant.clone(),
            capability_capsule_type: original.capability_capsule_type.clone(),
            content: patch.content.clone(),
            summary: None,
            evidence: patch.evidence.clone(),
            code_refs: patch.code_refs.clone(),
            scope: original.scope.clone(),
            visibility: original.visibility.clone(),
            project: original.project.clone(),
            repo: original.repo.clone(),
            module: original.module.clone(),
            task_type: original.task_type.clone(),
            tags: patch.tags.clone(),
            topics: original.topics.clone(),
            source_agent: original.source_agent.clone(),
            idempotency_key: None,
            write_mode: crate::domain::capability_capsule::WriteMode::Auto,
        };
        let now = current_timestamp();

        CapabilityCapsuleRecord {
            capability_capsule_id: next_memory_id(),
            tenant: original.tenant.clone(),
            capability_capsule_type: original.capability_capsule_type.clone(),
            status: CapabilityCapsuleStatus::Active,
            scope: original.scope.clone(),
            visibility: original.visibility.clone(),
            version: original.version + 1,
            summary: patch.summary,
            content: patch.content,
            evidence: patch.evidence,
            code_refs: patch.code_refs,
            project: original.project.clone(),
            repo: original.repo.clone(),
            module: original.module.clone(),
            task_type: original.task_type.clone(),
            tags: patch.tags,
            topics: original.topics.clone(),
            confidence: default_confidence(&CapabilityCapsuleStatus::Active),
            decay_score: 0.0,
            content_hash: compute_content_hash(&request),
            idempotency_key: None,
            session_id: None,
            supersedes_capability_capsule_id: Some(original.capability_capsule_id.clone()),
            source_agent: original.source_agent.clone(),
            created_at: now.clone(),
            updated_at: now,
            last_validated_at: None,
        }
    }

    pub async fn graph_neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, ServiceError> {
        Ok(self.store.neighbors(node_id).await?)
    }

    pub async fn search(
        &self,
        query: SearchCapabilityCapsuleRequest,
    ) -> Result<SearchCapabilityCapsuleResponse, ServiceError> {
        let tenant = query.tenant.as_deref().unwrap_or("local");

        // Wake-up fast path: SessionStart hooks call us with `intent="wake_up"`
        // and an empty `query` to seed "Recent Context" at session boot.
        // The full pipeline (embedding the empty string, HNSW lookup, scanning
        // every active memory for BM25, graph-aware ranking, tiktoken-based
        // compression of the entire live set) was observed to take 11–200 s
        // on a moderately-loaded local DB and made claude-code start sluggish.
        //
        // We instead fetch the most recently-updated active slice and hand it
        // straight to `compress`. Ranking is skipped on purpose: with no query
        // text the relevance floor in `retrieve::finalize` (default 25) gates
        // out almost everything except Preference / Workflow because the
        // text-match and scope signals are zero, and freshness alone caps at
        // +6. The DB-side `ORDER BY updated_at DESC` already gives the
        // ordering wake-up wants ("freshest first per section") and `compress`
        // does the per-section truncation by token budget.
        if query.intent == "wake_up" && query.query.trim().is_empty() {
            const WAKE_UP_LIMIT: usize = 64;
            // 70% capsules / 30% transcripts when transcript service is
            // attached; full budget to capsules when it's not (keeps
            // the legacy shape for tests / providers without the
            // transcript pipeline wired).
            let (capsule_budget, transcript_budget) = if self.transcript_service.is_some() {
                let cap = (query.token_budget * 70 / 100).max(80);
                (cap, query.token_budget.saturating_sub(cap))
            } else {
                (query.token_budget, 0)
            };

            let candidates = self
                .store
                .recent_active_capability_capsules(tenant, WAKE_UP_LIMIT)
                .await
                .map_err(ServiceError::Storage)?;
            let mut response = compress::compress(&candidates, capsule_budget);

            if let Some(transcripts) = self.transcript_service.as_ref() {
                if transcript_budget > 0 {
                    // 3 sessions × 4 highlights — small budget keeps
                    // the wake-up payload bounded; the agent is
                    // expected to reverse-look up via session_id if
                    // it wants more depth.
                    let recent = transcripts
                        .recent_for_wake_up(tenant, 3, 4)
                        .await
                        .unwrap_or_default();
                    response.recent_conversations =
                        compress::compress_recent_sessions(recent, transcript_budget);
                }
            }

            return Ok(response);
        }

        // Single SQL hybrid call replaces the dual lex/sem fan-out:
        // `Store::hybrid_candidates` runs `lance_fts` +
        // `lance_vector_search` joined by capability_capsule_id with
        // RRF (k=60) computed inline in DuckDB SQL. See
        // `examples/hybrid_sql_poc.rs` for the standalone validation.
        //
        // The lifecycle pool (`search_candidates`) still pulls the full
        // active tenant set in parallel — Preference / Workflow rows
        // that don't hit the query still surface via the floor
        // exemption in `retrieve::finalize`, and lifecycle / scope /
        // intent signals score against the broader pool.
        const HYBRID_K: usize = 48;
        let pool_fut = self.store.search_candidates(tenant);
        let query_vec_fut = async {
            let Some(provider) = self.embedding_search_provider.as_ref() else {
                return Vec::new();
            };
            provider.embed_text(&query.query).await.unwrap_or_default()
        };
        let (pool_res, query_vec) = tokio::join!(pool_fut, query_vec_fut);
        let pool = pool_res.map_err(ServiceError::Storage)?;
        let hybrid_hits = self
            .store
            .hybrid_candidates(tenant, &query.query, &query_vec, HYBRID_K)
            .await
            .map_err(ServiceError::Storage)?;

        let ranked = match retrieve::rank_with_hybrid_and_graph(
            pool.clone(),
            hybrid_hits.clone(),
            &query,
            self.store.as_ref(),
        )
        .await
        {
            Ok(ranked) => ranked,
            Err(e) => {
                tracing::warn!(error = %e, "graph backend error during rank_with_hybrid_and_graph; falling back to no graph boost");
                // Force-ungrafted retry: build a request with
                // expand_graph=false so the graph anchor lookup is
                // skipped and the call cannot return a graph error.
                let mut q2 = query.clone();
                q2.expand_graph = false;
                retrieve::rank_with_hybrid_and_graph(pool, hybrid_hits, &q2, self.store.as_ref())
                    .await
                    .unwrap_or_default()
            }
        };

        Ok(compress::compress(&ranked, query.token_budget))
    }

    async fn enqueue_embedding_job_for_memory(
        &self,
        memory: &CapabilityCapsuleRecord,
    ) -> Result<(), ServiceError> {
        let now = current_timestamp();
        let insert = EmbeddingJobInsert {
            job_id: next_embedding_job_id(),
            tenant: memory.tenant.clone(),
            capability_capsule_id: memory.capability_capsule_id.clone(),
            target_content_hash: memory.content_hash.clone(),
            provider: self.embedding_job_provider.clone(),
            available_at: now.clone(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.try_enqueue_embedding_job(insert).await?;
        Ok(())
    }
}

/// Resolve a batch of `GraphEdgeDraft`s into concrete `GraphEdge`s with stable
/// `to_node_id` strings.
///
/// `LiteralMemory` drafts pass through unchanged (`memory:<id>`). `EntityRef`
/// drafts are resolved through [`EntityRegistry::resolve_or_create`], which
/// maps `(tenant, alias, kind)` to a stable `entity_id`; the resulting node id
/// is `entity:<id>`.
///
/// Each call to `resolve_or_create` acquires-and-releases the DuckDB connection
/// mutex (see `entity_repo.rs`) — the locks are sequenced, not nested. Pure
/// async; the caller passes `now` so timestamps stay deterministic in tests.
///
/// Wired into `CapabilityCapsuleService::ingest` and `edit_and_accept_pending` by Task 9
/// of the entity-registry roadmap.
pub(crate) async fn resolve_drafts_to_edges(
    drafts: Vec<GraphEdgeDraft>,
    registry: &Store,
    tenant: &str,
    now: &str,
) -> Result<Vec<GraphEdge>, StorageError> {
    let mut out = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let to_node_id = match draft.to_kind {
            ToNodeKind::LiteralMemory(capability_capsule_id) => {
                format!("capability_capsule:{capability_capsule_id}")
            }
            ToNodeKind::EntityRef { kind, alias } => {
                let id = registry
                    .resolve_or_create(tenant, &alias, kind, now)
                    .await?;
                format!("entity:{id}")
            }
        };
        out.push(GraphEdge {
            from_node_id: draft.from_node_id,
            to_node_id,
            relation: draft.relation,
            valid_from: now.to_string(),
            valid_to: None,
        });
    }
    Ok(out)
}

fn summarize(content: &str) -> String {
    const SUMMARY_LIMIT: usize = 80;
    let summary: String = content.chars().take(SUMMARY_LIMIT).collect();
    if summary.is_empty() {
        "memory".to_string()
    } else {
        summary
    }
}

fn default_confidence(status: &CapabilityCapsuleStatus) -> f32 {
    match status {
        CapabilityCapsuleStatus::Active => 0.9,
        CapabilityCapsuleStatus::PendingConfirmation => 0.6,
        CapabilityCapsuleStatus::Provisional => 0.5,
        CapabilityCapsuleStatus::Archived | CapabilityCapsuleStatus::Rejected => 0.0,
    }
}

fn next_feedback_id() -> String {
    format!("fb_{}", uuid::Uuid::now_v7())
}

fn next_embedding_job_id() -> String {
    format!("ej_{}", uuid::Uuid::now_v7())
}

fn next_memory_id() -> String {
    format!("mem_{}", uuid::Uuid::now_v7())
}

fn next_episode_id() -> String {
    format!("ep_{}", uuid::Uuid::now_v7())
}
