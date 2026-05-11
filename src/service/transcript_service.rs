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
use crate::storage::{ContextWindow, StorageError, Store};

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

/// A single recent session's metadata + N freshest embed_eligible
/// blocks. Returned by [`TranscriptService::recent_for_wake_up`];
/// the caller (capsule service wake-up branch) compresses each
/// highlight's text under a token budget before exposing it on
/// `SearchCapabilityCapsuleResponse.recent_conversations`.
#[derive(Debug, Clone)]
pub struct RecentSession {
    pub session_id: String,
    pub last_at: String,
    pub block_count: i64,
    pub caller_agent: Option<String>,
    /// Newest-first (the order produced by
    /// `Store::recent_conversation_messages`). Capped per call.
    pub highlights: Vec<ConversationMessage>,
}

#[derive(Clone)]
pub struct TranscriptService {
    /// Shared storage handle. Writes flow to LanceStore (incl.
    /// `transcript_embedding_jobs` enqueue inside
    /// `create_conversation_message`); reads (BM25 + semantic) flow
    /// to DuckDbQuery.
    store: Arc<Store>,
    /// Optional embedding provider for the **query** vector — the
    /// transcript embedding *worker* writes vectors out-of-band; this
    /// provider only embeds the search query at request time. When
    /// `None`, the semantic channel is silently skipped (BM25-only
    /// hybrid). Tests / unit fixtures use the `None` path.
    provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl TranscriptService {
    pub fn new(store: Arc<Store>, provider: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self { store, provider }
    }

    /// Inserts a single transcript block via the repository's idempotent
    /// `create_conversation_message`. Embedding job enqueueing is handled
    /// inside the repository; this method does not touch the HNSW index.
    pub async fn ingest(&self, msg: ConversationMessage) -> Result<(), StorageError> {
        self.store.create_conversation_message(&msg).await
    }

    /// Bulk ingest of transcript blocks. Idempotent on
    /// `(transcript_path, line_number, block_index)` like the single-row
    /// form, but dedup + write + embedding-job enqueue all happen in
    /// one Lance round-trip per table — so total cost is independent of
    /// `msgs.len()`. Returns the number of rows that actually landed
    /// (input length minus dedup-skipped rows).
    pub async fn ingest_batch(
        &self,
        msgs: Vec<ConversationMessage>,
    ) -> Result<usize, StorageError> {
        if msgs.is_empty() {
            return Ok(0);
        }
        self.store.create_conversation_messages(&msgs).await
    }

    /// Per-session aggregate summary of the transcript archive. Backs the
    /// admin web page's transcripts list (one row per session, newest last
    /// activity first).
    pub async fn list_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<crate::storage::TranscriptSessionSummary>, StorageError> {
        self.store.list_transcript_sessions(tenant).await
    }

    /// Returns every transcript block belonging to `session_id` in
    /// chronological order. Thin pass-through over the repo method; exists
    /// here so HTTP handlers depend on the service rather than the repo.
    pub async fn get_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.store
            .get_conversation_messages_by_session(tenant, session_id)
            .await
    }

