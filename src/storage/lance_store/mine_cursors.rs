//! `mine_cursors` table — per-transcript-file high-water mark for the
//! `mem mine` CLI's incremental fast-path (v3 #32).
//!
//! Semantics: each row records the largest `line_number` that the
//! client has shipped to the server for a given `transcript_path`. A
//! re-run of `mine` against that path queries the cursor first and
//! skips parsed memories / blocks whose `line_number <= cursor`,
//! avoiding the parse + HTTP round-trip cost for content the server
//! has already deduped.
//!
//! **Cursor is a perf hint, not a correctness boundary.** Server-side
//! dedup (idempotency_key + content_hash on capsules; (path, line,
//! block_index) triple on blocks) still catches anything that slips
//! past, so a stale or missing cursor degrades to "re-mine + re-dedup"
//! — slower but produces the same final state.

use std::sync::Arc;

use arrow_array::{
    builder::{Int64Builder, StringBuilder},
    Int64Array, RecordBatch, StringArray,
};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{lancedb_err, mine_cursors_schema, parse_col, sql_quote, LanceStore};
use crate::storage::types::StorageError;

/// One row of the `mine_cursors` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MineCursor {
    pub transcript_path: String,
    pub last_line_number: i64,
    pub updated_at: String,
}

fn mine_cursor_to_record_batch(c: &MineCursor) -> Result<RecordBatch, StorageError> {
    let mut path = StringBuilder::new();
    let mut line = Int64Builder::new();
    let mut updated = StringBuilder::new();
    path.append_value(&c.transcript_path);
    line.append_value(c.last_line_number);
    updated.append_value(&c.updated_at);
    let schema = Arc::new(mine_cursors_schema());
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(path.finish()),
            Arc::new(line.finish()),
            Arc::new(updated.finish()),
        ],
    )
    .map_err(|e| StorageError::InvalidInput(format!("arrow batch: {e}")))
}

fn record_batch_to_mine_cursors(batch: &RecordBatch) -> Result<Vec<MineCursor>, StorageError> {
    const TABLE: &str = "mine_cursors";
    let path = parse_col::<StringArray>(batch, TABLE, "transcript_path")?;
    let line = parse_col::<Int64Array>(batch, TABLE, "last_line_number")?;
    let updated = parse_col::<StringArray>(batch, TABLE, "updated_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(MineCursor {
            transcript_path: path.value(i).to_string(),
            last_line_number: line.value(i),
            updated_at: updated.value(i).to_string(),
        });
    }
    Ok(out)
}

impl LanceStore {
    /// Read the cursor for `transcript_path`, or `None` if no row
    /// exists yet (first time mining this file).
    pub async fn get_mine_cursor(
        &self,
        transcript_path: &str,
    ) -> Result<Option<MineCursor>, StorageError> {
        let table = self
            .conn
            .open_table("mine_cursors")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!("transcript_path = {}", sql_quote(transcript_path),))
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        for b in &batches {
            let rows = record_batch_to_mine_cursors(b)?;
            if let Some(row) = rows.into_iter().next() {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    /// Upsert the cursor for `transcript_path`. Deletes any existing
    /// row first (LanceDB has no PK enforcement) then appends the new
    /// one. `last_line_number` must be monotonically non-decreasing
    /// across calls for a given path — the server doesn't enforce
    /// this; callers (`mem mine`) only update after a successful
    /// batch round-trip.
    pub async fn upsert_mine_cursor(
        &self,
        transcript_path: &str,
        last_line_number: i64,
        updated_at: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("mine_cursors")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .delete(&format!("transcript_path = {}", sql_quote(transcript_path),))
            .await
            .map_err(lancedb_err)?;
        let batch = mine_cursor_to_record_batch(&MineCursor {
            transcript_path: transcript_path.to_string(),
            last_line_number,
            updated_at: updated_at.to_string(),
        })?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(())
    }
}
