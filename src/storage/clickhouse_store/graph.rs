//! `GraphStore` for [`ClickHouseBackend`].
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** `graph_edges` is `ReplacingMergeTree(row_version)`;
//! bitemporal `valid_to` uses ''=open (active) per the §10 decision. Per §4(f),
//! multi-hop traversal (`neighbors_within` / `follow_tunnels` /
//! `related_capability_capsule_ids`) is an ITERATIVE Rust BFS over the active
//! edge set (read once via SQL, walk in memory), MAX_HOPS_CAP = 3 — the same
//! algorithm shape as `lance_store/graph.rs`. Numeric Option columns
//! (confidence/strength/stability/access_count) round-trip 0 as Some(0) (no
//! Nullable) — a known fidelity caveat (§10, pain #2 class).

use std::collections::{HashSet, VecDeque};

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use super::capsule_store::{now_version, opt};
use crate::domain::capability_capsule::{GraphEdge, GraphStats};
use crate::storage::types::GraphError;
use crate::storage::GraphStore;

const MAX_HOPS_CAP: u32 = 3;

fn ch_gerr(e: clickhouse::error::Error) -> GraphError {
    GraphError::Backend(format!("clickhouse: {e}"))
}

#[derive(Row, Serialize, Deserialize, Clone)]
struct ChEdgeRow {
    from_node_id: String,
    to_node_id: String,
    relation: String,
    valid_from: String,
    valid_to: String,
    confidence: f32,
    extractor: String,
    strength: f32,
    stability: f32,
    last_activated: String,
    access_count: i64,
    row_version: u64,
}

impl ChEdgeRow {
    fn from_edge(e: &GraphEdge) -> Self {
        Self {
            from_node_id: e.from_node_id.clone(),
            to_node_id: e.to_node_id.clone(),
            relation: e.relation.clone(),
            valid_from: e.valid_from.clone(),
            valid_to: e.valid_to.clone().unwrap_or_default(),
            confidence: e.confidence.unwrap_or(0.0),
            extractor: e.extractor.clone().unwrap_or_default(),
            strength: e.strength.unwrap_or(0.0),
            stability: e.stability.unwrap_or(0.0),
            last_activated: e.last_activated.clone().unwrap_or_default(),
            access_count: e.access_count.unwrap_or(0),
            row_version: now_version(),
        }
    }

    fn into_edge(self) -> GraphEdge {
        GraphEdge {
            from_node_id: self.from_node_id,
            to_node_id: self.to_node_id,
            relation: self.relation,
            valid_from: self.valid_from,
            valid_to: opt(self.valid_to),
            // 0.0 is the "unset" sentinel (no Nullable columns) — map it
            // back to None so a confidence-less edge reads back like
            // lance/pg NULL (audit 2026-07-03 ⑨: `Some(0.0)` zeroed the
            // K9-dynamics boost of every unweighted edge). A genuine
            // stored 0.0 is indistinguishable — documented module-header
            // caveat; strength/stability keep the Some(0) round-trip
            // because K9 potentiation owns their defaults.
            confidence: (self.confidence != 0.0).then_some(self.confidence),
            extractor: opt(self.extractor),
            strength: Some(self.strength),
            stability: Some(self.stability),
            last_activated: opt(self.last_activated),
            access_count: Some(self.access_count),
        }
    }
}

impl ClickHouseBackend {
    async fn ch_edges(&self, sql: &str, binds: &[&str]) -> Result<Vec<ChEdgeRow>, GraphError> {
        let mut q = self.client.query(sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.fetch_all::<ChEdgeRow>().await.map_err(ch_gerr)
    }

    /// All currently-active edges (`valid_to = ''`) — the BFS frontier source.
    async fn ch_active_edges(&self) -> Result<Vec<ChEdgeRow>, GraphError> {
        self.ch_edges(
            "SELECT ?fields FROM graph_edges FINAL WHERE valid_to = ''",
            &[],
        )
        .await
    }

    async fn ch_write_edges(&self, rows: &[ChEdgeRow]) -> Result<(), GraphError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut insert = self
            .client
            .insert::<ChEdgeRow>("graph_edges")
            .await
            .map_err(ch_gerr)?;
        for row in rows {
            insert.write(row).await.map_err(ch_gerr)?;
        }
        insert.end().await.map_err(ch_gerr)?;
        Ok(())
    }
}

