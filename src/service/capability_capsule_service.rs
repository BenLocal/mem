use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
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
    storage::{current_timestamp, Backend, EmbeddingJobInsert, GraphError, StorageError},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestCapabilityCapsuleResponse {
    pub capability_capsule_id: String,
    pub status: CapabilityCapsuleStatus,
}

/// Per-item outcome for [`CapabilityCapsuleService::ingest_batch`]. Wire
/// shape (snake_case via serde):
///
/// ```json
/// { "result": "ok", "capability_capsule_id": "mem_…", "status": "active" }
/// { "result": "err", "error": "…" }
/// ```
///
/// Order matches the input `requests` slice 1:1 — index `i` in the
/// response array corresponds to index `i` in the request array.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum BatchIngestItem {
    Ok {
        #[serde(flatten)]
        response: IngestCapabilityCapsuleResponse,
    },
    Err {
        error: String,
    },
}

/// One fuzzy-match suggestion returned by
/// [`CapabilityCapsuleService::graph_neighbor_suggestions`] when a
/// `graph_neighbors` call produced no results — mempalace's
/// `_fuzzy_match` analogue at the KG level. Each entry is a known
/// entity whose canonical name is Levenshtein-≤3 from the input
/// node_id's parsed alias / suffix.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NeighborSuggestion {
    /// Suggested replacement node_id (typically `entity:<uuid>`),
    /// ready to pass back into `graph_neighbors`.
    pub suggested_node_id: String,
    /// Human-readable canonical name as stored in the entity registry.
    pub canonical_name: String,
    /// Levenshtein distance between the input alias / suffix and the
    /// suggested canonical name. Lower is closer.
    pub edit_distance: usize,
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
    /// Shared storage handle. Phase 5: erased to `Arc<dyn Backend>`
    /// (umbrella supertrait over all 9 storage sub-traits). The
    /// service holds the umbrella, not the concrete `Store`, so a
    /// future Postgres backend that implements the same 9 traits
    /// drops in without touching this file. The graph surface lives
    /// on the `GraphStore` sub-trait — passed to the pipeline via
    /// trait upcasting (`&*self.store as &dyn GraphRead`).
    store: Arc<dyn Backend>,
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
    /// Per-session ingest throttle settings (cap). `None` ⇒ no cap.
    /// Closes `agent-memory-strategy-readiness §4.3 #3`.
    ingest_settings: crate::config::IngestSettings,
    /// Process-local per-session ingest counter. Keyed on
    /// `request.session_id`; entries are never evicted (counts grow
    /// until process restart). When `ingest_settings.max_per_session`
    /// is `None` the counter is still maintained (cheap) but never
    /// consulted — the gate short-circuits.
    ingest_counters: Arc<Mutex<HashMap<String, usize>>>,
    /// K9: when `Some` (set by `app` under `MEM_EDGE_DYNAMICS_ENABLED`),
    /// `search` weights the graph boost by each edge's decayed strength
    /// and enqueues co-access events to the potentiation worker via this
    /// sender. `None` ⇒ flat graph boost, no potentiation (pre-K9).
    edge_access_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::worker::potentiation_worker::EdgeAccess>>,
    /// O1: when `Some` (set by `app`), `search` enqueues a capsule-used
    /// event for every capsule emitted into its response. The last-used
    /// worker drains the channel off the read path and stamps
    /// `last_used_at`, which anchors the decay clock. `None` ⇒ no
    /// reinforcement (the response is unaffected either way).
    capsule_used_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::worker::last_used_worker::CapsuleUsed>>,
}

impl CapabilityCapsuleService {
    /// Primary constructor. `embedding_job_provider` defaults to
    /// `"fake"` (legacy compat); search provider is `None` (BM25-only
    /// recall). Use [`Self::with_providers`] to override.
    pub fn new(store: Arc<dyn Backend>) -> Self {
        Self {
            store,
            embedding_job_provider: "fake".to_string(),
            embedding_search_provider: None,
            transcript_service: None,
            ingest_settings: crate::config::IngestSettings::development_defaults(),
            ingest_counters: Arc::new(Mutex::new(HashMap::new())),
            edge_access_tx: None,
            capsule_used_tx: None,
        }
    }

    /// K9: attach the potentiation worker's channel sender so `search`
    /// enables edge-dynamics weighting + co-access enqueue. Builder-style;
    /// `app` calls this only when `MEM_EDGE_DYNAMICS_ENABLED`.
    pub fn with_potentiation_sender(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::worker::potentiation_worker::EdgeAccess>,
    ) -> Self {
        self.edge_access_tx = Some(tx);
        self
    }

    /// O1: attach the last-used worker's channel sender so `search`
    /// enqueues a capsule-used event for each emitted capsule (retrieval
    /// reinforcement). Builder-style; `app` always calls this.
    pub fn with_last_used_sender(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::worker::last_used_worker::CapsuleUsed>,
    ) -> Self {
        self.capsule_used_tx = Some(tx);
        self
    }

    /// O1: enqueue a capsule-used event for every capsule emitted into
    /// `response`, for the last-used worker to stamp off the read path.
    /// Best-effort and non-blocking — an absent or closed channel is
    /// silently ignored, so the read path never blocks on (or fails for)
    /// reinforcement bookkeeping.
    fn enqueue_capsules_used(&self, tenant: &str, response: &SearchCapabilityCapsuleResponse) {
        let Some(tx) = self.capsule_used_tx.as_ref() else {
            return;
        };
        for id in response.emitted_capsule_ids() {
            // Unbounded send never blocks; drop SendError (worker gone).
            let _ = tx.send(crate::worker::last_used_worker::CapsuleUsed {
                tenant: tenant.to_string(),
                capability_capsule_id: id,
            });
        }
    }

