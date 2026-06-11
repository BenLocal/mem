//! `evolution_candidates` table — durable anti-jitter state for the
//! capsule self-evolution worker (doc `docs/evolution-worker.md` §8.2).
//!
//! One row = one candidate evolution operation (merge / generalize)
//! with its accumulated evidence and consecutive-cycle counter. The
//! K-cycle gate reads/writes this table every sweep, so the state
//! survives process restarts — without durability a restart would
//! reset every candidate's clock and the gate could never open.
//!
//! Upsert is delete-then-add keyed on `candidate_id` (LanceDB has no
//! PK enforcement — same pattern as `mine_cursors`). `member_ids` /
//! `result_capsule_ids` are JSON-encoded string arrays: candidates
//! are read in worker sweeps only (never hot-path), so JSON keeps the
//! schema flat instead of pulling Arrow list columns into the parser.

use std::sync::Arc;

use arrow_array::{
    builder::{Float32Builder, Int64Builder, StringBuilder},
    Array, Float32Array, Int64Array, RecordBatch, StringArray,
};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{evolution_candidates_schema, lancedb_err, parse_col, sql_quote, LanceStore};
use crate::storage::types::StorageError;

/// One row of the `evolution_candidates` table.
#[derive(Debug, Clone, PartialEq)]
pub struct EvolutionCandidate {
    pub candidate_id: String,
    pub tenant: String,
    /// `"merge"` | `"generalize"` (E1 operator set).
    pub op_kind: String,
    /// Capsule ids participating in the operation, as first proposed.
    pub member_ids: Vec<String>,
    /// Operator parameters snapshot (JSON object, e.g. thresholds).
    pub params: String,
    /// `E_t = β·E_{t-1} + s_t` accumulated evidence.
    pub evidence: f32,
    /// Consecutive sweeps the signal held — the K-cycle gate counter.
    pub consecutive_cycles: i64,
    /// `pending` | `executed` | `cancelled`.
    pub status: String,
    pub first_proposed_at: String,
    pub last_signal_at: String,
    pub executed_at: Option<String>,
    /// Capsule ids produced by execution (rollback entry point).
    pub result_capsule_ids: Vec<String>,
}

fn encode_ids(ids: &[String]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string())
}

fn decode_ids(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn candidate_to_record_batch(c: &EvolutionCandidate) -> Result<RecordBatch, StorageError> {
    let mut candidate_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut op_kind = StringBuilder::new();
    let mut member_ids = StringBuilder::new();
    let mut params = StringBuilder::new();
    let mut evidence = Float32Builder::new();
    let mut consecutive = Int64Builder::new();
    let mut status = StringBuilder::new();
    let mut first_proposed = StringBuilder::new();
    let mut last_signal = StringBuilder::new();
    let mut executed = StringBuilder::new();
    let mut result_ids = StringBuilder::new();
    candidate_id.append_value(&c.candidate_id);
    tenant.append_value(&c.tenant);
    op_kind.append_value(&c.op_kind);
    member_ids.append_value(encode_ids(&c.member_ids));
    params.append_value(&c.params);
    evidence.append_value(c.evidence);
    consecutive.append_value(c.consecutive_cycles);
    status.append_value(&c.status);
    first_proposed.append_value(&c.first_proposed_at);
    last_signal.append_value(&c.last_signal_at);
    match &c.executed_at {
        Some(ts) => executed.append_value(ts),
        None => executed.append_null(),
    }
    result_ids.append_value(encode_ids(&c.result_capsule_ids));
    let schema = Arc::new(evolution_candidates_schema());
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(candidate_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(op_kind.finish()),
            Arc::new(member_ids.finish()),
            Arc::new(params.finish()),
            Arc::new(evidence.finish()),
            Arc::new(consecutive.finish()),
            Arc::new(status.finish()),
            Arc::new(first_proposed.finish()),
            Arc::new(last_signal.finish()),
            Arc::new(executed.finish()),
            Arc::new(result_ids.finish()),
        ],
    )
    .map_err(|e| StorageError::InvalidInput(format!("arrow batch: {e}")))
}

fn record_batch_to_candidates(
    batch: &RecordBatch,
) -> Result<Vec<EvolutionCandidate>, StorageError> {
    const TABLE: &str = "evolution_candidates";
    let candidate_id = parse_col::<StringArray>(batch, TABLE, "candidate_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let op_kind = parse_col::<StringArray>(batch, TABLE, "op_kind")?;
    let member_ids = parse_col::<StringArray>(batch, TABLE, "member_ids")?;
    let params = parse_col::<StringArray>(batch, TABLE, "params")?;
    let evidence = parse_col::<Float32Array>(batch, TABLE, "evidence")?;
    let consecutive = parse_col::<Int64Array>(batch, TABLE, "consecutive_cycles")?;
    let status = parse_col::<StringArray>(batch, TABLE, "status")?;
    let first_proposed = parse_col::<StringArray>(batch, TABLE, "first_proposed_at")?;
    let last_signal = parse_col::<StringArray>(batch, TABLE, "last_signal_at")?;
    let executed = parse_col::<StringArray>(batch, TABLE, "executed_at")?;
    let result_ids = parse_col::<StringArray>(batch, TABLE, "result_capsule_ids")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(EvolutionCandidate {
            candidate_id: candidate_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            op_kind: op_kind.value(i).to_string(),
            member_ids: decode_ids(member_ids.value(i)),
            params: params.value(i).to_string(),
            evidence: evidence.value(i),
            consecutive_cycles: consecutive.value(i),
            status: status.value(i).to_string(),
            first_proposed_at: first_proposed.value(i).to_string(),
            last_signal_at: last_signal.value(i).to_string(),
            executed_at: if executed.is_null(i) {
                None
            } else {
                Some(executed.value(i).to_string())
            },
            result_capsule_ids: decode_ids(result_ids.value(i)),
        });
    }
    Ok(out)
}

impl LanceStore {
    /// Upsert one candidate row keyed on `candidate_id` (delete + add,
    /// `mine_cursors` pattern — LanceDB has no PK enforcement).
    pub async fn upsert_evolution_candidate(
        &self,
        candidate: EvolutionCandidate,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("evolution_candidates")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .delete(&format!(
                "candidate_id = {}",
                sql_quote(&candidate.candidate_id),
            ))
            .await
            .map_err(lancedb_err)?;
        let batch = candidate_to_record_batch(&candidate)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(())
    }

    /// List candidates for one tenant, optionally filtered by status.
    /// Sweep-time read (small table) — no pagination.
    pub async fn list_evolution_candidates(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionCandidate>, StorageError> {
        let table = self
            .conn
            .open_table("evolution_candidates")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut filter = format!("tenant = {}", sql_quote(tenant));
        if let Some(s) = status {
            filter.push_str(&format!(" AND status = {}", sql_quote(s)));
        }
        let stream = table
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_candidates(b)?);
        }
        Ok(out)
    }
}
