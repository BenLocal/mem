use std::sync::Arc;

use thiserror::Error;

use crate::domain::memory::GraphEdge;
use super::{DuckDbRepository, StorageError};

#[derive(Debug, Error)]
pub enum DuckDbGraphError {
    #[error("graph backend error: {0}")]
    Backend(String),
}

impl From<StorageError> for DuckDbGraphError {
    fn from(e: StorageError) -> Self {
        DuckDbGraphError::Backend(e.to_string())
    }
}

impl From<duckdb::Error> for DuckDbGraphError {
    fn from(e: duckdb::Error) -> Self {
        DuckDbGraphError::Backend(e.to_string())
    }
}

pub struct DuckDbGraphStore {
    repo: Arc<DuckDbRepository>,
}

impl DuckDbGraphStore {
    pub fn new(repo: Arc<DuckDbRepository>) -> Self {
        Self { repo }
    }

    /// Active-edge neighbors. Returns edges where (from = node OR to = node) AND valid_to IS NULL.
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, DuckDbGraphError> {
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(
            "select from_node_id, to_node_id, relation, valid_from, valid_to
               from graph_edges
              where (from_node_id = ?1 or to_node_id = ?1)
                and valid_to is null
              order by relation, from_node_id, to_node_id",
        )?;
        let rows = stmt.query_map(duckdb::params![node_id], map_row_to_edge)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn map_row_to_edge(row: &duckdb::Row<'_>) -> Result<GraphEdge, duckdb::Error> {
    Ok(GraphEdge {
        from_node_id: row.get(0)?,
        to_node_id: row.get(1)?,
        relation: row.get(2)?,
        valid_from: row.get(3)?,
        valid_to: row.get::<_, Option<String>>(4)?,
    })
}