    /// Constructor that derives `embedding_jobs.provider` from
    /// `settings.job_provider_id()` so the worker's runtime provider
    /// check (`embedding_worker::tick`) succeeds against jobs this
    /// service enqueues.
    pub fn new_with_settings(
        store: Arc<dyn Backend>,
        settings: &crate::config::EmbeddingSettings,
    ) -> Self {
        Self {
            store,
            embedding_job_provider: settings.job_provider_id().to_string(),
            embedding_search_provider: None,
            transcript_service: None,
            ingest_settings: crate::config::IngestSettings::development_defaults(),
            ingest_counters: Arc::new(Mutex::new(HashMap::new())),
            edge_access_tx: None,
            capsule_used_tx: None,
        }
    }

    /// Full constructor used by `app.rs`. Wires both the
    /// `embedding_jobs.provider` stamp and the search-time embedding
    /// provider (so semantic recall is enabled).
    pub fn with_providers(
        store: Arc<dyn Backend>,
        embedding_job_provider: String,
        embedding_search_provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self {
            store,
            embedding_job_provider,
            embedding_search_provider,
            transcript_service: None,
            ingest_settings: crate::config::IngestSettings::development_defaults(),
            ingest_counters: Arc::new(Mutex::new(HashMap::new())),
            edge_access_tx: None,
            capsule_used_tx: None,
        }
    }

    /// Builder-style: attach an `IngestSettings` (typically from
    /// `Config::ingest`). The cap defaults to `None` (no throttle) when
    /// this isn't called.
    pub fn with_ingest_settings(mut self, settings: crate::config::IngestSettings) -> Self {
        self.ingest_settings = settings;
        self
    }

    /// Read the mine cursor for `transcript_path`. v3 #32 — pure
    /// perf hint, never a correctness boundary; missing cursor just
    /// means "re-mine the whole file."
    pub async fn mine_cursor_get(
        &self,
        transcript_path: &str,
    ) -> Result<Option<crate::storage::MineCursor>, ServiceError> {
        Ok(self.store.get_mine_cursor(transcript_path).await?)
    }

    /// Upsert the mine cursor for `transcript_path`. `mem mine`
    /// writes this after each successful batch round-trip so the next
    /// re-run can skip already-processed lines.
    pub async fn mine_cursor_upsert(
        &self,
        transcript_path: &str,
        last_line_number: i64,
        now: &str,
    ) -> Result<(), ServiceError> {
        Ok(self
            .store
            .upsert_mine_cursor(transcript_path, last_line_number, now)
            .await?)
    }

    /// Atomic check-and-increment of the per-session ingest counter.
    /// Returns `Ok(())` when slot reserved (caller is clear to write),
    /// or `Err(ServiceError::Storage(InvalidInput))` when the session's
    /// count already meets / exceeds the configured cap. Short-circuits
    /// when `ingest_settings.max_per_session` is `None`.
    fn check_and_reserve_ingest_slot(&self, session_id: &str) -> Result<(), ServiceError> {
        let Some(cap) = self.ingest_settings.max_per_session else {
            return Ok(());
        };
        // Soft bound on the process-local counter map so a long-running server
        // with high session churn can't grow it without limit. On overflow,
        // reset the whole window (fail-OPEN — counts restart, the same lenient
        // effect as a process restart): this throttle is a coarse abuse guard,
        // not an exact quota, so a rare reset of a 100k-session map is far
        // preferable to unbounded memory. Only reached when a NEW session would
        // push past the bound, so steady-state (≤ bound distinct sessions) never
        // clears.
        const MAX_TRACKED_SESSIONS: usize = 100_000;
        let mut counters = self
            .ingest_counters
            .lock()
            .expect("ingest_counters mutex poisoned");
        if counters.len() >= MAX_TRACKED_SESSIONS && !counters.contains_key(session_id) {
            counters.clear();
        }
        let count = counters.entry(session_id.to_string()).or_insert(0);
        if *count >= cap {
            return Err(ServiceError::Storage(StorageError::RateLimited(format!(
                "per-session ingest cap reached: session {session_id} has {count} accepted writes \
                 (MEM_MAX_INGEST_PER_SESSION={cap}); restart mem or unset the env var to clear",
            ))));
        }
        *count += 1;
        Ok(())
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
        crate::pipeline::ingest::validate_scope_boundary(
            &request.scope,
            request.project.as_deref(),
        )
        .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;
        crate::pipeline::ingest::assess_ingest_quality(
            &request.capability_capsule_type,
            &request.content,
            &request.evidence,
            &request.code_refs,
            &self.ingest_settings,
        )
        .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;

        let summary = request
            .summary
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| summarize(&request.content));

        let session_id = crate::pipeline::session::resolve_session(
            self.store.as_ref(),
            &request.tenant,
            &request.source_agent,
            &now,
            crate::pipeline::session::idle_minutes_from_env(),
        )
        .await
        .map_err(ServiceError::Storage)?;

