//! Transcript-archive service façade.
//!
//! Wraps `Arc<Store>` (Lance-backed transcript reads + writes) and an
//! optional embedding provider into a single interface used by
//! `http/transcripts.rs`. Mirrors `service/capability_capsule_service.rs`
//! in shape (struct holds `Clone`/`Arc`-wrapped collaborators; `Clone` is
//! cheap so it can sit on `AppState`).
//!
//! The provider is `Option<Arc<dyn EmbeddingProvider>>` so unit/integration
//! tests can construct a service with no provider; in that mode, non-empty
//! semantic queries skip the vector-search channel (BM25 via the in-RAM
//! Tantivy index still works) and the empty-query path falls back to the
//! recent-time lance-native listing. With or without a provider, the response
//! shape is `Vec<MergedWindow>`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::embedding::EmbeddingProvider;
use crate::pipeline::transcript_recall::{
    merge_windows, score_candidates, MergedWindow, PrimaryWithContext, ScoringOpts,
};
use crate::storage::{Backend, StorageError};

/// Process-wide guard: at most one in-request force-reindex (the transcript
/// ANN self-heal, see `TranscriptService::search`) runs at a time. The rebuild
/// covers the whole process's indexes (one dataset per process), so a burst of
/// queries that all hit the stale-index ragged-batch must NOT each kick off a
/// full rebuild — the first claims the flag and rebuilds, the rest soft-degrade
/// to BM25 for that query. Reset via the RAII [`ReindexGuard`] so a cancelled
/// request can't strand the flag.
static TRANSCRIPT_ANN_REINDEX_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII reset for [`TRANSCRIPT_ANN_REINDEX_IN_FLIGHT`]. Acquire with
/// [`ReindexGuard::try_acquire`]; the flag clears on drop (normal return,
/// `?`-propagation, or future cancellation) so the self-heal can never wedge.
struct ReindexGuard;

impl ReindexGuard {
    /// `Some` iff this caller won the CAS and now owns the in-flight slot.
    fn try_acquire() -> Option<Self> {
        TRANSCRIPT_ANN_REINDEX_IN_FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| ReindexGuard)
    }
}

