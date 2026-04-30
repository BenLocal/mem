//! Transcript embedding worker.
//!
//! Structural mirror of [`crate::service::embedding_worker`] for the
//! `transcript_embedding_jobs` queue. Differences from the memories worker:
//!
//! - Polls `transcript_embedding_jobs` via
//!   [`DuckDbRepository::claim_next_transcript_embedding_job`].
//! - Loads rows from `conversation_messages` (immutable on insert, so the
//!   memories worker's `content_hash` drift check is intentionally absent —
//!   transcript blocks cannot be rewritten while a job is queued).
//! - Embeds the message **content only** — no `summary` to concatenate, since
//!   transcript rows have no derived summary field.
//! - Upserts to `conversation_message_embeddings` and a separate transcript
//!   HNSW sidecar (`<db>.transcripts.usearch`). The vector index is passed
//!   explicitly into `tick`/`run` rather than fetched off the repo (the
//!   memories pipeline already owns the repo's `attach_vector_index` slot).
//!
//! The provider-id sanity check, retry/backoff schedule, and error truncation
//! are pasted from the memories worker — see `service/embedding_worker.rs` for
//! the canonical implementation. A future cleanup task may dedupe the helper
//! functions into a shared module; for now they're copied to keep the two
//! workers independently readable.

use std::sync::Arc;

use crate::config::EmbeddingSettings;
use crate::embedding::EmbeddingProvider;
use crate::storage::{DuckDbRepository, StorageError, VectorIndex, VectorIndexError};
use tracing::{error, info, warn};

pub async fn run(
    repo: DuckDbRepository,
    provider: Arc<dyn EmbeddingProvider>,
    settings: EmbeddingSettings,
    index: Arc<VectorIndex>,
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
        if let Err(err) = tick(&repo, provider.as_ref(), &settings, &index).await {
            error!(error = %err, "transcript embedding worker tick failed");
        }
    }
}

pub async fn tick(
    repo: &DuckDbRepository,
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
    index: &VectorIndex,
) -> Result<(), StorageError> {
    let now = current_timestamp();
    let Some(job) = repo
        .claim_next_transcript_embedding_job(&now, settings.max_retries)
        .await?
    else {
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
        repo.permanently_fail_transcript_embedding_job(
            &job.job_id,
            job.attempt_count + 1,
            "transcript embedding job provider does not match runtime configuration",
            &now,
        )
        .await?;
        return Ok(());
    }

    let Some(message) = repo
        .get_conversation_message_by_id(&job.tenant, &job.message_block_id)
        .await?
    else {
        let now = current_timestamp();
        repo.permanently_fail_transcript_embedding_job(
            &job.job_id,
            job.attempt_count + 1,
            "conversation message row missing for embedding job",
            &now,
        )
        .await?;
        return Ok(());
    };

    // Transcript embedding source is the verbatim message content. Memories
    // concatenate `summary + "\n" + content`, but transcripts have no
    // derived summary — there is nothing to prefix.
    let text = &message.content;
    let embedding = match provider.embed_text(text).await {
        Ok(v) => v,
        Err(err) => {
            record_failure(repo, &job, settings, &err.to_string()).await?;
            return Ok(());
        }
    };

    if embedding.len() != provider.dim() {
        record_failure(
            repo,
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

    if repo
        .get_transcript_embedding_job_status(&job.job_id)
        .await?
        .as_deref()
        != Some("processing")
    {
        return Ok(());
    }

    let blob = f32_slice_to_blob(&embedding);
    let now = current_timestamp();
    let content_hash = sha2_hex(text);
    repo.upsert_conversation_message_embedding(
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

    match index.upsert(&job.message_block_id, &embedding).await {
        Ok(()) => {
            let count = index.dirty_count_increment();
            if count >= settings.vector_index_flush_every {
                if let Err(err) = index.save_at_default_paths().await {
                    warn!(error = %err, "transcript vector index periodic save failed");
                } else {
                    index.dirty_count_reset();
                }
            }
        }
        Err(VectorIndexError::HashCollision { existing, incoming }) => {
            // Per spec: hash collisions are catastrophic. Permanently fail
            // rather than retry — the data integrity is already broken and
            // retrying cannot fix it.
            let now = current_timestamp();
            let msg = format!("transcript vector_index hash collision: {existing} vs {incoming}");
            error!(
                message_block_id = %job.message_block_id,
                error = %msg,
                "transcript vector index hash collision; permanently failing job"
            );
            repo.permanently_fail_transcript_embedding_job(
                &job.job_id,
                job.attempt_count + 1,
                &msg,
                &now,
            )
            .await?;
            return Ok(());
        }
        Err(err) => {
            warn!(
                job_id = %job.job_id,
                message_block_id = %job.message_block_id,
                error = %err,
                "transcript vector index upsert failed; embedding row already written"
            );
            // Best effort: do not fail the job. Row+index reconciliation
            // happens on next startup via open_or_rebuild_transcripts.
        }
    }

    repo.complete_transcript_embedding_job(&job.job_id, &now)
        .await?;
    info!(
        job_id = %job.job_id,
        message_block_id = %job.message_block_id,
        "transcript embedding worker completed job"
    );
    Ok(())
}

async fn record_failure(
    repo: &DuckDbRepository,
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
        repo.permanently_fail_transcript_embedding_job(&job.job_id, next, &err, &now)
            .await?;
    } else {
        let delay_ms = failure_backoff_ms(next);
        let available_at = timestamp_add_ms(&now, delay_ms);
        repo.reschedule_transcript_embedding_job_failure(
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

fn failure_backoff_ms(attempt_after_fail: i64) -> u128 {
    match attempt_after_fail {
        1 => 60_000,
        2 => 300_000,
        _ => 1_800_000,
    }
}

fn truncate_error(message: &str) -> String {
    const MAX: usize = 2000;
    if message.len() <= MAX {
        message.to_string()
    } else {
        message.chars().take(MAX).collect()
    }
}

fn f32_slice_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_ne_bytes());
    }
    out
}

fn sha2_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn timestamp_add_ms(ts: &str, add_ms: u128) -> String {
    let digits: String = ts.chars().filter(|c| c.is_ascii_digit()).collect();
    let base: u128 = digits.parse().unwrap_or(0);
    format!("{:020}", base.saturating_add(add_ms))
}

fn current_timestamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
}
