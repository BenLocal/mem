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

use super::{lancedb_err, parse_col, sessions_schema, sql_quote, LanceStore};
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
    .map_err(|e| StorageError::backend("arrow record batch", e))
}

fn record_batch_to_sessions(batch: &RecordBatch) -> Result<Vec<Session>, StorageError> {
    const TABLE: &str = "sessions";
    let session_id = parse_col::<StringArray>(batch, TABLE, "session_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let caller_agent = parse_col::<StringArray>(batch, TABLE, "caller_agent")?;
    let started_at = parse_col::<StringArray>(batch, TABLE, "started_at")?;
    let last_seen_at = parse_col::<StringArray>(batch, TABLE, "last_seen_at")?;
    let ended_at = parse_col::<StringArray>(batch, TABLE, "ended_at")?;
    let goal = parse_col::<StringArray>(batch, TABLE, "goal")?;
    let memory_count = parse_col::<UInt32Array>(batch, TABLE, "memory_count")?;

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
            .column(
                "last_seen_at",
                format!("greatest(last_seen_at, {})", sql_quote(last_seen_at)),
            )
            .column("memory_count", "memory_count + 1")
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn reconcile_session_after_ingest(
        &self,
        session_id: &str,
        capability_capsule_id: &str,
        occurred_at: &str,
    ) -> Result<(), StorageError> {
        let memories = self
            .conn
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let receipt_filter = format!(
            "session_id = {} AND capability_capsule_id = {}",
            sql_quote(session_id),
            sql_quote(capability_capsule_id),
        );
        if memories
            .count_rows(Some(receipt_filter))
            .await
            .map_err(lancedb_err)?
            == 0
        {
            return Err(StorageError::InvalidData("ingest session receipt missing"));
        }
        let persisted_count = memories
            .count_rows(Some(format!("session_id = {}", sql_quote(session_id))))
            .await
            .map_err(lancedb_err)?;

        let sessions = self
            .conn
            .open_table("sessions")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let persisted_count = u32::try_from(persisted_count).unwrap_or(u32::MAX);
        sessions
            .update()
            .only_if(format!("session_id = {}", sql_quote(session_id)))
            // Compute max against the live row inside the Lance update. Two
            // reconciles can arrive in either order without a stale absolute
            // write moving either aggregate backwards.
            .column(
                "last_seen_at",
                format!("greatest(last_seen_at, {})", sql_quote(occurred_at)),
            )
            .column(
                "memory_count",
                format!("greatest(memory_count, {persisted_count})"),
            )
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
            .map_err(|e| StorageError::backend("lancedb stream", e))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::CapabilityCapsuleRecord;
    use tempfile::tempdir;

    fn capsule(id: &str, session_id: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.to_string(),
            tenant: "tenant-a".into(),
            content: format!("content for {id}"),
            summary: format!("summary for {id}"),
            content_hash: format!("hash-{id}"),
            idempotency_key: Some(format!("idem-{id}")),
            session_id: Some(session_id.to_string()),
            source_agent: "session-recovery-test".into(),
            created_at: "00000001778000000001".into(),
            updated_at: "00000001778000000001".into(),
            ..CapabilityCapsuleRecord::default()
        }
    }

    #[tokio::test]
    async fn reconcile_session_after_ingest_is_idempotent_and_counts_batch_rows() {
        let dir = tempdir().unwrap();
        let repo = LanceStore::open(&dir.path().join("sessions.lance"))
            .await
            .unwrap();
        repo.open_session(
            "session-a",
            "tenant-a",
            "session-recovery-test",
            "00000001778000000000",
        )
        .await
        .unwrap();
        repo.insert_capability_capsules_batch(&[
            capsule("mem_session_1", "session-a"),
            capsule("mem_session_2", "session-a"),
        ])
        .await
        .unwrap();

        repo.reconcile_session_after_ingest("session-a", "mem_session_1", "00000001778000000002")
            .await
            .unwrap();
        repo.reconcile_session_after_ingest("session-a", "mem_session_1", "00000001778000000001")
            .await
            .unwrap();

        let session = repo
            .latest_active_session("tenant-a", "session-recovery-test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.memory_count, 2);
        assert_eq!(session.last_seen_at, "00000001778000000002");
    }
}
