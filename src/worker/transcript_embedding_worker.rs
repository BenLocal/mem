//! Transcript embedding worker.
//!
//! Structural mirror of [`crate::worker::embedding_worker`] for the
//! `transcript_embedding_jobs` queue. Differences from the memories
//! worker:
//!
//! - Polls `transcript_embedding_jobs` via
//!   [`Store::claim_next_n_transcript_embedding_jobs`].
//! - Loads rows from `conversation_messages` (immutable on insert,
//!   so the memories worker's `content_hash` drift check is
//!   intentionally absent — transcript blocks cannot be rewritten
//!   while a job is queued).
//! - Embeds the message **content only** — no `summary` to
//!   concatenate, since transcript rows have no derived summary
//!   field.
//! - Upserts to `conversation_message_embeddings` via Store. Lance
//!   handles vector indexing internally — no separate HNSW sidecar
//!   to update (the legacy DuckDB-as-storage backend maintained one
//!   manually here; that whole code path is gone).
//!
//! The provider-id sanity check, retry/backoff schedule, and error
//! truncation mirror the memories worker — see
//! `worker/embedding_worker.rs` for the canonical implementation.

use std::sync::Arc;

use crate::config::EmbeddingSettings;
use crate::embedding::wire::encode_f32_blob;
use crate::embedding::EmbeddingProvider;
use crate::service::embedding_helpers::{failure_backoff_ms, sha2_hex, truncate_error};
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
        "transcript embedding worker started"
    );
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        settings.worker_poll_interval_ms.max(1),
    ));
    loop {
        interval.tick().await;
        if let Err(err) = tick(&*store, provider.as_ref(), &settings).await {
            error!(error = %err, "transcript embedding worker tick failed");
        }
    }
}

pub async fn tick(
    store: &dyn Backend,
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
) -> Result<(), StorageError> {
    let now = current_timestamp();
    // Single-claim cadence (one job per tick) — matches the legacy
    // shape; `Store::claim_next_n_transcript_embedding_jobs` returns
    // up to N but we ask for 1.
    let claimed = store
        .claim_next_n_transcript_embedding_jobs(&now, settings.max_retries, 1)
        .await?;
    let Some(job) = claimed.into_iter().next() else {
        return Ok(());
    };
    info!(
        job_id = %job.job_id,
        tenant = %job.tenant,
        message_block_id = %job.message_block_id,
        attempt = job.attempt_count,
        "transcript embedding worker claimed job"
    );

    if job.provider != settings.job_provider_id() {
        let now = current_timestamp();
        store
            .permanently_fail_transcript_embedding_job(
                &job.job_id,
                job.attempt_count + 1,
                "transcript embedding job provider does not match runtime configuration",
                &now,
            )
            .await?;
        return Ok(());
    }

    let mut messages = store
        .fetch_conversation_messages_by_ids(
            &job.tenant,
            std::slice::from_ref(&job.message_block_id),
        )
        .await?;
    let Some(message) = messages.pop() else {
        let now = current_timestamp();
        store
            .permanently_fail_transcript_embedding_job(
                &job.job_id,
                job.attempt_count + 1,
                "conversation message row missing for embedding job",
                &now,
            )
            .await?;
        return Ok(());
    };

    // Transcript embedding source is the verbatim message content.
    // Memories concatenate `summary + "\n" + content`, but transcripts
    // have no derived summary — there is nothing to prefix.
    let text = &message.content;
    let embedding = match provider.embed_text(text).await {
        Ok(v) => v,
        Err(err) => {
            record_failure(store, &job, settings, &err.to_string()).await?;
            return Ok(());
        }
    };

    if embedding.len() != provider.dim() {
        record_failure(
            store,
            &job,
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

    if store
        .get_transcript_embedding_job_status(&job.job_id)
        .await?
        .as_deref()
        != Some("processing")
    {
        return Ok(());
    }

    let blob = encode_f32_blob(&embedding);
    let now = current_timestamp();
    let content_hash = sha2_hex(text);
    store
        .upsert_conversation_message_embedding(
            &job.message_block_id,
            &job.tenant,
            provider.model(),
            provider.dim() as i64,
            &blob,
            &content_hash,
            &message.created_at,
            &now,
        )
        .await?;

    store
        .complete_transcript_embedding_job(&job.job_id, &now)
        .await?;
    info!(
        job_id = %job.job_id,
        message_block_id = %job.message_block_id,
        "transcript embedding worker completed job"
    );
    Ok(())
}

async fn record_failure(
    store: &dyn Backend,
    job: &crate::storage::ClaimedTranscriptEmbeddingJob,
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
        "transcript embedding worker job failure"
    );
    if next >= i64::from(settings.max_retries) {
        store
            .permanently_fail_transcript_embedding_job(&job.job_id, next, &err, &now)
            .await?;
    } else {
        let delay_ms = failure_backoff_ms(next);
        let available_at = timestamp_add_ms(&now, delay_ms);
        store
            .reschedule_transcript_embedding_job_failure(
                &job.job_id,
                next,
                &err,
                &available_at,
                &now,
            )
            .await?;
    }
    Ok(())
}