impl Drop for ReindexGuard {
    fn drop(&mut self) {
        TRANSCRIPT_ANN_REINDEX_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

/// Cooldown between transcript-ANN self-heal reindexes (ms). The in-flight
/// guard only stops *concurrent* rebuilds; this stops *rapid sequential* ones.
/// Without it, a failure that a reindex can't actually fix would make EVERY
/// transcript search force a full (~1s+) index rebuild — a worse storm than the
/// plain BM25 soft-degrade it replaced. In the designed case (stale index →
/// reindex fixes it) the next query hits a fresh index and never re-enters this
/// path, so the cooldown is invisible.
const TRANSCRIPT_ANN_REINDEX_COOLDOWN_MS: u64 = 300_000;

/// Last self-heal reindex attempt (ms since epoch); `0` = never.
static TRANSCRIPT_ANN_LAST_REINDEX_MS: AtomicU64 = AtomicU64::new(0);

/// Whether a self-heal reindex may run now: true (and stamps "now") iff the
/// cooldown has elapsed since the last attempt. The concurrent case is still
/// handled by [`ReindexGuard`]; this only rate-limits sequential attempts, so a
/// benign load/store race at the window boundary is acceptable.
fn transcript_ann_reindex_cooldown_elapsed() -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = TRANSCRIPT_ANN_LAST_REINDEX_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= TRANSCRIPT_ANN_REINDEX_COOLDOWN_MS {
        TRANSCRIPT_ANN_LAST_REINDEX_MS.store(now, Ordering::Relaxed);
        true
    } else {
        false
    }
}

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

impl TranscriptSearchFilters {
    /// True if any filter is set — drives the candidate-pool widening in
    /// `search` so a strong filter doesn't under-recall behind the
    /// oversample cutoff.
    pub fn is_any_set(&self) -> bool {
        self.session_id.is_some()
            || self.role.is_some()
            || self.block_type.is_some()
            || self.time_from.is_some()
            || self.time_to.is_some()
    }
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
    /// Shared storage handle. Phase 5: erased to `Arc<dyn Backend>`
    /// (umbrella supertrait over the 9 storage sub-traits, including
    /// `TranscriptStore`). Writes / BM25 / semantic search all
    /// route through the trait surface.
    store: Arc<dyn Backend>,
    /// Optional embedding provider for the **query** vector — the
    /// transcript embedding *worker* writes vectors out-of-band; this
    /// provider only embeds the search query at request time. When
    /// `None`, the semantic channel is silently skipped (BM25-only
    /// hybrid). Tests / unit fixtures use the `None` path.
    provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl TranscriptService {
    pub fn new(store: Arc<dyn Backend>, provider: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self { store, provider }
    }

    /// Inserts a single transcript block via the repository's idempotent
    /// `create_conversation_message`. Embedding job enqueueing is handled
    /// inside the repository; this method does not touch the vector index.
    pub async fn ingest(&self, msg: ConversationMessage) -> Result<(), StorageError> {
        self.store.create_conversation_message(&msg).await?;
        crate::metrics::metrics().add_transcript_ingest(1);
        Ok(())
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
        let landed = self.store.create_conversation_messages(&msgs).await?;
        crate::metrics::metrics().add_transcript_ingest(landed as u64);
        Ok(landed)
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

    /// Cross-session time-window scan. Returns every transcript block
    /// for `tenant` whose `created_at` falls in `[time_from, time_to)`
    /// (each bound optional), narrowed by `role` / `block_type` when
    /// provided. Page size capped at 1000; the same composite cursor
    /// as [`Self::get_by_session_paged`] paginates.
    ///
    /// Both bounds being `None` scans the whole tenant archive; the
    /// limit + cursor still bound a single response so callers can
    /// paginate freely.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_in_range(
        &self,
        tenant: &str,
        time_from: Option<&str>,
        time_to: Option<&str>,
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        let limit = limit.clamp(1, 1000);
        self.store
            .list_conversation_messages_in_range(
                tenant, time_from, time_to, role, block_type, cursor, limit,
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
    ///   - semantic ranks via lance vector ANN (`Store::semantic_search_transcripts`)
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
        crate::metrics::metrics().inc_transcript_search();
        let limit = limit.clamp(1, 100);
        // Oversample factor (`k = limit * factor`) is read directly from env at
        // search time — keeps the override flexible without plumbing config
        // through the service. As of QW-4 there is no config-side parser; an
        // invalid `MEM_TRANSCRIPT_OVERSAMPLE` value (non-numeric or 0) silently
        // falls back to the default 4 below. Default 4 matches historical
        // hardcoded behavior.
        let oversample_factor = std::env::var("MEM_TRANSCRIPT_OVERSAMPLE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(4);
        let oversample = limit * oversample_factor;
        // When the caller narrows by session/role/block_type/time, matching
        // rows can rank beyond the per-channel oversample cutoff and never get
        // fetched — the post-fetch filter (Phase 4) only sees what was pulled,
        // so a strong filter silently under-recalls. Widen the candidate pool
        // when any filter is set to cut that loss. (The complete fix pushes the
        // predicates into the candidate SQL/ANN query; this is the low-risk
        // mitigation that needs no signature changes across the channels.)
        let oversample = if filters.is_any_set() {
            oversample.saturating_mul(4)
        } else {
            oversample
        };
        let context_window = opts.context_window.unwrap_or(2).min(10);

        // Phase-timing breakdown for profiling (debug! — off at the default
        // info level). Surfaced that BM25 over a stale FTS index cost ~455ms
        // and query embedding ~420ms dominate the residual transcript latency.
        let t_total = Instant::now();
        let mut bm25_ms = 0u128;
        let mut embed_ms = 0u128;
        let mut sem_ms = 0u128;

        // ─── Phase 1: gather candidate ids and per-channel ranks.
        let mut lexical_ranks: HashMap<String, usize> = HashMap::new();
        let mut semantic_ranks: HashMap<String, usize> = HashMap::new();
        let mut all_ids: HashSet<String> = HashSet::new();

        if !query.trim().is_empty() {
            // BM25 channel (route-B in-RAM Tantivy index). Soft-degrade:
            // keep the request alive if the BM25 lookup errors (e.g. a
            // Tantivy rebuild race) by letting the semantic channel carry
            // rather than 500 the whole request. (Before route-B this
            // channel was a lance FTS scan that hit the stale-index
            // ragged-batch read bug; the Tantivy index removed that
            // failure mode.)
            let t = Instant::now();
            match self
                .store
                .bm25_transcript_candidates(tenant, query, oversample)
                .await
            {
                Ok(bm25_hits) => {
                    for (rank0, m) in bm25_hits.iter().enumerate() {
                        lexical_ranks.insert(m.message_block_id.clone(), rank0 + 1);
                        all_ids.insert(m.message_block_id.clone());
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "transcript BM25 search failed; serving semantic-only results for this query"
                    );
                }
            }
            bm25_ms = t.elapsed().as_millis();

            // Semantic channel — only if provider attached.
            // Routes through `Store::semantic_search_transcripts`,
            // which runs a lance-native vector ANN (`nearest_to`) over
            // `conversation_message_embeddings` and hydrates against
            // `conversation_messages` for the full row. We only need
            // the message_block_id + rank position, so we discard
            // the message body and similarity score here.
            if let Some(provider) = &self.provider {
                let t = Instant::now();
                // Soft-degrade: query embedding is the prerequisite for the
                // ANN channel, but a failure here is an infrastructure fault
                // (local model reload / OOM / tensor error), NOT a malformed
                // query — so degrade to the always-on BM25 channel rather than
                // failing the whole request with a 400 and discarding the BM25
                // hits already gathered above. Mirrors the capsule search side
                // (`unwrap_or_default()` on the same call) and the ANN
                // soft-degrade immediately below.
                let q_vec = match provider.embed_query(query).await {
                    Ok(v) => Some(v),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "transcript query embed failed; serving BM25-only results for this query"
                        );
                        None
                    }
                };
                embed_ms = t.elapsed().as_millis();
                if let Some(q_vec) = q_vec {
                    let t = Instant::now();
                    // The ANN scan can fail on lancedb 0.30 / lance 7.0 with
                    // `IO Error: ... all columns in a record batch must have the
                    // same length` — a lance-core ragged-batch bug on a STALE /
                    // partially-covering IVF index (a scan merging the indexed
                    // segment with the unindexed append-tail yields unequal-
                    // length columns; see AGENTS.md "Lance STALE-INDEX
                    // ragged-batch" + `maintenance.rs`). The larger-partition
                    // tuning only reduces it; the true fix is upstream.
                    //
                    // Self-heal: on that failure, force a full index rebuild and
                    // retry the scan ONCE — turning "silent ANN-recall loss for
                    // this channel until the next scheduled reindex (≤ the vacuum
                    // interval, default 1h)" into "one slow self-healing query".
                    // A process-wide guard caps it at one in-flight rebuild so a
                    // burst of failing queries can't stampede. If the rebuild, a
                    // concurrent rebuild, or the retry still can't serve ANN,
                    // soft-degrade to the always-on BM25 channel rather than 500.
                    let mut sem_result = self
                        .store
                        .semantic_search_transcripts(tenant, &q_vec, oversample)
                        .await;
                    if sem_result.is_err() {
                        // Self-heal only if BOTH the cooldown has elapsed (no
                        // rapid-sequential rebuild storm when a reindex can't fix
                        // the failure) AND no rebuild is already in flight.
                        if transcript_ann_reindex_cooldown_elapsed() {
                            if let Some(_guard) = ReindexGuard::try_acquire() {
                                if let Err(e) = &sem_result {
                                    warn!(
                                        error = %e,
                                        "transcript ANN search failed (stale-index ragged-batch?); force-reindexing and retrying once"
                                    );
                                }
                                match self.store.rebuild_query_indexes().await {
                                    Ok(_) => {
                                        sem_result = self
                                            .store
                                            .semantic_search_transcripts(tenant, &q_vec, oversample)
                                            .await;
                                        if sem_result.is_ok() {
                                            info!("transcript ANN recovered after force-reindex");
                                        }
                                    }
                                    Err(re) => {
                                        warn!(error = %re, "force-reindex after ANN failure failed");
                                    }
                                }
                            } else {
                                warn!(
                                    "transcript ANN search failed; a force-reindex is already in flight, serving BM25-only for this query"
                                );
                            }
                        } else {
                            warn!(
                                "transcript ANN search failed; self-heal reindex on cooldown, serving BM25-only for this query"
                            );
                        }
                    }
                    match sem_result {
                        Ok(sem_hits) => {
                            for (rank0, (msg, _sim)) in sem_hits.iter().enumerate() {
                                semantic_ranks.insert(msg.message_block_id.clone(), rank0 + 1);
                                all_ids.insert(msg.message_block_id.clone());
                            }
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                "transcript ANN search failed after self-heal; serving BM25-only results for this query"
                            );
                        }
                    }
                    sem_ms = t.elapsed().as_millis();
                }
            }
        } else {
            // Empty query: recent-time browse mode. Soft-degrade like the
            // query channels — a lance read error here returns empty windows,
            // not a 500.
            match self
                .store
                .recent_conversation_messages(tenant, oversample)
                .await
            {
                Ok(recent) => {
                    for m in recent {
                        all_ids.insert(m.message_block_id);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "transcript recent-browse failed; returning empty");
                }
            }
        }

        // Anchor session injection (independent of channel). Soft-degrade.
        if let Some(anchor) = opts.anchor_session_id.as_deref() {
            match self
                .store
                .anchor_session_candidates(tenant, anchor, oversample)
                .await
            {
                Ok(injected) => {
                    for id in injected {
                        all_ids.insert(id);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "transcript anchor-session injection failed; skipping");
                }
            }
        }

        if all_ids.is_empty() {
            return Ok(TranscriptSearchResult { windows: vec![] });
        }

        // ─── Phase 2: hydrate. Soft-degrade: this scans conversation_messages
        // via the message_block_id BTree index, which can hit the same lance
        // read bug — return empty windows rather than 500.
        let id_vec: Vec<String> = all_ids.into_iter().collect();
        let n_ids = id_vec.len();
        let t = Instant::now();
        let candidates = match self
            .store
            .fetch_conversation_messages_by_ids(tenant, &id_vec)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "transcript hydrate failed; returning empty");
                return Ok(TranscriptSearchResult { windows: vec![] });
            }
        };
        let hydrate_ms = t.elapsed().as_millis();

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

        let t = Instant::now();
        let mut items: Vec<PrimaryWithContext> = Vec::with_capacity(scored.len());
        for sb in scored {
            // Soft-degrade: a context-window scan failure drops only this
            // primary's surrounding blocks (empty before/after), never the
            // whole request.
            let (before, after) = match self
                .store
                .context_window_for_block(
                    tenant,
                    &sb.message.message_block_id,
                    context_window,
                    context_window,
                    opts.include_tool_blocks_in_context,
                )
                .await
            {
                Ok(cw) => (cw.before, cw.after),
                Err(e) => {
                    warn!(error = %e, "transcript context-window fetch failed; primary only");
                    (Vec::new(), Vec::new())
                }
            };
            items.push(PrimaryWithContext {
                primary: sb,
                before,
                after,
            });
        }
        let ctx_ms = t.elapsed().as_millis();
        let n_ctx = items.len();

        debug!(
            total_ms = t_total.elapsed().as_millis() as u64,
            bm25_ms = bm25_ms as u64,
            embed_ms = embed_ms as u64,
            sem_ms = sem_ms as u64,
            hydrate_ms = hydrate_ms as u64,
            ctx_ms = ctx_ms as u64,
            n_ids,
            n_ctx,
            oversample,
            "transcript_search phase timings",
        );

        // ─── Phase 6: merge windows.
        let mut windows = merge_windows(items);

        // ─── Phase 7 (O5): redact secrets in the search output (see
        // `redact_window_blocks`).
        redact_window_blocks(&mut windows);
        Ok(TranscriptSearchResult { windows })
    }
}

