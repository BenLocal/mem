use std::sync::Arc;

use crate::config::EmbeddingSettings;
use crate::embedding::EmbeddingProvider;
use crate::service::embedding_helpers::{failure_backoff_ms, truncate_error};
use crate::storage::{current_timestamp, timestamp_add_ms, Backend, StorageError};
use tracing::{error, info, warn};

pub async fn run(
    store: Arc<dyn Backend>,
    provider: Arc<dyn EmbeddingProvider>,
    settings: EmbeddingSettings,
) {
    info!(
        provider = provider.name(),
        model = provider.model(),
        dim = provider.dim(),
        poll_interval_ms = settings.worker_poll_interval_ms,
        "embedding worker started"
    );
    // ⑬b: MEM_INGEST_LINK_THRESHOLD only validates ∈ [0,1] — a value at
    // or above the near-dup floor silently empties the H1 band while
    // both lanes are on. Loud misconfiguration beats zero edges.
    if settings.ingest_link_enabled
        && settings.neardup_enabled
        && settings.ingest_link_threshold >= settings.neardup_threshold
    {
        warn!(
            link_threshold = settings.ingest_link_threshold,
            neardup_threshold = settings.neardup_threshold,
            "H1 ingest-link band is EMPTY (link_threshold >= neardup_threshold) — no related_to edges will be minted"
        );
    }
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        settings.worker_poll_interval_ms.max(1),
    ));
    loop {
        interval.tick().await;
        if let Err(err) = tick(&*store, provider.as_ref(), &settings).await {
            error!(error = %err, "embedding worker tick failed");
        }
    }
}

pub async fn tick(
    store: &dyn Backend,
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
) -> Result<(), StorageError> {
    let n = settings.batch_size.max(1);
    let now = current_timestamp();

    let jobs = store
        .claim_next_n_embedding_jobs(&now, settings.max_retries, n)
        .await?;
    if jobs.is_empty() {
        return Ok(());
    }

    // Pre-embed phase: per-job validation. Filters out jobs we resolve
    // inline (wrong provider, missing parent memory, stale content_hash).
    let mut ready: Vec<PendingEmbed> = Vec::with_capacity(jobs.len());
    for job in jobs {
        info!(
            job_id = %job.job_id,
            tenant = %job.tenant,
            capability_capsule_id = %job.capability_capsule_id,
            attempt = job.attempt_count,
            "embedding worker claimed job"
        );
        match pre_embed(store, &job, settings).await? {
            Some(texts) => ready.push(PendingEmbed { job, texts }),
            None => continue,
        }
    }
    if ready.is_empty() {
        return Ok(());
    }

    // Batch embed across ALL chunks of ALL jobs in one provider call.
    // Flatten each job's chunk texts into one batch, remembering each
    // job's [start, start+len) slice so results regroup per job. (③: a
    // long capsule contributes several chunk-rows; a short one keeps its
    // single row.) embed_batch returns Vec<Result<Vec<f32>>> of the same
    // length as inputs; whole-batch failure → per-job reschedule.
    let mut flat: Vec<&str> = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(ready.len());
    for p in &ready {
        let start = flat.len();
        flat.extend(p.texts.iter().map(|s| s.as_str()));
        ranges.push((start, p.texts.len()));
    }

    let results = match provider.embed_batch(&flat).await {
        Ok(v) => v,
        Err(err) => {
            warn!(error = %err, batch = flat.len(), "embedding worker whole-batch failure; rescheduling each");
            for p in &ready {
                record_failure(store, &p.job, settings, &err.to_string()).await?;
            }
            return Ok(());
        }
    };
    if results.len() != flat.len() {
        // Defensive: trait contract says "same length"; if a provider
        // breaks it, treat as whole-batch failure to avoid index
        // misalignment.
        warn!(
            expected = flat.len(),
            got = results.len(),
            "embedding provider returned wrong batch length"
        );
        for p in &ready {
            record_failure(store, &p.job, settings, "provider batch length mismatch").await?;
        }
        return Ok(());
    }

    // Post-embed phase. Regroup results per job; a job succeeds only if
    // every one of its chunks embedded (any chunk error reschedules the
    // whole job, so a capsule never persists a partial chunk set).
    for (p, (start, len)) in ready.iter().zip(ranges) {
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(len);
        let mut chunk_err: Option<String> = None;
        for result in &results[start..start + len] {
            match result {
                Ok(embedding) => vectors.push(embedding.clone()),
                Err(err) => {
                    chunk_err = Some(err.to_string());
                    break;
                }
            }
        }
        match chunk_err {
            Some(err) => record_failure(store, &p.job, settings, &err).await?,
            None => post_embed(store, &p.job, &vectors, provider, settings).await?,
        }
    }
    Ok(())
}

