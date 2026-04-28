use std::sync::Arc;

use thiserror::Error;

use super::{DuckDbRepository, StorageError};
use crate::domain::memory::{GraphEdge, MemoryRecord};
use crate::pipeline::ingest::extract_graph_edges;

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("graph backend error: {0}")]
    Backend(String),
}

impl From<StorageError> for GraphError {
    fn from(e: StorageError) -> Self {
        GraphError::Backend(e.to_string())
    }
}

impl From<duckdb::Error> for GraphError {
    fn from(e: duckdb::Error) -> Self {
        GraphError::Backend(e.to_string())
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
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
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

    pub async fn sync_memory(&self, memory: &MemoryRecord) -> Result<(), GraphError> {
        let edges = extract_graph_edges(memory);
        if edges.is_empty() {
            return Ok(());
        }
        let now = current_timestamp();
        let mut conn = self.repo.conn()?;
        let tx = conn.transaction()?;
        for edge in edges {
            let exists: i64 = tx.query_row(
                "select count(*) from graph_edges
                  where from_node_id = ?1 and to_node_id = ?2
                    and relation = ?3 and valid_to is null",
                duckdb::params![&edge.from_node_id, &edge.to_node_id, &edge.relation],
                |row| row.get(0),
            )?;
            if exists > 0 {
                continue;
            }
            tx.execute(
                "insert into graph_edges
                   (from_node_id, to_node_id, relation, valid_from, valid_to)
                 values (?1, ?2, ?3, ?4, NULL)",
                duckdb::params![&edge.from_node_id, &edge.to_node_id, &edge.relation, &now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError> {
        let from = format!("memory:{memory_id}");
        let now = current_timestamp();
        let conn = self.repo.conn()?;
        let count = conn.execute(
            "update graph_edges
                set valid_to = ?1
              where from_node_id = ?2
                and valid_to is null",
            duckdb::params![&now, &from],
        )?;
        Ok(count)
    }

    pub async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = std::iter::repeat_n("?", node_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "select distinct
               case when from_node_id in ({0}) then to_node_id else from_node_id end as adjacent
             from graph_edges
             where (from_node_id in ({0}) or to_node_id in ({0}))
               and valid_to is null",
            placeholders
        );
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(&sql)?;
        // Three IN clauses → bind the parameter list three times.
        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::with_capacity(node_ids.len() * 3);
        for _ in 0..3 {
            for n in node_ids {
                params_vec.push(Box::new(n.clone()));
            }
        }
        let params_refs: Vec<&dyn duckdb::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(&params_refs[..], |row| row.get::<_, String>(0))?;
        let mut memory_ids = std::collections::HashSet::new();
        for row in rows {
            let adjacent: String = row?;
            if let Some(memory_id) = adjacent.strip_prefix("memory:") {
                memory_ids.insert(memory_id.to_string());
            }
        }
        let mut out: Vec<String> = memory_ids.into_iter().collect();
        out.sort();
        Ok(out)
    }

    pub async fn neighbors_at(
        &self,
        node_id: &str,
        at: &str,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(
            "select from_node_id, to_node_id, relation, valid_from, valid_to
               from graph_edges
              where (from_node_id = ?1 or to_node_id = ?1)
                and valid_from <= ?2
                and (valid_to is null or valid_to > ?2)
              order by relation, from_node_id, to_node_id",
        )?;
        let rows = stmt.query_map(duckdb::params![node_id, at], map_row_to_edge)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub async fn all_edges_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let from = format!("memory:{memory_id}");
        let conn = self.repo.conn()?;
        let mut stmt = conn.prepare(
            "select from_node_id, to_node_id, relation, valid_from, valid_to
               from graph_edges
              where from_node_id = ?1
              order by valid_from",
        )?;
        let rows = stmt.query_map(duckdb::params![&from], map_row_to_edge)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn current_timestamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
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
