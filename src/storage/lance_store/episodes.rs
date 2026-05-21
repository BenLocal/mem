//! Episodes table: insert + list-successful. `workflow_candidate` is
//! JSON-encoded as a nullable string column (Arrow doesn't have a
//! native sum type; serde to/from on the field boundary).

use std::sync::Arc;

use arrow_array::{
    builder::{ListBuilder, StringBuilder},
    Array, ListArray, RecordBatch, StringArray,
};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{
    enum_from_str, enum_to_str, episodes_schema, lancedb_err, parse_col, sql_quote, LanceStore,
};
use crate::domain::capability_capsule::{Scope, Visibility};
use crate::domain::episode::EpisodeRecord;
use crate::domain::workflow::WorkflowCandidate;
use crate::storage::types::StorageError;

fn append_list(builder: &mut ListBuilder<StringBuilder>, items: &[String]) {
    for s in items {
        builder.values().append_value(s);
    }
    builder.append(true);
}

fn episode_to_record_batch(e: &EpisodeRecord) -> Result<RecordBatch, StorageError> {
    let mut episode_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut goal = StringBuilder::new();
    let mut steps = ListBuilder::new(StringBuilder::new());
    let mut outcome = StringBuilder::new();
    let mut evidence = ListBuilder::new(StringBuilder::new());
    let mut scope = StringBuilder::new();
    let mut visibility = StringBuilder::new();
    let mut project = StringBuilder::new();
    let mut repo = StringBuilder::new();
    let mut module = StringBuilder::new();
    let mut tags = ListBuilder::new(StringBuilder::new());
    let mut source_agent = StringBuilder::new();
    let mut idempotency_key = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    let mut workflow_candidate = StringBuilder::new();

    episode_id.append_value(&e.episode_id);
    tenant.append_value(&e.tenant);
    goal.append_value(&e.goal);
    append_list(&mut steps, &e.steps);
    outcome.append_value(&e.outcome);
    append_list(&mut evidence, &e.evidence);
    scope.append_value(enum_to_str(&e.scope)?);
    visibility.append_value(enum_to_str(&e.visibility)?);
    match &e.project {
        Some(v) => project.append_value(v),
        None => project.append_null(),
    }
    match &e.repo {
        Some(v) => repo.append_value(v),
        None => repo.append_null(),
    }
    match &e.module {
        Some(v) => module.append_value(v),
        None => module.append_null(),
    }
    append_list(&mut tags, &e.tags);
    source_agent.append_value(&e.source_agent);
    match &e.idempotency_key {
        Some(v) => idempotency_key.append_value(v),
        None => idempotency_key.append_null(),
    }
    created_at.append_value(&e.created_at);
    updated_at.append_value(&e.updated_at);
    match &e.workflow_candidate {
        Some(c) => workflow_candidate.append_value(
            serde_json::to_string(c)
                .map_err(|err| StorageError::InvalidInput(format!("workflow_candidate: {err}")))?,
        ),
        None => workflow_candidate.append_null(),
    }

    let schema = Arc::new(episodes_schema());
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(episode_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(goal.finish()),
            Arc::new(steps.finish()),
            Arc::new(outcome.finish()),
            Arc::new(evidence.finish()),
            Arc::new(scope.finish()),
            Arc::new(visibility.finish()),
            Arc::new(project.finish()),
            Arc::new(repo.finish()),
            Arc::new(module.finish()),
            Arc::new(tags.finish()),
            Arc::new(source_agent.finish()),
            Arc::new(idempotency_key.finish()),
            Arc::new(created_at.finish()),
            Arc::new(updated_at.finish()),
            Arc::new(workflow_candidate.finish()),
        ],
    )
    .map_err(|e| StorageError::InvalidInput(format!("arrow batch: {e}")))
}

fn record_batch_to_episodes(batch: &RecordBatch) -> Result<Vec<EpisodeRecord>, StorageError> {
    const TABLE: &str = "episodes";
    let episode_id = parse_col::<StringArray>(batch, TABLE, "episode_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let goal = parse_col::<StringArray>(batch, TABLE, "goal")?;
    let steps = parse_col::<ListArray>(batch, TABLE, "steps")?;
    let outcome = parse_col::<StringArray>(batch, TABLE, "outcome")?;
    let evidence = parse_col::<ListArray>(batch, TABLE, "evidence")?;
    let scope = parse_col::<StringArray>(batch, TABLE, "scope")?;
    let visibility = parse_col::<StringArray>(batch, TABLE, "visibility")?;
    let project = parse_col::<StringArray>(batch, TABLE, "project")?;
    let repo = parse_col::<StringArray>(batch, TABLE, "repo")?;
    let module = parse_col::<StringArray>(batch, TABLE, "module")?;
    let tags = parse_col::<ListArray>(batch, TABLE, "tags")?;
    let source_agent = parse_col::<StringArray>(batch, TABLE, "source_agent")?;
    let idempotency_key = parse_col::<StringArray>(batch, TABLE, "idempotency_key")?;
    let created_at = parse_col::<StringArray>(batch, TABLE, "created_at")?;
    let updated_at = parse_col::<StringArray>(batch, TABLE, "updated_at")?;
    let workflow_candidate = parse_col::<StringArray>(batch, TABLE, "workflow_candidate")?;

    let opt_str = |arr: &StringArray, i: usize| -> Option<String> {
        if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i).to_string())
        }
    };
    let read_list = |arr: &ListArray, i: usize| -> Result<Vec<String>, StorageError> {
        let row = arr.value(i);
        let s = row
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or(StorageError::InvalidData("list inner not string"))?;
        Ok((0..s.len()).map(|j| s.value(j).to_string()).collect())
    };

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let scope_v: Scope = enum_from_str(scope.value(i))?;
        let visibility_v: Visibility = enum_from_str(visibility.value(i))?;
        let workflow = if workflow_candidate.is_null(i) {
            None
        } else {
            Some(
                serde_json::from_str::<WorkflowCandidate>(workflow_candidate.value(i))
                    .map_err(|e| StorageError::InvalidInput(format!("workflow_candidate: {e}")))?,
            )
        };
        out.push(EpisodeRecord {
            episode_id: episode_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            goal: goal.value(i).to_string(),
            steps: read_list(steps, i)?,
            outcome: outcome.value(i).to_string(),
            evidence: read_list(evidence, i)?,
            scope: scope_v,
            visibility: visibility_v,
            project: opt_str(project, i),
            repo: opt_str(repo, i),
            module: opt_str(module, i),
            tags: read_list(tags, i)?,
            source_agent: source_agent.value(i).to_string(),
            idempotency_key: opt_str(idempotency_key, i),
            created_at: created_at.value(i).to_string(),
            updated_at: updated_at.value(i).to_string(),
            workflow_candidate: workflow,
        });
    }
    Ok(out)
}

impl LanceStore {
    pub async fn insert_episode(
        &self,
        episode: EpisodeRecord,
    ) -> Result<EpisodeRecord, StorageError> {
        let table = self
            .conn
            .open_table("episodes")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = episode_to_record_batch(&episode)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(episode)
    }

    pub async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        // Legacy DuckDB filter: status = 'succeeded'. The Lance schema
        // doesn't carry status (we never stored failures), so all rows
        // for the tenant qualify.
        let table = self
            .conn
            .open_table("episodes")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!("tenant = {}", sql_quote(tenant)))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_episodes(b)?);
        }
        // ORDER BY created_at DESC.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }
}