struct PendingEmbed {
    job: crate::storage::ClaimedEmbeddingJob,
    texts: Vec<String>,
}

/// Per-job pre-embed validation. Returns the per-chunk embed inputs on
/// success (one element for short content, several for long — see
/// `embed_input_chunks`); `None` if the job was resolved inline.
async fn pre_embed(
    store: &dyn Backend,
    job: &crate::storage::ClaimedEmbeddingJob,
    settings: &EmbeddingSettings,
) -> Result<Option<Vec<String>>, StorageError> {
    if job.provider != settings.job_provider_id() {
        let now = current_timestamp();
        store
            .permanently_fail_embedding_job(
                &job.job_id,
                job.attempt_count + 1,
                "embedding job provider does not match runtime configuration",
                &now,
            )
            .await?;
        return Ok(None);
    }

    let Some(memory) = store
        .get_capability_capsule_for_tenant(&job.tenant, &job.capability_capsule_id)
        .await?
    else {
        let now = current_timestamp();
        store
            .permanently_fail_embedding_job(
                &job.job_id,
                job.attempt_count + 1,
                "memory row missing for embedding job",
                &now,
            )
            .await?;
        return Ok(None);
    };

    if memory.content_hash != job.target_content_hash {
        let now = current_timestamp();
        store.mark_embedding_job_stale(&job.job_id, &now).await?;
        return Ok(None);
    }

    Ok(Some(embed_input_chunks(&memory.summary, &memory.content)))
}

/// Per-job post-embed finalization: dim-check every chunk vector,
/// re-fetch memory, upsert N embedding rows (one per chunk — lance
/// handles vector indexing internally, no separate sidecar), complete
/// the job. `embeddings` holds one vector per chunk from `pre_embed`.
async fn post_embed(
    store: &dyn Backend,
    job: &crate::storage::ClaimedEmbeddingJob,
    embeddings: &[Vec<f32>],
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
) -> Result<(), StorageError> {
    for embedding in embeddings {
        if embedding.len() != provider.dim() {
            record_failure(
                store,
                job,
                settings,
                &format!(
                    "provider returned length {} (expected {})",
                    embedding.len(),
                    provider.dim()
                ),
            )
            .await?;
            return Ok(());
        }
    }

    let Some(memory_after) = store
        .get_capability_capsule_for_tenant(&job.tenant, &job.capability_capsule_id)
        .await?
    else {
        let now = current_timestamp();
        store
            .permanently_fail_embedding_job(
                &job.job_id,
                job.attempt_count + 1,
                "memory disappeared after embedding",
                &now,
            )
            .await?;
        return Ok(());
    };

    if memory_after.content_hash != job.target_content_hash {
        let now = current_timestamp();
        store.mark_embedding_job_stale(&job.job_id, &now).await?;
        return Ok(());
    }

    if store
        .get_embedding_job_status(&job.job_id)
        .await?
        .as_deref()
        != Some("processing")
    {
        return Ok(());
    }

    let now = current_timestamp();
    store
        .upsert_capability_capsule_embedding_chunks(
            &job.capability_capsule_id,
            &job.tenant,
            provider.model(),
            provider.dim() as i64,
            embeddings,
            &job.target_content_hash,
            &memory_after.updated_at,
            &now,
        )
        .await?;

    // The embedding is just added to the capability_capsule_embeddings
    // table; the ANN (IVF_PQ) index is maintained out-of-band by the
    // vacuum worker (`ensure_query_indexes`), so there's no separate
    // vector sidecar to update here. (The legacy DuckDB-as-storage backend
    // maintained a usearch index manually at this point; that whole code
    // path is gone in the route-B architecture.)

    store.complete_embedding_job(&job.job_id, &now).await?;
    info!(
        job_id = %job.job_id,
        capability_capsule_id = %job.capability_capsule_id,
        "embedding worker completed job"
    );

    // O2 + O7(a) (closes oss-memory-diff O2; O7 lane a): write-time
    // near-duplicate review flagging. The capsule's embedding now exists
    // (just upserted), so we can find its near-dup cluster and propose a
    // supersede toward the cluster canonical without re-embedding. Only an
    // `Active` capsule is eligible — a Pending / Provisional / Archived
    // row is left alone. Best-effort: an error here is logged but never
    // fails the (already completed) embedding job. `embeddings[0]` is the
    // summary+head chunk — the representative vector for a multi-chunk
    // capsule.
    if settings.neardup_enabled
        && memory_after.status == crate::domain::capability_capsule::CapabilityCapsuleStatus::Active
        && !embeddings.is_empty()
    {
        if let Err(e) = flag_if_near_duplicate(
            store,
            &job.tenant,
            &job.capability_capsule_id,
            &embeddings[0],
            provider.model(),
            settings.neardup_threshold,
        )
        .await
        {
            warn!(
                error = %e,
                capability_capsule_id = %job.capability_capsule_id,
                "O2 near-dup check failed (capsule still active)"
            );
        }
    }

    // H1 (oss-memory-diff §9): ingest-time neighbor linking. Same
    // eligibility as the near-dup lane (fresh Active capsule, vector
    // just landed), same best-effort posture. Runs AFTER the near-dup
    // check on purpose: the [link, neardup) band never overlaps the
    // supersede lane, and a capsule the O2 lane just flipped to
    // PendingConfirmation still gets its connectivity (edges are
    // status-agnostic).
    if settings.ingest_link_enabled
        && memory_after.status == crate::domain::capability_capsule::CapabilityCapsuleStatus::Active
        && !embeddings.is_empty()
    {
        if let Err(e) = link_related_neighbors(
            store,
            &job.tenant,
            &job.capability_capsule_id,
            &embeddings[0],
            provider.model(),
            settings.ingest_link_threshold,
            ingest_link_upper(settings.neardup_enabled, settings.neardup_threshold),
        )
        .await
        {
            warn!(
                error = %e,
                capability_capsule_id = %job.capability_capsule_id,
                "H1 ingest-link failed (embedding job already completed)"
            );
        }
    }
    Ok(())
}

