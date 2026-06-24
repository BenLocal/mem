//! Graph edges (`graph_edges` table). LanceStore-side WRITE methods —
//! `sync_memory_edges`, `add_edge_direct`, `invalidate_edge`,
//! `close_edges_for_capability_capsule` — plus the Route-B graph NATIVE
//! READS. The "graph" bucket: `neighbors_within` (iterative BFS),
//! `related_capability_capsule_ids`, `graph_stats`. The "graph-tunnel"
//! bucket (batch C): `neighbors`, `get_active_edge`, `kg_timeline`,
//! `query_predicate`, `list_user_tunnels`, `find_tunnels`,
//! `follow_tunnels`, `incident_edges_for_nodes`. All mirror the DuckDB
//! backend (`DuckDbQuery::*`) field-for-field and are parity-gated by
//! `tests/parity_golden.rs`; `Store` routes between the two engines on
//! `read_engine`. With batch C every graph read has a lance-native arm.

use arrow_array::{Array, Float32Array, Int64Array, RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{graph_edge_to_record_batch, parse_col, sql_quote, LanceStore};
use crate::domain::capability_capsule::{GraphEdge, GraphStats};
use crate::storage::types::GraphError;

/// Upper cap on `neighbors_within` `max_hops`. Mirrors
/// `DuckDbQuery::MAX_HOPS_CAP` so the two engines clamp identically.
const MAX_HOPS_CAP: u32 = 3;

/// Upper cap on the BFS visited-set size. Mirrors
/// `DuckDbQuery::NEIGHBORS_VISITED_CAP`.
const NEIGHBORS_VISITED_CAP: usize = 10_000;

/// Top-N relation kinds reported by [`LanceStore::graph_stats`]. Mirrors
/// the DuckDB `... ORDER BY c DESC, relation ASC LIMIT 16`.
const TOP_RELATIONS_LIMIT: usize = 16;

/// Parse one or more `graph_edges` record batches into [`GraphEdge`]s.
/// Mirrors `DuckDbQuery::row_to_graph_edge` field-for-field: the five
/// required columns plus the six nullable K1/K3/K9 columns.
fn record_batch_to_graph_edges(batch: &RecordBatch) -> Result<Vec<GraphEdge>, GraphError> {
    const TABLE: &str = "graph_edges";
    let from = parse_col::<StringArray>(batch, TABLE, "from_node_id")?;
    let to = parse_col::<StringArray>(batch, TABLE, "to_node_id")?;
    let relation = parse_col::<StringArray>(batch, TABLE, "relation")?;
    let valid_from = parse_col::<StringArray>(batch, TABLE, "valid_from")?;
    let valid_to = parse_col::<StringArray>(batch, TABLE, "valid_to")?;
    let confidence = parse_col::<Float32Array>(batch, TABLE, "confidence")?;
    let extractor = parse_col::<StringArray>(batch, TABLE, "extractor")?;
    let strength = parse_col::<Float32Array>(batch, TABLE, "strength")?;
    let stability = parse_col::<Float32Array>(batch, TABLE, "stability")?;
    let last_activated = parse_col::<StringArray>(batch, TABLE, "last_activated")?;
    let access_count = parse_col::<Int64Array>(batch, TABLE, "access_count")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(GraphEdge {
            from_node_id: from.value(i).to_string(),
            to_node_id: to.value(i).to_string(),
            relation: relation.value(i).to_string(),
            valid_from: valid_from.value(i).to_string(),
            valid_to: opt_str(valid_to, i),
            confidence: if confidence.is_null(i) {
                None
            } else {
                Some(confidence.value(i))
            },
            extractor: opt_str(extractor, i),
            strength: if strength.is_null(i) {
                None
            } else {
                Some(strength.value(i))
            },
            stability: if stability.is_null(i) {
                None
            } else {
                Some(stability.value(i))
            },
            last_activated: opt_str(last_activated, i),
            access_count: if access_count.is_null(i) {
                None
            } else {
                Some(access_count.value(i))
            },
        });
    }
    Ok(out)
}

