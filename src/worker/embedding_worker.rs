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

    // Lance handles vector indexing automatically when the embedding
    // is added to the capability_capsule_embeddings table — no separate HNSW
    // sidecar to update (the legacy DuckDB-as-storage backend
    // maintained a usearch index manually here; that whole code path
    // is gone in the new architecture).

    store.complete_embedding_job(&job.job_id, &now).await?;
    info!(
        job_id = %job.job_id,
        capability_capsule_id = %job.capability_capsule_id,
        "embedding worker completed job"
    );
    Ok(())
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
}