        // Per-session ingest cap (§4.3 #3). Reserves a slot BEFORE
        // the write — caller sees rejection deterministically rather
        // than racing with successful but undercounted writes. The
        // counter is incremented atomically; rejection returns
        // without bumping.
        self.check_and_reserve_ingest_slot(&session_id)?;

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
            supersedes_capability_capsule_id: request.supersedes_capability_capsule_id,
            source_agent: request.source_agent,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_validated_at: None,
            last_used_at: None,
            last_recalled_at: None,
            expires_at: request.expires_at.clone(),
        };

        let stored = self.store.insert_capability_capsule(memory).await?;
        crate::metrics::metrics().inc_capsule_ingest();
        let drafts = crate::pipeline::ingest::extract_graph_edge_drafts(&stored);
        let edges = resolve_drafts_to_edges(drafts, &*self.store, &stored.tenant, &now).await?;
        self.store.sync_memory_edges(&edges, &now).await?;
        self.enqueue_embedding_job_for_memory(&stored).await?;
        self.store
            .touch_session(&session_id, &now)
            .await
            .map_err(ServiceError::Storage)?;
        Ok(stored.into())
    }

    /// Bulk version of [`Self::ingest`]. Each request is prepared
    /// independently (idempotency probe / verbatim validation / session
    /// resolve / record build), then the new rows are flushed as a single
    /// Lance write — same for the graph-edge sync
    /// and the embedding-job enqueue. Per-item failures are isolated:
    /// the slot in the result vector becomes `BatchIngestItem::Err`,
    /// other items still land. Output preserves input order 1:1.
    pub async fn ingest_batch(
        &self,
        requests: Vec<IngestCapabilityCapsuleRequest>,
    ) -> Result<Vec<BatchIngestItem>, ServiceError> {
        if requests.is_empty() {
            return Ok(vec![]);
        }
        let now = current_timestamp();

        // ── Phase 1: prepare per-item state. Reads are sequential
        //    (idempotency probe + session resolve both hit storage) but
        //    cheap relative to the per-row writes we used to do.
        let mut outcomes: Vec<Option<BatchIngestItem>> = vec![None; requests.len()];
        let mut to_insert: Vec<CapabilityCapsuleRecord> = Vec::new();
        let mut session_ids: Vec<String> = Vec::new();

        for (idx, request) in requests.into_iter().enumerate() {
            match self.prepare_one(request, &now).await {
                Ok(PreparedIngest::Existing(resp)) => {
                    outcomes[idx] = Some(BatchIngestItem::Ok { response: resp });
                }
                Ok(PreparedIngest::New { record, session_id }) => {
                    session_ids.push(session_id);
                    to_insert.push(*record);
                }
                Err(e) => {
                    outcomes[idx] = Some(BatchIngestItem::Err {
                        error: e.to_string(),
                    });
                }
            }
        }

        // ── Phase 2: bulk insert.
        if !to_insert.is_empty() {
            self.store.insert_capability_capsules(&to_insert).await?;
            for _ in &to_insert {
                crate::metrics::metrics().inc_capsule_ingest();
            }

            // Collect graph edges across all new rows; resolve through
            // the entity registry; flush in one sync_memory_edges call.
            let mut all_edges: Vec<GraphEdge> = Vec::new();
            for stored in &to_insert {
                let drafts = crate::pipeline::ingest::extract_graph_edge_drafts(stored);
                let edges =
                    resolve_drafts_to_edges(drafts, &*self.store, &stored.tenant, &now).await?;
                all_edges.extend(edges);
            }
            self.store.sync_memory_edges(&all_edges, &now).await?;

            // One bulk enqueue for embedding jobs.
            let inserts: Vec<EmbeddingJobInsert> = to_insert
                .iter()
                .map(|m| EmbeddingJobInsert {
                    job_id: next_embedding_job_id(),
                    tenant: m.tenant.clone(),
                    capability_capsule_id: m.capability_capsule_id.clone(),
                    target_content_hash: m.content_hash.clone(),
                    provider: self.embedding_job_provider.clone(),
                    available_at: now.clone(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                })
                .collect();
            self.store.enqueue_embedding_jobs(&inserts).await?;

            // Touch each distinct session once. `resolve_session` already
            // either re-uses an open session or opened a fresh one for
            // each item — `touch_session` only updates `last_active_at`,
            // so dedup is purely an I/O reduction.
            let mut unique_sessions = session_ids.clone();
            unique_sessions.sort();
            unique_sessions.dedup();
            for sid in &unique_sessions {
                self.store
                    .touch_session(sid, &now)
                    .await
                    .map_err(ServiceError::Storage)?;
            }
        }

        // ── Phase 3: stitch outcomes.
        let mut new_iter = to_insert.into_iter();
        let result: Vec<BatchIngestItem> = outcomes
            .into_iter()
            .map(|slot| match slot {
                Some(item) => item,
                None => {
                    // The order of `to_insert` matches the order of
                    // `None` slots (we only pushed to `to_insert` from
                    // the New branch above), so a single forward
                    // iterator stays in lockstep.
                    let stored = new_iter
                        .next()
                        .expect("to_insert length matches None-slot count");
                    BatchIngestItem::Ok {
                        response: stored.into(),
                    }
                }
            })
            .collect();
        Ok(result)
    }

    /// Run the per-item half of `ingest`: dedup probe, validate,
    /// summarize, resolve session, build the record. Returns
    /// `PreparedIngest::Existing` if the row already exists,
    /// `PreparedIngest::New` if a fresh row is ready to insert.
    async fn prepare_one(
        &self,
        request: IngestCapabilityCapsuleRequest,
        now: &str,
    ) -> Result<PreparedIngest, ServiceError> {
        let content_hash = compute_content_hash(&request);
        if let Some(existing) = self
            .store
            .find_by_idempotency_or_hash(&request.tenant, &request.idempotency_key, &content_hash)
            .await?
        {
            return Ok(PreparedIngest::Existing(existing.into()));
        }

        let status = initial_status(&request.capability_capsule_type, &request.write_mode);
        crate::pipeline::ingest::validate_verbatim(&request.content, request.summary.as_deref())
            .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;
        crate::pipeline::ingest::validate_scope_boundary(
            &request.scope,
            request.project.as_deref(),
        )
        .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;
        crate::pipeline::ingest::assess_ingest_quality(
            &request.capability_capsule_type,
            &request.content,
            &request.evidence,
            &request.code_refs,
            &self.ingest_settings,
        )
        .map_err(|e| ServiceError::Storage(StorageError::InvalidInput(e)))?;
        let summary = request
            .summary
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| summarize(&request.content));
        let session_id = crate::pipeline::session::resolve_session(
            self.store.as_ref(),
            &request.tenant,
            &request.source_agent,
            now,
            crate::pipeline::session::idle_minutes_from_env(),
        )
        .await
        .map_err(ServiceError::Storage)?;

        let record = CapabilityCapsuleRecord {
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
            created_at: now.to_string(),
            updated_at: now.to_string(),
            last_validated_at: None,
            last_used_at: None,
            last_recalled_at: None,
            expires_at: request.expires_at.clone(),
        };
        Ok(PreparedIngest::New {
            record: Box::new(record),
            session_id,
        })
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
        crate::metrics::metrics().inc_episode_ingest();

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

    /// Distinct project names for `tenant`, alphabetically — the
    /// list-wings analogue.
    pub async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, ServiceError> {
        Ok(self.store.list_wings(tenant).await?)
    }

    /// Capsule-pool snapshot for `tenant`: total + per-status counts.
    /// Backing `mem_health`'s richer payload and the `/mem:summary`
    /// slash command — read-only, no side effects.
    pub async fn capsule_stats(
        &self,
        tenant: &str,
    ) -> Result<crate::domain::capability_capsule::CapsuleStats, ServiceError> {
        Ok(self.store.capsule_stats(tenant).await?)
    }

    /// Two-level (project → repos) taxonomy. Returns a Vec of
    /// `(project, repos)` pairs; service-layer pure passthrough.
    pub async fn get_taxonomy(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, Vec<String>)>, ServiceError> {
        Ok(self.store.get_taxonomy(tenant).await?)
    }

    /// Scope-filtered, paginated browse. See repo doc-comment on
    /// `list_capability_capsules_in_scope` for the cursor protocol.
    /// Service-layer guard: `limit` defaults to 50 if 0, capped at 200
    /// inside the repo.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_capability_capsules_in_scope(
        &self,
        tenant: &str,
        project: Option<&str>,
        repo: Option<&str>,
        module: Option<&str>,
        capsule_type: Option<&str>,
        status: Option<&str>,
        source_agent: Option<&str>,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<(Vec<CapabilityCapsuleRecord>, bool), ServiceError> {
        let lim = if limit == 0 { 50 } else { limit };
        Ok(self
            .store
            .list_capability_capsules_in_scope(
                tenant,
                project,
                repo,
                module,
                capsule_type,
                status,
                source_agent,
                cursor,
                lim,
            )
            .await?)
    }

    /// Hard-delete a memory. **Irreversible.** Backs
    /// `DELETE /capability_capsules/{id}` from the admin web page.
    ///
    /// Order:
    ///   1. Verify the row exists for this tenant (clean 404 if not).
    ///   2. Issue `LanceStore::delete_capability_capsule_hard` — drops the
    ///      `capability_capsules` row.
    ///
    /// **Cascade caveat**: the storage-layer call leaves a TODO for
    /// cascade-deleting from `capability_capsule_embeddings`,
    /// `embedding_jobs`, `feedback_events`, and `graph_edges`. Lance is
    /// the authoritative source of truth for capsule existence; orphan
    /// rows in those satellite tables don't surface in queries that
    /// JOIN against the parent capsule (which is the read path for
    /// every public surface). Closing that TODO is tracked separately.
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

    /// Service entry point for the auto-promote sweep — delegates to
    /// `crate::worker::auto_promote_worker::sweep_once`. Used by the
    /// HTTP `/reviews/auto_promote` endpoint for manual / cron-driven
    /// runs; the background worker (when enabled) bypasses this and
    /// calls the worker function directly.
    pub async fn auto_promote_sweep(
        &self,
        tenant: &str,
        settings: &crate::config::AutoPromoteSettings,
        dry_run: bool,
    ) -> Result<Vec<String>, ServiceError> {
        crate::worker::auto_promote_worker::sweep_once(&*self.store, settings, tenant, dry_run)
            .await
            .map_err(ServiceError::Storage)
    }

    /// Service entry point for the idle-archive sweep (governance Step 2).
    /// Delegates to `idle_archive_worker::sweep_once`. Backs
    /// `POST /reviews/idle_archive` — `dry_run=true` previews candidate ids
    /// without writing; `dry_run=false` archives (and is a no-op while the
    /// worker is disabled, per the worker's safety gate).
    pub async fn idle_archive_sweep(
        &self,
        tenant: &str,
        settings: &crate::config::IdleArchiveSettings,
        dry_run: bool,
    ) -> Result<Vec<String>, ServiceError> {
        crate::worker::idle_archive_worker::sweep_once(&*self.store, settings, tenant, dry_run)
            .await
            .map_err(ServiceError::Storage)
    }

    /// Service entry point for the capsule self-evolution sweep (doc
    /// `docs/evolution-worker.md` E1). Delegates to
    /// `evolution_worker::sweep_once`. Backs `POST /reviews/evolution`
    /// — `dry_run=true` previews proposals without writing anything;
    /// `dry_run=false` runs a real cycle (and is a no-op while the
    /// worker is disabled, per the worker's safety gate).
    pub async fn evolution_sweep(
        &self,
        tenant: &str,
        settings: &crate::config::EvolutionSettings,
        dry_run: bool,
    ) -> Result<crate::worker::evolution_worker::EvolutionReport, ServiceError> {
        crate::worker::evolution_worker::sweep_once(&*self.store, settings, tenant, dry_run)
            .await
            .map_err(ServiceError::Storage)
    }

    /// Service entry point for the Lance vacuum sweep — delegates to
    /// `MaintenanceStore::vacuum_old_versions_with` via the worker
    /// helper. Used by `POST /admin/vacuum` for on-demand operator
    /// runs; the background worker (when enabled) bypasses this and
    /// calls the worker function directly. `aggressive=true`
    /// bypasses Lance's 7-day in-flight safety floor (single-writer
    /// local-first default per `VacuumSettings::development_defaults`).
    pub async fn vacuum(
        &self,
        older_than_days: i64,
        aggressive: bool,
    ) -> Result<crate::storage::VacuumStats, ServiceError> {
        crate::worker::vacuum_worker::sweep_once(&*self.store, older_than_days, aggressive)
            .await
            .map_err(ServiceError::Storage)
    }

    /// Service entry point for `POST /admin/reindex`: force-rebuild every
    /// managed ANN/scalar/FTS index regardless of its unindexed delta.
    /// Needed after an index *parameter* change (e.g. the IVF partition-count
    /// fix) where the delta-driven `ensure_query_indexes` would Skip an index
    /// that is stale in shape, not coverage. Non-Lance backends no-op.
    pub async fn reindex(
        &self,
    ) -> Result<crate::storage::lance_store::IndexMaintenanceStats, ServiceError> {
        self.store
            .rebuild_query_indexes()
            .await
            .map_err(ServiceError::Storage)
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
        let edges =
            resolve_drafts_to_edges(drafts, &*self.store, &superseding.tenant, &now).await?;
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
        note: Option<String>,
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
            note: note.filter(|s| !s.is_empty()),
        };

        let updated = self.store.apply_feedback(&memory, feedback).await?;
        crate::metrics::metrics().record_feedback(&feedback_kind);
        Ok(updated)
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
            supersedes_capability_capsule_id: Some(original.capability_capsule_id.clone()),
            expires_at: None,
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
            last_used_at: None,
            last_recalled_at: None,
            // A superseding successor gets a fresh lease — no inherited expiry.
            expires_at: None,
        }
    }

    pub async fn graph_neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, ServiceError> {
        Ok(self.store.neighbors(node_id).await?)
    }

    /// Multi-hop BFS variant. `max_hops` defaults to 1 when 0 is
    /// passed; the storage layer caps at 3 to prevent dense-graph
    /// blow-up.
    pub async fn graph_neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, ServiceError> {
        let hops = if max_hops == 0 { 1 } else { max_hops };
        Ok(self.store.neighbors_within(node_id, hops, as_of).await?)
    }

    /// Full edge history for a node, closed edges included. Used by
    /// the `kg_timeline` MCP tool.
    pub async fn graph_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, ServiceError> {
        Ok(self.store.kg_timeline(node_id).await?)
    }

    /// All edges with `relation = predicate`. Optionally restricted to
    /// edges active at `as_of` (20-digit ms string). mempalace's
    /// `query_relationship` analogue — KG K4.
    pub async fn graph_query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, ServiceError> {
        Ok(self.store.query_predicate(predicate, as_of).await?)
    }

    /// Suggest entity canonical_names close to `node_id` for callers
    /// who hit an empty-result `graph_neighbors`. mempalace's
    /// `_fuzzy_match` analogue — KG K5.
    ///
    /// Parses `entity:<id>` / `entity:<alias>` prefixes; for everything
    /// else (`capability_capsule:` / `session:` / bare strings) the
    /// suggestion source is still entity canonical names since those
    /// are the only human-readable corpus mem indexes. Returns up to
    /// `limit` matches with Levenshtein ≤ 3 on the normalized form;
    /// empty when nothing close.
    pub async fn graph_neighbor_suggestions(
        &self,
        tenant: &str,
        node_id: &str,
        limit: usize,
    ) -> Result<Vec<NeighborSuggestion>, ServiceError> {
        let token = node_id.strip_prefix("entity:").unwrap_or(node_id);
        let normalized = crate::pipeline::entity_normalize::normalize_alias(token);
        if normalized.is_empty() {
            return Ok(Vec::new());
        }
        // Scan the registry — capped at 1000 to bound worst case;
        // larger tenants need an index but that's a future concern.
        let entities = self.store.list_entities(tenant, None, None, 1000).await?;
        let mut scored: Vec<NeighborSuggestion> = entities
            .into_iter()
            .filter_map(|e| {
                let candidate =
                    crate::pipeline::entity_normalize::normalize_alias(&e.canonical_name);
                if candidate.is_empty() || candidate == normalized {
                    return None;
                }
                let dist = crate::service::fact_check_service::levenshtein(&normalized, &candidate);
                if dist > 3 || dist == 0 {
                    return None;
                }
                Some(NeighborSuggestion {
                    suggested_node_id: format!("entity:{}", e.entity_id),
                    canonical_name: e.canonical_name,
                    edit_distance: dist,
                })
            })
            .collect();
        scored.sort_by(|a, b| {
            a.edit_distance
                .cmp(&b.edit_distance)
                .then_with(|| a.canonical_name.cmp(&b.canonical_name))
        });
        scored.truncate(limit.max(1));
        Ok(scored)
    }

    /// Caller-curated tunnel listing (relation prefix `user_tunnel:`).
    pub async fn graph_list_user_tunnels(
        &self,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, ServiceError> {
        let lim = if limit == 0 { 50 } else { limit };
        Ok(self.store.list_user_tunnels(lim).await?)
    }

    /// Tunnels whose endpoints match the two node-id prefixes
    /// (bidirectional). Empty prefix = "any".
    pub async fn graph_find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, ServiceError> {
        let lim = if limit == 0 { 50 } else { limit };
        Ok(self.store.find_tunnels(prefix_a, prefix_b, lim).await?)
    }

    /// BFS from `node_id` following only user-tunnel edges. `max_hops`
    /// defaults to 1 when 0; storage caps at `MAX_HOPS_CAP = 3`.
    pub async fn graph_follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, ServiceError> {
        let hops = if max_hops == 0 { 1 } else { max_hops };
        Ok(self.store.follow_tunnels(node_id, hops).await?)
    }

    /// Whole-graph aggregate counts (`GraphStats`).
    pub async fn graph_stats(
        &self,
    ) -> Result<crate::domain::capability_capsule::GraphStats, ServiceError> {
        Ok(self.store.graph_stats().await?)
    }

    /// Caller-supplied direct edge write. `edge.valid_from` is taken
    /// verbatim; when the caller omits a meaningful timestamp the
    /// service stamps `current_timestamp()` as a courtesy.
    pub async fn graph_add_edge(&self, mut edge: GraphEdge) -> Result<bool, ServiceError> {
        if edge.valid_from.trim().is_empty() {
            edge.valid_from = crate::storage::current_timestamp();
        }
        // G4 (zero-LLM contradiction auto-invalidation): if `predicate` is
        // configured single-valued, asserting this new fact closes any existing
        // active `(from, predicate, other_to)` edge first (Graphiti's "new fact
        // supersedes the conflicting old edge"). Opt-in / default-off.
        let closed = self
            .auto_invalidate_conflicts(&edge, &kg_functional_predicates())
            .await?;
        if closed > 0 {
            crate::metrics::metrics().add_kg_auto_invalidated(closed as u64);
        }
        Ok(self.store.add_edge_direct(&edge).await?)
    }

    /// G4 core (pure of env reads — takes the functional-predicate set
    /// explicitly so it is unit-testable): when `edge.relation` is functional
    /// (single-valued), close every existing **active** edge sharing the same
    /// `(from_node_id, relation)` but pointing at a different `to_node_id`.
    /// Returns the number of edges closed. A no-op (returns 0) when the
    /// predicate is not in `functional` — so a multi-valued predicate keeps all
    /// its edges. Only OUTGOING edges from `edge.from_node_id` are considered.
    /// `pub` so callers can run explicit conflict resolution and tests can drive
    /// it with an explicit set (no env coupling).
    pub async fn auto_invalidate_conflicts(
        &self,
        edge: &GraphEdge,
        functional: &HashSet<String>,
    ) -> Result<usize, ServiceError> {
        if !functional.contains(&edge.relation.to_ascii_lowercase()) {
            return Ok(0);
        }
        let now = crate::storage::current_timestamp();
        let mut closed = 0usize;
        for e in self.store.neighbors(&edge.from_node_id).await? {
            if e.valid_to.is_none()
                && e.from_node_id == edge.from_node_id
                && e.relation == edge.relation
                && e.to_node_id != edge.to_node_id
            {
                self.store
                    .invalidate_edge(&e.from_node_id, &e.relation, &e.to_node_id, &now)
                    .await?;
                closed += 1;
            }
        }
        Ok(closed)
    }

    /// Invalidate one active edge by triple. `ended_at` defaults to
    /// `current_timestamp()` when None or empty.
    pub async fn graph_invalidate_edge(
        &self,
        from_node_id: &str,
        predicate: &str,
        to_node_id: &str,
        ended_at: Option<&str>,
    ) -> Result<usize, ServiceError> {
        let now = match ended_at {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => crate::storage::current_timestamp(),
        };
        Ok(self
            .store
            .invalidate_edge(from_node_id, predicate, to_node_id, &now)
            .await?)
    }

    pub async fn search(
        &self,
        query: SearchCapabilityCapsuleRequest,
    ) -> Result<SearchCapabilityCapsuleResponse, ServiceError> {
        crate::metrics::metrics().inc_capsule_search();
        let tenant = query.tenant.as_deref().unwrap_or("local");

        // Wake-up fast path: SessionStart hooks call us with `intent="wake_up"`
        // and an empty `query` to seed "Recent Context" at session boot.
        // The full pipeline (embedding the empty string, lance vector ANN
        // lookup, scanning every active memory for BM25, graph-aware ranking, tiktoken-based
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

            let candidates = if query.scope_filters.is_empty() {
                self.store
                    .recent_active_capability_capsules(tenant, WAKE_UP_LIMIT)
                    .await
                    .map_err(ServiceError::Storage)?
            } else {
                // Repo-scoped wake-up: the SessionStart hook passes
                // `repo:<dir>` / `project:<dir>` so the boot context is
                // about THIS project, not whatever was globally freshest.
                // Fetch a wider recent slice, float in-scope capsules to
                // the front, then truncate — backfilling with recent
                // global rows so a brand-new repo still gets useful seed
                // context instead of an empty block.
                let widened = (WAKE_UP_LIMIT * 4).min(512);
                let recent = self
                    .store
                    .recent_active_capability_capsules(tenant, widened)
                    .await
                    .map_err(ServiceError::Storage)?;
                let (mut in_scope, rest): (Vec<_>, Vec<_>) = recent.into_iter().partition(|c| {
                    crate::pipeline::retrieve::matches_scope_filters(c, &query.scope_filters)
                });
                in_scope.extend(rest);
                in_scope.truncate(WAKE_UP_LIMIT);
                in_scope
            };
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

            // O1: do NOT bump last_used here. Wake-up is automatic boot
            // context (recent-active capsules surfaced every session
            // start), not query-driven *use* by the agent. Bumping it
            // would reset the decay clock on the newest cohort on every
            // session start, self-reinforcing them so they never decay
            // regardless of whether they were used. Only the explicit
            // query path below counts as "used".
            return Ok(response);
        }

        // `Store::hybrid_candidates` composes the two recall channels:
        // `bm25_candidate_ids` (in-RAM Tantivy) + `ann_candidate_ids`
        // (lance vector ANN), fused by capability_capsule_id with RRF
        // (k=60) in Rust (`pipeline::ranking::rrf_merge`).
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

        // K9: when edge dynamics is enabled, weight the graph boost by
        // each connecting edge's decayed strength and enqueue co-access
        // events to the potentiation worker. `None` ⇒ pre-K9 flat boost.
        let dynamics_ctx = self
            .edge_access_tx
            .as_ref()
            .map(|tx| retrieve::EdgeDynamicsCtx {
                now: crate::storage::current_timestamp(),
                tx: tx.clone(),
            });
        let ranked = match retrieve::rank_with_hybrid_and_graph(
            pool.clone(),
            hybrid_hits.clone(),
            &query,
            self.store.as_ref(),
            dynamics_ctx.as_ref(),
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
                retrieve::rank_with_hybrid_and_graph(
                    pool,
                    hybrid_hits,
                    &q2,
                    self.store.as_ref(),
                    None,
                )
                .await
                .unwrap_or_default()
            }
        };

        let response = compress::compress(&ranked, query.token_budget);
        self.enqueue_capsules_used(tenant, &response);
        Ok(response)
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
/// Each `resolve_or_create` is a lance-native lookup-then-insert against the
/// `entities` / `entity_aliases` tables (`lance_store/entities.rs`); concurrent
/// callers are reconciled by the `(tenant, alias_text)` PK. Pure async; the
/// caller passes `now` so timestamps stay deterministic in tests.
///
/// Wired into `CapabilityCapsuleService::ingest` and `edit_and_accept_pending` by Task 9
/// of the entity-registry roadmap.
pub(crate) async fn resolve_drafts_to_edges(
    drafts: Vec<GraphEdgeDraft>,
    registry: &dyn crate::storage::EntityRegistry,
    tenant: &str,
    now: &str,
) -> Result<Vec<GraphEdge>, StorageError> {
    let mut out = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let to_node_id = match draft.to_kind {
            ToNodeKind::LiteralMemory(capability_capsule_id) => {
                format!("capability_capsule:{capability_capsule_id}")
            }
            ToNodeKind::LiteralSession(session_id) => {
                format!("session:{session_id}")
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
            confidence: None,
            extractor: None,
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        });
    }
    Ok(out)
}

