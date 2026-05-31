//! Graph reads (`graph_edges` table). Methods inherent on
//! `DuckDbQuery`. Active-only by convention — closed edges
//! (`valid_to IS NOT NULL`) stay for audit but never enter recall.

use duckdb::params;

use super::{row_to_graph_edge, spawn_blocking_graph, DuckDbQuery};
use crate::domain::capability_capsule::{GraphEdge, GraphStats};
use crate::storage::types::GraphError;

/// Upper cap on `neighbors_within` `max_hops` to prevent blow-up on
/// dense graphs. Three hops is enough to express the common patterns
/// (capsule → entity → adjacent capsule → entity) without combinatorial
/// explosion.
const MAX_HOPS_CAP: u32 = 3;

/// Upper cap on the visited-set size during BFS. Stops a runaway walk
/// over a pathologically dense subgraph before it eats memory.
const NEIGHBORS_VISITED_CAP: usize = 10_000;

impl DuckDbQuery {
    /// Active edges incident on `node_id` (either endpoint), 1-hop.
    /// Convenience wrapper over [`Self::neighbors_within`] that
    /// hard-codes `max_hops = 1` and `as_of = None`. Kept for
    /// callers (and tests) that don't need the multi-hop / time-point
    /// machinery.
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.fresh_conn_for_graph().await?;
        let node_id = node_id.to_string();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
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

    /// Fetch the single active edge identified by `(from, to, relation)`
    /// with its full K9 dynamics fields, or `None` if no active edge
    /// matches. Used by the potentiation worker (K9) to read-modify-write
    /// an edge's `strength` / `stability` / `last_activated` /
    /// `access_count`.
    pub async fn get_active_edge(
        &self,
        from_node_id: &str,
        to_node_id: &str,
        relation: &str,
    ) -> Result<Option<GraphEdge>, GraphError> {
        let conn = self.fresh_conn_for_graph().await?;
        let from = from_node_id.to_string();
        let to = to_node_id.to_string();
        let rel = relation.to_string();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                 FROM ns.main.graph_edges \
                 WHERE from_node_id = ?1 AND to_node_id = ?2 AND relation = ?3 AND valid_to IS NULL \
                 LIMIT 1",
            )?;
            let mut rows = stmt.query_map(params![from, to, rel], row_to_graph_edge)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Multi-hop BFS from `node_id`. Returns every edge reachable in
    /// at most `max_hops` hops (clamped to `MAX_HOPS_CAP = 3`), with
    /// edge-set dedup (an edge that closes two different walks counts
    /// once). The edge filter follows the `valid_from <= as_of` /
    /// `valid_to IS NULL OR valid_to > as_of` convention when `as_of`
    /// is supplied; otherwise it falls back to "active now"
    /// (`valid_to IS NULL`).
    ///
    /// Walk order: BFS level by level. Visited-set capped at
    /// `NEIGHBORS_VISITED_CAP = 10_000` to bound memory on a
    /// pathological subgraph.
    ///
    /// Returns the **edge** list (sorted `(relation, from, to)` for
    /// determinism), not the node list — the caller can derive the
    /// node set from the edges.
    pub async fn neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let hops = max_hops.clamp(1, MAX_HOPS_CAP);
        let conn = self.fresh_conn_for_graph().await?;
        let start = node_id.to_string();
        let as_of = as_of.map(str::to_owned);
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let validity_sql = match &as_of {
                Some(_) => "valid_from <= ?2 AND (valid_to IS NULL OR valid_to > ?2)",
                None => "valid_to IS NULL",
            };

            // BFS: maintain `frontier` (nodes to expand this round),
            // `visited` (all nodes seen so far), `edges` (the
            // accumulated edge set, deduped on
            // (from, to, relation, valid_from)).
            let mut visited: std::collections::HashSet<String> =
                std::collections::HashSet::from([start.clone()]);
            let mut frontier: Vec<String> = vec![start];
            let mut edges: std::collections::HashMap<(String, String, String, String), GraphEdge> =
                std::collections::HashMap::new();

