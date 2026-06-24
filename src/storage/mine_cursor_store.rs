//! Backend-agnostic mine-cursor store — v3 #32 sub-trait.
//!
//! Records the highest line number that the `mem mine` CLI has
//! shipped to the server for each transcript file, so re-runs can
//! fast-skip already-processed lines. Pure perf optimization (server
//! still dedupes via idempotency_key + content_hash), so a missing /
//! stale cursor just makes the next mine slower — never wrong.

use async_trait::async_trait;

pub use crate::storage::lance_store::mine_cursors::MineCursor;
use crate::storage::types::StorageError;
use crate::storage::Store;

#[async_trait]
pub trait MineCursorStore: Send + Sync {
    /// Read the cursor for `transcript_path`. `None` when no row
    /// exists yet (first time mining this file).
    async fn get_mine_cursor(
        &self,
        transcript_path: &str,
    ) -> Result<Option<MineCursor>, StorageError>;

    /// Upsert the cursor for `transcript_path`. `last_line_number`
    /// should be monotonically non-decreasing across calls (the
    /// server doesn't enforce this — `mem mine` only writes after a
    /// successful batch).
    async fn upsert_mine_cursor(
        &self,
        transcript_path: &str,
        last_line_number: i64,
        updated_at: &str,
    ) -> Result<(), StorageError>;
}

#[async_trait]
impl MineCursorStore for Store {
    async fn get_mine_cursor(
        &self,
        transcript_path: &str,
    ) -> Result<Option<MineCursor>, StorageError> {
        self.lance.get_mine_cursor(transcript_path).await
    }

    async fn upsert_mine_cursor(
        &self,
        transcript_path: &str,
        last_line_number: i64,
        updated_at: &str,
    ) -> Result<(), StorageError> {
        // Route the write through commit_lance_write for a uniform write
        // shape (it's a pass-through since route-B removed the DuckDB read
        // engine; reads are lance-native, and nothing reads mine_cursors
        // on the recall path anyway).
        self.commit_lance_write(
            self.lance
                .upsert_mine_cursor(transcript_path, last_line_number, updated_at)
                .await,
        )
        .await
    }
}