/// O2 + O7(a): find the new capsule's near-duplicate *cluster* (every other
/// active capsule with cosine ≥ `threshold`), pick the cluster **canonical**
/// (longest content, tie → earlier — the keep-longest rule evolution merge
/// uses), and if one exists flip `capsule_id` to `PendingConfirmation` and
/// record a `suspected_supersede` graph edge (new → canonical) for human /
/// agent review. No-op when nothing clears the bar. Runs off the ingest path
/// in the embedding worker.
///
/// O7(a) generalizes O2: O2 pointed the proposal at the single cosine-nearest
/// neighbor, which can be a short fragment; O7(a) points it at the most
/// complete member of the near-dup cluster, so the reviewer sees the right
/// merge target. Degenerates to O2 when only one near-dup exists. Verbatim-safe:
/// proposes (PendingConfirmation + edge) only — never auto-archives, unlike the
/// evolution merge it borrows the canonical rule from.
async fn flag_if_near_duplicate(
    store: &dyn Backend,
    tenant: &str,
    capsule_id: &str,
    vector: &[f32],
    model: &str,
    threshold: f32,
) -> Result<(), StorageError> {
    let Some((canonical_id, cosine)) =
        cluster_canonical_near_duplicate(store, tenant, capsule_id, vector, model, threshold)
            .await?
    else {
        return Ok(());
    };

    // Active → PendingConfirmation. The caller pre-checked Active; the
    // setter is unconditional (a concurrent status change is rare and
    // the flag is advisory, not load-bearing).
    store
        .set_capsule_status(
            tenant,
            capsule_id,
            crate::domain::capability_capsule::CapabilityCapsuleStatus::PendingConfirmation,
        )
        .await?;
    // The status flip is the load-bearing part of the proposal — count it here
    // (the edge write below is best-effort).
    crate::metrics::metrics().inc_neardup_flag();

    // Record the suspected supersede as a graph edge — a memory→memory
    // pointer in the same family as `supersedes` / `contradicts`, which
    // review tooling and graph queries can read. Edge write is
    // best-effort (the status flip is the load-bearing part).
    let now = current_timestamp();
    let edge = crate::domain::capability_capsule::GraphEdge {
        from_node_id: crate::pipeline::ingest::memory_node_id(capsule_id),
        to_node_id: crate::pipeline::ingest::memory_node_id(&canonical_id),
        relation: "suspected_supersede".to_string(),
        valid_from: now,
        valid_to: None,
        confidence: Some(cosine),
        extractor: Some("o7_neardup_cluster".to_string()),
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    };
    if let Err(e) = store.add_edge_direct(&edge).await {
        warn!(error = %e, "O7(a): suspected_supersede edge write failed");
    }
    info!(
        capability_capsule_id = %capsule_id,
        canonical = %canonical_id,
        cosine = cosine,
        "O7(a): flagged near-duplicate for review (cluster canonical)"
    );
    Ok(())
}

/// H1 (oss-memory-diff §9) per-capsule link budget — keeps the graph
/// sparse; only the top-K in-band neighbors by cosine get an edge.
const INGEST_LINK_MAX_EDGES: usize = 4;

