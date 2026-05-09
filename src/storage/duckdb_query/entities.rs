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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EntityKind;
    use crate::storage::lance_store::LanceStore;
    use tempfile::tempdir;

    ///     canonical_name, created_at DESC ordering, limit
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_graph_and_entity_reads() {
        use crate::domain::capability_capsule::GraphEdge;

        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // ── seed graph_edges via the writer ─────────────────────
        // mem:m1 mentions ent:e1; mem:m2 mentions ent:e1; mem:m1
        // discusses ent:e2. All active.
        let edges = vec![
            GraphEdge {
                from_node_id: "capability_capsule:m1".into(),
                to_node_id: "entity:e1".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000000000".into(),
                valid_to: None,
            },
            GraphEdge {
                from_node_id: "capability_capsule:m2".into(),
                to_node_id: "entity:e1".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000000000".into(),
                valid_to: None,
            },
            GraphEdge {
                from_node_id: "capability_capsule:m1".into(),
                to_node_id: "entity:e2".into(),
                relation: "discusses".into(),
                valid_from: "00000001778000000000".into(),
                valid_to: None,
            },
        ];
        lance
            .sync_memory_edges(&edges, "00000001778000010000")
            .await
            .unwrap();
        // Add a closed edge — must NOT surface from neighbors /
        // related_capability_capsule_ids. Easiest way: add then close.
        lance
            .sync_memory_edges(
                &[GraphEdge {
                    from_node_id: "capability_capsule:m_closed".into(),
                    to_node_id: "entity:e1".into(),
                    relation: "mentions".into(),
                    valid_from: "00000001778000000000".into(),
                    valid_to: None,
                }],
                "00000001778000010000",
            )
            .await
            .unwrap();
        lance
            .close_edges_for_capability_capsule("m_closed")
            .await
            .unwrap();

        // ── seed entities + aliases via the writer ──────────────
        let id_rust = lance
            .resolve_or_create(
                "tenant-a",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000000",
            )
            .await
            .unwrap();
        let id_duck = lance
            .resolve_or_create(
                "tenant-a",
                "DuckDB",
                EntityKind::Project,
                "00000001778000000010",
            )
            .await
            .unwrap();
        let id_b = lance
            .resolve_or_create(
                "tenant-b",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000005",
            )
            .await
            .unwrap();
        // Add a second alias on id_rust.
        lance
            .add_alias("tenant-a", &id_rust, "Tokio", "00000001778000000020")
            .await
            .unwrap();

        let q = DuckDbQuery::open(&path).await.unwrap();

        // ── graph: neighbors ────────────────────────────────────
        // entity:e1 has 2 active neighbors (m1, m2 via 'mentions');
        // m_closed's edge is closed → excluded. Order: relation,
        // from, to → 'mentions'/m1, 'mentions'/m2.
        let n_e1 = q.neighbors("entity:e1").await.unwrap();
        assert_eq!(n_e1.len(), 2);
        assert_eq!(n_e1[0].from_node_id, "capability_capsule:m1");
        assert_eq!(n_e1[1].from_node_id, "capability_capsule:m2");

        // capability_capsule:m1 has 2 active outgoing edges.
        let n_m1 = q.neighbors("capability_capsule:m1").await.unwrap();
        assert_eq!(n_m1.len(), 2);

        // No-neighbor node returns empty (not error).
        let n_none = q.neighbors("entity:nonexistent").await.unwrap();
        assert!(n_none.is_empty());

        // ── graph: related_capability_capsule_ids ───────────────────────────
        // Empty input → empty Vec.
        let r_empty = q.related_capability_capsule_ids(&[]).await.unwrap();
        assert!(r_empty.is_empty());

        // Seeds [e1, e2] → reachable memories: m1 (via e1+e2), m2
        // (via e1). Output sorted; dedupe by HashSet.
        let r = q
            .related_capability_capsule_ids(&["entity:e1".into(), "entity:e2".into()])
            .await
            .unwrap();
        assert_eq!(r, vec!["m1".to_string(), "m2".to_string()]);

        // ── entity: lookup_alias ────────────────────────────────
        // Caller-verbatim casing/whitespace collapses to the same
        // normalized form as the seed.
        let look = q.lookup_alias("tenant-a", "rust async").await.unwrap();
        assert_eq!(look.as_deref(), Some(id_rust.as_str()));
        let look_ws = q
            .lookup_alias("tenant-a", "  RUST   ASYNC  ")
            .await
            .unwrap();
        assert_eq!(look_ws.as_deref(), Some(id_rust.as_str()));
        let look_other = q.lookup_alias("tenant-a", "Tokio").await.unwrap();
        assert_eq!(look_other.as_deref(), Some(id_rust.as_str()));
        let miss = q.lookup_alias("tenant-a", "unknown").await.unwrap();
        assert!(miss.is_none());

        // ── entity: get_entity ──────────────────────────────────
        let with_aliases = q
            .get_entity("tenant-a", &id_rust)
            .await
            .unwrap()
            .expect("rust entity exists");
        assert_eq!(with_aliases.entity.canonical_name, "Rust Async");
        assert_eq!(with_aliases.entity.kind, EntityKind::Topic);
        // Aliases ordered by created_at ASC: 'rust async' (added at
        // resolve_or_create time, earlier ts) then 'tokio'.
        assert_eq!(
            with_aliases.aliases,
            vec!["rust async".to_string(), "tokio".to_string()]
        );

        let none = q.get_entity("tenant-a", "does-not-exist").await.unwrap();
        assert!(none.is_none());

        // ── entity: list_entities ───────────────────────────────
        // tenant-a has 2 entities, ordered created_at DESC: id_duck
        // (later ts) → id_rust.
        let all_a = q.list_entities("tenant-a", None, None, 10).await.unwrap();
        assert_eq!(all_a.len(), 2);
        assert_eq!(all_a[0].entity_id, id_duck);
        assert_eq!(all_a[1].entity_id, id_rust);

        // kind filter.
        let topics = q
            .list_entities("tenant-a", Some(EntityKind::Topic), None, 10)
            .await
            .unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].entity_id, id_rust);

        // LIKE filter on canonical_name (case-sensitive, mirrors
        // legacy backend).
        let like = q
            .list_entities("tenant-a", None, Some("Rust"), 10)
            .await
            .unwrap();
        assert_eq!(like.len(), 1);
        assert_eq!(like[0].canonical_name, "Rust Async");

        // tenant-b has only id_b (cross-tenant duplicate alias →
        // distinct entity).
        let all_b = q.list_entities("tenant-b", None, None, 10).await.unwrap();
        assert_eq!(all_b.len(), 1);
        assert_eq!(all_b[0].entity_id, id_b);
    }
}
