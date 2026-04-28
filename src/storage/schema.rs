use duckdb::Connection;
use tracing::info;

use super::duckdb::{migrate_content_hash_to_sha256, StorageError};

const INIT_SCHEMA_SQL: &str = include_str!("../../db/schema/001_init.sql");
const EMBEDDINGS_SCHEMA_SQL: &str = include_str!("../../db/schema/002_embeddings.sql");
const GRAPH_SCHEMA_SQL: &str = include_str!("../../db/schema/003_graph.sql");

pub fn bootstrap(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(INIT_SCHEMA_SQL)?;
    conn.execute_batch(EMBEDDINGS_SCHEMA_SQL)?;
    conn.execute_batch(GRAPH_SCHEMA_SQL)?;
    let migrated = migrate_content_hash_to_sha256(conn)?;
    if migrated > 0 {
        info!(
            count = migrated,
            "migrated legacy content_hash rows to sha256 (mempalace-diff §8 #1)"
        );
    }
    Ok(())
}
