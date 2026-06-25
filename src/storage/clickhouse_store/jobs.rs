//! `EmbeddingJobStore` for [`ClickHouseBackend`] — the capsule + transcript
//! embedding-job queues.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** `embedding_jobs` + `transcript_embedding_jobs`
//! are `ReplacingMergeTree(row_version)`. Status transitions are read-latest +
//! versioned re-insert (§4(a)). **Non-atomic claim** (§10): ClickHouse has no
//! `SELECT … FOR UPDATE SKIP LOCKED` / transactions, so `claim_next_n` is
//! best-effort — two concurrent workers can claim the same job. mem's embedding
//! worker is effectively single, so this is acceptable for the scaffold.

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, now_version, opt};
use crate::domain::embeddings::EmbeddingJobInfo;
use crate::storage::types::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, EmbeddingJobInsert, StorageError,
};
use crate::storage::EmbeddingJobStore;

#[derive(Row, Serialize, Deserialize, Clone)]
struct ChJobRow {
    job_id: String,
    tenant: String,
    capability_capsule_id: String,
    target_content_hash: String,
    provider: String,
    status: String,
    attempt_count: i64,
    last_error: String,
    available_at: String,
    created_at: String,
    updated_at: String,
    row_version: u64,
}

#[derive(Row, Serialize, Deserialize, Clone)]
struct ChTxJobRow {
    job_id: String,
    tenant: String,
    message_block_id: String,
    provider: String,
    status: String,
    attempt_count: i64,
    last_error: String,
    available_at: String,
    created_at: String,
    updated_at: String,
    row_version: u64,
}

impl ClickHouseBackend {
    async fn ch_job(&self, job_id: &str) -> Result<Option<ChJobRow>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM embedding_jobs FINAL WHERE job_id = ? \
                 ORDER BY row_version DESC LIMIT 1",
            )
            .bind(job_id)
            .fetch_all::<ChJobRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next())
    }

    async fn ch_write_job(&self, row: &ChJobRow) -> Result<(), StorageError> {
        let mut insert = self
            .client
            .insert::<ChJobRow>("embedding_jobs")
            .await
            .map_err(ch_err)?;
        insert.write(row).await.map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }

    /// Read-modify-reinsert a job's status fields (None = leave unchanged).
    async fn ch_set_job(
        &self,
        job_id: &str,
        status: Option<&str>,
        attempt: Option<i64>,
        error: Option<&str>,
        available_at: Option<&str>,
        now: &str,
    ) -> Result<(), StorageError> {
        let Some(mut row) = self.ch_job(job_id).await? else {
            return Ok(());
        };
        if let Some(s) = status {
            row.status = s.to_owned();
        }
        if let Some(a) = attempt {
            row.attempt_count = a;
        }
        if let Some(e) = error {
            row.last_error = e.to_owned();
        }
        if let Some(av) = available_at {
            row.available_at = av.to_owned();
        }
        row.updated_at = now.to_owned();
        row.row_version = now_version();
        self.ch_write_job(&row).await
    }

    async fn ch_tx_job(&self, job_id: &str) -> Result<Option<ChTxJobRow>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM transcript_embedding_jobs FINAL WHERE job_id = ? \
                 ORDER BY row_version DESC LIMIT 1",
            )
            .bind(job_id)
            .fetch_all::<ChTxJobRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next())
    }

    async fn ch_set_tx_job(
        &self,
        job_id: &str,
        status: Option<&str>,
        attempt: Option<i64>,
        error: Option<&str>,
        available_at: Option<&str>,
        now: &str,
    ) -> Result<(), StorageError> {
        let Some(mut row) = self.ch_tx_job(job_id).await? else {
            return Ok(());
        };
        if let Some(s) = status {
            row.status = s.to_owned();
        }
        if let Some(a) = attempt {
            row.attempt_count = a;
        }
        if let Some(e) = error {
            row.last_error = e.to_owned();
        }
        if let Some(av) = available_at {
            row.available_at = av.to_owned();
        }
        row.updated_at = now.to_owned();
        row.row_version = now_version();
        let mut insert = self
            .client
            .insert::<ChTxJobRow>("transcript_embedding_jobs")
            .await
            .map_err(ch_err)?;
        insert.write(&row).await.map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }
}