    /// Paginated `get_by_session`. Page size capped at 1000 to bound memory
    /// per request; the admin UI defaults to 200 and scrolls. `since`/`until`
    /// are 20-digit millisecond strings (same encoding as
    /// `current_timestamp`); a wrong-format value is passed through and
    /// simply matches nothing. `role` ∈ {user, assistant, system} and
    /// `block_type` ∈ {text, tool_use, tool_result, thinking} narrow
    /// the page when provided.
    #[allow(clippy::too_many_arguments)]
    pub async fn get_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        let limit = limit.clamp(1, 1000);
        self.store
            .get_conversation_messages_by_session_paged(
                tenant, session_id, since, until, role, block_type, cursor, limit,
            )
            .await
    }

    /// Wake-up enrichment: pick the N most recently active transcript
    /// sessions for `tenant`, hydrate up to `blocks_per_session` of
    /// each session's freshest embed_eligible blocks (text / thinking
    /// only — agents don't want tool_use / tool_result noise on
    /// session boot), and return a flat list of (session_id, highlights)
    /// pairs. Caller layers `compress_text` over each highlight to
    /// honor the wake-up token budget.
    ///
    /// Sessions are ordered by `last_at DESC` (newest activity first).
    /// Session metadata (block_count, caller_agent) comes from the
    /// existing `list_transcript_sessions` aggregate; highlights are
    /// pulled per-session via `recent_conversation_messages` filtered
    /// to that session id and embed_eligible.
    ///
    /// Empty result is returned when the tenant has no transcript
    /// activity yet — handler must treat as "no recent conversations
    /// to surface" and skip rendering the section.
    pub async fn recent_for_wake_up(
        &self,
        tenant: &str,
        max_sessions: usize,
        blocks_per_session: usize,
    ) -> Result<Vec<RecentSession>, StorageError> {
        let max_sessions = max_sessions.clamp(1, 10);
        let blocks_per_session = blocks_per_session.clamp(1, 10);

        let sessions = self.store.list_transcript_sessions(tenant).await?;
        let mut out = Vec::with_capacity(max_sessions.min(sessions.len()));
        for session in sessions.into_iter().take(max_sessions) {
            // Filter the recent feed by this session_id. The current
            // recent_conversation_messages is tenant-scoped only, so
            // we widen its limit and drop non-matching rows. For typical
            // sessions of 100s–1000s of blocks this is acceptable; if it
            // becomes hot, push the predicate into SQL via a new repo
            // method.
            let recent_widened = blocks_per_session.saturating_mul(20).max(50);
            let blocks = self
                .store
                .recent_conversation_messages(tenant, recent_widened)
                .await?;
            let highlights: Vec<ConversationMessage> = blocks
                .into_iter()
                .filter(|m| {
                    m.session_id.as_deref() == Some(session.session_id.as_str())
                        && m.embed_eligible
                        && matches!(m.block_type, BlockType::Text | BlockType::Thinking)
                })
                .take(blocks_per_session)
                .collect();
            out.push(RecentSession {
                session_id: session.session_id,
                last_at: session.last_at,
                block_count: session.block_count,
                caller_agent: session.caller_agent,
                highlights,
            });
        }
        Ok(out)
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
    /// primaries; anything more should use `POST /transcripts {session_id}`).
    pub async fn search(
        &self,
        tenant: &str,
        query: &str,
        filters: &TranscriptSearchFilters,
        limit: usize,
        opts: &TranscriptSearchOpts,
    ) -> Result<TranscriptSearchResult, StorageError> {
        let limit = limit.clamp(1, 100);
        // Oversample factor (`k = limit * factor`) is read directly from env at
        // search time, mirroring the `MEM_VECTOR_INDEX_OVERSAMPLE` pattern in
        // `DuckDbRepository::semantic_search_capability_capsules` (Option B): keeps the
        // override flexible without plumbing config through the service. The
        // parser in `config.rs` rejects 0 for `MEM_TRANSCRIPT_OVERSAMPLE`, but
        // since this read bypasses the parser we apply the same `> 0` filter
        // here defensively. Default 4 matches historical hardcoded behavior.
        let oversample_factor = std::env::var("MEM_TRANSCRIPT_OVERSAMPLE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(4);
        let oversample = limit * oversample_factor;
        let context_window = opts.context_window.unwrap_or(2).min(10);

        // ─── Phase 1: gather candidate ids and per-channel ranks.
        let mut lexical_ranks: HashMap<String, usize> = HashMap::new();
        let mut semantic_ranks: HashMap<String, usize> = HashMap::new();
        let mut all_ids: HashSet<String> = HashSet::new();

        if !query.trim().is_empty() {
            // BM25 channel — always available.
            let bm25_hits = self
                .store
                .bm25_transcript_candidates(tenant, query, oversample)
                .await?;
            for (rank0, m) in bm25_hits.iter().enumerate() {
                lexical_ranks.insert(m.message_block_id.clone(), rank0 + 1);
                all_ids.insert(m.message_block_id.clone());
            }

            // Semantic channel — only if provider attached.
            // Routes through `Store::semantic_search_transcripts`,
            // which runs `lance_vector_search` against
            // `conversation_message_embeddings` and JOINs back to
            // `conversation_messages` for the full row. We only need
            // the message_block_id + rank position, so we discard
            // the message body and similarity score here.
            if let Some(provider) = &self.provider {
                let q_vec = provider
                    .embed_text(query)
                    .await
                    .map_err(|e| StorageError::InvalidInput(format!("query embed failed: {e}")))?;
                let sem_hits = self
                    .store
                    .semantic_search_transcripts(tenant, &q_vec, oversample)
                    .await?;
                for (rank0, (msg, _sim)) in sem_hits.iter().enumerate() {
                    semantic_ranks.insert(msg.message_block_id.clone(), rank0 + 1);
                    all_ids.insert(msg.message_block_id.clone());
                }
            }
        } else {
            // Empty query: recent-time browse mode.
            let recent = self
                .store
                .recent_conversation_messages(tenant, oversample)
                .await?;
            for m in recent {
                all_ids.insert(m.message_block_id);
            }
        }

        // Anchor session injection (independent of channel).
        if let Some(anchor) = opts.anchor_session_id.as_deref() {
            let injected = self
                .store
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
            .store
            .fetch_conversation_messages_by_ids(tenant, &id_vec)
            .await?;

        // ─── Phase 3: score.
        let scoring_opts = ScoringOpts {
            anchor_session_id: opts.anchor_session_id.as_deref(),
            ..ScoringOpts::default()
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
                .store
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
