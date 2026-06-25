//! [`ClickHouseBackend`] — the `clickhouse::Client` wrapper.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P1).**

use clickhouse::Client;

use crate::storage::types::StorageError;

/// ClickHouse backend handle. Holds a clickhouse-rs [`Client`] (cheap to
/// clone; pools connections internally). Implements [`CapsuleStore`] today
/// (P1); the remaining sub-traits + `Backend` arrive in P2+.
///
/// [`CapsuleStore`]: crate::storage::capsule_store::CapsuleStore
pub struct ClickHouseBackend {
    pub(crate) client: Client,
}

impl ClickHouseBackend {
    /// Build a backend from a `MEM_CLICKHOUSE_URL` (e.g.
    /// `http://localhost:8123`). The clickhouse-rs client is lazy — it
    /// opens no socket here, so a bad URL surfaces on first query, not at
    /// construction. Kept `async` to mirror `PostgresCapsuleStore::connect`
    /// and leave room for an eager ping in P2.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        if url.trim().is_empty() {
            return Err(StorageError::InvalidInput(
                "MEM_CLICKHOUSE_URL is empty".to_owned(),
            ));
        }
        let client = Client::default().with_url(url);
        Ok(Self { client })
    }

    /// Idempotently apply `migrations/clickhouse/0001_capsule_store.sql`
    /// (`CREATE TABLE IF NOT EXISTS …`). clickhouse-rs runs one statement
    /// per `execute()`, so the file is split on `;`. Used by the gated
    /// integration test; not on the serve path.
    pub async fn apply_migrations(&self) -> Result<(), StorageError> {
        const SQL: &str = include_str!("../../../migrations/clickhouse/0001_capsule_store.sql");
        for stmt in SQL.split(';') {
            let trimmed = stmt.trim();
            if trimmed.is_empty() || trimmed.starts_with("--") {
                continue;
            }
            self.client
                .query(trimmed)
                .execute()
                .await
                .map_err(|e| StorageError::InvalidInput(format!("clickhouse migration: {e}")))?;
        }
        Ok(())
    }
}
