use duckdb::Connection;
use tracing::info;

use super::duckdb::{migrate_content_hash_to_sha256, StorageError};

const INIT_SCHEMA_SQL: &str = include_str!("../../db/schema/001_init.sql");
const EMBEDDINGS_SCHEMA_SQL: &str = include_str!("../../db/schema/002_embeddings.sql");
const GRAPH_SCHEMA_SQL: &str = include_str!("../../db/schema/003_graph.sql");
// 004_sessions.sql contains `alter table memories add column session_id`, which
// DuckDB does not support with `if not exists`. On a re-run the statement fails
// with "Column with name ... already exists!". We apply this file
// statement-by-statement and swallow that specific error so the migration is
// idempotent. See docs/superpowers/specs/2026-04-29-sessions-design.md §DuckDB
// caveats.
const SESSIONS_SCHEMA_SQL: &str = include_str!("../../db/schema/004_sessions.sql");
const CONVERSATION_MESSAGES_SCHEMA_SQL: &str =
    include_str!("../../db/schema/005_conversation_messages.sql");

pub fn bootstrap(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(INIT_SCHEMA_SQL)?;
    conn.execute_batch(EMBEDDINGS_SCHEMA_SQL)?;
    conn.execute_batch(GRAPH_SCHEMA_SQL)?;
    apply_sessions_schema(conn)?;
    conn.execute_batch(CONVERSATION_MESSAGES_SCHEMA_SQL)?;
    let migrated = migrate_content_hash_to_sha256(conn)?;
    if migrated > 0 {
        info!(
            count = migrated,
            "migrated legacy content_hash rows to sha256 (mempalace-diff §8 #1)"
        );
    }
    Ok(())
}

/// Apply `004_sessions.sql` statement-by-statement so that the
/// `ALTER TABLE memories ADD COLUMN session_id` line is idempotent.
/// DuckDB rejects a second `ALTER TABLE ADD COLUMN` for an existing column
/// with "Column with name … already exists!" — we swallow only that error.
fn apply_sessions_schema(conn: &Connection) -> Result<(), StorageError> {
    for stmt in SESSIONS_SCHEMA_SQL
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Err(e) = conn.execute_batch(stmt) {
            // Tolerate re-application of the ALTER TABLE ADD COLUMN: the column
            // is already present from a previous bootstrap run.
            let msg = e.to_string();
            if stmt.to_ascii_lowercase().contains("alter table")
                && msg.to_ascii_lowercase().contains("already exists")
            {
                // Column already present — safe to continue.
                continue;
            }
            return Err(StorageError::DuckDb(e));
        }
    }
    Ok(())
}
