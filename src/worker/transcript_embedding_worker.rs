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
//!   field. (③) Content is split into overlapping token windows
//!   (`embed_input_chunks`) so a block longer than the embedder's
//!   context window has its tail embedded instead of silently
//!   truncated; short content stays a single chunk.
//! - Upserts N chunk rows to `conversation_message_embeddings` via
//!   `upsert_conversation_message_embedding_chunks` (one row per
//!   chunk, all sharing `message_block_id`; `semantic_search_transcripts`
//!   dedups them via GROUP BY). Lance handles vector indexing
//!   internally — no separate HNSW sidecar (the legacy
//!   DuckDB-as-storage backend maintained one manually; that whole
//!   code path is gone).
//!
//! The provider-id sanity check, retry/backoff schedule, and error
//! truncation mirror the memories worker — see
//! `worker/embedding_worker.rs` for the canonical implementation.

use std::sync::Arc;

use crate::config::EmbeddingSettings;
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
    // Drain up to `batch_size` jobs per tick. This was a hardcoded 1, which made
    // `EMBEDDING_BATCH_SIZE` a silent no-op for the transcript queue (a `mem mine`
    // backlog then cleared at ~one block per poll interval). Each job embeds
    // independently; per-job embedding failures are recorded and swallowed. A
    // StorageError aborts the rest of THIS batch — the still-`processing` jobs it
    // already claimed are reclaimed after their lease expires (via the lease
    // disjunct in `claim_next_n_transcript_embedding_jobs`), not on the next tick.
    // Matches the memories worker's per-job `?`.
    let n = settings.batch_size.max(1);
    let claimed = store
        .claim_next_n_transcript_embedding_jobs(&now, settings.max_retries, n)
        .await?;
    for job in claimed {
        process_job(store, provider, settings, job).await?;
    }
    Ok(())
}