/// H1 band upper bound (audit 2026-07-03 ⑬): [`neardup_threshold`,
/// 1.0] belongs to the O2/O7(a) supersede lane ONLY while that lane is
/// enabled. With near-dup flagging off, nothing owns the top band —
/// capping there left a capsule's strongest semantic neighbors (cos ≥
/// 0.92) with neither a link nor a supersede proposal, invisible to
/// the G2 graph channel.
pub(crate) fn ingest_link_upper(neardup_enabled: bool, neardup_threshold: f32) -> f32 {
    if neardup_enabled {
        neardup_threshold
    } else {
        f32::INFINITY
    }
}

/// H1 (oss-memory-diff §9): zero-LLM slice of A-Mem's link network.
/// After embedding a fresh `Active` capsule, connect it to its semantic
/// neighborhood with `related_to` edges (`extractor="ingest_link"`,
/// cosine riding as edge confidence). Band is [link_threshold,
/// link_upper), where the upper bound is [`ingest_link_upper`]: the
/// near-dup floor while the O2/O7(a) supersede lane is on (that lane
/// owns the top band), open otherwise (audit ⑬). Neighbors whose
/// stored vector was minted by another model are skipped (audit ⑭).
/// Pure
/// connectivity — no status flips, no content writes; the edges feed
/// O4's graph boost immediately and pre-build density for a future G2
/// third retrieval channel. Best-effort like the O2 lane: errors are
/// logged, never fail the completed embedding job. `pub` so parity
/// benches (and operators) can build production-shaped link graphs on
/// corpora that bypass the embedding worker.
pub async fn link_related_neighbors(
    store: &dyn Backend,
    tenant: &str,
    capsule_id: &str,
    vector: &[f32],
    model: &str,
    link_threshold: f32,
    link_upper: f32,
) -> Result<(), StorageError> {
    const K: usize = 12;
    let candidates = store.hybrid_candidates(tenant, "", vector, K).await?;
    let mut in_band: Vec<(String, f32)> = Vec::new();
    for (cand, _rrf) in candidates {
        if cand.capability_capsule_id == capsule_id {
            continue;
        }
        // ⑭ (audit 2026-07-03): only trust vectors minted by the CURRENT
        // model — mid-migration after a same-dim model switch, a
        // not-yet-re-embedded neighbor's cosine is meaningless and would
        // mint a wrong-confidence edge that persists into ranking.
        // (Cross-dim already degrades: `cosine` returns 0 on length
        // mismatch.)
        match store
            .get_capability_capsule_embedding_row(&cand.capability_capsule_id)
            .await?
        {
            Some((cand_model, _, _)) if cand_model == model => {}
            _ => continue,
        }
        let Some(cand_vec) = store
            .get_capability_capsule_embedding_vector(&cand.capability_capsule_id)
            .await?
        else {
            continue;
        };
        let c = cosine(vector, &cand_vec);
        if c >= link_threshold && c < link_upper {
            in_band.push((cand.capability_capsule_id.clone(), c));
        }
    }
    in_band.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let now = current_timestamp();
    for (neighbor, c) in in_band.into_iter().take(INGEST_LINK_MAX_EDGES) {
        let edge = crate::domain::capability_capsule::GraphEdge {
            from_node_id: crate::pipeline::ingest::memory_node_id(capsule_id),
            to_node_id: crate::pipeline::ingest::memory_node_id(&neighbor),
            relation: "related_to".to_string(),
            valid_from: now.clone(),
            valid_to: None,
            confidence: Some(c),
            extractor: Some("ingest_link".to_string()),
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        };
        if let Err(e) = store.add_edge_direct(&edge).await {
            warn!(error = %e, neighbor = %neighbor, "H1: related_to edge write failed");
        }
    }
    Ok(())
}

/// A near-duplicate of the new capsule: an existing active capsule whose
/// cosine to the new vector cleared `threshold`. `content_len` / `created_at`
/// drive the cluster-canonical pick (O7(a)).
struct NearDup {
    id: String,
    cosine: f32,
    content_len: usize,
    created_at: String,
}

/// O7(a): among the new capsule's near-duplicates (each already ≥ threshold),
/// pick the cluster **canonical** — longest content, tie-broken by *earlier*
/// `created_at` — mirroring the keep-longest rule `evolution_worker::execute_merge`
/// uses, so the supersede proposal points the reviewer at the most complete
/// member of the cluster rather than the merely cosine-nearest one. Returns
/// `(canonical_id, cosine_of_new_to_canonical)`. Pure → unit-testable without a
/// store.
fn pick_cluster_canonical(neardups: &[NearDup]) -> Option<(String, f32)> {
    neardups
        .iter()
        .max_by(|a, b| {
            a.content_len
                .cmp(&b.content_len)
                .then_with(|| b.created_at.cmp(&a.created_at))
        })
        .map(|c| (c.id.clone(), c.cosine))
}