            for _ in 0..hops {
                if frontier.is_empty() {
                    break;
                }
                let mut next_frontier: Vec<String> = Vec::new();
                for node in frontier.drain(..) {
                    let mut sql = String::from(
                        "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                         FROM ns.main.graph_edges \
                         WHERE (from_node_id = ?1 OR to_node_id = ?1) AND ",
                    );
                    sql.push_str(validity_sql);
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = match &as_of {
                        Some(ts) => stmt.query_map(params![node, ts], row_to_graph_edge)?,
                        None => stmt.query_map(params![node], row_to_graph_edge)?,
                    };
                    for r in rows {
                        let edge = r?;
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
        })
        .await
    }

    /// Caller-curated edges where both endpoints' node_id starts with
    /// the given prefixes — the parallel of MemPalace's
    /// `tool_find_tunnels(wing_a, wing_b)`. The directionality is
    /// **bidirectional**: an edge with `from` matching `prefix_a` AND
    /// `to` matching `prefix_b` qualifies, AS DOES the reverse pair
    /// (`from`→`prefix_b`, `to`→`prefix_a`). Active user-tunnel only.
    ///
    /// `prefix_a` / `prefix_b` are caller-supplied node-id prefixes
    /// such as `capability_capsule:`, `entity:`, `topic:project_x`,
    /// or specific full ids. Use empty string for "any". Both being
    /// empty is equivalent to a broad [`Self::list_user_tunnels`].
    pub async fn find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.fresh_conn_for_graph().await?;
        let prefix_a = format!("{prefix_a}%");
        let prefix_b = format!("{prefix_b}%");
        let lim = i64::try_from(limit.clamp(1, 200)).unwrap_or(50);
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // (from LIKE A AND to LIKE B) OR (from LIKE B AND to LIKE A)
            // covers both directions; we de-dupe via the SET shape
            // below (HashSet on the tuple) in case prefix_a == prefix_b.
            let mut stmt = conn.prepare(
                "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                 FROM ns.main.graph_edges \
                 WHERE relation LIKE 'user_tunnel:%' AND valid_to IS NULL \
                   AND ((from_node_id LIKE ?1 AND to_node_id LIKE ?2) \
                     OR (from_node_id LIKE ?2 AND to_node_id LIKE ?1)) \
                 ORDER BY relation, from_node_id, to_node_id \
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![prefix_a, prefix_b, lim], row_to_graph_edge)?;
            let mut seen: std::collections::HashSet<(String, String, String, String)> =
                std::collections::HashSet::new();
            let mut out = Vec::new();
            for r in rows {
                let e = r?;
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
            Ok(out)
        })
        .await
    }

    /// BFS from `node_id` following **only** user-tunnel edges
    /// (`relation LIKE 'user_tunnel:%'`), up to `max_hops` (cap 3).
    /// Active only — closed tunnels are skipped. Returns the edge
    /// set (sorted `(relation, from, to)`) rather than the node set
    /// so callers can render the tunnel labels.
    ///
    /// Use when starting from a known node (e.g. a capsule the user
    /// is reading) and wanting to surface the user-curated bridges
    /// outward — distinct from `neighbors_within`, which walks all
    /// active edges including auto-extracted ones.
    pub async fn follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let hops = max_hops.clamp(1, MAX_HOPS_CAP);
        let conn = self.fresh_conn_for_graph().await?;
        let start = node_id.to_string();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut visited: std::collections::HashSet<String> =
                std::collections::HashSet::from([start.clone()]);
            let mut frontier: Vec<String> = vec![start];
            let mut edges: std::collections::HashMap<(String, String, String, String), GraphEdge> =
                std::collections::HashMap::new();

            for _ in 0..hops {
                if frontier.is_empty() {
                    break;
                }
                let mut next_frontier: Vec<String> = Vec::new();
                for node in frontier.drain(..) {
                    let mut stmt = conn.prepare(
                        "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                         FROM ns.main.graph_edges \
                         WHERE (from_node_id = ?1 OR to_node_id = ?1) \
                           AND relation LIKE 'user_tunnel:%' \
                           AND valid_to IS NULL",
                    )?;
                    let rows = stmt.query_map(params![node], row_to_graph_edge)?;
                    for r in rows {
                        let edge = r?;
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
        })
        .await
    }

    /// Caller-curated graph edges — by convention these are written
    /// with `relation` prefixed `user_tunnel:` (the rest of `relation`
    /// is the human-readable label). Distinct from auto-extracted
    /// edges (`mentions`, `tagged`, `supersedes`, etc.) which
    /// originate from `pipeline::ingest::extract_graph_edges`. Active
    /// only; ordered `(relation, from_node_id, to_node_id)`.
    ///
    /// This is a `relation LIKE` filter rather than a schema-level
    /// `origin` column on `graph_edges` (see mempalace-diff-v2 §3
    /// #20 phase-A discussion). When/if a real `origin` column lands,
    /// this method should switch to a strict equality filter.
    pub async fn list_user_tunnels(&self, limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.fresh_conn_for_graph().await?;
        let lim = i64::try_from(limit.clamp(1, 200)).unwrap_or(50);
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                 FROM ns.main.graph_edges \
                 WHERE relation LIKE 'user_tunnel:%' AND valid_to IS NULL \
                 ORDER BY relation, from_node_id, to_node_id \
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![lim], row_to_graph_edge)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// All edges involving `node_id` (either endpoint), **including
    /// closed ones**, ordered `(valid_from ASC, relation ASC)`. The
    /// canonical "show me the full history of this entity" view —
    /// equivalent of MemPalace's `tool_kg_timeline`. Unlike
    /// [`Self::neighbors`], closed edges (`valid_to IS NOT NULL`) are
    /// surfaced here because that's the whole point of a timeline.
    pub async fn kg_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.fresh_conn_for_graph().await?;
        let node_id = node_id.to_string();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                 FROM ns.main.graph_edges \
                 WHERE from_node_id = ?1 OR to_node_id = ?1 \
                 ORDER BY valid_from ASC, relation ASC, from_node_id ASC, to_node_id ASC",
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

    /// All edges with `relation = predicate`, optionally restricted
    /// to those active at `as_of` (20-digit ms string). When `as_of`
    /// is `None` the result includes both active and closed edges —
    /// the canonical "show me every assertion of this relation"
    /// inspection. mempalace's `query_relationship` analogue (KG K4).
    ///
    /// Ordered `valid_from ASC, from_node_id ASC, to_node_id ASC` for
    /// deterministic output. No pagination — predicate-scoped reads
    /// are expected to return tens to low hundreds of rows in
    /// practice; if a predicate explodes past that, caller should
    /// switch to `neighbors_within` from a specific node.
    pub async fn query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.fresh_conn_for_graph().await?;
        let predicate = predicate.to_string();
        let as_of = as_of.map(str::to_string);
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let rows = match as_of {
                Some(ts) => {
                    let mut stmt = conn.prepare(
                        "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                         FROM ns.main.graph_edges \
                         WHERE relation = ?1 \
                           AND valid_from <= ?2 \
                           AND (valid_to IS NULL OR valid_to > ?2) \
                         ORDER BY valid_from ASC, from_node_id ASC, to_node_id ASC",
                    )?;
                    let mapped = stmt.query_map(params![predicate, ts], row_to_graph_edge)?;
                    let mut out = Vec::new();
                    for r in mapped {
                        out.push(r?);
                    }
                    out
                }
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT from_node_id, to_node_id, relation, valid_from, valid_to, confidence, extractor, strength, stability, last_activated, access_count \
                         FROM ns.main.graph_edges \
                         WHERE relation = ?1 \
                         ORDER BY valid_from ASC, from_node_id ASC, to_node_id ASC",
                    )?;
                    let mapped = stmt.query_map(params![predicate], row_to_graph_edge)?;
                    let mut out = Vec::new();
                    for r in mapped {
                        out.push(r?);
                    }
                    out
                }
            };
            Ok(rows)
        })
        .await
    }

    /// Whole-graph aggregate: node and edge counts, active vs closed
    /// split, top-N relation kinds. Tenant-less because the
    /// `graph_edges` schema has no tenant column (all tenants share
    /// one graph — see schema doc-comment for the design rationale).
    /// Use the `top_relations` field for at-a-glance relation
    /// distribution; full per-kind breakdown can be derived with a
    /// dedicated SQL query.
    pub async fn graph_stats(&self) -> Result<GraphStats, GraphError> {
        let conn = self.fresh_conn_for_graph().await?;
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let total_edges: i64 =
                conn.query_row("SELECT count(*) FROM ns.main.graph_edges", [], |r| r.get(0))?;
            let active_edges: i64 = conn.query_row(
                "SELECT count(*) FROM ns.main.graph_edges WHERE valid_to IS NULL",
                [],
                |r| r.get(0),
            )?;
            let closed_edges = total_edges - active_edges;
            let node_count: i64 = conn.query_row(
                "SELECT count(*) FROM ( \
                   SELECT from_node_id AS n FROM ns.main.graph_edges \
                   UNION SELECT to_node_id FROM ns.main.graph_edges \
                 )",
                [],
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(
                "SELECT relation, count(*) AS c FROM ns.main.graph_edges \
                 GROUP BY relation ORDER BY c DESC, relation ASC LIMIT 16",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut top_relations: Vec<(String, i64)> = Vec::new();
            for r in rows {
                top_relations.push(r?);
            }
            Ok(GraphStats {
                node_count,
                total_edges,
                active_edges,
                closed_edges,
                top_relations,
            })
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
        let conn = self.fresh_conn_for_graph().await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::lance_store::LanceStore;
    use tempfile::tempdir;

    /// Build a four-node graph for the multi-hop / timeline / stats tests:
    ///
    ///   capability_capsule:c1 ─mentions─> entity:e_alpha ─mentions─> capability_capsule:c2
    ///                                                    \─tagged─>  topic:t1
    ///   capability_capsule:c3 ─supersedes─> capability_capsule:c1   (closed at ts=20020)
    ///
    /// 1-hop from `entity:e_alpha` → 3 edges (mentions × 2 + tagged × 1)
    /// 2-hop → adds the c1↔c3 supersedes edge if it's still active
    async fn seed_graph(path: &std::path::Path) -> LanceStore {
        let store = LanceStore::open(path).await.unwrap();
        let edges = vec![
            GraphEdge {
                from_node_id: "capability_capsule:c1".into(),
                to_node_id: "entity:e_alpha".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000010000".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            },
            GraphEdge {
                from_node_id: "capability_capsule:c2".into(),
                to_node_id: "entity:e_alpha".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000010001".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            },
            GraphEdge {
                from_node_id: "entity:e_alpha".into(),
                to_node_id: "topic:t1".into(),
                relation: "tagged".into(),
                valid_from: "00000001778000010002".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            },
            // Pre-closed historical edge — relevant for timeline ordering
            // and graph_stats `closed_edges` count.
            GraphEdge {
                from_node_id: "capability_capsule:c3".into(),
                to_node_id: "capability_capsule:c1".into(),
                relation: "supersedes".into(),
                valid_from: "00000001778000010003".into(),
                valid_to: Some("00000001778000020000".into()),
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            },
        ];
        for e in &edges {
            store.add_edge_direct(e).await.unwrap();
        }
        store
    }

    /// K1 + K3: an edge written with a caller-supplied `confidence`
    /// (K1) and `extractor` provenance tag (K3) round-trips through the
    /// Lance write path and the DuckDB read path unchanged.
    #[tokio::test(flavor = "multi_thread")]
    async fn graph_edge_round_trips_confidence_and_extractor() {
        let dir = tempdir().unwrap();
        let store = LanceStore::open(dir.path()).await.unwrap();
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "capability_capsule:c1".into(),
                to_node_id: "entity:e_alpha".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000010000".into(),
                valid_to: None,
                confidence: Some(0.6),
                extractor: Some("caller".into()),
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();

        let q = DuckDbQuery::open(dir.path()).await.unwrap();
        let edges = q.neighbors_within("entity:e_alpha", 1, None).await.unwrap();
        let edge = edges
            .iter()
            .find(|e| e.from_node_id == "capability_capsule:c1")
            .expect("seeded edge must be present");
        assert_eq!(
            edge.confidence,
            Some(0.6),
            "confidence (K1) must round-trip"
        );
        assert_eq!(
            edge.extractor.as_deref(),
            Some("caller"),
            "extractor provenance (K3) must round-trip"
        );
    }

    /// K1 + K3 migration (closes mempalace-diff-v3 K1, K3): a
    /// `graph_edges` table created with the pre-K1 5-column schema is
    /// transparently upgraded with the `confidence` / `extractor`
    /// columns (backfilled NULL) when `LanceStore::open` runs its
    /// ensure/migrate path. Legacy rows read back `None`; new 7-column
    /// writes succeed against the migrated table.
    #[tokio::test(flavor = "multi_thread")]
    async fn graph_edges_5col_table_migrates_to_add_confidence_extractor() {
        use arrow_array::{RecordBatch, StringArray};
        use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

        let dir = tempdir().unwrap();
        let uri = dir.path().to_str().unwrap();

        // 1. Hand-create graph_edges with the OLD 5-column schema + one
        //    legacy row, simulating a pre-K1 on-disk database.
        {
            let conn = lancedb::connect(uri).execute().await.unwrap();
            let old_schema = std::sync::Arc::new(Schema::new(vec![
                Field::new("from_node_id", DataType::Utf8, false),
                Field::new("to_node_id", DataType::Utf8, false),
                Field::new("relation", DataType::Utf8, false),
                Field::new("valid_from", DataType::Utf8, false),
                Field::new("valid_to", DataType::Utf8, true),
            ]));
            let batch = RecordBatch::try_new(
                old_schema.clone(),
                vec![
                    std::sync::Arc::new(StringArray::from(vec!["capability_capsule:legacy"])),
                    std::sync::Arc::new(StringArray::from(vec!["entity:old"])),
                    std::sync::Arc::new(StringArray::from(vec!["mentions"])),
                    std::sync::Arc::new(StringArray::from(vec!["00000001778000000001"])),
                    std::sync::Arc::new(StringArray::from(vec![None as Option<&str>])),
                ],
            )
            .unwrap();
            let tbl = conn
                .create_empty_table("graph_edges", old_schema)
                .execute()
                .await
                .unwrap();
            tbl.add(batch).execute().await.unwrap();
        }

        // 2. Open through LanceStore — triggers the ensure/migrate path
        //    that must detect the missing columns and add them.
        let store = LanceStore::open(dir.path()).await.unwrap();

        // 3. A NEW edge carrying confidence + extractor must write
        //    cleanly against the migrated (7-col) table.
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "capability_capsule:new".into(),
                to_node_id: "entity:old".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000000002".into(),
                valid_to: None,
                confidence: Some(0.9),
                extractor: Some("caller".into()),
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();

        // 4. Read back via DuckDB: the legacy row reads None/None; the
        //    fresh row carries its declared values.
        let q = DuckDbQuery::open(dir.path()).await.unwrap();
        let edges = q.neighbors_within("entity:old", 1, None).await.unwrap();
        let legacy = edges
            .iter()
            .find(|e| e.from_node_id == "capability_capsule:legacy")
            .expect("legacy row must survive migration");
        assert_eq!(
            legacy.confidence, None,
            "legacy confidence backfills NULL→None"
        );
        assert_eq!(
            legacy.extractor, None,
            "legacy extractor backfills NULL→None"
        );
        let fresh = edges
            .iter()
            .find(|e| e.from_node_id == "capability_capsule:new")
            .expect("post-migration write must be present");
        assert_eq!(fresh.confidence, Some(0.9));
        assert_eq!(fresh.extractor.as_deref(), Some("caller"));
    }

    /// K1 + K3: the `sync_memory_edges` write path (used by the ingest
    /// graph-edge sync) must carry `confidence` / `extractor` through to
    /// storage. It overrides `valid_from` with the server `now` but must
    /// NOT drop the provenance/confidence the caller supplied.
    #[tokio::test(flavor = "multi_thread")]
    async fn sync_memory_edges_preserves_confidence_and_extractor() {
        let dir = tempdir().unwrap();
        let store = LanceStore::open(dir.path()).await.unwrap();
        store
            .sync_memory_edges(
                &[GraphEdge {
                    from_node_id: "capability_capsule:s1".into(),
                    to_node_id: "entity:e_sync".into(),
                    relation: "mentions".into(),
                    valid_from: "ignored-overridden-by-now".into(),
                    valid_to: None,
                    confidence: Some(0.42),
                    extractor: Some("ingest".into()),
                    strength: None,
                    stability: None,
                    last_activated: None,
                    access_count: None,
                }],
                "00000001778000099999",
            )
            .await
            .unwrap();

        let q = DuckDbQuery::open(dir.path()).await.unwrap();
        let edges = q.neighbors_within("entity:e_sync", 1, None).await.unwrap();
        let e = edges
            .iter()
            .find(|e| e.from_node_id == "capability_capsule:s1")
            .expect("synced edge must be present");
        assert_eq!(
            e.confidence,
            Some(0.42),
            "sync_memory_edges must preserve caller confidence"
        );
        assert_eq!(e.extractor.as_deref(), Some("ingest"));
        assert_eq!(
            e.valid_from, "00000001778000099999",
            "sync_memory_edges still overrides valid_from with server now"
        );
    }

    /// K12 (closes mempalace-diff-v4 K12): an edge whose `valid_to`
    /// precedes its `valid_from` is durably invisible — the bitemporal
    /// recall filter `valid_from <= as_of AND (valid_to IS NULL OR
    /// valid_to > as_of)` matches no `as_of`, so the row is stored but
    /// unreachable (mempalace #1214's P0 foot-gun). Reject it at write.
    /// Open intervals (valid_to = None) and point-in-time facts
    /// (valid_to == valid_from) remain allowed.
    #[tokio::test(flavor = "multi_thread")]
    async fn add_edge_direct_rejects_inverted_valid_interval() {
        let dir = tempdir().unwrap();
        let store = LanceStore::open(dir.path()).await.unwrap();

        let err = store
            .add_edge_direct(&GraphEdge {
                from_node_id: "entity:a".into(),
                to_node_id: "entity:b".into(),
                relation: "rel".into(),
                valid_from: "00000001778000020000".into(),
                valid_to: Some("00000001778000010000".into()), // < valid_from
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await;
        assert!(
            matches!(err, Err(GraphError::InvalidInput(_))),
            "inverted valid interval must be rejected: {err:?}"
        );

        // Point-in-time fact (valid_to == valid_from) is allowed.
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "entity:c".into(),
                to_node_id: "entity:d".into(),
                relation: "rel".into(),
                valid_from: "00000001778000010000".into(),
                valid_to: Some("00000001778000010000".into()),
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .expect("point-in-time edge (valid_to == valid_from) is allowed");
    }

    /// K9: `Store::potentiate_edge` reads the active edge, applies the
    /// pure potentiation, and writes the four dynamics columns back.
    /// A non-existent edge is a dropped no-op.
    #[tokio::test(flavor = "multi_thread")]
    async fn potentiate_edge_reads_modifies_writes_dynamics() {
        use crate::storage::Store;
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("p.lance")).await.unwrap();
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "entity:a".into(),
                to_node_id: "entity:b".into(),
                relation: "rel".into(),
                valid_from: "00000001780000000000".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();

        // now is 2h past valid_from (the baseline) → spaced reinforcement.
        let written = store
            .potentiate_edge("entity:a", "entity:b", "rel", "00000001780007200000")
            .await
            .unwrap();
        assert!(written, "active edge must potentiate");

        let edges = store.neighbors_within("entity:a", 1, None).await.unwrap();
        let e = edges
            .iter()
            .find(|e| e.from_node_id == "entity:a")
            .expect("edge present");
        assert!(
            (e.strength.unwrap() - 1.05).abs() < 1e-6,
            "strength grew from default 1.0 to 1.05: {:?}",
            e.strength
        );
        assert_eq!(e.access_count, Some(1));
        assert_eq!(e.last_activated.as_deref(), Some("00000001780007200000"));

        // Potentiating a non-existent edge is a dropped no-op.
        let written2 = store
            .potentiate_edge("entity:x", "entity:y", "rel", "00000001780007200000")
            .await
            .unwrap();
        assert!(!written2, "no active edge → false");
    }

    /// K9: the four dynamics fields (strength / stability /
    /// last_activated / access_count) round-trip through the Lance
    /// write path and the DuckDB read path.
    #[tokio::test(flavor = "multi_thread")]
    async fn graph_edge_round_trips_dynamics_fields() {
        let dir = tempdir().unwrap();
        let store = LanceStore::open(dir.path()).await.unwrap();
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "entity:k9a".into(),
                to_node_id: "entity:k9b".into(),
                relation: "co_occurs_with".into(),
                valid_from: "00000001778000010000".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: Some(2.5),
                stability: Some(1.5),
                last_activated: Some("00000001778000009999".into()),
                access_count: Some(7),
            })
            .await
            .unwrap();

        let q = DuckDbQuery::open(dir.path()).await.unwrap();
        let edges = q.neighbors_within("entity:k9a", 1, None).await.unwrap();
        let e = edges
            .iter()
            .find(|e| e.from_node_id == "entity:k9a")
            .expect("seeded edge must be present");
        assert_eq!(e.strength, Some(2.5));
        assert_eq!(e.stability, Some(1.5));
        assert_eq!(e.last_activated.as_deref(), Some("00000001778000009999"));
        assert_eq!(e.access_count, Some(7));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn neighbors_within_walks_multi_hop_and_dedupes() {
        let dir = tempdir().unwrap();
        let _store = seed_graph(dir.path()).await;
        let q = DuckDbQuery::open(dir.path()).await.unwrap();

        // 1-hop from entity:e_alpha → 3 incident active edges
        // (c1-mentions, c2-mentions, alpha-tagged-t1).
        let one = q.neighbors_within("entity:e_alpha", 1, None).await.unwrap();
        assert_eq!(one.len(), 3, "1-hop neighbors_within mismatch: {one:?}");

        // 2-hop from entity:e_alpha should still only see those 3
        // edges — the only other active edge is c1->e_alpha (already
        // captured) and the c3->c1 edge is closed (valid_to set).
        let two = q.neighbors_within("entity:e_alpha", 2, None).await.unwrap();
        assert_eq!(two.len(), 3, "2-hop with no closed-edge inclusion: {two:?}");

        // Edge-set dedup: the c1->e_alpha edge is reached both as a
        // 1-hop from e_alpha and as a 1-hop from c1 in the 2-hop pass.
        // It must appear exactly once.
        let c1_two = q
            .neighbors_within("capability_capsule:c1", 2, None)
            .await
            .unwrap();
        let c1_mentions: Vec<&GraphEdge> = c1_two
            .iter()
            .filter(|e| e.from_node_id == "capability_capsule:c1" && e.relation == "mentions")
            .collect();
        assert_eq!(
            c1_mentions.len(),
            1,
            "edge-set dedup failed: {c1_mentions:?}"
        );

        // max_hops=0 should clamp to 1 (no off-by-one early return).
        let zero = q.neighbors_within("entity:e_alpha", 0, None).await.unwrap();
        assert_eq!(zero.len(), 3);

        // max_hops well above cap clamps to 3 — same graph, same edge
        // count, just doesn't error.
        let huge = q
            .neighbors_within("entity:e_alpha", 99, None)
            .await
            .unwrap();
        assert_eq!(huge.len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn neighbors_within_honors_as_of() {
        let dir = tempdir().unwrap();
        let store = seed_graph(dir.path()).await;
        // Add one freshly-active edge whose valid_from is *after* the
        // historical inspection point. The as_of probe should NOT see
        // it.
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "entity:e_alpha".into(),
                to_node_id: "entity:e_beta".into(),
                relation: "co_occurs_with".into(),
                valid_from: "00000001778000030000".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();
        let q = DuckDbQuery::open(dir.path()).await.unwrap();

        // as_of = 00000001778000015000 → between the seed edges and
        // the late edge. Late edge is excluded (valid_from > as_of);
        // the closed c3->c1 edge is also excluded because it was
        // closed at 20000, which is after 15000, so it WAS active
        // then — meaning the closed edge SHOULD reappear here.
        let historic = q
            .neighbors_within("entity:e_alpha", 2, Some("00000001778000015000"))
            .await
            .unwrap();
        assert!(
            !historic.iter().any(|e| e.to_node_id == "entity:e_beta"),
            "as_of must exclude edges with valid_from > as_of"
        );
        // Late edge surfaces only when as_of >= its valid_from.
        let now_view = q
            .neighbors_within("entity:e_alpha", 1, Some("00000001778000040000"))
            .await
            .unwrap();
        assert!(
            now_view.iter().any(|e| e.to_node_id == "entity:e_beta"),
            "as_of past the late edge's valid_from must surface it: {now_view:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kg_timeline_includes_closed_edges_in_chrono_order() {
        let dir = tempdir().unwrap();
        let _store = seed_graph(dir.path()).await;
        let q = DuckDbQuery::open(dir.path()).await.unwrap();

        // c1 is on two edges: mentions e_alpha (active, valid_from
        // 10000) and supersedes c1 reverse (closed, valid_from 10003).
        let timeline = q.kg_timeline("capability_capsule:c1").await.unwrap();
        assert_eq!(
            timeline.len(),
            2,
            "expected 2 edges in c1 timeline: {timeline:?}"
        );
        // Chrono order — earliest valid_from first.
        assert_eq!(timeline[0].valid_from, "00000001778000010000");
        assert_eq!(timeline[1].valid_from, "00000001778000010003");
        // Closed edge IS surfaced (timeline shows history, not just
        // active state).
        assert!(timeline.iter().any(|e| e.valid_to.is_some()));
    }

    /// `list_user_tunnels` filters by the `user_tunnel:` relation
    /// prefix convention and ignores closed / auto edges.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_user_tunnels_filters_by_relation_prefix() {
        let dir = tempdir().unwrap();
        let store = seed_graph(dir.path()).await; // 3 active + 1 closed auto-style edges
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "capability_capsule:c1".into(),
                to_node_id: "capability_capsule:c2".into(),
                relation: "user_tunnel:cross_project_link".into(),
                valid_from: "00000001778000050000".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "topic:t1".into(),
                to_node_id: "topic:t2".into(),
                relation: "user_tunnel:related_topics".into(),
                valid_from: "00000001778000050001".into(),
                valid_to: None,
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();
        // closed user-tunnel — must not surface (active-only)
        store
            .add_edge_direct(&GraphEdge {
                from_node_id: "topic:t1".into(),
                to_node_id: "topic:t3".into(),
                relation: "user_tunnel:archived".into(),
                valid_from: "00000001778000050002".into(),
                valid_to: Some("00000001778000060000".into()),
                confidence: None,
                extractor: None,
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            })
            .await
            .unwrap();

        let q = DuckDbQuery::open(dir.path()).await.unwrap();
        let tunnels = q.list_user_tunnels(100).await.unwrap();
        let relations: Vec<&str> = tunnels.iter().map(|e| e.relation.as_str()).collect();
        // Two active user_tunnel edges; ordered by (relation, from, to).
        assert_eq!(
            relations,
            vec![
                "user_tunnel:cross_project_link",
                "user_tunnel:related_topics"
            ]
        );
        // Auto edges and closed tunnels must be excluded.
        assert!(
            !tunnels.iter().any(|e| e.relation == "mentions"),
            "auto edges must not appear: {tunnels:?}"
        );
        assert!(
            !tunnels.iter().any(|e| e.relation == "user_tunnel:archived"),
            "closed tunnel must not appear: {tunnels:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graph_stats_counts_split_and_top_relations() {
        let dir = tempdir().unwrap();
        let _store = seed_graph(dir.path()).await;
        let q = DuckDbQuery::open(dir.path()).await.unwrap();

        let s = q.graph_stats().await.unwrap();
        assert_eq!(s.total_edges, 4);
        assert_eq!(s.active_edges, 3);
        assert_eq!(s.closed_edges, 1);
        // Five distinct nodes: c1, c2, c3, e_alpha, t1.
        assert_eq!(s.node_count, 5);
        // top_relations is ordered desc by count, then asc by name.
        // mentions (2), then supersedes (1) and tagged (1) tied by
        // count and broken by name asc → supersedes, tagged.
        assert_eq!(
            s.top_relations,
            vec![
                ("mentions".to_string(), 2),
                ("supersedes".to_string(), 1),
                ("tagged".to_string(), 1),
            ]
        );
    }
}
