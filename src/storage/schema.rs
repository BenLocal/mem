use duckdb::Connection;

use super::duckdb::StorageError;

const INIT_SCHEMA_SQL: &str = include_str!("../../db/schema/001_init.sql");
const EMBEDDINGS_SCHEMA_SQL: &str = include_str!("../../db/schema/002_embeddings.sql");

pub fn bootstrap(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(INIT_SCHEMA_SQL)?;
    conn.execute_batch(EMBEDDINGS_SCHEMA_SQL)?;
    Ok(())
}
