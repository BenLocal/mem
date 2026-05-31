//! Graph edges (`graph_edges` table). LanceStore-side WRITE methods
//! only — `sync_memory_edges`, `add_edge_direct`, `invalidate_edge`,
//! `close_edges_for_capability_capsule`. Reads (neighbors, related
//! capsule ids, BFS, kg_timeline, graph_stats) all live on
//! `DuckDbQuery` and are reached via `Store::neighbors` etc.

use super::{graph_edge_to_record_batch, sql_quote, LanceStore};
use crate::domain::capability_capsule::GraphEdge;
use crate::storage::types::GraphError;

impl LanceStore {
    pub async fn sync_memory_edges(
        &self,
        edges: &[GraphEdge],
        now: &str,
    ) -> Result<(), GraphError> {
        if edges.is_empty() {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        // Idempotent insert: skip rows where an active edge with the
        // same (from, to, relation) already exists. LanceDB has no
        // transactions; a concurrent writer could race the existence
        // check, but mem serve is single-instance per DB so this is
        // safe in practice (same posture as embedding_jobs enqueue).
        for edge in edges {
            let exists = table
                .count_rows(Some(format!(
                    "from_node_id = {} AND to_node_id = {} AND relation = {} AND valid_to IS NULL",
                    sql_quote(&edge.from_node_id),
                    sql_quote(&edge.to_node_id),
                    sql_quote(&edge.relation),
                )))
                .await
                .map_err(|e| GraphError::Backend(e.to_string()))?;
            if exists > 0 {
                continue;
            }
            // Server overrides valid_from with `now` (matching DuckDB
            // behavior — callers don't need to think about clocks) and
            // forces valid_to = NULL (active).
            let to_write = GraphEdge {
                from_node_id: edge.from_node_id.clone(),
                to_node_id: edge.to_node_id.clone(),
                relation: edge.relation.clone(),
                valid_from: now.to_string(),
                valid_to: None,
                confidence: edge.confidence,
                extractor: edge.extractor.clone(),
                strength: edge.strength,
                stability: edge.stability,
                last_activated: edge.last_activated.clone(),
                access_count: edge.access_count,
            };
            let batch = graph_edge_to_record_batch(&to_write)
                .map_err(|e| GraphError::Backend(e.to_string()))?;
            table
                .add(batch)
                .execute()
                .await
                .map_err(|e| GraphError::Backend(e.to_string()))?;
        }
        Ok(())
    }

    /// Write one caller-supplied edge. Same idempotency rule as
    /// [`Self::sync_memory_edges`]: skip when an active edge with the
    /// same `(from, to, relation)` already exists. `valid_from` is
    /// taken from `edge` (unlike `sync_memory_edges`, which overrides
    /// it with the server's `now` — callers using this direct path
    /// can backdate edges, useful for importing historical facts).
    /// `valid_to` from `edge` is preserved verbatim so the caller can
    /// insert a pre-closed edge in one shot if they want.
    ///
    /// Returns `true` if the edge was actually written, `false` if
    /// the idempotency check found a duplicate.
    pub async fn add_edge_direct(&self, edge: &GraphEdge) -> Result<bool, GraphError> {
        // K12 (closes mempalace-diff-v4 K12): reject an inverted
        // bitemporal interval. An edge whose `valid_to` precedes its
        // `valid_from` can never satisfy the recall filter
        // (`valid_from <= as_of AND (valid_to IS NULL OR valid_to > as_of)`),
        // so it would be stored-but-permanently-invisible — the P0
        // foot-gun mempalace fixed in #1214. Open intervals (`None`) and
        // point-in-time facts (`valid_to == valid_from`) stay valid.
        if let Some(valid_to) = &edge.valid_to {
            if valid_to.as_str() < edge.valid_from.as_str() {
                return Err(GraphError::InvalidInput(format!(
                    "edge valid_to ({}) precedes valid_from ({}); a recall query would never match it",
                    valid_to, edge.valid_from
                )));
            }
        }
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let exists = table
            .count_rows(Some(format!(
                "from_node_id = {} AND to_node_id = {} AND relation = {} AND valid_to IS NULL",
                sql_quote(&edge.from_node_id),
                sql_quote(&edge.to_node_id),
                sql_quote(&edge.relation),
            )))
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        if exists > 0 {
            return Ok(false);
        }
        let batch =
            graph_edge_to_record_batch(edge).map_err(|e| GraphError::Backend(e.to_string()))?;
        table
            .add(batch)
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        Ok(true)
    }

    /// Invalidate (close) one active edge identified by the
    /// `(from, predicate, to)` triple by setting `valid_to = ended_at`.
    /// Idempotent: a triple that has no active edge (already closed
    /// or never existed) returns `0` without erroring. Use this when
    /// the caller learns a previously-true fact is no longer true at
    /// a specific time — the parallel to MemPalace's
    /// `tool_kg_invalidate(subj, pred, obj, ended)`.
    pub async fn invalidate_edge(
        &self,
        from_node_id: &str,
        predicate: &str,
        to_node_id: &str,
        ended_at: &str,
    ) -> Result<usize, GraphError> {
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let filter = format!(
            "from_node_id = {} AND to_node_id = {} AND relation = {} AND valid_to IS NULL",
            sql_quote(from_node_id),
            sql_quote(to_node_id),
            sql_quote(predicate),
        );
        let count = table
            .count_rows(Some(filter.clone()))
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .update()
            .only_if(filter)
            .column("valid_to", sql_quote(ended_at))
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        Ok(usize::try_from(result.rows_updated).unwrap_or(count as usize))
    }

    pub async fn close_edges_for_capability_capsule(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, GraphError> {
        let from = format!("capability_capsule:{capability_capsule_id}");
        let now = crate::storage::current_timestamp();
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let filter = format!("from_node_id = {} AND valid_to IS NULL", sql_quote(&from));
        let count = table
            .count_rows(Some(filter.clone()))
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .update()
            .only_if(filter)
            .column("valid_to", sql_quote(&now))
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        if result.rows_updated == 0 {
            Ok(count)
        } else {
            Ok(usize::try_from(result.rows_updated).unwrap_or(count))
        }
    }
}

// Tests removed: this file used to host a `lancedb_graph_store_round_trip`
// that wrote via `sync_memory_edges` and read back via `neighbors` /
// `related_capability_capsule_ids` on `LanceStore`. The reads now
// live solely on `DuckDbQuery`, so the round trip is canonically
// tested in `src/storage/duckdb_query/graph.rs::tests` —
// `neighbors_within_walks_multi_hop_and_dedupes`,
// `kg_timeline_includes_closed_edges_in_chrono_order`,
// `list_user_tunnels_filters_by_relation_prefix`,
// `graph_stats_counts_split_and_top_relations` — all of which seed
// data with `sync_memory_edges` and assert the read shape via the
// canonical DuckDB-side methods.
