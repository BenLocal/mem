use std::sync::Arc;

use crate::config::EmbeddingSettings;
use crate::embedding::EmbeddingProvider;
use crate::storage::{DuckDbRepository, StorageError};

pub async fn run(
    repo: DuckDbRepository,
    provider: Arc<dyn EmbeddingProvider>,
    settings: EmbeddingSettings,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        settings.worker_poll_interval_ms.max(1),
    ));
    loop {
        interval.tick().await;
        if let Err(err) = tick(&repo, provider.as_ref(), &settings).await {
            eprintln!("embedding worker: {err}");
        }
    }
}

pub async fn tick(
    repo: &DuckDbRepository,
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
) -> Result<(), StorageError> {
    let now = current_timestamp();
    let Some(job) = repo
        .claim_next_embedding_job(&now, settings.max_retries)
        .await?
    else {
        return Ok(());
    };

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
    repo.complete_embedding_job(&job.job_id, &now).await?;
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