/// Derive an index-only `summary` from `content` when the caller supplies
/// none. Deterministic, no LLM (mem offloads real summarization to the
/// writing agent): lift the first line that carries real text — markdown
/// decoration (`#` headers, list bullets, blockquotes, fenced code blocks)
/// stripped/skipped — then cut at the first sentence boundary or
/// `SUMMARY_LIMIT` chars, whichever comes first, on a char/whitespace
/// boundary so it never splits a word or a multi-byte character. Falls back
/// to `"memory"` when nothing usable remains. This is a *hint* for ranking /
/// display, never a fact source — the verbatim rule lives on `content`.
fn summarize(content: &str) -> String {
    const SUMMARY_LIMIT: usize = 100;

    let mut in_fence = false;
    let line = content.lines().find_map(|raw| {
        let t = raw.trim();
        // Fenced code blocks: ``` toggles in/out; everything inside is skipped.
        if t.starts_with("```") {
            in_fence = !in_fence;
            return None;
        }
        if in_fence {
            return None;
        }
        let stripped = strip_markdown_noise(t);
        (!stripped.is_empty()).then_some(stripped)
    });

    match line {
        Some(l) => {
            let s = first_sentence_or_cap(&l, SUMMARY_LIMIT);
            if s.is_empty() {
                "memory".to_string()
            } else {
                s
            }
        }
        None => "memory".to_string(),
    }
}

