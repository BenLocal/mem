//! Backend-agnostic graph operations — Phase 3 sub-trait.
//!
//! Covers active-edge reads (`neighbors_within`, `kg_timeline`,
//! `related_capability_capsule_ids`), tunnel discovery
//! (`find_tunnels`, `follow_tunnels`, `list_user_tunnels`), graph
//! stats, and edge writes (`sync_memory_edges`, `add_edge_direct`,
//! `invalidate_edge`, `close_edges_for_capability_capsule`). Returns
//! `Result<_, GraphError>` (not `StorageError`) because graph errors
//! have their own variant set — see `src/storage/types.rs`.
//!
//! See `docs/backend-coupling.md` §3.1 + §6.4.

use async_trait::async_trait;

use crate::domain::capability_capsule::{GraphEdge, GraphStats};
use crate::storage::types::GraphError;
use crate::storage::Store;

#[async_trait]
pub trait GraphStore: Send + Sync {
    /// 1-hop active neighbors of `node_id` (either endpoint).
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError>;

    /// Multi-hop BFS — `max_hops` is capped at 3 by the underlying
    /// query layer. `as_of=None` means "active now".
    async fn neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// Full edge history (active + closed) incident on `node_id`,
    /// ordered chronologically by `valid_from`.
    async fn kg_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError>;

    /// All edges with `relation = predicate`, optionally restricted
    /// to those active at `as_of` (20-digit ms string). When `as_of`
    /// is `None` the result includes both active and closed edges.
    /// mempalace's `query_relationship` analogue (KG K4).
    async fn query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// Active user-tunnel edges (relation `user:tunnel:*`), bounded
    /// by `limit`.
    async fn list_user_tunnels(&self, limit: usize) -> Result<Vec<GraphEdge>, GraphError>;

    /// User-tunnel edges between two node-id prefixes (e.g.
    /// `prefix_a="repo:mem"`, `prefix_b="repo:other"`).
    async fn find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// Walk user-tunnel edges from `node_id` up to `max_hops`.
    async fn follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError>;

    /// Whole-graph aggregate: node count, edge counts (active /
    /// closed / total), top relations by count.
    async fn graph_stats(&self) -> Result<GraphStats, GraphError>;

    /// 1-hop capsule-id expansion for ranking's graph boost — used
    /// by `pipeline::retrieve`'s graph anchor lookup.
    async fn related_capability_capsule_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError>;

    /// O4 (perf): every active 1-hop edge incident to any node in
    /// `node_ids`, as raw `(from, to)` pairs, in one query. Lets
    /// `compute_graph_boosts` derive per-anchor fanout degree + the
    /// degree-decayed boost without a `neighbors_within` round-trip per
    /// anchor.
    async fn incident_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String)>, GraphError>;

    /// Idempotent edge upsert for memory-derived edges. Writes
    /// active edges (`valid_from = now`, `valid_to = NULL`); a
    /// `(from, to, relation)` triple with an existing active edge
    /// is silently skipped.
    async fn sync_memory_edges(&self, edges: &[GraphEdge], now: &str) -> Result<(), GraphError>;

    /// Direct edge write — preserves caller's `valid_from` verbatim
    /// (no server-side `now` override). Returns `true` if a new
    /// active edge was inserted, `false` if an active duplicate
    /// already existed.
    async fn add_edge_direct(&self, edge: &GraphEdge) -> Result<bool, GraphError>;

    /// Stamp `valid_to = ended_at` on one specific active
    /// `(from, predicate, to)` edge. Idempotent — returns 0 if the
    /// triple had no active edge.
    async fn invalidate_edge(
        &self,
        from_node_id: &str,
        predicate: &str,
        to_node_id: &str,
        ended_at: &str,
    ) -> Result<usize, GraphError>;

    /// Close every active edge originating from `capsule:<id>`. Used
    /// when superseding or hard-deleting a capsule. Returns the
    /// number of edges closed.
    async fn close_edges_for_capability_capsule(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, GraphError>;
}

#[async_trait]
impl GraphStore for Store {
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        Store::neighbors(self, node_id).await
    }

    async fn neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        Store::neighbors_within(self, node_id, max_hops, as_of).await
    }

    async fn kg_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        Store::kg_timeline(self, node_id).await
    }

    async fn query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        Store::query_predicate(self, predicate, as_of).await
    }

    async fn list_user_tunnels(&self, limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        Store::list_user_tunnels(self, limit).await
    }

    async fn find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        Store::find_tunnels(self, prefix_a, prefix_b, limit).await
    }

    async fn follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        Store::follow_tunnels(self, node_id, max_hops).await
    }

    async fn graph_stats(&self) -> Result<GraphStats, GraphError> {
        Store::graph_stats(self).await
    }

    async fn related_capability_capsule_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        Store::related_capability_capsule_ids(self, node_ids).await
    }

    async fn incident_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String)>, GraphError> {
        Store::incident_edges_for_nodes(self, node_ids).await
    }

    async fn sync_memory_edges(&self, edges: &[GraphEdge], now: &str) -> Result<(), GraphError> {
        Store::sync_memory_edges(self, edges, now).await
    }

    async fn add_edge_direct(&self, edge: &GraphEdge) -> Result<bool, GraphError> {
        Store::add_edge_direct(self, edge).await
    }

    async fn invalidate_edge(
        &self,
        from_node_id: &str,
        predicate: &str,
        to_node_id: &str,
        ended_at: &str,
    ) -> Result<usize, GraphError> {
        Store::invalidate_edge(self, from_node_id, predicate, to_node_id, ended_at).await
    }

    async fn close_edges_for_capability_capsule(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, GraphError> {
        Store::close_edges_for_capability_capsule(self, capability_capsule_id).await
    }
}
