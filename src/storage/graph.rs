use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};

use indradb::{
    AllEdgeQuery, AllVertexQuery, Database, Edge, Identifier, Json, MemoryDatastore, QueryExt,
    QueryOutputValue, SpecificVertexQuery, Vertex,
};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

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
    #[error("invalid graph identifier")]
    InvalidIdentifier,
    #[error("graph backend error: {0}")]
    Backend(String),
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

#[derive(Clone)]
pub struct IndraDbGraphAdapter {
    db: Arc<Mutex<Database<MemoryDatastore>>>,
}

impl IndraDbGraphAdapter {
    pub fn new() -> Self {
        Self::with_path(None)
    }

    pub fn with_path(path: Option<PathBuf>) -> Self {
        let db = match path {
            Some(path) => MemoryDatastore::create_msgpack_db(path),
            None => MemoryDatastore::new_db(),
        };
        Self {
            db: Arc::new(Mutex::new(db)),
        }
    }
}

impl Default for IndraDbGraphAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphStore for IndraDbGraphAdapter {
    fn sync_memory<'a>(&'a self, memory: &'a MemoryRecord) -> GraphFuture<'a, ()> {
        Box::pin(async move {
            let edges = extract_graph_edges(memory);
            let db = self.db.lock().map_err(|_| GraphError::Poisoned)?;
            let node_type = identifier("node")?;
            let node_id_prop = identifier("node_id")?;

            for edge in edges {
                let from_uuid = node_uuid(&edge.from_node_id);
                let to_uuid = node_uuid(&edge.to_node_id);

                let from_vertex = Vertex::with_id(from_uuid, node_type);
                let to_vertex = Vertex::with_id(to_uuid, node_type);
                db.create_vertex(&from_vertex)
                    .map_err(|e| GraphError::Backend(e.to_string()))?;
                db.create_vertex(&to_vertex)
                    .map_err(|e| GraphError::Backend(e.to_string()))?;

                db.set_properties(
                    SpecificVertexQuery::single(from_uuid),
                    node_id_prop,
                    &Json::new(Value::String(edge.from_node_id.clone())),
                )
                .map_err(|e| GraphError::Backend(e.to_string()))?;
                db.set_properties(
                    SpecificVertexQuery::single(to_uuid),
                    node_id_prop,
                    &Json::new(Value::String(edge.to_node_id.clone())),
                )
                .map_err(|e| GraphError::Backend(e.to_string()))?;

                let relation = identifier(&edge.relation)?;
                db.create_edge(&Edge::new(from_uuid, relation, to_uuid))
                    .map_err(|e| GraphError::Backend(e.to_string()))?;
            }

            db.sync().map_err(|e| GraphError::Backend(e.to_string()))?;
            Ok(())
        })
    }

    fn neighbors<'a>(&'a self, node_id: &'a str) -> GraphFuture<'a, Vec<GraphEdge>> {
        Box::pin(async move {
            let db = self.db.lock().map_err(|_| GraphError::Poisoned)?;
            let target = node_uuid(node_id);
            let mut map = vertex_node_id_map(&db)?;
            map.entry(target).or_insert_with(|| node_id.to_string());

            let outputs = db
                .get(AllEdgeQuery)
                .map_err(|e| GraphError::Backend(e.to_string()))?;
            let mut edges = vec![];
            for output in outputs {
                if let QueryOutputValue::Edges(all_edges) = output {
                    for edge in all_edges {
                        if edge.outbound_id != target && edge.inbound_id != target {
                            continue;
                        }
                        let Some(from_node_id) = map.get(&edge.outbound_id).cloned() else {
                            continue;
                        };
                        let Some(to_node_id) = map.get(&edge.inbound_id).cloned() else {
                            continue;
                        };
                        edges.push(GraphEdge {
                            from_node_id,
                            to_node_id,
                            relation: edge.t.as_str().to_string(),
                        });
                    }
                }
            }
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
            let db = self.db.lock().map_err(|_| GraphError::Poisoned)?;
            let mut map = vertex_node_id_map(&db)?;
            for node_id in node_ids {
                map.entry(node_uuid(node_id))
                    .or_insert_with(|| node_id.clone());
            }

            let node_lookup = node_ids.iter().map(String::as_str).collect::<HashSet<_>>();
            let outputs = db
                .get(AllEdgeQuery)
                .map_err(|e| GraphError::Backend(e.to_string()))?;

            let mut memory_ids = HashSet::new();
            for output in outputs {
                if let QueryOutputValue::Edges(all_edges) = output {
                    for edge in all_edges {
                        let Some(from_node_id) = map.get(&edge.outbound_id).map(String::as_str)
                        else {
                            continue;
                        };
                        let Some(to_node_id) = map.get(&edge.inbound_id).map(String::as_str) else {
                            continue;
                        };

                        let adjacent = if node_lookup.contains(from_node_id) {
                            Some(to_node_id)
                        } else if node_lookup.contains(to_node_id) {
                            Some(from_node_id)
                        } else {
                            None
                        };

                        if let Some(memory_id) =
                            adjacent.and_then(|node| node.strip_prefix("memory:"))
                        {
                            memory_ids.insert(memory_id.to_string());
                        }
                    }
                }
            }

            let mut sorted = memory_ids.into_iter().collect::<Vec<_>>();
            sorted.sort();
            Ok(sorted)
        })
    }
}

fn node_uuid(node_id: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, node_id.as_bytes())
}

fn identifier(raw: &str) -> Result<Identifier, GraphError> {
    let normalized = raw
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect::<String>();
    let value = if normalized.is_empty() {
        "edge".to_string()
    } else {
        normalized
    };
    Identifier::new(value).map_err(|_| GraphError::InvalidIdentifier)
}

fn vertex_node_id_map(db: &Database<MemoryDatastore>) -> Result<HashMap<Uuid, String>, GraphError> {
    let query = AllVertexQuery
        .properties()
        .map_err(|_| GraphError::InvalidIdentifier)?;
    let outputs = db
        .get(query)
        .map_err(|e| GraphError::Backend(e.to_string()))?;

    let mut map = HashMap::new();
    for output in outputs {
        if let QueryOutputValue::VertexProperties(properties) = output {
            for vertex_properties in properties {
                for prop in vertex_properties.props {
                    if prop.name.as_str() == "node_id" {
                        if let Some(node) = prop.value.as_str() {
                            map.insert(vertex_properties.vertex.id, node.to_string());
                        }
                    }
                }
            }
        }
    }
    Ok(map)
}
