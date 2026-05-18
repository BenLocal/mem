//! Sessions table: opens/closes/touches and `latest_active_session`.
//! Schema: session_id PK, tenant + caller_agent for identity,
//! started_at + last_seen_at + ended_at (nullable) for lifecycle,
//! goal (nullable string), memory_count (uint32) for usage stats.

use std::sync::Arc;

use arrow_array::{
    builder::{StringBuilder, UInt32Builder},
    Array, RecordBatch, StringArray, UInt32Array,
};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{lancedb_err, sessions_schema, sql_quote, LanceStore};
use crate::domain::session::Session;
use crate::storage::types::StorageError;

fn session_to_record_batch(s: &Session) -> Result<RecordBatch, StorageError> {
    let mut session_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut caller_agent = StringBuilder::new();
    let mut started_at = StringBuilder::new();
    let mut last_seen_at = StringBuilder::new();
    let mut ended_at = StringBuilder::new();
    let mut goal = StringBuilder::new();
    let mut memory_count = UInt32Builder::new();

    session_id.append_value(&s.session_id);
    tenant.append_value(&s.tenant);
    caller_agent.append_value(&s.caller_agent);
    started_at.append_value(&s.started_at);
    last_seen_at.append_value(&s.last_seen_at);
    match &s.ended_at {
        Some(v) => ended_at.append_value(v),
        None => ended_at.append_null(),
    }
    match &s.goal {
        Some(v) => goal.append_value(v),
        None => goal.append_null(),
    }
    memory_count.append_value(s.memory_count);

    let schema = Arc::new(sessions_schema());
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(session_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(caller_agent.finish()),
            Arc::new(started_at.finish()),
            Arc::new(last_seen_at.finish()),
            Arc::new(ended_at.finish()),
            Arc::new(goal.finish()),
            Arc::new(memory_count.finish()),
        ],
    )
    .map_err(|e| StorageError::InvalidInput(format!("arrow batch: {e}")))
}

fn record_batch_to_sessions(batch: &RecordBatch) -> Result<Vec<Session>, StorageError> {
    fn col<'a, T: 'static>(b: &'a RecordBatch, name: &'static str) -> Result<&'a T, StorageError> {
        b.column_by_name(name)
            .ok_or(StorageError::InvalidData("missing column"))?
            .as_any()
            .downcast_ref::<T>()
            .ok_or(StorageError::InvalidData("column type mismatch"))
    }
    let session_id = col::<StringArray>(batch, "session_id")?;
    let tenant = col::<StringArray>(batch, "tenant")?;
    let caller_agent = col::<StringArray>(batch, "caller_agent")?;
    let started_at = col::<StringArray>(batch, "started_at")?;
    let last_seen_at = col::<StringArray>(batch, "last_seen_at")?;
    let ended_at = col::<StringArray>(batch, "ended_at")?;
    let goal = col::<StringArray>(batch, "goal")?;
    let memory_count = col::<UInt32Array>(batch, "memory_count")?;

    let opt_str = |arr: &StringArray, i: usize| -> Option<String> {
        if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i).to_string())
        }
    };

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(Session {
            session_id: session_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            caller_agent: caller_agent.value(i).to_string(),
            started_at: started_at.value(i).to_string(),
            last_seen_at: last_seen_at.value(i).to_string(),
            ended_at: opt_str(ended_at, i),
            goal: opt_str(goal, i),
            memory_count: memory_count.value(i),
        });
    }
    Ok(out)
}

impl LanceStore {
    pub async fn touch_session(
        &self,
        session_id: &str,
        last_seen_at: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("sessions")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // Bump memory_count by 1 and stamp last_seen_at. LanceDB's update
        // `column` accepts SQL expressions, so `memory_count + 1` works.
        table
            .update()
            .only_if(format!("session_id = {}", sql_quote(session_id)))
            .column("last_seen_at", sql_quote(last_seen_at))
            .column("memory_count", "memory_count + 1")
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        let table = self
            .conn
            .open_table("sessions")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!(
                "tenant = {} AND caller_agent = {} AND ended_at IS NULL",
                sql_quote(tenant),
                sql_quote(caller_agent),
            ))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut sessions = Vec::new();
        for b in &batches {
            sessions.extend(record_batch_to_sessions(b)?);
        }
        // ORDER BY last_seen_at DESC LIMIT 1.
        sessions.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        Ok(sessions.into_iter().next())
    }

    pub async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        let session = Session {
            session_id: session_id.to_string(),
            tenant: tenant.to_string(),
            caller_agent: caller_agent.to_string(),
            started_at: now.to_string(),
            last_seen_at: now.to_string(),
            ended_at: None,
            goal: None,
            memory_count: 0,
        };
        let table = self
            .conn
            .open_table("sessions")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = session_to_record_batch(&session)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(session)
    }

    pub async fn close_session(
        &self,
        session_id: &str,
        ended_at: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("sessions")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(format!("session_id = {}", sql_quote(session_id)))
            .column("ended_at", sql_quote(ended_at))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }
}
