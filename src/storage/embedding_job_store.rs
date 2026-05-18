//! Backend-agnostic embedding-job queue — Phase 3 sub-trait.
//!
//! Covers both queue tables: `embedding_jobs` (capsule embeddings)
//! and `transcript_embedding_jobs` (transcript-block embeddings).
//! Same `pending → processing → completed | failed | stale` state
//! machine, separate row types because the work-unit differs
//! ([`ClaimedEmbeddingJob`] vs [`ClaimedTranscriptEmbeddingJob`]).
//!
//! **LANCE-SPECIFIC bits**: `claim_next_n_*` relies on Lance's
//! `update().only_if(...)` + `rows_updated` optimistic claim. A
//! Postgres backend would use `SELECT FOR UPDATE SKIP LOCKED`;
//! Redis would use `BLPOP` / Streams. The signature stays uniform
//! across backends — the implementation strategy is hidden.
//!
//! See `docs/backend-coupling.md` §3.1 + §6.4.

use async_trait::async_trait;

use crate::domain::embeddings::EmbeddingJobInfo;
use crate::storage::types::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, EmbeddingJobInsert, StorageError,
};
use crate::storage::Store;

#[async_trait]
pub trait EmbeddingJobStore: Send + Sync {
    // ── Capsule embedding jobs ──────────────────────────────────────

    /// Enqueue one job after a per-row
    /// `(tenant, capability_capsule_id, target_content_hash,
    /// provider)` idempotency probe. Returns `true` if a new row
    /// was inserted, `false` if an existing live row covered the
    /// same tuple.
    async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError>;

    /// Multi-row variant — caller guarantees no live job exists
    /// for the inputs (typically run right after a fresh
    /// `insert_capability_capsules`). No-op when empty.
    async fn enqueue_embedding_jobs(
        &self,
        inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError>;

    /// Atomically claim up to `n` ready jobs (`status=pending` or
    /// retryable `status=failed`). Returns the claimed rows with
    /// status flipped to `processing`. Each backend's claim
    /// primitive differs (see module docs).
    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError>;

    /// Mark a claimed job as completed.
    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError>;

    /// Mark a job as stale (`processing` → `stale`). Used when the
    /// underlying capsule's `content_hash` changed mid-flight so
    /// the embedding output would target the wrong row.
    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError>;

    /// Reschedule a failed job for a future retry with exponential
    /// backoff. `new_attempt_count` is the post-increment count.
    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    /// Permanently fail a job — no more retries.
    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    /// Delete every job row for one capsule id. Used by
    /// `delete_capability_capsule_hard` to cascade.
    async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, StorageError>;

    /// Mark every live (`pending` or `processing`) job for one
    /// `(tenant, capsule_id, provider)` triple as stale. Used when
    /// a capsule's content changes — any in-flight embedding
    /// becomes targeting an obsolete content_hash.
    async fn stale_live_embedding_jobs_for_capability_capsule(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError>;

    /// Single-row status lookup by id. Returns the string form
    /// (`"pending"` / `"processing"` / `"completed"` / `"failed"`
    /// / `"stale"`).
    async fn get_embedding_job_status(&self, job_id: &str) -> Result<Option<String>, StorageError>;

    /// Most-recent job's status for a `(tenant, capsule_id,
    /// content_hash)` triple. Used by the embedding service to
    /// decide whether to enqueue a new job.
    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError>;

    /// Admin-page listing of jobs for `tenant`, optionally filtered
    /// by status and capsule id, bounded by `limit`.
    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError>;

    // ── Transcript embedding jobs ───────────────────────────────────
    //
    // No `try_enqueue_*` — transcript jobs are inserted inline by
    // `create_conversation_message` when the block is embed-eligible.
    // The claim / complete / fail state machine is the same as the
    // capsule-side queue.

    /// Same shape as [`Self::claim_next_n_embedding_jobs`] for the
    /// transcript-side queue.
    async fn claim_next_n_transcript_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError>;

    async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn reschedule_transcript_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn permanently_fail_transcript_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError>;
}

#[async_trait]
impl EmbeddingJobStore for Store {
    async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        Store::try_enqueue_embedding_job(self, insert).await
    }

    async fn enqueue_embedding_jobs(
        &self,
        inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError> {
        Store::enqueue_embedding_jobs(self, inserts).await
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        Store::claim_next_n_embedding_jobs(self, now, max_retries, n).await
    }

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        Store::complete_embedding_job(self, job_id, now).await
    }

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        Store::mark_embedding_job_stale(self, job_id, now).await
    }

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        Store::reschedule_embedding_job_failure(
            self,
            job_id,
            new_attempt_count,
            last_error,
            available_at,
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
        Store::permanently_fail_embedding_job(self, job_id, new_attempt_count, last_error, now)
            .await
    }

    async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, StorageError> {
        Store::delete_embedding_jobs_by_capability_capsule_id(self, capability_capsule_id).await
    }

    async fn stale_live_embedding_jobs_for_capability_capsule(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        Store::stale_live_embedding_jobs_for_capability_capsule(
            self,
            tenant,
            capability_capsule_id,
            provider,
            now,
        )
        .await
    }

    async fn get_embedding_job_status(&self, job_id: &str) -> Result<Option<String>, StorageError> {
        Store::get_embedding_job_status(self, job_id).await
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        self.lance
            .latest_embedding_job_status_for_hash(
                tenant,
                capability_capsule_id,
                target_content_hash,
            )
            .await
    }

    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        self.lance
            .list_embedding_jobs(tenant, status_filter, memory_id_filter, limit)
            .await
    }

    async fn claim_next_n_transcript_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        Store::claim_next_n_transcript_embedding_jobs(self, now, max_retries, n).await
    }

    async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        Store::complete_transcript_embedding_job(self, job_id, now).await
    }

    async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        Store::mark_transcript_embedding_job_stale(self, job_id, now).await
    }

    async fn reschedule_transcript_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        Store::reschedule_transcript_embedding_job_failure(
            self,
            job_id,
            new_attempt_count,
            last_error,
            available_at,
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
        Store::permanently_fail_transcript_embedding_job(
            self,
            job_id,
            new_attempt_count,
            last_error,
            now,
        )
        .await
    }

    async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        Store::get_transcript_embedding_job_status(self, job_id).await
    }
}