#[async_trait]
impl EmbeddingJobStore for ClickHouseBackend {
    async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        // Skip if a live (pending|processing) job already covers this content.
        let existing: Vec<u64> = self
            .client
            .query(
                "SELECT count() FROM embedding_jobs FINAL \
                 WHERE tenant = ? AND capability_capsule_id = ? AND target_content_hash = ? \
                 AND provider = ? AND status IN ('pending', 'processing')",
            )
            .bind(&insert.tenant)
            .bind(&insert.capability_capsule_id)
            .bind(&insert.target_content_hash)
            .bind(&insert.provider)
            .fetch_all::<u64>()
            .await
            .map_err(ch_err)?;
        if existing.first().copied().unwrap_or(0) > 0 {
            return Ok(false);
        }
        self.ch_write_job(&ChJobRow {
            job_id: insert.job_id,
            tenant: insert.tenant,
            capability_capsule_id: insert.capability_capsule_id,
            target_content_hash: insert.target_content_hash,
            provider: insert.provider,
            status: "pending".to_owned(),
            attempt_count: 0,
            last_error: String::new(),
            available_at: insert.available_at,
            created_at: insert.created_at,
            updated_at: insert.updated_at,
            row_version: now_version(),
        })
        .await?;
        Ok(true)
    }

    async fn enqueue_embedding_jobs(
        &self,
        inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError> {
        for ins in inserts {
            self.try_enqueue_embedding_job(ins.clone()).await?;
        }
        Ok(())
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM embedding_jobs FINAL \
                 WHERE available_at <= ? AND (status = 'pending' \
                 OR (status = 'failed' AND attempt_count < ?)) \
                 ORDER BY available_at ASC LIMIT ?",
            )
            .bind(now)
            .bind(max_retries as i64)
            .bind(n as u64)
            .fetch_all::<ChJobRow>()
            .await
            .map_err(ch_err)?;
        let mut claimed = Vec::with_capacity(rows.len());
        for mut row in rows {
            row.status = "processing".to_owned();
            row.attempt_count += 1;
            row.updated_at = now.to_owned();
            row.row_version = now_version();
            self.ch_write_job(&row).await?;
            claimed.push(ClaimedEmbeddingJob {
                job_id: row.job_id,
                tenant: row.tenant,
                capability_capsule_id: row.capability_capsule_id,
                target_content_hash: row.target_content_hash,
                provider: row.provider,
                attempt_count: row.attempt_count,
            });
        }
        Ok(claimed)
    }

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        self.ch_set_job(job_id, Some("completed"), None, None, None, now)
            .await
    }

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        self.ch_set_job(job_id, Some("stale"), None, None, None, now)
            .await
    }

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.ch_set_job(
            job_id,
            Some("failed"),
            Some(new_attempt_count),
            Some(last_error),
            Some(available_at),
            now,
        )
        .await
    }

    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.ch_set_job(
            job_id,
            Some("failed_permanent"),
            Some(new_attempt_count),
            Some(last_error),
            None,
            now,
        )
        .await
    }

    async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, StorageError> {
        self.client
            .query("ALTER TABLE embedding_jobs DELETE WHERE capability_capsule_id = ?")
            .bind(capability_capsule_id)
            .execute()
            .await
            .map_err(ch_err)?;
        Ok(0)
    }

    async fn stale_live_embedding_jobs_for_capability_capsule(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        self.client
            .query(
                "ALTER TABLE embedding_jobs UPDATE status = 'stale', updated_at = ? \
                 WHERE tenant = ? AND capability_capsule_id = ? AND provider = ? \
                 AND status IN ('pending', 'processing')",
            )
            .bind(now)
            .bind(tenant)
            .bind(capability_capsule_id)
            .bind(provider)
            .execute()
            .await
            .map_err(ch_err)?;
        Ok(0)
    }

    async fn get_embedding_job_status(&self, job_id: &str) -> Result<Option<String>, StorageError> {
        Ok(self.ch_job(job_id).await?.map(|r| r.status))
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT status FROM embedding_jobs FINAL \
                 WHERE tenant = ? AND capability_capsule_id = ? AND target_content_hash = ? \
                 ORDER BY updated_at DESC LIMIT 1",
            )
            .bind(tenant)
            .bind(capability_capsule_id)
            .bind(target_content_hash)
            .fetch_all::<String>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next())
    }

    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        let mut sql = String::from("SELECT ?fields FROM embedding_jobs FINAL WHERE tenant = ?");
        if status_filter.is_some() {
            sql.push_str(" AND status = ?");
        }
        if memory_id_filter.is_some() {
            sql.push_str(" AND capability_capsule_id = ?");
        }
        sql.push_str(" ORDER BY updated_at DESC LIMIT ?");
        let mut q = self.client.query(&sql).bind(tenant);
        if let Some(s) = status_filter {
            q = q.bind(s);
        }
        if let Some(m) = memory_id_filter {
            q = q.bind(m);
        }
        let rows = q
            .bind(limit as u64)
            .fetch_all::<ChJobRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows
            .into_iter()
            .map(|r| EmbeddingJobInfo {
                job_id: r.job_id,
                tenant: r.tenant,
                capability_capsule_id: r.capability_capsule_id,
                target_content_hash: r.target_content_hash,
                provider: r.provider,
                status: r.status,
                attempt_count: r.attempt_count as u32,
                last_error: opt(r.last_error),
                available_at: r.available_at,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect())
    }

    async fn claim_next_n_transcript_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM transcript_embedding_jobs FINAL \
                 WHERE available_at <= ? AND (status = 'pending' \
                 OR (status = 'failed' AND attempt_count < ?)) \
                 ORDER BY available_at ASC LIMIT ?",
            )
            .bind(now)
            .bind(max_retries as i64)
            .bind(n as u64)
            .fetch_all::<ChTxJobRow>()
            .await
            .map_err(ch_err)?;
        let mut claimed = Vec::with_capacity(rows.len());
        for mut row in rows {
            row.status = "processing".to_owned();
            row.attempt_count += 1;
            row.updated_at = now.to_owned();
            row.row_version = now_version();
            let mut insert = self
                .client
                .insert::<ChTxJobRow>("transcript_embedding_jobs")
                .await
                .map_err(ch_err)?;
            insert.write(&row).await.map_err(ch_err)?;
            insert.end().await.map_err(ch_err)?;
            claimed.push(ClaimedTranscriptEmbeddingJob {
                job_id: row.job_id,
                tenant: row.tenant,
                message_block_id: row.message_block_id,
                provider: row.provider,
                attempt_count: row.attempt_count,
            });
        }
        Ok(claimed)
    }

    async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.ch_set_tx_job(job_id, Some("completed"), None, None, None, now)
            .await
    }

    async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.ch_set_tx_job(job_id, Some("stale"), None, None, None, now)
            .await
    }

    async fn reschedule_transcript_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.ch_set_tx_job(
            job_id,
            Some("failed"),
            Some(new_attempt_count),
            Some(last_error),
            Some(available_at),
            now,
        )
        .await
    }

    async fn permanently_fail_transcript_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.ch_set_tx_job(
            job_id,
            Some("failed_permanent"),
            Some(new_attempt_count),
            Some(last_error),
            None,
            now,
        )
        .await
    }

    async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        Ok(self.ch_tx_job(job_id).await?.map(|r| r.status))
    }
}