/// (O5) Mask high-confidence secrets in transcript **search output** — the
/// prompt-bound path whose blocks ride straight into an agent's context, so it
/// is the transcript analog of `compress_text` for capsules. The verbatim-fetch
/// paths (`transcripts_range` / `get_by_session`) are deliberately left
/// unmasked, like `capability_capsule_get`. Storage is never touched — only the
/// returned in-memory copy is rewritten. `redact_secrets` returns `Cow::Owned`
/// iff a pattern matched, so clean blocks pay no allocation, and the whole pass
/// is a no-op (every block `Borrowed`) when `MEM_REDACT_SECRETS_DISABLED` is set.
fn redact_window_blocks(windows: &mut [MergedWindow]) {
    for window in windows {
        for block in &mut window.blocks {
            if let std::borrow::Cow::Owned(red) =
                crate::pipeline::redact::redact_secrets(&block.content)
            {
                block.content = red;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockType, ConversationMessage, MessageRole};
    use crate::embedding::{EmbeddingError, EmbeddingProvider};
    use crate::storage::Store;
    use async_trait::async_trait;
    use tempfile::tempdir;

    /// A provider whose query embedding always fails — models a transient
    /// local-inference fault (model reload, OOM, tensor error), which is the
    /// real cause of `embed_text` errors on the embedanything path (NOT a bad
    /// query, so it must not 400 the request).
    struct AlwaysFailEmbed;

    #[async_trait]
    impl EmbeddingProvider for AlwaysFailEmbed {
        fn name(&self) -> &'static str {
            "always-fail"
        }
        fn model(&self) -> &str {
            "always-fail"
        }
        fn dim(&self) -> usize {
            64
        }
        async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
            Err(EmbeddingError::Internal("simulated embed failure".into()))
        }
    }

    fn text_block(id: &str, content: &str) -> ConversationMessage {
        ConversationMessage {
            message_block_id: id.into(),
            session_id: Some("S1".into()),
            tenant: "local".into(),
            caller_agent: "claude-code".into(),
            transcript_path: format!("/tmp/{id}.jsonl"),
            line_number: 1,
            block_index: 0,
            message_uuid: None,
            role: MessageRole::Assistant,
            block_type: BlockType::Text,
            content: content.into(),
            tool_name: None,
            tool_use_id: None,
            embed_eligible: true,
            created_at: "00000001778000000000".into(),
            meta_json: None,
        }
    }

    #[test]
    fn search_output_redacts_secrets_but_leaves_clean_blocks_o5() {
        let secret = text_block(
            "mb-secret",
            "deploy used openai key sk-FAKEabcdEFGH1234ijklMNOP then restarted",
        );
        let clean = text_block("mb-clean", "we discussed the Rust project layout");
        let mut windows = vec![MergedWindow {
            session_id: Some("S1".into()),
            blocks: vec![secret, clean],
            primary_ids: vec!["mb-secret".into()],
            primary_scores: HashMap::new(),
            score: 0,
        }];

        redact_window_blocks(&mut windows);

        let blocks = &windows[0].blocks;
        assert!(
            blocks[0].content.contains("[redacted:sk]"),
            "secret must be masked in search output: {}",
            blocks[0].content
        );
        assert!(
            !blocks[0].content.contains("sk-FAKEabcd"),
            "key leaked into search output: {}",
            blocks[0].content
        );
        // A clean block is left byte-for-byte verbatim (no spurious rewrite).
        assert_eq!(blocks[1].content, "we discussed the Rust project layout");
    }

    #[test]
    fn reindex_guard_is_mutually_exclusive_and_resets_on_drop() {
        // First claim wins.
        let g1 = ReindexGuard::try_acquire();
        assert!(g1.is_some(), "first acquire must win");
        // While held, a second claim is refused (no stampede of rebuilds).
        assert!(
            ReindexGuard::try_acquire().is_none(),
            "second acquire must be refused while the first is held"
        );
        // Dropping the holder frees the slot (cancellation-safe self-heal).
        drop(g1);
        let g2 = ReindexGuard::try_acquire();
        assert!(g2.is_some(), "acquire must succeed again after drop");
        drop(g2);
    }

    #[test]
    fn reindex_cooldown_rate_limits_sequential_attempts() {
        // First attempt (last = 0, far in the past) is allowed and stamps "now";
        // an immediate second attempt is refused — so a persistent failure can't
        // trigger a full reindex on every query. Only this test touches the
        // cooldown static, so the sequence is deterministic.
        assert!(
            transcript_ann_reindex_cooldown_elapsed(),
            "first self-heal attempt must be allowed"
        );
        assert!(
            !transcript_ann_reindex_cooldown_elapsed(),
            "an immediate second attempt must be rate-limited by the cooldown"
        );
    }

    /// MED-bug regression: when the query fails to embed (an infrastructure
    /// fault, not a client error), transcript search must DEGRADE to the
    /// always-on BM25 channel — not abort the whole request — so the lexical
    /// hits already gathered are still served. Mirrors the capsule search
    /// side, which uses `unwrap_or_default()` on the same call.
    #[tokio::test]
    async fn search_degrades_to_bm25_when_query_embed_fails() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("mem.duckdb");
        let store = Arc::new(Store::open(&db).await.unwrap());
        store.set_transcript_job_provider("always-fail");

        let svc = TranscriptService::new(store.clone(), Some(Arc::new(AlwaysFailEmbed)));
        svc.ingest(text_block("mb-a", "we discussed the Rust project layout"))
            .await
            .unwrap();

        let result = svc
            .search(
                "local",
                "Rust",
                &TranscriptSearchFilters::default(),
                5,
                &TranscriptSearchOpts::default(),
            )
            .await
            .expect("query-embed failure must degrade to BM25, not error the whole request");

        assert!(
            !result.windows.is_empty(),
            "BM25 channel should still surface the lexical 'Rust' hit despite the embed failure"
        );
    }
}
