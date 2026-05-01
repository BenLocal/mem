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
//! semantic queries skip the HNSW channel (BM25 still works) and the empty-
//! query path falls back to the recent-time SQL listing. With or without a
//! provider, the response shape is `Vec<MergedWindow>`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::embedding::EmbeddingProvider;
use crate::pipeline::transcript_recall::{
    merge_windows, score_candidates, MergedWindow, PrimaryWithContext, ScoringOpts,
};
use crate::storage::{ContextWindow, DuckDbRepository, StorageError, VectorIndex};

/// Optional filters layered on top of the candidate set returned by
/// scoring. All fields are AND-ed.
///
/// `time_from` / `time_to` are matched lexicographically against
/// `created_at` — fine for ISO-8601 / RFC-3339 strings, the only format
/// produced by the ingest pipeline.
#[derive(Debug, Clone, Default)]
pub struct TranscriptSearchFilters {
    pub session_id: Option<String>,
    pub role: Option<MessageRole>,
    pub block_type: Option<BlockType>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
}

/// Optional, request-scoped recall tuning.
#[derive(Debug, Clone, Default)]
pub struct TranscriptSearchOpts {
    pub anchor_session_id: Option<String>,
    /// ±N blocks of context around each primary. None → 2 (default).
    /// Capped at 10 by the service.
    pub context_window: Option<usize>,
    pub include_tool_blocks_in_context: bool,
}

/// Result of `TranscriptService::search` — a list of merged conversation
/// windows, each containing one or more primary hits and their context.
#[derive(Debug, Clone)]
pub struct TranscriptSearchResult {
    pub windows: Vec<MergedWindow>,
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

    /// Three-channel hybrid recall:
    ///   - HNSW (semantic) ranks via `Arc<VectorIndex>`
    ///   - BM25 (lexical) ranks via `repo.bm25_transcript_candidates`
    ///   - Optional anchor-session injection (no rank; bonus only)
    ///
    /// then `transcript_recall::score_candidates` + filter + hydrate +
    /// `transcript_recall::merge_windows`.
    ///
    /// Empty query path: candidates come from `recent_conversation_messages`,
    /// all scored 0; same hydrate + merge applies → response shape stays
    /// consistent (`Vec<MergedWindow>`).
    ///
    /// `limit` is capped to 100 in this layer (window merge is O(N²) in
    /// primaries; anything more should use `GET /transcripts?session_id=…`).
    pub async fn search(
        &self,
        tenant: &str,
        query: &str,
        filters: &TranscriptSearchFilters,
        limit: usize,
        opts: &TranscriptSearchOpts,
    ) -> Result<TranscriptSearchResult, StorageError> {
        let limit = limit.clamp(1, 100);
        let oversample = limit * 4;
        let context_window = opts.context_window.unwrap_or(2).min(10);

        // ─── Phase 1: gather candidate ids and per-channel ranks.
        let mut lexical_ranks: HashMap<String, usize> = HashMap::new();
        let mut semantic_ranks: HashMap<String, usize> = HashMap::new();
        let mut all_ids: HashSet<String> = HashSet::new();

        if !query.trim().is_empty() {
            // BM25 channel — always available.
            let bm25_hits = self
                .repo
                .bm25_transcript_candidates(tenant, query, oversample)
                .await?;
            for (rank0, m) in bm25_hits.iter().enumerate() {
                lexical_ranks.insert(m.message_block_id.clone(), rank0 + 1);
                all_ids.insert(m.message_block_id.clone());
            }

            // HNSW channel — only if provider attached.
            if let Some(provider) = &self.provider {
                let q_vec = provider
                    .embed_text(query)
                    .await
                    .map_err(|e| StorageError::InvalidInput(format!("query embed failed: {e}")))?;
                let sem_hits = self
                    .index
                    .search(&q_vec, oversample)
                    .await
                    .map_err(|e| StorageError::VectorIndex(e.to_string()))?;
                for (rank0, (id, _score)) in sem_hits.iter().enumerate() {
                    semantic_ranks.insert(id.clone(), rank0 + 1);
                    all_ids.insert(id.clone());
                }
            }
        } else {
            // Empty query: recent-time browse mode.
            let recent = self
                .repo
                .recent_conversation_messages(tenant, oversample)
                .await?;
            for m in recent {
                all_ids.insert(m.message_block_id);
            }
        }

        // Anchor session injection (independent of channel).
        if let Some(anchor) = opts.anchor_session_id.as_deref() {
            let injected = self
                .repo
                .anchor_session_candidates(tenant, anchor, oversample)
                .await?;
            for id in injected {
                all_ids.insert(id);
            }
        }

        if all_ids.is_empty() {
            return Ok(TranscriptSearchResult { windows: vec![] });
        }

        // ─── Phase 2: hydrate.
        let id_vec: Vec<String> = all_ids.into_iter().collect();
        let candidates = self
            .repo
            .fetch_conversation_messages_by_ids(tenant, &id_vec)
            .await?;

        // ─── Phase 3: score.
        let scoring_opts = ScoringOpts {
            anchor_session_id: opts.anchor_session_id.as_deref(),
        };
        let mut scored =
            score_candidates(candidates, &lexical_ranks, &semantic_ranks, scoring_opts);

        // ─── Phase 4: filter.
        scored.retain(|sb| {
            let m = &sb.message;
            filters
                .session_id
                .as_ref()
                .is_none_or(|s| m.session_id.as_deref() == Some(s.as_str()))
                && filters.role.is_none_or(|r| m.role == r)
                && filters.block_type.is_none_or(|b| m.block_type == b)
                && filters
                    .time_from
                    .as_ref()
                    .is_none_or(|t| m.created_at.as_str() >= t.as_str())
                && filters
                    .time_to
                    .as_ref()
                    .is_none_or(|t| m.created_at.as_str() <= t.as_str())
        });

        // ─── Phase 5: take top-`limit` as primaries; hydrate context.
        scored.truncate(limit);

        let mut items: Vec<PrimaryWithContext> = Vec::with_capacity(scored.len());
        for sb in scored {
            let cw: ContextWindow = self
                .repo
                .context_window_for_block(
                    tenant,
                    &sb.message.message_block_id,
                    context_window,
                    context_window,
                    opts.include_tool_blocks_in_context,
                )
                .await?;
            items.push(PrimaryWithContext {
                primary: sb,
                before: cw.before,
                after: cw.after,
            });
        }

        // ─── Phase 6: merge windows.
        let windows = merge_windows(items);
        Ok(TranscriptSearchResult { windows })
    }
}
