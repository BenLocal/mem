//! Graph edges (`graph_edges` table). Methods previously bound by
//! the `GraphStore` trait, now inherent on `LanceStore`.

use arrow_array::RecordBatch;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{graph_edge_to_record_batch, record_batch_to_graph_edges, sql_quote, LanceStore};
use crate::domain::memory::GraphEdge;
use crate::storage::types::GraphError;

impl LanceStore {
    /// Read all `graph_edges` rows matching `filter`, parsed into
    /// [`GraphEdge`]s. Helper shared by `neighbors`, `related_memory_ids`,
    /// and the existence check in `sync_memory_edges`.
    pub async fn query_graph_edges(&self, filter: String) -> Result<Vec<GraphEdge>, GraphError> {
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let stream = table
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| GraphError::Backend(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(
                record_batch_to_graph_edges(b).map_err(|e| GraphError::Backend(e.to_string()))?,
            );
        }
        Ok(out)
    }

    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        // Active edges only (valid_to is null) where the node sits on
        // either side. Order by (relation, from, to) to match DuckDB's
        // SQL — done in-memory because LanceDB has no ORDER BY.
        let mut edges = self
            .query_graph_edges(format!(
                "(from_node_id = {0} OR to_node_id = {0}) AND valid_to IS NULL",
                sql_quote(node_id),
            ))
            .await?;
        edges.sort_by(|a, b| {
            a.relation
                .cmp(&b.relation)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        Ok(edges)
    }

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

    pub async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError> {
        let from = format!("memory:{memory_id}");
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

    pub async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(vec![]);
        }
        // Build "id IN ('a', 'b', ...)" — LanceDB supports SQL IN, so
        // we match the DuckDB shape directly. No CASE expression
        // though, so we project both endpoints in Rust below.
        let in_list = node_ids
            .iter()
            .map(|n| sql_quote(n))
            .collect::<Vec<_>>()
            .join(",");
        let filter = format!(
            "(from_node_id IN ({0}) OR to_node_id IN ({0})) AND valid_to IS NULL",
            in_list,
        );
        let edges = self.query_graph_edges(filter).await?;
        let node_set: std::collections::HashSet<&str> =
            node_ids.iter().map(|s| s.as_str()).collect();
        let mut memory_ids = std::collections::HashSet::new();
        for e in edges {
            // Adjacency: pick the endpoint that's NOT in node_ids; if
            // both sides are in node_ids, both are recorded (matches
            // the DuckDB "case when from in (...) then to else from"
            // semantics — the SELECT DISTINCT collapses the duplicate).
            for endpoint in [&e.from_node_id, &e.to_node_id] {
                if !node_set.contains(endpoint.as_str()) {
                    if let Some(memory_id) = endpoint.strip_prefix("memory:") {
                        memory_ids.insert(memory_id.to_string());
                    }
                }
            }
        }
        let mut out: Vec<String> = memory_ids.into_iter().collect();
        out.sort();
        Ok(out)
    }
}
