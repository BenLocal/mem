use std::sync::Arc;

use crate::config::EmbeddingSettings;
use crate::embedding::EmbeddingProvider;
use crate::service::embedding_helpers::{f32_slice_to_blob, failure_backoff_ms, truncate_error};
use crate::storage::{current_timestamp, timestamp_add_ms, StorageError, Store};
use tracing::{error, info, warn};

pub async fn run(
    store: Arc<Store>,
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
        if let Err(err) = tick(&store, provider.as_ref(), &settings).await {
            error!(error = %err, "embedding worker tick failed");
        }
    }
}

pub async fn tick(
    store: &Store,
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
            Some(text) => ready.push(PendingEmbed { job, text }),
            None => continue,
        }
    }
    if ready.is_empty() {
        return Ok(());
    }

    // Batch embed. embed_batch returns Vec<Result<Vec<f32>>> of same
    // length as inputs. Whole-batch failure → per-job reschedule.
    let texts: Vec<&str> = ready.iter().map(|p| p.text.as_str()).collect();
    let results = match provider.embed_batch(&texts).await {
        Ok(v) => v,
        Err(err) => {
            warn!(error = %err, batch = ready.len(), "embedding worker whole-batch failure; rescheduling each");
            for p in &ready {
                record_failure(store, &p.job, settings, &err.to_string()).await?;
            }
            return Ok(());
        }
    };
    if results.len() != ready.len() {
        // Defensive: trait contract says "same length"; if a provider
        // breaks it, treat as whole-batch failure to avoid index
        // misalignment.
        warn!(
            expected = ready.len(),
            got = results.len(),
            "embedding provider returned wrong batch length"
        );
        for p in &ready {
            record_failure(store, &p.job, settings, "provider batch length mismatch").await?;
        }
        return Ok(());
    }

    // Post-embed phase.
    for (p, result) in ready.into_iter().zip(results) {
        match result {
            Ok(embedding) => {
                post_embed(store, &p.job, &embedding, provider, settings).await?;
            }
            Err(err) => {
                record_failure(store, &p.job, settings, &err.to_string()).await?;
            }
        }
    }
    Ok(())
}

struct PendingEmbed {
    job: crate::storage::ClaimedEmbeddingJob,
    text: String,
}

/// Per-job pre-embed validation. Returns the embed-input text on
/// success; `None` if the job was resolved inline.
async fn pre_embed(
    store: &Store,
    job: &crate::storage::ClaimedEmbeddingJob,
    settings: &EmbeddingSettings,
) -> Result<Option<String>, StorageError> {
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

    Ok(Some(format!("{}\n{}", memory.summary, memory.content)))
}

/// Per-job post-embed finalization: dim check, re-fetch memory,
/// upsert embedding row (lance handles vector indexing internally —
/// no separate sidecar to update), complete the job.
async fn post_embed(
    store: &Store,
    job: &crate::storage::ClaimedEmbeddingJob,
    embedding: &[f32],
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
) -> Result<(), StorageError> {
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

    let blob = f32_slice_to_blob(embedding);
    let now = current_timestamp();
    store
        .upsert_capability_capsule_embedding(
            &job.capability_capsule_id,
            &job.tenant,
            provider.model(),
            provider.dim() as i64,
            &blob,
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
    store: &Store,
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