/// Iterative BFS over an in-memory active-edge set, following edges incident
/// to the frontier up to `max_hops`. `relation_filter` (if set) restricts to
/// edges whose relation starts with it (tunnel walks).
fn bfs(
    edges: &[ChEdgeRow],
    start: &str,
    max_hops: u32,
    relation_filter: Option<&str>,
) -> Vec<GraphEdge> {
    let hops = max_hops.min(MAX_HOPS_CAP);
    let mut visited: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<(String, String, String, String)> = HashSet::new();
    let mut out = Vec::new();
    let mut frontier: VecDeque<(String, u32)> = VecDeque::new();
    frontier.push_back((start.to_owned(), 0));
    visited.insert(start.to_owned());
    while let Some((node, depth)) = frontier.pop_front() {
        if depth >= hops {
            continue;
        }
        for e in edges {
            if relation_filter.is_some_and(|p| !e.relation.starts_with(p)) {
                continue;
            }
            let touches = e.from_node_id == node || e.to_node_id == node;
            if !touches {
                continue;
            }
            let key = (
                e.from_node_id.clone(),
                e.relation.clone(),
                e.to_node_id.clone(),
                e.valid_from.clone(),
            );
            if seen_edges.insert(key) {
                out.push(e.clone().into_edge());
            }
            let other = if e.from_node_id == node {
                &e.to_node_id
            } else {
                &e.from_node_id
            };
            if visited.insert(other.clone()) {
                frontier.push_back((other.clone(), depth + 1));
            }
        }
    }
    out
}