/// Vector-search the top-K nearest capsules (empty query text → the
/// vector-only branch of `hybrid_candidates`), skip self, compute exact cosine
/// against each candidate's stored vector, keep every candidate at or above
/// `threshold` (the near-dup cluster), then return the cluster canonical via
/// [`pick_cluster_canonical`]. `None` when nothing clears the bar.
async fn cluster_canonical_near_duplicate(
    store: &dyn Backend,
    tenant: &str,
    self_id: &str,
    vector: &[f32],
    model: &str,
    threshold: f32,
) -> Result<Option<(String, f32)>, StorageError> {
    // K bumped from O2's 5 → 12 so a multi-member near-dup cluster is captured
    // (the canonical may not be the single cosine-nearest).
    const K: usize = 12;
    let candidates = store.hybrid_candidates(tenant, "", vector, K).await?;
    let mut neardups: Vec<NearDup> = Vec::new();
    for (cand, _rrf) in candidates {
        if cand.capability_capsule_id == self_id {
            continue;
        }
        // ⑭: same current-model guard as the H1 lane — a stale-model
        // cosine must not drive a supersede proposal either.
        match store
            .get_capability_capsule_embedding_row(&cand.capability_capsule_id)
            .await?
        {
            Some((cand_model, _, _)) if cand_model == model => {}
            _ => continue,
        }
        let Some(cand_vec) = store
            .get_capability_capsule_embedding_vector(&cand.capability_capsule_id)
            .await?
        else {
            continue;
        };
        let c = cosine(vector, &cand_vec);
        if c >= threshold {
            neardups.push(NearDup {
                id: cand.capability_capsule_id.clone(),
                cosine: c,
                content_len: cand.content.len(),
                created_at: cand.created_at.clone(),
            });
        }
    }
    Ok(pick_cluster_canonical(&neardups))
}

/// Standard cosine similarity: `dot / (|a| · |b|)`. Returns 0 on a
/// length mismatch or a zero-norm vector. (Mirrors `dedup_worker`'s
/// private helper; kept local to avoid coupling the two workers.)
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

async fn record_failure(
    store: &dyn Backend,
    job: &crate::storage::ClaimedEmbeddingJob,
    settings: &EmbeddingSettings,
    message: &str,
) -> Result<(), StorageError> {
    let now = current_timestamp();
    let next = job.attempt_count + 1;
    let err = truncate_error(message);
    warn!(
        job_id = %job.job_id,
        attempt = next,
        error = %err,
        "embedding worker job failure"
    );
    if next >= i64::from(settings.max_retries) {
        store
            .permanently_fail_embedding_job(&job.job_id, next, &err, &now)
            .await?;
    } else {
        let delay_ms = failure_backoff_ms(next);
        let available_at = timestamp_add_ms(&now, delay_ms);
        store
            .reschedule_embedding_job_failure(&job.job_id, next, &err, &available_at, &now)
            .await?;
    }
    Ok(())
}

