//! Transcript-archive service façade.
//!
//! Combines the transcript repository, the transcript HNSW sidecar
//! (`Arc<VectorIndex>`), and an optional embedding provider into a single
//! interface used by `http/transcripts.rs`. Mirrors `service/memory_service.rs`
//! in shape (struct holds `Clone`/`Arc`-wrapped collaborators; `Clone` is
//! cheap so it can sit on `AppState`).
//!
//! The provider is `Option<Arc<dyn EmbeddingProvider>>` so unit/integration
//! tests can construct a service with no provider; in that mode, non-empty
//! semantic queries return zero hits (not an error). Empty-query searches
//! always work, falling back to the recent-time SQL listing.

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::embedding::EmbeddingProvider;
use crate::storage::{DuckDbRepository, StorageError, VectorIndex};

/// One entry in a [`TranscriptService::search`] result. Wraps the underlying
/// `ConversationMessage` together with a numeric score (cosine similarity for
/// semantic results, `0.0` for the empty-query / time-based fallback).
#[derive(Debug, Clone)]
pub struct TranscriptSearchHit {
    pub message: ConversationMessage,
    pub score: f32,
}

/// Optional filters layered on top of the candidate set returned by the
/// HNSW search (or the recent-time fallback). All fields are AND-ed.
///
/// `time_from` / `time_to` are matched lexicographically against
/// `created_at` — fine for ISO-8601 / RFC-3339 strings, which is the only
/// format produced by the ingest pipeline.
#[derive(Debug, Clone, Default)]
pub struct TranscriptSearchFilters {
    pub session_id: Option<String>,
    pub role: Option<MessageRole>,
    pub block_type: Option<BlockType>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
}

#[derive(Clone)]
pub struct TranscriptService {
    repo: DuckDbRepository,
    index: Arc<VectorIndex>,
    provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl TranscriptService {
    pub fn new(
        repo: DuckDbRepository,
        index: Arc<VectorIndex>,
        provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self {
            repo,
            index,
            provider,
        }
    }

    /// Inserts a single transcript block via the repository's idempotent
    /// `create_conversation_message`. Embedding job enqueueing is handled
    /// inside the repository; this method does not touch the HNSW index.
    pub async fn ingest(&self, msg: ConversationMessage) -> Result<(), StorageError> {
        self.repo.create_conversation_message(&msg).await
    }

    /// Returns every transcript block belonging to `session_id` in
    /// chronological order. Thin pass-through over the repo method; exists
    /// here so HTTP handlers depend on the service rather than the repo.
    pub async fn get_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.repo
            .get_conversation_messages_by_session(tenant, session_id)
            .await
    }

    /// Ranked transcript search.
    ///
    /// - `query.trim().is_empty()` → recent-time SQL listing (limit*4 to
    ///   leave headroom for filters), score `0.0` per row.
    /// - non-empty + provider attached → embed `query`, ANN-search the HNSW
    ///   sidecar (oversampled 4×), hydrate by id, filter, take `limit`.
    /// - non-empty + no provider → return `Ok(vec![])` (server can't
    ///   semantically search without an embedder).
    ///
    /// Phases:
    /// 1. Build candidate `(id, score)` list.
    /// 2. Hydrate into full `ConversationMessage` (drops missing rows).
    /// 3. Apply filters and zip with score.
    pub async fn search(
        &self,
        tenant: &str,
        query: &str,
        filters: &TranscriptSearchFilters,
        limit: usize,
    ) -> Result<Vec<TranscriptSearchHit>, StorageError> {
        let oversample = limit.max(1) * 4;

        // Phase 1: candidates — either semantic ANN or recent-time SQL.
        let candidates: Vec<(String, f32)> = if !query.trim().is_empty() {
            if let Some(provider) = &self.provider {
                let q_vec = provider
                    .embed_text(query)
                    .await
                    .map_err(|e| StorageError::InvalidInput(format!("query embed failed: {e}")))?;
                self.index
                    .search(&q_vec, oversample)
                    .await
                    .map_err(|e| StorageError::VectorIndex(e.to_string()))?
            } else {
                // No provider attached; semantic query cannot be served.
                vec![]
            }
        } else {
            self.repo
                .recent_conversation_messages(tenant, oversample)
                .await?
                .into_iter()
                .map(|m| (m.message_block_id, 0.0))
                .collect()
        };

        // Phase 2: hydrate to full rows (preserves rank order from candidates).
        let ids: Vec<String> = candidates.iter().map(|(id, _)| id.clone()).collect();
        let hydrated = self
            .repo
            .fetch_conversation_messages_by_ids(tenant, &ids)
            .await?;

        // Phase 3: apply filters, zip with score, cap at `limit`.
        let scores: HashMap<String, f32> = candidates.into_iter().collect();
        let hits: Vec<TranscriptSearchHit> = hydrated
            .into_iter()
            .filter(|m| {
                filters
                    .session_id
                    .as_ref()
                    .is_none_or(|s| m.session_id.as_deref() == Some(s.as_str()))
            })
            .filter(|m| filters.role.is_none_or(|r| m.role == r))
            .filter(|m| filters.block_type.is_none_or(|b| m.block_type == b))
            .filter(|m| {
                filters
                    .time_from
                    .as_ref()
                    .is_none_or(|t| m.created_at.as_str() >= t.as_str())
            })
            .filter(|m| {
                filters
                    .time_to
                    .as_ref()
                    .is_none_or(|t| m.created_at.as_str() <= t.as_str())
            })
            .take(limit)
            .map(|m| {
                let score = scores.get(&m.message_block_id).copied().unwrap_or(0.0);
                TranscriptSearchHit { message: m, score }
            })
            .collect();
        Ok(hits)
    }
}