/// Embed one claimed transcript job: validate the provider, fetch the block,
/// chunk + redact + embed, then complete it. Per-job embedding failures are
/// recorded and swallowed (returns Ok) so they don't abort the rest of the
/// tick's batch; only a StorageError propagates.
async fn process_job(
    store: &dyn Backend,
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
    job: crate::storage::ClaimedTranscriptEmbeddingJob,
) -> Result<(), StorageError> {
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
    // have no derived summary — there is nothing to prefix. (③) Long
    // content is split into overlapping token windows so the tail is
    // embedded instead of silently truncated; short content stays a
    // single chunk equal to the original, one embedding row as before.
    // (O5) Redact high-confidence secrets before chunking so a key accidentally
    // pasted into a transcript never enters the conversation_message_embeddings
    // vector index — the same pre-embedding mask the capsule embedding worker
    // applies (`embedding_worker::embed_input_chunks`). Storage stays verbatim:
    // `message.content` on disk is untouched; only this embedding copy is masked.
    let redacted = crate::pipeline::redact::redact_secrets(&message.content);
    let chunks = embed_input_chunks(redacted.as_ref());
    let chunk_refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();
    let results = match provider.embed_batch(&chunk_refs).await {
        Ok(v) => v,
        Err(err) => {
            record_failure(store, &job, settings, &err.to_string()).await?;
            return Ok(());
        }
    };
    if results.len() != chunk_refs.len() {
        // Defensive: trait contract says "same length"; treat a breach
        // as a whole-job failure to avoid persisting a partial chunk set.
        record_failure(store, &job, settings, "provider batch length mismatch").await?;
        return Ok(());
    }

    // Regroup per chunk; the job succeeds only if every chunk embedded
    // at the right dim (any error reschedules the whole job, so a block
    // never persists a partial chunk set).
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(results.len());
    for result in &results {
        match result {
            Ok(embedding) if embedding.len() == provider.dim() => vectors.push(embedding.clone()),
            Ok(embedding) => {
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
            Err(err) => {
                record_failure(store, &job, settings, &err.to_string()).await?;
                return Ok(());
            }
        }
    }

    if store
        .get_transcript_embedding_job_status(&job.job_id)
        .await?
        .as_deref()
        != Some("processing")
    {
        return Ok(());
    }

    let now = current_timestamp();
    let content_hash = sha2_hex(&message.content);
    store
        .upsert_conversation_message_embedding_chunks(
            &job.message_block_id,
            &job.tenant,
            provider.model(),
            provider.dim() as i64,
            &vectors,
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

/// Build the per-chunk embed inputs for a transcript block. Unlike the
/// memories worker there is no `summary` to prefix — the embed source is
/// the verbatim message `content`. Long content is split into overlapping
/// token windows (③) so a block longer than the embedder's context window
/// has its tail embedded instead of silently truncated. Short content
/// (`<= DEFAULT_CHUNK_TOKENS`) yields exactly one chunk equal to the
/// original `content`, so the common case is one embedding row, byte-for-
/// byte unchanged.
fn embed_input_chunks(content: &str) -> Vec<String> {
    crate::pipeline::chunk::chunk_text(
        content,
        crate::pipeline::chunk::DEFAULT_CHUNK_TOKENS,
        crate::pipeline::chunk::DEFAULT_CHUNK_OVERLAP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{EmbeddingProviderKind, EmbeddingSettings};
    use crate::domain::{BlockType, ConversationMessage, MessageRole};
    use crate::embedding::FakeEmbeddingProvider;
    use crate::storage::Store;

    fn tmsg(line: u64, content: &str) -> ConversationMessage {
        ConversationMessage {
            message_block_id: format!("mb_{line}"),
            session_id: Some("s".into()),
            tenant: "t".into(),
            caller_agent: "test".into(),
            transcript_path: "/tmp/d.jsonl".into(),
            line_number: line,
            block_index: 0,
            message_uuid: None,
            role: MessageRole::Assistant,
            block_type: BlockType::Text,
            content: content.into(),
            tool_name: None,
            tool_use_id: None,
            embed_eligible: true,
            created_at: format!("0000000177800000{line:04}"),
            meta_json: None,
        }
    }

    /// One tick must drain up to `batch_size` jobs, not a hardcoded 1 — otherwise
    /// `EMBEDDING_BATCH_SIZE` is silently a no-op for the transcript queue and a
    /// `mem mine` backlog clears at ~one block per poll interval.
    #[tokio::test]
    async fn tick_drains_batch_size_jobs_per_tick() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).await.unwrap();
        store.set_transcript_job_provider("fake");

        // 3 embed-eligible blocks → 3 enqueued jobs.
        for i in 0..3u64 {
            store
                .create_conversation_message(&tmsg(i + 1, &format!("block {i}")))
                .await
                .unwrap();
        }

        let mut settings = EmbeddingSettings::development_defaults();
        settings.provider = EmbeddingProviderKind::Fake; // job_provider_id() == "fake"
        settings.batch_size = 3;
        let provider = FakeEmbeddingProvider::from_settings(&settings);

        // Single tick.
        tick(&store, &provider, &settings).await.unwrap();

        // If the tick honored batch_size it processed all 3 → nothing left to
        // claim. The old hardcoded-1 tick leaves 2 pending.
        let remaining = store
            .claim_next_n_transcript_embedding_jobs(
                "99999999999999999999",
                settings.max_retries,
                10,
            )
            .await
            .unwrap();
        assert!(
            remaining.is_empty(),
            "one tick must drain all 3 jobs (batch_size); {} left",
            remaining.len()
        );
    }

    #[test]
    fn short_message_is_single_chunk_equal_to_content() {
        // The common case must be byte-for-byte unchanged: one chunk
        // equal to the verbatim block content (no summary prefix), so
        // existing single-row transcript embeddings are unaffected.
        let content = "DuckDB single mutex serializes writes";
        let chunks = embed_input_chunks(content);
        assert_eq!(chunks.len(), 1, "short content must stay one chunk");
        assert_eq!(chunks[0], content);
    }

    #[test]
    fn long_message_splits_into_multiple_chunks_covering_head_and_tail() {
        // A block longer than one embedder window must split so the tail
        // is embedded, not truncated — the bug ③ fixes for transcripts.
        let content = format!(
            "HEADMARKER {} TAILMARKER",
            "lorem ipsum dolor sit amet ".repeat(2000)
        );
        let chunks = embed_input_chunks(&content);
        assert!(
            chunks.len() > 1,
            "long content must split into >1 chunk, got {}",
            chunks.len()
        );
        assert!(
            chunks[0].contains("HEADMARKER"),
            "content head must lead the first chunk"
        );
        assert!(
            chunks.last().unwrap().contains("TAILMARKER"),
            "content tail must survive in the last chunk (not truncated)"
        );
    }
}