/// Build the per-chunk embed inputs for a capsule. Joins `summary` and
/// `content` (the historical whole-capsule embed input) then splits the
/// result into overlapping token windows (③) so a long capsule's tail is
/// embedded instead of silently truncated by the embedder's context
/// window. Short content yields exactly one chunk equal to the original
/// `"{summary}\n{content}"` string, so the common case is byte-for-byte
/// unchanged (one embedding row, today's behaviour).
fn embed_input_chunks(summary: &str, content: &str) -> Vec<String> {
    let combined = format!("{summary}\n{content}");
    // O5: redact secrets before embedding so a leaked key in captured content
    // never rides into the vector index. Storage is untouched (verbatim).
    let combined = crate::pipeline::redact::redact_secrets(&combined).into_owned();
    crate::pipeline::chunk::chunk_text(
        &combined,
        crate::pipeline::chunk::DEFAULT_CHUNK_TOKENS,
        crate::pipeline::chunk::DEFAULT_CHUNK_OVERLAP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_capsule_is_single_chunk_equal_to_joined_input() {
        // The common case must be byte-for-byte unchanged: one chunk
        // equal to today's `"{summary}\n{content}"` embed input, so
        // existing single-row capsules embed exactly as before.
        let summary = "rust lance ANN ranking";
        let content = "mem fuses BM25 and vector search with RRF in DuckDB.";
        let chunks = embed_input_chunks(summary, content);
        assert_eq!(chunks.len(), 1, "short content must stay one chunk");
        assert_eq!(chunks[0], format!("{summary}\n{content}"));
    }

    #[test]
    fn long_capsule_splits_into_multiple_chunks_covering_head_and_tail() {
        // A capsule whose content blows past one embedder window must
        // split so the tail is embedded, not truncated. The bug ③ fixes:
        // semantic recall silently lost everything past the first window.
        let summary = "HEADMARKER summary line";
        // Far more than DEFAULT_CHUNK_TOKENS (2000) tokens of content.
        let content = format!("{} TAILMARKER", "lorem ipsum dolor sit amet ".repeat(2000));
        let chunks = embed_input_chunks(summary, &content);
        assert!(
            chunks.len() > 1,
            "long content must split into >1 chunk, got {}",
            chunks.len()
        );
        assert!(
            chunks[0].contains("HEADMARKER"),
            "summary must lead the first chunk"
        );
        assert!(
            chunks.last().unwrap().contains("TAILMARKER"),
            "content tail must survive in the last chunk (not truncated)"
        );
    }

    // ── O2: near-duplicate review flagging ──────────────────────────
    use crate::domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use crate::pipeline::ingest::memory_node_id;
    use crate::storage::Store;
    use tempfile::tempdir;

    fn active_capsule(id: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: "local".into(),
            capability_capsule_type: CapabilityCapsuleType::Experience,
            status: CapabilityCapsuleStatus::Active,
            scope: Scope::Repo,
            visibility: Visibility::Shared,
            version: 1,
            summary: format!("summary-{id}"),
            content: format!("content-{id}"),
            content_hash: format!("hash-{id}"),
            source_agent: "test".into(),
            created_at: "00000000000000000001".into(),
            updated_at: "00000000000000000001".into(),
            ..Default::default()
        }
    }

    async fn put_embedding(store: &Store, id: &str, vector: &[f32]) {
        store
            .upsert_capability_capsule_embedding_chunks(
                id,
                "local",
                "fake-test",
                vector.len() as i64,
                &[vector.to_vec()],
                "h",
                "00000000000000000001",
                "00000000000000000002",
            )
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn near_duplicate_flags_for_review_and_records_edge() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("o2.lance")).await.unwrap();
        store
            .insert_capability_capsule(active_capsule("orig"))
            .await
            .unwrap();
        store
            .insert_capability_capsule(active_capsule("dup"))
            .await
            .unwrap();
        // Identical vectors → cosine 1.0, well above the 0.9 threshold.
        let v = vec![0.10f32, 0.20, 0.30, 0.40];
        put_embedding(&store, "orig", &v).await;
        put_embedding(&store, "dup", &v).await;

        flag_if_near_duplicate(&store, "local", "dup", &v, "fake-test", 0.90)
            .await
            .unwrap();

        // dup flipped to PendingConfirmation; orig left Active.
        let dup = store
            .get_capability_capsule_for_tenant("local", "dup")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dup.status, CapabilityCapsuleStatus::PendingConfirmation);
        let orig = store
            .get_capability_capsule_for_tenant("local", "orig")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(orig.status, CapabilityCapsuleStatus::Active);

        // A suspected_supersede edge dup → orig was recorded.
        let edges = store
            .neighbors_within(&memory_node_id("dup"), 1, None)
            .await
            .unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "suspected_supersede"
                    && e.to_node_id == memory_node_id("orig")),
            "expected a suspected_supersede edge dup→orig, got {edges:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_capsule_is_not_flagged() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("o2b.lance")).await.unwrap();
        store
            .insert_capability_capsule(active_capsule("a"))
            .await
            .unwrap();
        store
            .insert_capability_capsule(active_capsule("b"))
            .await
            .unwrap();
        // Orthogonal vectors → cosine 0, below threshold.
        put_embedding(&store, "a", &[1.0, 0.0, 0.0, 0.0]).await;
        let vb = vec![0.0f32, 1.0, 0.0, 0.0];
        put_embedding(&store, "b", &vb).await;

        flag_if_near_duplicate(&store, "local", "b", &vb, "fake-test", 0.90)
            .await
            .unwrap();

        let b = store
            .get_capability_capsule_for_tenant("local", "b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            b.status,
            CapabilityCapsuleStatus::Active,
            "a distinct capsule must not be flagged for review",
        );
    }

    #[test]
    fn pick_cluster_canonical_prefers_longest_then_earlier() {
        // Longest content wins regardless of cosine (the cosine-nearest is short).
        let nd = vec![
            NearDup {
                id: "short".into(),
                cosine: 1.0,
                content_len: 5,
                created_at: "002".into(),
            },
            NearDup {
                id: "long".into(),
                cosine: 0.95,
                content_len: 50,
                created_at: "003".into(),
            },
        ];
        let (id, cos) = pick_cluster_canonical(&nd).unwrap();
        assert_eq!(id, "long");
        assert_eq!(cos, 0.95);

        // Tie on length → earlier created_at wins (mirrors evolution merge).
        let tie = vec![
            NearDup {
                id: "newer".into(),
                cosine: 0.99,
                content_len: 10,
                created_at: "005".into(),
            },
            NearDup {
                id: "older".into(),
                cosine: 0.99,
                content_len: 10,
                created_at: "001".into(),
            },
        ];
        assert_eq!(pick_cluster_canonical(&tie).unwrap().0, "older");

        // Empty cluster → no proposal.
        assert!(pick_cluster_canonical(&[]).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cluster_canonical_picks_longest_not_nearest() {
        // O7(a) vs O2: two near-dups of the new capsule —
        //   short_near: identical vector (cosine 1.0) but short content
        //   long_near:  slightly-off vector (cosine ~0.99) but long content
        // O2 pointed the supersede proposal at short_near (cosine-nearest);
        // O7(a) must point it at long_near (cluster canonical = longest).
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("o7a.lance")).await.unwrap();

        let mut short_near = active_capsule("short_near");
        short_near.content = "short".into();
        let mut long_near = active_capsule("long_near");
        long_near.content = "this is a substantially longer and more complete capsule body".into();
        store.insert_capability_capsule(short_near).await.unwrap();
        store.insert_capability_capsule(long_near).await.unwrap();
        store
            .insert_capability_capsule(active_capsule("new"))
            .await
            .unwrap();

        let v = vec![1.0f32, 0.0, 0.0, 0.0];
        put_embedding(&store, "new", &v).await;
        put_embedding(&store, "short_near", &v).await; // cosine 1.0
        put_embedding(&store, "long_near", &[0.99f32, 0.14, 0.0, 0.0]).await; // cosine ~0.990

        flag_if_near_duplicate(&store, "local", "new", &v, "fake-test", 0.90)
            .await
            .unwrap();

        let new_rec = store
            .get_capability_capsule_for_tenant("local", "new")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(new_rec.status, CapabilityCapsuleStatus::PendingConfirmation);

        let edges = store
            .neighbors_within(&memory_node_id("new"), 1, None)
            .await
            .unwrap();
        let supersede: Vec<_> = edges
            .iter()
            .filter(|e| e.relation == "suspected_supersede")
            .collect();
        assert_eq!(
            supersede.len(),
            1,
            "exactly one supersede proposal expected, got {edges:?}"
        );
        assert_eq!(
            supersede[0].to_node_id,
            memory_node_id("long_near"),
            "O7(a) must propose toward the longest-content canonical, not the cosine-nearest",
        );
    }

    // ── H1: ingest-time neighbor linking (oss-memory-diff §9) ───────

    /// Only neighbors inside [link_threshold, neardup_threshold) get a
    /// `related_to` edge — near-identical rows belong to the O2/O7(a)
    /// supersede lane, distant rows are noise. Cosine rides as edge
    /// confidence, extractor marks provenance.
    #[tokio::test(flavor = "multi_thread")]
    async fn related_neighbors_get_linked_within_band_only() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("h1.lance")).await.unwrap();
        for id in ["new", "kin", "twin", "far"] {
            store
                .insert_capability_capsule(active_capsule(id))
                .await
                .unwrap();
        }
        let v_new = [1.0_f32, 0.0, 0.0, 0.0];
        put_embedding(&store, "new", &v_new).await;
        put_embedding(&store, "kin", &[0.85, 0.527, 0.0, 0.0]).await; // cos ≈ 0.85 — in band
        put_embedding(&store, "twin", &[0.999, 0.02, 0.0, 0.0]).await; // cos ≈ 1.0 — neardup lane
        put_embedding(&store, "far", &[0.3, 0.954, 0.0, 0.0]).await; // cos ≈ 0.3 — noise

        link_related_neighbors(&store, "local", "new", &v_new, "fake-test", 0.80, 0.92)
            .await
            .unwrap();

        let edges = store
            .neighbors_within(&memory_node_id("new"), 1, None)
            .await
            .unwrap();
        let related: Vec<_> = edges
            .iter()
            .filter(|e| e.relation == "related_to")
            .collect();
        assert_eq!(related.len(), 1, "exactly the in-band neighbor: {edges:?}");
        assert_eq!(related[0].to_node_id, memory_node_id("kin"));
        assert_eq!(related[0].extractor.as_deref(), Some("ingest_link"));
        let conf = related[0].confidence.expect("cosine rides as confidence");
        assert!(
            (conf - 0.85).abs() < 0.02,
            "confidence ≈ cosine, got {conf}"
        );
    }

    /// The per-capsule link budget keeps the graph sparse: only the
    /// top-`INGEST_LINK_MAX_EDGES` in-band neighbors by cosine link.
    #[tokio::test(flavor = "multi_thread")]
    async fn ingest_links_are_capped_at_top_k_by_cosine() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("h1cap.lance")).await.unwrap();
        store
            .insert_capability_capsule(active_capsule("new"))
            .await
            .unwrap();
        let v_new = [1.0_f32, 0.0, 0.0, 0.0];
        put_embedding(&store, "new", &v_new).await;
        // Six in-band neighbors at descending cosine.
        for (i, x) in [0.91_f32, 0.90, 0.89, 0.88, 0.85, 0.82].iter().enumerate() {
            let id = format!("n{i}");
            store
                .insert_capability_capsule(active_capsule(&id))
                .await
                .unwrap();
            let y = (1.0 - x * x).sqrt();
            put_embedding(&store, &id, &[*x, y, 0.0, 0.0]).await;
        }

        link_related_neighbors(&store, "local", "new", &v_new, "fake-test", 0.80, 0.92)
            .await
            .unwrap();

        let edges = store
            .neighbors_within(&memory_node_id("new"), 1, None)
            .await
            .unwrap();
        let mut related: Vec<String> = edges
            .iter()
            .filter(|e| e.relation == "related_to")
            .map(|e| e.to_node_id.clone())
            .collect();
        related.sort();
        assert_eq!(
            related.len(),
            INGEST_LINK_MAX_EDGES,
            "cap must hold: {related:?}"
        );
        for kept in ["n0", "n1", "n2", "n3"] {
            assert!(
                related.contains(&memory_node_id(kept)),
                "top-cosine neighbor {kept} must be linked: {related:?}"
            );
        }
    }

    /// Audit 2026-07-03 ⑬: the near-dup floor caps the band ONLY while
    /// the O2/O7(a) lane is on. Lane off → the bound opens, so a
    /// cosine-0.99 twin still gets its `related_to` link instead of
    /// falling into a dead zone nothing owns.
    #[test]
    fn ingest_link_upper_opens_only_when_neardup_lane_is_off() {
        assert_eq!(ingest_link_upper(true, 0.92), 0.92);
        assert!(ingest_link_upper(false, 0.92).is_infinite());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn band_upper_opens_when_neardup_lane_is_off() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("h1open.lance")).await.unwrap();
        for id in ["new", "twin"] {
            store
                .insert_capability_capsule(active_capsule(id))
                .await
                .unwrap();
        }
        let v_new = [1.0_f32, 0.0, 0.0, 0.0];
        put_embedding(&store, "new", &v_new).await;
        put_embedding(&store, "twin", &[0.999, 0.02, 0.0, 0.0]).await; // cos ≈ 1.0

        link_related_neighbors(
            &store,
            "local",
            "new",
            &v_new,
            "fake-test",
            0.80,
            ingest_link_upper(false, 0.92),
        )
        .await
        .unwrap();

        let edges = store
            .neighbors_within(&memory_node_id("new"), 1, None)
            .await
            .unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.relation == "related_to" && e.to_node_id == memory_node_id("twin")),
            "with the supersede lane off, the strongest neighbor must still link: {edges:?}"
        );
    }

    /// Audit 2026-07-03 ⑭: a neighbor still carrying ANOTHER model's
    /// vector (mid re-embed migration) must contribute no cosine — a
    /// cross-model cosine is meaningless and its edge would persist
    /// into ranking.
    #[tokio::test(flavor = "multi_thread")]
    async fn link_skips_neighbors_embedded_by_another_model() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("h1model.lance"))
            .await
            .unwrap();
        for id in ["new", "kin"] {
            store
                .insert_capability_capsule(active_capsule(id))
                .await
                .unwrap();
        }
        let v_new = [1.0_f32, 0.0, 0.0, 0.0];
        put_embedding(&store, "new", &v_new).await;
        // kin is squarely in band — but its row was minted by an older model.
        store
            .upsert_capability_capsule_embedding_chunks(
                "kin",
                "local",
                "stale-model",
                4,
                &[vec![0.85, 0.527, 0.0, 0.0]],
                "h",
                "00000000000000000001",
                "00000000000000000002",
            )
            .await
            .unwrap();

        link_related_neighbors(&store, "local", "new", &v_new, "fake-test", 0.80, 0.92)
            .await
            .unwrap();

        let edges = store
            .neighbors_within(&memory_node_id("new"), 1, None)
            .await
            .unwrap();
        assert!(
            !edges.iter().any(|e| e.relation == "related_to"),
            "a stale-model neighbor must not be linked: {edges:?}"
        );
    }
}
