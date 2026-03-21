use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use thiserror::Error;

use crate::{
    domain::memory::{GraphEdge, MemoryRecord},
    pipeline::ingest::extract_graph_edges,
};

type GraphFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, GraphError>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EdgeKey {
    from_node_id: String,
    to_node_id: String,
    relation: String,
}

impl From<&GraphEdge> for EdgeKey {
    fn from(edge: &GraphEdge) -> Self {
        Self {
            from_node_id: edge.from_node_id.clone(),
            to_node_id: edge.to_node_id.clone(),
            relation: edge.relation.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("graph backend unavailable: {0}")]
    Unavailable(&'static str),
    #[error("graph state mutex poisoned")]
    Poisoned,
}

pub trait GraphStore: Send + Sync {
    fn sync_memory<'a>(&'a self, memory: &'a MemoryRecord) -> GraphFuture<'a, ()>;
    fn neighbors<'a>(&'a self, node_id: &'a str) -> GraphFuture<'a, Vec<GraphEdge>>;
    fn related_memory_ids<'a>(&'a self, node_ids: &'a [String]) -> GraphFuture<'a, Vec<String>>;
}

#[derive(Debug, Clone, Default)]
pub struct LocalGraphAdapter {
    state: Arc<Mutex<GraphState>>,
}

#[derive(Debug, Default)]
struct GraphState {
    edges: Vec<GraphEdge>,
    seen: HashSet<EdgeKey>,
}

impl LocalGraphAdapter {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GraphStore for LocalGraphAdapter {
    fn sync_memory<'a>(&'a self, memory: &'a MemoryRecord) -> GraphFuture<'a, ()> {
        Box::pin(async move {
            let extracted = extract_graph_edges(memory);
            let mut state = self.state.lock().map_err(|_| GraphError::Poisoned)?;

            for edge in extracted {
                let key = EdgeKey::from(&edge);
                if state.seen.insert(key) {
                    state.edges.push(edge);
                }
            }

            Ok(())
        })
    }

    fn neighbors<'a>(&'a self, node_id: &'a str) -> GraphFuture<'a, Vec<GraphEdge>> {
        Box::pin(async move {
            let state = self.state.lock().map_err(|_| GraphError::Poisoned)?;
            let mut edges = state
                .edges
                .iter()
                .filter(|edge| edge.from_node_id == node_id || edge.to_node_id == node_id)
                .cloned()
                .collect::<Vec<_>>();
            edges.sort_by(|left, right| {
                left.relation
                    .cmp(&right.relation)
                    .then_with(|| left.from_node_id.cmp(&right.from_node_id))
                    .then_with(|| left.to_node_id.cmp(&right.to_node_id))
            });
            Ok(edges)
        })
    }

    fn related_memory_ids<'a>(&'a self, node_ids: &'a [String]) -> GraphFuture<'a, Vec<String>> {
        Box::pin(async move {
            let state = self.state.lock().map_err(|_| GraphError::Poisoned)?;
            let node_lookup = node_ids.iter().map(String::as_str).collect::<HashSet<_>>();
            let mut memory_ids = state
                .edges
                .iter()
                .filter_map(|edge| {
                    let adjacent = if node_lookup.contains(edge.from_node_id.as_str()) {
                        Some(edge.to_node_id.as_str())
                    } else if node_lookup.contains(edge.to_node_id.as_str()) {
                        Some(edge.from_node_id.as_str())
                    } else {
                        None
                    }?;

                    adjacent
                        .strip_prefix("memory:")
                        .map(|memory_id| memory_id.to_string())
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            memory_ids.sort();
            Ok(memory_ids)
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct IndraDbGraphAdapter;

impl IndraDbGraphAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl GraphStore for IndraDbGraphAdapter {
    fn sync_memory<'a>(&'a self, _memory: &'a MemoryRecord) -> GraphFuture<'a, ()> {
        Box::pin(async move {
            Err(GraphError::Unavailable(
                "indra db graph sync is not configured",
            ))
        })
    }

    fn neighbors<'a>(&'a self, _node_id: &'a str) -> GraphFuture<'a, Vec<GraphEdge>> {
        Box::pin(async move {
            Err(GraphError::Unavailable(
                "indra db graph lookup is not configured",
            ))
        })
    }

    fn related_memory_ids<'a>(&'a self, _node_ids: &'a [String]) -> GraphFuture<'a, Vec<String>> {
        Box::pin(async move {
            Err(GraphError::Unavailable(
                "indra db graph lookup is not configured",
            ))
        })
    }
}
