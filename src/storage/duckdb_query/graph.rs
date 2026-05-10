//! Graph reads (`graph_edges` table). Methods inherent on
//! `DuckDbQuery`. Active-only by convention — closed edges
//! (`valid_to IS NOT NULL`) stay for audit but never enter recall.

use duckdb::params;

use super::{row_to_graph_edge, spawn_blocking_graph, DuckDbQuery};
use crate::domain::capability_capsule::GraphEdge;
use crate::storage::types::GraphError;

impl DuckDbQuery {
    /// Active edges incident on `node_id` (either endpoint). Only
    /// `valid_to IS NULL` are surfaced — closed (superseded) edges
    /// stay in the table for audit but never enter recall. Ordered
    /// `(relation, from_node_id, to_node_id)` for deterministic
    /// output (mirrors the legacy backend).
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.conn.clone();
        let node_id = node_id.to_string();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT from_node_id, to_node_id, relation, valid_from, valid_to \
                 FROM ns.main.graph_edges \
                 WHERE (from_node_id = ?1 OR to_node_id = ?1) AND valid_to IS NULL \
                 ORDER BY relation, from_node_id, to_node_id",
            )?;
            let rows = stmt.query_map(params![node_id], row_to_graph_edge)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Memory ids reachable in one hop from any of `node_ids`,
    /// across active edges only. Used by `pipeline::retrieve` to
    /// expand the candidate pool with graph neighbors of seed
    /// nodes (e.g. memories that share an entity).
    ///
    /// Implementation: pull all edges where either endpoint is in
    /// `node_ids`, then for each edge keep the **opposite** endpoint
    /// (the one not in the input set), strip the `memory:` prefix,
    /// dedupe via HashSet, sort. SQL `IN (...)` push-down handles
    /// the filter; the endpoint-selection logic stays in Rust
    /// because it's per-row and DuckDB has no clean "the side that
    /// is NOT in (...)" expression.
    ///
    /// Empty input short-circuits.
    pub async fn related_capability_capsule_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let node_ids: Vec<String> = node_ids.to_vec();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let placeholders = (1..=node_ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT from_node_id, to_node_id FROM ns.main.graph_edges \
                 WHERE (from_node_id IN ({placeholders}) OR to_node_id IN ({placeholders})) \
                   AND valid_to IS NULL"
            );
            let mut stmt = conn.prepare(&sql)?;
            // params are (bound 1..N then 1..N again — same set used
            // twice, once per IN clause).
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::with_capacity(node_ids.len());
            for n in &node_ids {
                params_vec.push(Box::new(n.clone()));
            }
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let node_set: std::collections::HashSet<&str> =
                node_ids.iter().map(|s| s.as_str()).collect();
            let mut capability_capsule_ids = std::collections::HashSet::new();
            for r in rows {
                let (from, to) = r?;
                for endpoint in [&from, &to] {
                    if !node_set.contains(endpoint.as_str()) {
                        if let Some(mid) = endpoint.strip_prefix("capability_capsule:") {
                            capability_capsule_ids.insert(mid.to_string());
                        }
                    }
                }
            }
            let mut out: Vec<String> = capability_capsule_ids.into_iter().collect();
            out.sort();
            Ok(out)
        })
        .await
    }
}