/// Strip leading markdown decoration from a single (already-trimmed) line.
/// A line that is *only* decoration (`###`, `---`, `***`, `> `) collapses to
/// the empty string so the caller skips it.
fn strip_markdown_noise(line: &str) -> String {
    if line.is_empty()
        || line
            .chars()
            .all(|c| matches!(c, '#' | '-' | '*' | '_' | '=' | '>' | ' '))
    {
        return String::new();
    }
    let s = line.trim_start_matches(['#', '>', '`']).trim_start();
    let s = s
        .strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("+ "))
        .unwrap_or(s);
    s.trim().to_string()
}

/// Cut `line` at the first sentence boundary within `cap` chars, else at the
/// last whitespace before `cap` (so an ASCII word isn't split; CJK has no
/// spaces, so it just hits the cap). An ASCII `.`/`!`/`?` only ends a
/// sentence when followed by whitespace or end-of-line — that keeps version
/// numbers (`v3.5.0.3`) and paths (`src/a.rs`) intact. CJK `。！？` always end.
fn first_sentence_or_cap(line: &str, cap: usize) -> String {
    const CJK_TERMS: [char; 3] = ['。', '！', '？'];
    const ASCII_TERMS: [char; 3] = ['.', '!', '?'];

    let chars: Vec<char> = line.chars().collect();
    let scan = chars.len().min(cap);
    let mut end = scan;
    let mut at_boundary = false;
    for i in 0..scan {
        let c = chars[i];
        let next_breaks = match chars.get(i + 1) {
            Some(n) => n.is_whitespace(),
            None => true,
        };
        if CJK_TERMS.contains(&c) || (ASCII_TERMS.contains(&c) && next_breaks) {
            end = i + 1;
            at_boundary = true;
            break;
        }
    }
    // No sentence end within the cap and the line overruns it: back off to the
    // last whitespace (only when that keeps ≥60% of the budget, else just cap).
    if !at_boundary && chars.len() > cap {
        if let Some(ws) = (0..end).rev().find(|&i| chars[i].is_whitespace()) {
            if ws >= cap * 6 / 10 {
                end = ws;
            }
        }
    }
    chars[..end].iter().collect::<String>().trim().to_string()
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

/// G4: parse a comma-separated functional-predicate list (lowercased, trimmed,
/// empties dropped). Pure — unit-testable without touching the environment.
fn parse_functional_predicates(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// G4: the set of graph predicates treated as **functional** (single-valued) —
/// asserting a new `(from, predicate, to)` edge auto-closes any existing active
/// `(from, predicate, other_to)` edge. Read live from
/// `MEM_KG_FUNCTIONAL_PREDICATES` (comma-separated); **default empty = feature
/// OFF** (no edge is ever auto-closed). A deployment opts in only for predicates
/// it knows are single-valued (e.g. `located_in`, `current_status`); a
/// multi-valued predicate like `uses` must NOT be listed or valid edges would be
/// wrongly closed. Read at the use site so it tunes without a restart.
fn kg_functional_predicates() -> HashSet<String> {
    std::env::var("MEM_KG_FUNCTIONAL_PREDICATES")
        .ok()
        .map(|v| parse_functional_predicates(&v))
        .unwrap_or_default()
}

fn next_embedding_job_id() -> String {
    format!("ej_{}", uuid::Uuid::now_v7())
}

fn next_memory_id() -> String {
    format!("mem_{}", uuid::Uuid::now_v7())
}

/// Result of `CapabilityCapsuleService::prepare_one` — either an early
/// dedup hit (existing row) or a fresh record ready to be flushed in
/// the batch insert. The `New` variant boxes the record because
/// `CapabilityCapsuleRecord` is large (~512 B); without the box clippy
/// flags `large_enum_variant`.
enum PreparedIngest {
    Existing(IngestCapabilityCapsuleResponse),
    New {
        record: Box<CapabilityCapsuleRecord>,
        session_id: String,
    },
}

fn next_episode_id() -> String {
    format!("ep_{}", uuid::Uuid::now_v7())
}

#[cfg(test)]
mod summarize_tests {
    use super::summarize;

    #[test]
    fn strips_markdown_header() {
        // Old [:80] kept the leading `# ` and bled into later lines; the
        // heuristic lifts the header text as the index hint.
        assert_eq!(
            summarize("# Auto-promote sweep\n\n## Symptom\nblah"),
            "Auto-promote sweep"
        );
    }

    #[test]
    fn takes_first_real_line_skipping_blanks_and_bullets() {
        assert_eq!(
            summarize("\n\n- the fix is a new column\nmore detail"),
            "the fix is a new column"
        );
    }

    #[test]
    fn cuts_at_sentence_boundary() {
        assert_eq!(
            summarize("This is the lesson. And more after."),
            "This is the lesson."
        );
    }

    #[test]
    fn does_not_cut_on_version_or_path_dots() {
        // '.' inside v3.5.0.3 / src/a.rs is followed by a non-space, so it is
        // NOT a sentence end — the whole short line is kept.
        let s = summarize("commit abc (zgy-v3.5.0.3) shipped src/a.rs behind a flag");
        assert_eq!(
            s,
            "commit abc (zgy-v3.5.0.3) shipped src/a.rs behind a flag"
        );
    }

    #[test]
    fn caps_long_line_without_splitting_a_word() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa \
                    lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega";
        let s = summarize(long);
        assert!(s.chars().count() <= 100, "too long: {}", s.chars().count());
        assert!(long.starts_with(&s), "summary must be a prefix");
        // the byte right after the prefix is a space → clean word boundary
        assert!(
            long.len() == s.len() || long.as_bytes()[s.len()] == b' ',
            "cut landed mid-word: {s:?}"
        );
    }

    #[test]
    fn cjk_sentence_boundary() {
        assert_eq!(
            summarize("这是一条中文教训。后面还有更多细节"),
            "这是一条中文教训。"
        );
    }

    #[test]
    fn skips_fenced_code_block() {
        let s = summarize("```rust\nlet x = 1;\n```\nThe real takeaway here.");
        assert_eq!(s, "The real takeaway here.");
    }

    #[test]
    fn decoration_only_lines_skipped() {
        assert_eq!(
            summarize("###\n---\nActual lesson text"),
            "Actual lesson text"
        );
    }

    #[test]
    fn empty_or_whitespace_is_memory() {
        assert_eq!(summarize(""), "memory");
        assert_eq!(summarize("   \n\t \n"), "memory");
    }
}

#[cfg(test)]
mod g4_predicate_tests {
    use super::parse_functional_predicates;

    #[test]
    fn parses_comma_list_lowercased_trimmed_empties_dropped() {
        let set = parse_functional_predicates(" Located_In , current_status ,, uses_db ,");
        assert!(set.contains("located_in"));
        assert!(set.contains("current_status"));
        assert!(set.contains("uses_db"));
        assert_eq!(
            set.len(),
            3,
            "empties between commas must be dropped: {set:?}"
        );
    }

    #[test]
    fn empty_input_is_empty_set_feature_off() {
        assert!(parse_functional_predicates("").is_empty());
        assert!(parse_functional_predicates("   ,  , ").is_empty());
    }
}
