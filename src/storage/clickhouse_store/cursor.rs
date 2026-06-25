//! `MineCursorStore` for [`ClickHouseBackend`].
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** Versioned insert + `FINAL`/`row_version DESC`
//! read over a `ReplacingMergeTree`, same §4(a) shape as the rest.

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, now_version};
use crate::storage::lance_store::mine_cursors::MineCursor;
use crate::storage::types::StorageError;
use crate::storage::MineCursorStore;

#[derive(Row, Serialize, Deserialize)]
struct ChMineCursorRow {
    transcript_path: String,
    last_line_number: i64,
    updated_at: String,
    row_version: u64,
}

#[async_trait]
impl MineCursorStore for ClickHouseBackend {
    async fn get_mine_cursor(
        &self,
        transcript_path: &str,
    ) -> Result<Option<MineCursor>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM mine_cursors FINAL WHERE transcript_path = ? \
                 ORDER BY row_version DESC LIMIT 1",
            )
            .bind(transcript_path)
            .fetch_all::<ChMineCursorRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().next().map(|r| MineCursor {
            transcript_path: r.transcript_path,
            last_line_number: r.last_line_number,
            updated_at: r.updated_at,
        }))
    }

    async fn upsert_mine_cursor(
        &self,
        transcript_path: &str,
        last_line_number: i64,
        updated_at: &str,
    ) -> Result<(), StorageError> {
        let mut insert = self
            .client
            .insert::<ChMineCursorRow>("mine_cursors")
            .await
            .map_err(ch_err)?;
        insert
            .write(&ChMineCursorRow {
                transcript_path: transcript_path.to_owned(),
                last_line_number,
                updated_at: updated_at.to_owned(),
                row_version: now_version(),
            })
            .await
            .map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }
}
