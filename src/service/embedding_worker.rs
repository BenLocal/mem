use std::sync::Arc;

use crate::config::EmbeddingSettings;
use crate::embedding::EmbeddingProvider;
use crate::service::embedding_helpers::{f32_slice_to_blob, failure_backoff_ms, truncate_error};
use crate::storage::{current_timestamp, timestamp_add_ms, DuckDbRepository, StorageError};
use tracing::{error, info, warn};

pub async fn run(
    repo: DuckDbRepository,
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
        if let Err(err) = tick(&repo, provider.as_ref(), &settings).await {
            error!(error = %err, "embedding worker tick failed");
        }
    }
}

pub async fn tick(
    repo: &DuckDbRepository,
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
) -> Result<(), StorageError> {
    let now = current_timestamp();
    let job = match repo
        .claim_next_embedding_job(&now, settings.max_retries)
        .await
    {
        Ok(Some(j)) => j,
        Ok(None) => return Ok(()),
        Err(err) => {
            // Last-resort defense against orphan FK violations: if claim raises
            // a "key does not exist in the referenced table" error, the
            // open-time sweep failed to catch this row (stale binary, multiple
            // processes, or fresh orphan inserted after sweep). Parse the
            // memory_id out of the error message and direct-delete every job
            // referencing it. DELETE-by-memory_id has no INSERT half so it
            // does not re-fire FK; the loop dies on the next tick.
            if let Some(mid) = extract_orphan_memory_id(&err.to_string()) {
                let deleted = repo
                    .delete_embedding_jobs_by_memory_id(&mid)
                    .await
                    .unwrap_or(0);
                warn!(
                    memory_id = %mid,
                    deleted_jobs = deleted,
                    "FK violation in claim_next_embedding_job; direct-deleted orphan jobs by memory_id (open-time sweep apparently missed this row)"
                );
                return Ok(());
            }
            return Err(err);
        }
    };
    info!(
        job_id = %job.job_id,
        tenant = %job.tenant,
        memory_id = %job.memory_id,
        attempt = job.attempt_count,
        "embedding worker claimed job"
    );

    if job.provider != settings.job_provider_id() {
        let now = current_timestamp();
        repo.permanently_fail_embedding_job(
            &job.job_id,
            job.attempt_count + 1,
            "embedding job provider does not match runtime configuration",
            &now,
        )
        .await?;
        return Ok(());
    }

    let Some(memory) = repo
        .get_memory_for_tenant(&job.tenant, &job.memory_id)
        .await?
    else {
        let now = current_timestamp();
        repo.permanently_fail_embedding_job(
            &job.job_id,
            job.attempt_count + 1,
            "memory row missing for embedding job",
            &now,
        )
        .await?;
        return Ok(());
    };

    if memory.content_hash != job.target_content_hash {
        let now = current_timestamp();
        repo.mark_embedding_job_stale(&job.job_id, &now).await?;
        return Ok(());
    }

    let text = format!("{}\n{}", memory.summary, memory.content);
    let embedding = match provider.embed_text(&text).await {
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

    let Some(memory_after) = repo
        .get_memory_for_tenant(&job.tenant, &job.memory_id)
        .await?
    else {
        let now = current_timestamp();
        repo.permanently_fail_embedding_job(
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
        repo.mark_embedding_job_stale(&job.job_id, &now).await?;
        return Ok(());
    }

    if repo.get_embedding_job_status(&job.job_id).await?.as_deref() != Some("processing") {
        return Ok(());
    }

    let blob = f32_slice_to_blob(&embedding);
    let now = current_timestamp();
    repo.upsert_memory_embedding(
        &job.memory_id,
        &job.tenant,
        provider.model(),
        provider.dim() as i64,
        &blob,
        &job.target_content_hash,
        &memory_after.updated_at,
        &now,
    )
    .await?;

    if let Some(idx) = repo.vector_index() {
        match idx.upsert(&job.memory_id, &embedding).await {
            Ok(()) => {
                let count = idx.dirty_count_increment();
                if count >= settings.vector_index_flush_every {
                    if let Err(err) = idx.save_at_default_paths().await {
                        warn!(error = %err, "vector index periodic save failed");
                    } else {
                        idx.dirty_count_reset();
                    }
                }
            }
            Err(crate::storage::VectorIndexError::HashCollision { existing, incoming }) => {
                // Per spec: hash collisions are catastrophic data integrity events.
                // Permanently fail the job rather than retry.
                let now = current_timestamp();
                let msg = format!("vector_index hash collision: {existing} vs {incoming}");
                error!(memory_id = %job.memory_id, error = %msg, "vector index hash collision; permanently failing job");
                repo.permanently_fail_embedding_job(&job.job_id, job.attempt_count + 1, &msg, &now)
                    .await?;
                return Ok(());
            }
            Err(err) => {
                warn!(
                    job_id = %job.job_id,
                    memory_id = %job.memory_id,
                    error = %err,
                    "vector index upsert failed; embedding row already written"
                );
                // Best effort: do not fail the job. Row+index reconciliation happens on next startup.
            }
        }
    }

    repo.complete_embedding_job(&job.job_id, &now).await?;
    info!(
        job_id = %job.job_id,
        memory_id = %job.memory_id,
        "embedding worker completed job"
    );
    Ok(())
}

async fn record_failure(
    repo: &DuckDbRepository,
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
        repo.permanently_fail_embedding_job(&job.job_id, next, &err, &now)
            .await?;
    } else {
        let delay_ms = failure_backoff_ms(next);
        let available_at = timestamp_add_ms(&now, delay_ms);
        repo.reschedule_embedding_job_failure(&job.job_id, next, &err, &available_at, &now)
            .await?;
    }
    Ok(())
}

/// Parse a memory_id out of a DuckDB FK violation error message of the form
/// `... key "memory_id: <id>" does not exist in the referenced table ...`.
/// Returns None if the pattern doesn't match (caller propagates the error).
fn extract_orphan_memory_id(err: &str) -> Option<String> {
    // Look for the literal sentinel `key "memory_id: ` followed by id then `"`.
    let needle = r#"key "memory_id: "#;
    let start = err.find(needle)? + needle.len();
    let rest = &err[start..];
    let end = rest.find('"')?;
    let id = &rest[..end];
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn extracts_memory_id_from_fk_error() {
        let err = "duckdb error: Constraint Error: Violates foreign key constraint because key \"memory_id: mem_019de690-431d-7133-8b5a-2becc0e2ea43\" does not exist in the referenced table";
        assert_eq!(
            extract_orphan_memory_id(err).as_deref(),
            Some("mem_019de690-431d-7133-8b5a-2becc0e2ea43")
        );
    }

    #[test]
    fn returns_none_for_unrelated_error() {
        assert!(extract_orphan_memory_id("some other error").is_none());
    }

    #[test]
    fn returns_none_when_no_id_after_marker() {
        let err = "key \"memory_id: \"";
        assert_eq!(extract_orphan_memory_id(err), None);
    }
}
