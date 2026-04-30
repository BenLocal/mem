use std::collections::HashSet;

use serde::Serialize;
use std::sync::Arc;
use thiserror::Error;

use crate::domain::{
    embeddings::{EmbeddingJobInfo, EmbeddingsRebuildResponse, MemoryEmbeddingMeta},
    episode::EpisodeResponse,
    memory::MemoryDetailResponse,
};
use crate::embedding::EmbeddingProvider;
use crate::{
    domain::{
        episode::{EpisodeRecord, IngestEpisodeRequest},
        memory::{
            EditPendingRequest, EditPendingResponse, FeedbackKind, GraphEdge, IngestMemoryRequest,
            MemoryRecord, MemoryStatus,
        },
        query::{SearchMemoryRequest, SearchMemoryResponse},
    },
    pipeline::ingest::{compute_content_hash, initial_status, memory_node_id},
    pipeline::workflow,
    pipeline::{compress, retrieve},
    storage::{
        current_timestamp, DuckDbGraphStore, DuckDbRepository, EmbeddingJobInsert, GraphError,
        StorageError,
    },
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestMemoryResponse {
    pub memory_id: String,
    pub status: MemoryStatus,
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

impl From<MemoryRecord> for IngestMemoryResponse {
    fn from(memory: MemoryRecord) -> Self {
        Self {
            memory_id: memory.memory_id,
            status: memory.status,
        }
    }
}

#[derive(Clone)]
pub struct MemoryService {
    repository: DuckDbRepository,
    graph: Arc<DuckDbGraphStore>,
    /// Value stored on `embedding_jobs.provider` (e.g. `fake`, `openai`).
    embedding_job_provider: String,
    /// When set, search runs hybrid lexical + semantic retrieval.
    embedding_search_provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl MemoryService {
    /// Primary constructor. Creates a fresh `DuckDbGraphStore` backed by the same repository.
    pub fn new(repository: DuckDbRepository) -> Self {
        let repo_arc = Arc::new(repository.clone());
        let graph = Arc::new(DuckDbGraphStore::new(repo_arc));
        Self::new_with_graph(repository, graph)
    }

    /// Constructor that also attaches a vector index before building the service.
    pub fn new_with_index(
        repository: DuckDbRepository,
        vector_index: Arc<crate::storage::VectorIndex>,
    ) -> Self {
        repository.attach_vector_index(vector_index);
        Self::new(repository)
    }

    /// Constructor that accepts a pre-built `DuckDbGraphStore` (used by tests and `app.rs`).
    pub fn new_with_graph(repository: DuckDbRepository, graph: Arc<DuckDbGraphStore>) -> Self {
        Self::with_graph_and_embedding_providers(repository, graph, "fake".to_string(), None)
    }

    /// Kept for compatibility with call-sites that supply a graph and a provider id.
    pub fn with_graph_and_embedding_provider(
        repository: DuckDbRepository,
        graph: Arc<DuckDbGraphStore>,
        embedding_job_provider: String,
    ) -> Self {
        Self::with_graph_and_embedding_providers(repository, graph, embedding_job_provider, None)
    }

    /// Full constructor used by `app.rs`.
    pub fn with_graph_and_embedding_providers(
        repository: DuckDbRepository,
        graph: Arc<DuckDbGraphStore>,
        embedding_job_provider: String,
        embedding_search_provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self {
            repository,
            graph,
            embedding_job_provider,
            embedding_search_provider,
        }
    }

    pub async fn ingest(
        &self,
        request: IngestMemoryRequest,
    ) -> Result<IngestMemoryResponse, ServiceError> {
        let content_hash = compute_content_hash(&request);

        if let Some(existing) = self
            .repository
            .find_by_idempotency_or_hash(&request.tenant, &request.idempotency_key, &content_hash)
            .await?
        {
            return Ok(existing.into());
        }

        let status = initial_status(&request.memory_type, &request.write_mode);
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
            &self.repository,
            &request.tenant,
            &request.source_agent,
            &now,
            crate::pipeline::session::idle_minutes_from_env(),
        )
        .await
        .map_err(ServiceError::Storage)?;

        let memory = MemoryRecord {
            memory_id: next_memory_id(),
            tenant: request.tenant,
            memory_type: request.memory_type,
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
            confidence: default_confidence(&status),
            decay_score: 0.0,
            content_hash,
            idempotency_key: request.idempotency_key,
            session_id: Some(session_id.clone()),
            supersedes_memory_id: None,
            source_agent: request.source_agent,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_validated_at: None,
        };

        let stored = self.repository.insert_memory(memory).await?;
        self.graph.sync_memory(&stored).await?;
        self.enqueue_embedding_job_for_memory(&stored).await?;
        self.repository
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
                .repository
                .list_successful_episodes_for_tenant(&episode.tenant)
                .await?;
            episodes.push(episode.clone());
            workflow::maybe_extract_workflow(&episodes)
        };

        if let Some(candidate) = workflow_candidate.as_mut() {
            let workflow_memory = self
                .ingest(workflow::workflow_memory_request(&episode, candidate))
                .await?;
            candidate.memory_id = Some(workflow_memory.memory_id);
            episode.workflow_candidate = Some(candidate.clone());
        }

        self.repository.insert_episode(episode).await?;

        Ok(EpisodeResponse {
            episode_id,
            status: "created".to_string(),
            workflow_candidate,
        })
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, ServiceError> {
        Ok(self.repository.list_pending_review(tenant).await?)
    }

    pub async fn get_memory(
        &self,
        tenant: Option<&str>,
        memory_id: &str,
    ) -> Result<MemoryDetailResponse, ServiceError> {
        let memory = match tenant {
            Some(tenant) => {
                self.repository
                    .get_memory_for_tenant(tenant, memory_id)
                    .await?
            }
            None => self.repository.get_memory(memory_id.to_string()).await?,
        }
        .ok_or(ServiceError::NotFound)?;

        let embedding = self.embedding_meta_for_memory(&memory).await?;

        let graph_links = self
            .graph
            .neighbors(&memory_node_id(memory_id))
            .await
            .unwrap_or_default();

        Ok(MemoryDetailResponse {
            version_chain: self
                .repository
                .list_memory_versions_for_tenant(&memory.tenant, memory_id)
                .await?,
            graph_links,
            feedback_summary: self.repository.feedback_summary(memory_id).await?,
            memory,
            embedding,
        })
    }

    pub async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status: Option<&str>,
        memory_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, ServiceError> {
        Ok(self
            .repository
            .list_embedding_jobs(tenant, status, memory_id, limit)
            .await?)
    }

    pub async fn rebuild_embeddings(
        &self,
        tenant: &str,
        memory_ids: &[String],
        force: bool,
    ) -> Result<EmbeddingsRebuildResponse, ServiceError> {
        let ids: Vec<String> = if memory_ids.is_empty() {
            self.repository.list_memory_ids_for_tenant(tenant).await?
        } else {
            memory_ids.to_vec()
        };

        let mut enqueued: u32 = 0;
        for mid in ids {
            let memory = self
                .repository
                .get_memory_for_tenant(tenant, &mid)
                .await?
                .ok_or(ServiceError::NotFound)?;

            let now = current_timestamp();
            if force {
                self.repository.delete_memory_embedding(&mid).await?;
                self.repository
                    .stale_live_embedding_jobs_for_memory(
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
                memory_id: memory.memory_id.clone(),
                target_content_hash: memory.content_hash.clone(),
                provider: self.embedding_job_provider.clone(),
                available_at: now.clone(),
                created_at: now.clone(),
                updated_at: now,
            };
            if self.repository.try_enqueue_embedding_job(insert).await? {
                enqueued += 1;
            }
        }

        Ok(EmbeddingsRebuildResponse { enqueued })
    }

    async fn embedding_meta_for_memory(
        &self,
        memory: &MemoryRecord,
    ) -> Result<MemoryEmbeddingMeta, ServiceError> {
        if let Some((model, hash, updated_at)) = self
            .repository
            .get_memory_embedding_row(&memory.memory_id)
            .await?
        {
            if hash == memory.content_hash {
                return Ok(MemoryEmbeddingMeta {
                    status: "indexed".to_string(),
                    model: Some(model),
                    updated_at: Some(updated_at),
                    content_hash: Some(hash),
                });
            }
            return Ok(MemoryEmbeddingMeta {
                status: "stale".to_string(),
                model: Some(model),
                updated_at: Some(updated_at),
                content_hash: Some(hash),
            });
        }

        let job_status = self
            .repository
            .latest_embedding_job_status_for_hash(
                &memory.tenant,
                &memory.memory_id,
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

        Ok(MemoryEmbeddingMeta {
            status: status_label.to_string(),
            ..Default::default()
        })
    }

    pub async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, ServiceError> {
        self.repository
            .get_pending(tenant, memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        Ok(self.repository.accept_pending(tenant, memory_id).await?)
    }

    pub async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, ServiceError> {
        self.repository
            .get_pending(tenant, memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        Ok(self.repository.reject_pending(tenant, memory_id).await?)
    }

    /// Supersede flow: accept a pending memory by replacing it with an edited active version.
    ///
    /// After storage is updated, the graph is kept consistent:
    /// 1. v1's edges are closed (`close_edges_for_memory`)
    /// 2. v2's edges are opened (`sync_memory`)
    pub async fn edit_and_accept_pending(
        &self,
        tenant: &str,
        patch: EditPendingRequest,
    ) -> Result<EditPendingResponse, ServiceError> {
        let original_memory_id = patch.memory_id.clone();
        let original = self
            .repository
            .get_pending(tenant, &original_memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;

        let superseding = self
            .repository
            .replace_pending_with_successor(
                tenant,
                &original_memory_id,
                self.superseding_active_version(&original, patch),
            )
            .await?;

        // Close v1's graph edges, then open v2's — order matters.
        self.graph
            .close_edges_for_memory(&original.memory_id)
            .await?;
        self.graph.sync_memory(&superseding).await?;

        self.enqueue_embedding_job_for_memory(&superseding).await?;

        Ok(EditPendingResponse {
            original_memory_id: original.memory_id,
            memory: superseding,
        })
    }

    pub async fn submit_feedback(
        &self,
        tenant: &str,
        memory_id: &str,
        feedback_kind: FeedbackKind,
    ) -> Result<MemoryRecord, ServiceError> {
        let memory = self
            .repository
            .get_memory_for_tenant(tenant, memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;

        let feedback = crate::storage::duckdb::FeedbackEvent {
            feedback_id: next_feedback_id(),
            memory_id: memory.memory_id.clone(),
            feedback_kind: feedback_kind.as_str().to_string(),
            created_at: current_timestamp(),
        };

        Ok(self.repository.apply_feedback(&memory, feedback).await?)
    }

    fn superseding_active_version(
        &self,
        original: &MemoryRecord,
        patch: EditPendingRequest,
    ) -> MemoryRecord {
        let request = IngestMemoryRequest {
            tenant: original.tenant.clone(),
            memory_type: original.memory_type.clone(),
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
            source_agent: original.source_agent.clone(),
            idempotency_key: None,
            write_mode: crate::domain::memory::WriteMode::Auto,
        };
        let now = current_timestamp();

        MemoryRecord {
            memory_id: next_memory_id(),
            tenant: original.tenant.clone(),
            memory_type: original.memory_type.clone(),
            status: MemoryStatus::Active,
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
            confidence: default_confidence(&MemoryStatus::Active),
            decay_score: 0.0,
            content_hash: compute_content_hash(&request),
            idempotency_key: None,
            session_id: None,
            supersedes_memory_id: Some(original.memory_id.clone()),
            source_agent: original.source_agent.clone(),
            created_at: now.clone(),
            updated_at: now,
            last_validated_at: None,
        }
    }

    pub async fn graph_neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, ServiceError> {
        Ok(self.graph.neighbors(node_id).await?)
    }

    pub async fn search(
        &self,
        query: SearchMemoryRequest,
    ) -> Result<SearchMemoryResponse, ServiceError> {
        let tenant = query.tenant.as_deref().unwrap_or("local");
        let lexical = self.lexical_candidates(tenant, &query.query).await?;

        let semantic = if let Some(provider) = self.embedding_search_provider.as_ref() {
            match provider.embed_text(&query.query).await {
                Ok(q) => self
                    .repository
                    .semantic_search_memories(tenant, &q, 48)
                    .await
                    .unwrap_or_default(),
                Err(_) => vec![],
            }
        } else {
            vec![]
        };

        let ranked = match retrieve::rank_with_graph_hybrid(
            lexical,
            semantic.clone(),
            &query,
            self.graph.as_ref(),
        )
        .await
        {
            Ok(ranked) => ranked,
            Err(e) => {
                tracing::warn!(error = %e, "graph backend error during rank_with_graph_hybrid; falling back to no graph boost");
                let lex2 = self.lexical_candidates(tenant, &query.query).await?;
                if semantic.is_empty() {
                    retrieve::rank_candidates(lex2, &query)
                } else {
                    retrieve::merge_and_rank_hybrid(lex2, semantic, &query, &HashSet::new(), 0)
                }
            }
        };

        Ok(compress::compress(&ranked, query.token_budget))
    }

    /// Lexical candidate selection.
    ///
    /// Returns the full live set for the tenant, but with BM25-matching rows
    /// **moved to the front** so they get the highest RRF lexical rank in
    /// `retrieve::merge_and_rank_hybrid`. Non-matching rows still enter the
    /// pipeline so semantic / scope / freshness signals can score them; the
    /// relevance floor in `retrieve::finalize` drops anything that fails to
    /// accumulate enough signal.
    ///
    /// This keeps BM25 a ranking signal rather than a hard filter — relevant
    /// rows bubble up, irrelevant rows get filtered by the threshold instead
    /// of being pre-excluded from scoring.
    async fn lexical_candidates(
        &self,
        tenant: &str,
        query: &str,
    ) -> Result<Vec<MemoryRecord>, ServiceError> {
        const BM25_TOP_K: usize = 48;

        let all = self
            .repository
            .search_candidates(tenant)
            .await
            .map_err(ServiceError::Storage)?;

        if query.trim().is_empty() || all.is_empty() {
            return Ok(all);
        }

        let bm25 = self
            .repository
            .bm25_candidates(tenant, query, BM25_TOP_K)
            .await
            .map_err(ServiceError::Storage)?;

        if bm25.is_empty() {
            return Ok(all);
        }

        let bm25_ids: HashSet<String> = bm25.iter().map(|m| m.memory_id.clone()).collect();
        let mut combined = bm25;
        for memory in all {
            if !bm25_ids.contains(&memory.memory_id) {
                combined.push(memory);
            }
        }
        Ok(combined)
    }

    async fn enqueue_embedding_job_for_memory(
        &self,
        memory: &MemoryRecord,
    ) -> Result<(), ServiceError> {
        let now = current_timestamp();
        let insert = EmbeddingJobInsert {
            job_id: next_embedding_job_id(),
            tenant: memory.tenant.clone(),
            memory_id: memory.memory_id.clone(),
            target_content_hash: memory.content_hash.clone(),
            provider: self.embedding_job_provider.clone(),
            available_at: now.clone(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.repository.try_enqueue_embedding_job(insert).await?;
        Ok(())
    }
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

fn default_confidence(status: &MemoryStatus) -> f32 {
    match status {
        MemoryStatus::Active => 0.9,
        MemoryStatus::PendingConfirmation => 0.6,
        MemoryStatus::Provisional => 0.5,
        MemoryStatus::Archived | MemoryStatus::Rejected => 0.0,
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