#[async_trait]
impl GraphStore for ClickHouseBackend {
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let rows = self
            .ch_edges(
                "SELECT ?fields FROM graph_edges FINAL \
                 WHERE valid_to = '' AND (from_node_id = ? OR to_node_id = ?) \
                 ORDER BY relation, from_node_id, to_node_id",
                &[node_id, node_id],
            )
            .await?;
        Ok(rows.into_iter().map(ChEdgeRow::into_edge).collect())
    }

    async fn neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        // BFS over the edge set active at `as_of` (bitemporal window), or the
        // currently-active set when `as_of` is None. `valid_to = ''` is the CH
        // "still open" sentinel — the analog of lance's `valid_to IS NULL`.
        let edges = match as_of {
            Some(ts) => {
                self.ch_edges(
                    "SELECT ?fields FROM graph_edges FINAL \
                     WHERE valid_from <= ? AND (valid_to = '' OR valid_to > ?)",
                    &[ts, ts],
                )
                .await?
            }
            None => self.ch_active_edges().await?,
        };
        Ok(bfs(&edges, node_id, max_hops, None))
    }

    async fn kg_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let rows = self
            .ch_edges(
                "SELECT ?fields FROM graph_edges FINAL \
                 WHERE from_node_id = ? OR to_node_id = ? ORDER BY valid_from ASC",
                &[node_id, node_id],
            )
            .await?;
        Ok(rows.into_iter().map(ChEdgeRow::into_edge).collect())
    }

    async fn query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        // `as_of=Some(ts)` → edges active at ts; `as_of=None` → the FULL history
        // (active + closed) for the predicate. Ordered `(valid_from, from_node_id,
        // to_node_id) ASC` to match the lance + postgres backends exactly — the
        // tie-break keys make same-`valid_from` edges deterministic (ClickHouse
        // FINAL + parallel merge gives no order among equal sort keys otherwise).
        // (The scaffold ignored `as_of` and returned only the currently-active set.)
        let rows = match as_of {
            Some(ts) => {
                self.ch_edges(
                    "SELECT ?fields FROM graph_edges FINAL \
                     WHERE relation = ? AND valid_from <= ? AND (valid_to = '' OR valid_to > ?) \
                     ORDER BY valid_from ASC, from_node_id ASC, to_node_id ASC",
                    &[predicate, ts, ts],
                )
                .await?
            }
            None => {
                self.ch_edges(
                    "SELECT ?fields FROM graph_edges FINAL \
                     WHERE relation = ? \
                     ORDER BY valid_from ASC, from_node_id ASC, to_node_id ASC",
                    &[predicate],
                )
                .await?
            }
        };
        Ok(rows.into_iter().map(ChEdgeRow::into_edge).collect())
    }

    async fn list_user_tunnels(&self, limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        let rows = self
            .ch_edges(
                "SELECT ?fields FROM graph_edges FINAL \
                 WHERE valid_to = '' AND startsWith(relation, 'user_tunnel:') \
                 ORDER BY relation LIMIT ?",
                &[&limit.to_string()],
            )
            .await?;
        Ok(rows.into_iter().map(ChEdgeRow::into_edge).collect())
    }

    async fn find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let rows = self
            .ch_edges(
                "SELECT ?fields FROM graph_edges FINAL \
                 WHERE valid_to = '' AND startsWith(relation, 'user_tunnel:') \
                 AND ((startsWith(from_node_id, ?) AND startsWith(to_node_id, ?)) \
                   OR (startsWith(from_node_id, ?) AND startsWith(to_node_id, ?))) \
                 ORDER BY relation LIMIT ?",
                &[prefix_a, prefix_b, prefix_b, prefix_a, &limit.to_string()],
            )
            .await?;
        Ok(rows.into_iter().map(ChEdgeRow::into_edge).collect())
    }

    async fn follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let edges = self.ch_active_edges().await?;
        Ok(bfs(&edges, node_id, max_hops, Some("user_tunnel:")))
    }

    async fn graph_stats(&self) -> Result<GraphStats, GraphError> {
        let total: Vec<u64> = self
            .client
            .query("SELECT count() FROM graph_edges FINAL")
            .fetch_all::<u64>()
            .await
            .map_err(ch_gerr)?;
        let active: Vec<u64> = self
            .client
            .query("SELECT count() FROM graph_edges FINAL WHERE valid_to = ''")
            .fetch_all::<u64>()
            .await
            .map_err(ch_gerr)?;
        let nodes: Vec<u64> = self
            .client
            .query(
                "SELECT uniqExact(n) FROM ( \
                 SELECT from_node_id AS n FROM graph_edges FINAL \
                 UNION ALL SELECT to_node_id AS n FROM graph_edges FINAL)",
            )
            .fetch_all::<u64>()
            .await
            .map_err(ch_gerr)?;
        let top: Vec<(String, u64)> = self
            .client
            .query(
                "SELECT relation, count() AS c FROM graph_edges FINAL \
                 WHERE valid_to = '' GROUP BY relation ORDER BY c DESC, relation ASC LIMIT 16",
            )
            .fetch_all::<(String, u64)>()
            .await
            .map_err(ch_gerr)?;
        let total = total.first().copied().unwrap_or(0) as i64;
        let active = active.first().copied().unwrap_or(0) as i64;
        Ok(GraphStats {
            node_count: nodes.first().copied().unwrap_or(0) as i64,
            total_edges: total,
            active_edges: active,
            closed_edges: total - active,
            top_relations: top.into_iter().map(|(r, c)| (r, c as i64)).collect(),
        })
    }

    async fn related_capability_capsule_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        let edges = self.ch_active_edges().await?;
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for start in node_ids {
            for e in bfs(&edges, start, MAX_HOPS_CAP, None) {
                for node in [&e.from_node_id, &e.to_node_id] {
                    if let Some(id) = node.strip_prefix("capability_capsule:") {
                        if seen.insert(id.to_owned()) {
                            out.push(id.to_owned());
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    async fn incident_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<crate::storage::graph_store::IncidentEdge>, GraphError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let set: HashSet<&str> = node_ids.iter().map(String::as_str).collect();
        let edges = self.ch_active_edges().await?;
        let mut out = Vec::new();
        // NO pair-dedup (audit 2026-07-03 ⑩): lance/pg return every
        // active row and ranking maxes over them — deduping here kept
        // an arbitrary row's confidence/class for a pair carrying
        // several relations.
        for e in edges {
            if set.contains(e.from_node_id.as_str()) || set.contains(e.to_node_id.as_str()) {
                // CH stores confidence as a bare f32 with 0.0 standing in
                // for "unset" (see module header) — map 0.0 back to None so
                // an unweighted edge keeps full ranking weight, matching
                // lance/pg.
                let confidence = (e.confidence != 0.0).then_some(e.confidence);
                out.push(crate::storage::graph_store::IncidentEdge {
                    from: e.from_node_id,
                    to: e.to_node_id,
                    confidence,
                    extractor: opt(e.extractor),
                });
            }
        }
        Ok(out)
    }

    async fn sync_memory_edges(&self, edges: &[GraphEdge], now: &str) -> Result<(), GraphError> {
        // Close any currently-active edge for each (from, relation, to) by
        // re-inserting it with valid_to = now, then insert the new active row.
        for e in edges {
            self.client
                .query(
                    "ALTER TABLE graph_edges UPDATE valid_to = ? \
                     WHERE valid_to = '' AND from_node_id = ? AND relation = ? AND to_node_id = ?",
                )
                .bind(now)
                .bind(&e.from_node_id)
                .bind(&e.relation)
                .bind(&e.to_node_id)
                .execute()
                .await
                .map_err(ch_gerr)?;
        }
        let rows: Vec<ChEdgeRow> = edges.iter().map(ChEdgeRow::from_edge).collect();
        self.ch_write_edges(&rows).await
    }

    async fn add_edge_direct(&self, edge: &GraphEdge) -> Result<bool, GraphError> {
        self.ch_write_edges(std::slice::from_ref(&ChEdgeRow::from_edge(edge)))
            .await?;
        Ok(true)
    }

    async fn invalidate_edge(
        &self,
        from_node_id: &str,
        predicate: &str,
        to_node_id: &str,
        ended_at: &str,
    ) -> Result<usize, GraphError> {
        self.client
            .query(
                "ALTER TABLE graph_edges UPDATE valid_to = ? \
                 WHERE valid_to = '' AND from_node_id = ? AND relation = ? AND to_node_id = ?",
            )
            .bind(ended_at)
            .bind(from_node_id)
            .bind(predicate)
            .bind(to_node_id)
            .execute()
            .await
            .map_err(ch_gerr)?;
        // Mutations are async; the affected-count isn't returned synchronously.
        Ok(1)
    }

    async fn close_edges_for_capability_capsule(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, GraphError> {
        let now = crate::storage::time::current_timestamp();
        let node = format!("capability_capsule:{capability_capsule_id}");
        self.client
            .query(
                "ALTER TABLE graph_edges UPDATE valid_to = ? \
                 WHERE valid_to = '' AND (from_node_id = ? OR to_node_id = ?)",
            )
            .bind(now)
            .bind(node.as_str())
            .bind(node.as_str())
            .execute()
            .await
            .map_err(ch_gerr)?;
        Ok(1)
    }
}
