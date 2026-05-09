//! Embedding-job status reads (`embedding_jobs` /
//! `transcript_embedding_jobs` tables). Methods inherent on
//! `DuckDbQuery`. Used by both workers to skip mid-flight processing
//! when a concurrent caller has marked the job stale.

use duckdb::{params, OptionalExt};

use super::{spawn_blocking_storage, DuckDbQuery};
use crate::storage::types::StorageError;

impl DuckDbQuery {
    /// Read the `status` column of an embedding_jobs row by id. Used
    /// by the embedding worker to skip mid-flight processing if a
    /// concurrent caller (e.g. a supersede flow) marked the job
    /// stale before the embed completed. `None` if the row is gone.
    pub async fn get_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn.clone();
        let job_id = job_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.query_row(
                "SELECT status FROM ns.main.embedding_jobs WHERE job_id = ?1",
                params![job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Same shape as [`Self::get_embedding_job_status`] but for the
    /// transcript-side queue. Used by the transcript embedding worker
    /// to skip mid-flight processing if the job got marked stale by
    /// a concurrent caller.
    pub async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn.clone();
        let job_id = job_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.query_row(
                "SELECT status FROM ns.main.transcript_embedding_jobs WHERE job_id = ?1",
                params![job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }
}