fn opt_str(arr: &StringArray, i: usize) -> Option<String> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i).to_string())
    }
}

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

    /// Overwrite the K9 dynamics columns of the active
    /// `(from, to, relation)` edge. The caller computes the new values
    /// via [`crate::domain::edge_dynamics::potentiate`]. Returns `false`
    /// when no active edge matches (it was closed/superseded between the
    /// access and the potentiation — the event is simply dropped). Only
    /// the `Some` fields are written, as SQL update expressions.
    pub async fn update_edge_dynamics(&self, edge: &GraphEdge) -> Result<bool, GraphError> {
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let filter = format!(
            "from_node_id = {} AND to_node_id = {} AND relation = {} AND valid_to IS NULL",
            sql_quote(&edge.from_node_id),
            sql_quote(&edge.to_node_id),
            sql_quote(&edge.relation),
        );
        let mut upd = table.update().only_if(filter);
        if let Some(v) = edge.strength {
            upd = upd.column("strength", v.to_string());
        }
        if let Some(v) = edge.stability {
            upd = upd.column("stability", v.to_string());
        }
        if let Some(v) = &edge.last_activated {
            upd = upd.column("last_activated", sql_quote(v));
        }
        if let Some(v) = edge.access_count {
            upd = upd.column("access_count", v.to_string());
        }
        let result = upd
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        Ok(result.rows_updated > 0)
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

    /// Run a `graph_edges` filter query and parse all returned batches
    /// into [`GraphEdge`]s. Shared by the BFS hop reads and the
    /// related-capsule scan. Mirrors `LanceStore::query_capability_capsules`.
    async fn query_graph_edges(&self, filter: String) -> Result<Vec<GraphEdge>, GraphError> {
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
            out.extend(record_batch_to_graph_edges(b)?);
        }
        Ok(out)
    }

    /// Route-B bucket "graph": native lancedb-Rust equivalent of
    /// `DuckDbQuery::neighbors_within`. Iterative BFS (≤
    /// `MAX_HOPS_CAP = 3` hops) over `graph_edges`, default active-only
    /// (`valid_to IS NULL`) with optional point-in-time `as_of`. Each
    /// hop reads its frontier node's incident edges via a lancedb
    /// `only_if` query. Edge-set dedup on
    /// `(from, to, relation, valid_from)`; output sorted
    /// `(relation, from, to, valid_from)` — identical to the DuckDB
    /// backend. Parity-gated by `tests/parity_golden.rs`.
    pub async fn neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let hops = max_hops.clamp(1, MAX_HOPS_CAP);

        // BFS: `frontier` = nodes to expand this round, `visited` = every
        // node seen, `edges` = accumulated edge set deduped on
        // (from, to, relation, valid_from). Same shapes as the DuckDB impl.
        let mut visited: std::collections::HashSet<String> =
            std::collections::HashSet::from([node_id.to_string()]);
        let mut frontier: Vec<String> = vec![node_id.to_string()];
        let mut edges: std::collections::HashMap<(String, String, String, String), GraphEdge> =
            std::collections::HashMap::new();

        for _ in 0..hops {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier: Vec<String> = Vec::new();
            for node in frontier.drain(..) {
                // Per-node incident-edge query with the same validity
                // filter the DuckDB backend applies: bitemporal window
                // when `as_of` is supplied, else active-now.
                let validity = match as_of {
                    Some(ts) => format!(
                        "valid_from <= {ts} AND (valid_to IS NULL OR valid_to > {ts})",
                        ts = sql_quote(ts),
                    ),
                    None => "valid_to IS NULL".to_string(),
                };
                let filter = format!(
                    "(from_node_id = {node} OR to_node_id = {node}) AND {validity}",
                    node = sql_quote(&node),
                );
                let incident = self.query_graph_edges(filter).await?;
                for edge in incident {
                    let key = (
                        edge.from_node_id.clone(),
                        edge.to_node_id.clone(),
                        edge.relation.clone(),
                        edge.valid_from.clone(),
                    );
                    edges.entry(key).or_insert_with(|| edge.clone());
                    for endpoint in [&edge.from_node_id, &edge.to_node_id] {
                        if !visited.contains(endpoint.as_str()) {
                            if visited.len() >= NEIGHBORS_VISITED_CAP {
                                continue;
                            }
                            visited.insert(endpoint.clone());
                            next_frontier.push(endpoint.clone());
                        }
                    }
                }
            }
            frontier = next_frontier;
        }

        let mut out: Vec<GraphEdge> = edges.into_values().collect();
        out.sort_by(|a, b| {
            a.relation
                .cmp(&b.relation)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
                .then_with(|| a.valid_from.cmp(&b.valid_from))
        });
        Ok(out)
    }

    /// Route-B bucket "graph": native lancedb-Rust equivalent of
    /// `DuckDbQuery::related_capability_capsule_ids`. Selects active
    /// edges (`valid_to IS NULL`) where either endpoint is in
    /// `node_ids`, keeps the OPPOSITE endpoint when it carries the
    /// literal `capability_capsule:` prefix, strips the prefix, dedups,
    /// sorts. Bare ids without the prefix are intentionally excluded —
    /// identical to the DuckDB backend. Parity-gated by
    /// `tests/parity_golden.rs`.
    pub async fn related_capability_capsule_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        // `from_node_id IN (...) OR to_node_id IN (...)` AND active.
        let in_list = node_ids
            .iter()
            .map(|n| sql_quote(n))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = format!(
            "(from_node_id IN ({in_list}) OR to_node_id IN ({in_list})) AND valid_to IS NULL"
        );
        let edges = self.query_graph_edges(filter).await?;

        let node_set: std::collections::HashSet<&str> =
            node_ids.iter().map(|s| s.as_str()).collect();
        let mut capsule_ids = std::collections::HashSet::new();
        for edge in &edges {
            for endpoint in [&edge.from_node_id, &edge.to_node_id] {
                if !node_set.contains(endpoint.as_str()) {
                    if let Some(cid) = endpoint.strip_prefix("capability_capsule:") {
                        capsule_ids.insert(cid.to_string());
                    }
                }
            }
        }
        let mut out: Vec<String> = capsule_ids.into_iter().collect();
        out.sort();
        Ok(out)
    }

    /// Route-B bucket "graph": native lancedb-Rust equivalent of
    /// `DuckDbQuery::graph_stats`. Whole-graph aggregate (tenant-less —
    /// `graph_edges` has no tenant column): `total_edges`,
    /// `active_edges` (`valid_to IS NULL`), `closed_edges`,
    /// `node_count` (DISTINCT over both endpoints), and the top-16
    /// `(relation, count)` pairs ordered `count DESC, relation ASC`.
    /// Counts are computed in Rust from a full scan of `graph_edges`
    /// (LanceDB has no GROUP BY / DISTINCT); the orderings match the
    /// DuckDB backend exactly. Parity-gated by `tests/parity_golden.rs`.
    pub async fn graph_stats(&self) -> Result<GraphStats, GraphError> {
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        // Pull every edge — counts + distinct-node + relation histogram
        // are all derived in Rust. (Empty filter = all rows.)
        let stream = table
            .query()
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| GraphError::Backend(format!("lancedb stream: {e}")))?;
        let mut edges = Vec::new();
        for b in &batches {
            edges.extend(record_batch_to_graph_edges(b)?);
        }

        let total_edges = edges.len() as i64;
        let active_edges = edges.iter().filter(|e| e.valid_to.is_none()).count() as i64;
        let closed_edges = total_edges - active_edges;

        let mut nodes: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut counts: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
        for e in &edges {
            nodes.insert(e.from_node_id.as_str());
            nodes.insert(e.to_node_id.as_str());
            *counts.entry(e.relation.as_str()).or_insert(0) += 1;
        }
        let node_count = nodes.len() as i64;

        // `ORDER BY c DESC, relation ASC LIMIT 16`.
        let mut top_relations: Vec<(String, i64)> = counts
            .into_iter()
            .map(|(r, c)| (r.to_string(), c))
            .collect();
        top_relations.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top_relations.truncate(TOP_RELATIONS_LIMIT);

        Ok(GraphStats {
            node_count,
            total_edges,
            active_edges,
            closed_edges,
            top_relations,
        })
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::neighbors`. Active edges (`valid_to IS NULL`)
    /// incident on `node_id` (either endpoint), 1-hop, no dedup/limit.
    /// Output sorted `(relation, from_node_id, to_node_id)` — mirrors the
    /// DuckDB `ORDER BY relation, from_node_id, to_node_id`. Parity-gated
    /// by `tests/parity_golden.rs`.
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let filter = format!(
            "(from_node_id = {node} OR to_node_id = {node}) AND valid_to IS NULL",
            node = sql_quote(node_id),
        );
        let mut out = self.query_graph_edges(filter).await?;
        out.sort_by(|a, b| {
            a.relation
                .cmp(&b.relation)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        Ok(out)
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::get_active_edge`. The single active edge identified
    /// by `(from, to, relation)` with its full K9 dynamics, or `None`.
    /// Used by the potentiation worker's read-modify-write. Mirrors the
    /// DuckDB `WHERE … AND valid_to IS NULL LIMIT 1`. Parity-gated by
    /// `tests/parity_golden.rs`.
    pub async fn get_active_edge(
        &self,
        from_node_id: &str,
        to_node_id: &str,
        relation: &str,
    ) -> Result<Option<GraphEdge>, GraphError> {
        let filter = format!(
            "from_node_id = {} AND to_node_id = {} AND relation = {} AND valid_to IS NULL",
            sql_quote(from_node_id),
            sql_quote(to_node_id),
            sql_quote(relation),
        );
        let edges = self.query_graph_edges(filter).await?;
        Ok(edges.into_iter().next())
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::kg_timeline`. ALL edges (incl. closed/historical)
    /// involving `node_id` (either endpoint), ordered
    /// `(valid_from ASC, relation ASC, from_node_id ASC, to_node_id ASC)`.
    /// Unlike [`Self::neighbors`], closed edges are surfaced — the whole
    /// point of a timeline. Parity-gated by `tests/parity_golden.rs`.
    pub async fn kg_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let filter = format!(
            "from_node_id = {node} OR to_node_id = {node}",
            node = sql_quote(node_id),
        );
        let mut out = self.query_graph_edges(filter).await?;
        out.sort_by(|a, b| {
            a.valid_from
                .cmp(&b.valid_from)
                .then_with(|| a.relation.cmp(&b.relation))
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        Ok(out)
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::query_predicate`. All edges with `relation =
    /// predicate`, optionally restricted to those active at `as_of`
    /// (`valid_from <= as_of AND (valid_to IS NULL OR valid_to > as_of)`).
    /// When `as_of` is `None`, includes both active and closed edges.
    /// Ordered `(valid_from ASC, from_node_id ASC, to_node_id ASC)`.
    /// Parity-gated by `tests/parity_golden.rs`.
    pub async fn query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let filter = match as_of {
            Some(ts) => format!(
                "relation = {rel} AND valid_from <= {ts} AND (valid_to IS NULL OR valid_to > {ts})",
                rel = sql_quote(predicate),
                ts = sql_quote(ts),
            ),
            None => format!("relation = {}", sql_quote(predicate)),
        };
        let mut out = self.query_graph_edges(filter).await?;
        out.sort_by(|a, b| {
            a.valid_from
                .cmp(&b.valid_from)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        Ok(out)
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::list_user_tunnels`. Caller-curated active edges
    /// (`relation LIKE 'user_tunnel:%' AND valid_to IS NULL`), ordered
    /// `(relation, from_node_id, to_node_id)`, capped at `limit` (clamped
    /// 1..200, matching DuckDB). The LIMIT is applied AFTER the sort — a
    /// LanceDB scan has no ORDER BY, so the cap is taken in Rust on the
    /// sorted vec. Parity-gated by `tests/parity_golden.rs`.
    pub async fn list_user_tunnels(&self, limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        let lim = limit.clamp(1, 200);
        let filter = "relation LIKE 'user_tunnel:%' AND valid_to IS NULL".to_string();
        let mut out = self.query_graph_edges(filter).await?;
        out.sort_by(|a, b| {
            a.relation
                .cmp(&b.relation)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        out.truncate(lim);
        Ok(out)
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::find_tunnels`. Active user-tunnel edges
    /// (`relation LIKE 'user_tunnel:%' AND valid_to IS NULL`) where the
    /// endpoints match `prefix_a`/`prefix_b` in EITHER direction:
    /// `(from LIKE A% AND to LIKE B%) OR (from LIKE B% AND to LIKE A%)`.
    /// Dedup on `(from, to, relation, valid_from)` (matters when
    /// `prefix_a == prefix_b`), ordered `(relation, from, to)`, capped at
    /// `limit` (clamped 1..200). LIMIT after sort+dedup in Rust.
    /// Parity-gated by `tests/parity_golden.rs`.
    pub async fn find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let lim = limit.clamp(1, 200);
        // LIKE-escape is unnecessary here: callers pass node-id prefixes
        // (no `%`/`_` of their own); we append the wildcard ourselves. The
        // pattern literal is sql_quote'd so an embedded quote can't break
        // out of the string.
        let pat_a = sql_quote(&format!("{prefix_a}%"));
        let pat_b = sql_quote(&format!("{prefix_b}%"));
        let filter = format!(
            "relation LIKE 'user_tunnel:%' AND valid_to IS NULL \
             AND ((from_node_id LIKE {pat_a} AND to_node_id LIKE {pat_b}) \
               OR (from_node_id LIKE {pat_b} AND to_node_id LIKE {pat_a}))"
        );
        let edges = self.query_graph_edges(filter).await?;
        let mut seen: std::collections::HashSet<(String, String, String, String)> =
            std::collections::HashSet::new();
        let mut out = Vec::new();
        for e in edges {
            let key = (
                e.from_node_id.clone(),
                e.to_node_id.clone(),
                e.relation.clone(),
                e.valid_from.clone(),
            );
            if seen.insert(key) {
                out.push(e);
            }
        }
        out.sort_by(|a, b| {
            a.relation
                .cmp(&b.relation)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        out.truncate(lim);
        Ok(out)
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::follow_tunnels`. BFS from `node_id` following ONLY
    /// active user-tunnel edges (`relation LIKE 'user_tunnel:%' AND
    /// valid_to IS NULL`), up to `max_hops` (clamped to `MAX_HOPS_CAP =
    /// 3`). Edge-set dedup on `(from, to, relation, valid_from)`; output
    /// sorted `(relation, from, to)`. Distinct from
    /// [`Self::neighbors_within`] (which walks all active edges) — here
    /// only user-curated bridges are traversed. Parity-gated by
    /// `tests/parity_golden.rs`.
    pub async fn follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let hops = max_hops.clamp(1, MAX_HOPS_CAP);

        let mut visited: std::collections::HashSet<String> =
            std::collections::HashSet::from([node_id.to_string()]);
        let mut frontier: Vec<String> = vec![node_id.to_string()];
        let mut edges: std::collections::HashMap<(String, String, String, String), GraphEdge> =
            std::collections::HashMap::new();

        for _ in 0..hops {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier: Vec<String> = Vec::new();
            for node in frontier.drain(..) {
                let filter = format!(
                    "(from_node_id = {node} OR to_node_id = {node}) \
                     AND relation LIKE 'user_tunnel:%' AND valid_to IS NULL",
                    node = sql_quote(&node),
                );
                let incident = self.query_graph_edges(filter).await?;
                for edge in incident {
                    let key = (
                        edge.from_node_id.clone(),
                        edge.to_node_id.clone(),
                        edge.relation.clone(),
                        edge.valid_from.clone(),
                    );
                    edges.entry(key).or_insert_with(|| edge.clone());
                    for endpoint in [&edge.from_node_id, &edge.to_node_id] {
                        if !visited.contains(endpoint.as_str()) {
                            if visited.len() >= NEIGHBORS_VISITED_CAP {
                                continue;
                            }
                            visited.insert(endpoint.clone());
                            next_frontier.push(endpoint.clone());
                        }
                    }
                }
            }
            frontier = next_frontier;
        }

        let mut out: Vec<GraphEdge> = edges.into_values().collect();
        out.sort_by(|a, b| {
            a.relation
                .cmp(&b.relation)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        Ok(out)
    }

    /// Route-B bucket "graph-tunnel": native lancedb-Rust equivalent of
    /// `DuckDbQuery::incident_edges_for_nodes`. Every active 1-hop edge
    /// (`valid_to IS NULL`) incident to any node in `node_ids`, as raw
    /// `(from_node_id, to_node_id)` pairs. The DuckDB query has no ORDER
    /// BY, so the pair order is not load-bearing — callers (and the
    /// golden) sort deterministically. Empty input short-circuits.
    /// Parity-gated by `tests/parity_golden.rs`.
    pub async fn incident_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String)>, GraphError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let in_list = node_ids
            .iter()
            .map(|n| sql_quote(n))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = format!(
            "(from_node_id IN ({in_list}) OR to_node_id IN ({in_list})) AND valid_to IS NULL"
        );
        let edges = self.query_graph_edges(filter).await?;
        Ok(edges
            .into_iter()
            .map(|e| (e.from_node_id, e.to_node_id))
            .collect())
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
