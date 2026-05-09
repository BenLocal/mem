//! Entity-registry reads (`entities` + `entity_aliases` tables).
//! Methods inherent on `DuckDbQuery`.

use duckdb::{params, OptionalExt};

use super::{row_to_entity, spawn_blocking_storage, DuckDbQuery};
use crate::domain::{Entity, EntityKind, EntityWithAliases};
use crate::pipeline::entity_normalize::normalize_alias;
use crate::storage::types::StorageError;

impl DuckDbQuery {
    /// Fetch an entity row plus its alias list (ordered
    /// `created_at ASC, alias_text ASC`). Returns `Ok(None)` when no
    /// row matches `(tenant, entity_id)`. Two SELECTs because DuckDB
    /// SQL `array_agg(... ORDER BY ...)` would force the alias rows
    /// onto a single GROUP BY row but the legacy code keeps them
    /// in distinct rows; we mirror its shape.
    pub async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let entity_id = entity_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let entity = conn
                .query_row(
                    "SELECT entity_id, tenant, canonical_name, kind, created_at \
                     FROM ns.main.entities \
                     WHERE tenant = ?1 AND entity_id = ?2",
                    params![&tenant, &entity_id],
                    row_to_entity,
                )
                .optional()
                .map_err(StorageError::DuckDb)?;
            let Some(entity) = entity else {
                return Ok(None);
            };

            let mut stmt = conn.prepare(
                "SELECT alias_text FROM ns.main.entity_aliases \
                 WHERE tenant = ?1 AND entity_id = ?2 \
                 ORDER BY created_at ASC, alias_text ASC",
            )?;
            let rows =
                stmt.query_map(params![&tenant, &entity_id], |row| row.get::<_, String>(0))?;
            let mut aliases = Vec::new();
            for r in rows {
                aliases.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(Some(EntityWithAliases { entity, aliases }))
        })
        .await
    }

    /// Read-only normalized-alias lookup: returns the `entity_id`
    /// currently bound to `normalize_alias(alias)` under `tenant`,
    /// or `None`. Used by service-layer flows that need to pre-check
    /// alias ownership before attempting writes.
    pub async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        let normalized = normalize_alias(alias);
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.query_row(
                "SELECT entity_id FROM ns.main.entity_aliases \
                 WHERE tenant = ?1 AND alias_text = ?2",
                params![tenant, normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// List entities under `tenant`, optionally filtered by `kind`
    /// and a `LIKE`-substring on `canonical_name`. Ordered
    /// `created_at DESC`, capped at `limit`.
    ///
    /// The `LIKE` pattern is parameterised — wrap the query in
    /// `%...%` so substring match works without the caller knowing
    /// about SQL wildcards. (DuckDB `LIKE` is case-sensitive; the
    /// legacy backend was the same — kept for parity. A future
    /// follow-up could swap to `ILIKE` for case-insensitive
    /// search.)
    pub async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let kind_filter = kind_filter.map(|k| k.as_db_str().to_string());
        let query = query.map(|q| format!("%{q}%"));
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut sql = String::from(
                "SELECT entity_id, tenant, canonical_name, kind, created_at \
                 FROM ns.main.entities WHERE tenant = ?1",
            );
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant)];
            if let Some(k) = kind_filter {
                sql.push_str(&format!(" AND kind = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(k));
            }
            if let Some(pat) = query {
                sql.push_str(&format!(
                    " AND canonical_name LIKE ?{}",
                    params_vec.len() + 1
                ));
                params_vec.push(Box::new(pat));
            }
            sql.push_str(" ORDER BY created_at DESC");
            sql.push_str(&format!(" LIMIT ?{}", params_vec.len() + 1));
            params_vec.push(Box::new(lim));

            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_entity)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }
}
