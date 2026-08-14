use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use arrow_array::{
    builder::{StringBuilder, UInt32Builder, UInt64Builder},
    Array, RecordBatch, StringArray, UInt32Array, UInt64Array,
};
use futures::TryStreamExt;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::NewColumnTransform;

use super::{ensure_table, lancedb_err, parse_col, sql_quote, LanceStore};
use crate::domain::{
    CompletedToolRound, CompletedToolRoundIndexBuild, LatestCompletedToolRounds,
    RoundIndexBuildStatus, RoundIntegrity, RoundSealKind, SkillCandidateEvidence,
    SkillCandidateRoundEvidence, SourceAdapter, COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
    COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
};
use crate::storage::StorageError;

const ROUNDS_TABLE: &str = "completed_tool_rounds";
const BUILDS_TABLE: &str = "completed_tool_round_index_builds";
const HEADS_TABLE: &str = "completed_tool_round_index_heads";
const MAX_BUILD_POINTER_ROWS: usize = 100_000;
const MAX_LATEST_ROUNDS: usize = 1_000;

#[derive(Debug, Clone)]
struct CompletedToolRoundHead {
    tenant: String,
    session_id: String,
    generation_id: String,
    updated_at: String,
}

pub(super) async fn ensure_completed_tool_round_tables(
    conn: &lancedb::Connection,
) -> Result<(), StorageError> {
    ensure_table(conn, ROUNDS_TABLE, completed_tool_rounds_schema()).await?;
    migrate_completed_tool_round_task_signal_columns(conn).await?;
    ensure_table(conn, BUILDS_TABLE, completed_tool_round_builds_schema()).await?;
    migrate_completed_tool_round_build_columns(conn).await?;
    ensure_table(conn, HEADS_TABLE, completed_tool_round_heads_schema()).await
}

async fn migrate_completed_tool_round_build_columns(
    conn: &lancedb::Connection,
) -> Result<(), StorageError> {
    let table = conn
        .open_table(BUILDS_TABLE)
        .execute()
        .await
        .map_err(lancedb_err)?;
    let schema = table.schema().await.map_err(lancedb_err)?;
    if schema.field_with_name("task_signal_version").is_err() {
        table
            .add_columns(
                NewColumnTransform::AllNulls(Arc::new(Schema::new(vec![Field::new(
                    "task_signal_version",
                    DataType::UInt32,
                    true,
                )]))),
                None,
            )
            .await
            .map_err(lancedb_err)?;
    }
    Ok(())
}

async fn migrate_completed_tool_round_task_signal_columns(
    conn: &lancedb::Connection,
) -> Result<(), StorageError> {
    let table = conn
        .open_table(ROUNDS_TABLE)
        .execute()
        .await
        .map_err(lancedb_err)?;
    let schema = table.schema().await.map_err(lancedb_err)?;
    let mut missing = Vec::new();
    for (name, data_type) in [
        ("task_fingerprint", DataType::Utf8),
        ("task_signal_version", DataType::UInt32),
        ("tool_pattern_fingerprint", DataType::Utf8),
        ("round_completed_at", DataType::Utf8),
    ] {
        if schema.field_with_name(name).is_err() {
            missing.push(Field::new(name, data_type, true));
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    table
        .add_columns(
            NewColumnTransform::AllNulls(Arc::new(Schema::new(missing))),
            None,
        )
        .await
        .map_err(lancedb_err)?;
    Ok(())
}

fn completed_tool_rounds_schema() -> Schema {
    Schema::new(vec![
        Field::new("round_id", DataType::Utf8, false),
        Field::new("generation_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("caller_agent", DataType::Utf8, false),
        Field::new("source_adapter", DataType::Utf8, false),
        Field::new("transcript_path", DataType::Utf8, false),
        Field::new("start_line_number", DataType::UInt64, false),
        Field::new("start_block_index", DataType::UInt32, false),
        Field::new("end_line_number", DataType::UInt64, false),
        Field::new("end_block_index", DataType::UInt32, false),
        Field::new("start_message_uuid", DataType::Utf8, true),
        Field::new("final_message_uuid", DataType::Utf8, true),
        Field::new("tool_call_ids_json", DataType::Utf8, false),
        Field::new("tool_names_json", DataType::Utf8, false),
        Field::new("tool_call_count", DataType::UInt32, false),
        Field::new("matched_result_count", DataType::UInt32, false),
        Field::new("missing_result_count", DataType::UInt32, false),
        Field::new("orphan_result_count", DataType::UInt32, false),
        Field::new("error_result_count", DataType::UInt32, false),
        Field::new("unknown_result_status_count", DataType::UInt32, false),
        Field::new("round_completed_at", DataType::Utf8, true),
        Field::new("seal_kind", DataType::Utf8, false),
        Field::new("integrity", DataType::Utf8, false),
        Field::new("source_fingerprint", DataType::Utf8, false),
        Field::new("task_fingerprint", DataType::Utf8, true),
        Field::new("task_signal_version", DataType::UInt32, true),
        Field::new("tool_pattern_fingerprint", DataType::Utf8, true),
        Field::new("projector_version", DataType::UInt32, false),
        Field::new("projected_at", DataType::Utf8, false),
    ])
}

fn completed_tool_round_builds_schema() -> Schema {
    Schema::new(vec![
        Field::new("generation_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("projector_version", DataType::UInt32, false),
        Field::new("task_signal_version", DataType::UInt32, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("source_block_count", DataType::UInt64, false),
        Field::new("source_fingerprint", DataType::Utf8, false),
        Field::new("round_count", DataType::UInt64, false),
        Field::new("started_at", DataType::Utf8, false),
        Field::new("completed_at", DataType::Utf8, true),
    ])
}

fn completed_tool_round_heads_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("generation_id", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

fn rounds_to_record_batch(
    build: &CompletedToolRoundIndexBuild,
    rounds: &[CompletedToolRound],
) -> Result<RecordBatch, StorageError> {
    let mut round_id = StringBuilder::new();
    let mut generation_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut caller_agent = StringBuilder::new();
    let mut source_adapter = StringBuilder::new();
    let mut transcript_path = StringBuilder::new();
    let mut start_line_number = UInt64Builder::new();
    let mut start_block_index = UInt32Builder::new();
    let mut end_line_number = UInt64Builder::new();
    let mut end_block_index = UInt32Builder::new();
    let mut start_message_uuid = StringBuilder::new();
    let mut final_message_uuid = StringBuilder::new();
    let mut tool_call_ids_json = StringBuilder::new();
    let mut tool_names_json = StringBuilder::new();
    let mut tool_call_count = UInt32Builder::new();
    let mut matched_result_count = UInt32Builder::new();
    let mut missing_result_count = UInt32Builder::new();
    let mut orphan_result_count = UInt32Builder::new();
    let mut error_result_count = UInt32Builder::new();
    let mut unknown_result_status_count = UInt32Builder::new();
    let mut round_completed_at = StringBuilder::new();
    let mut seal_kind = StringBuilder::new();
    let mut integrity = StringBuilder::new();
    let mut source_fingerprint = StringBuilder::new();
    let mut task_fingerprint = StringBuilder::new();
    let mut task_signal_version = UInt32Builder::new();
    let mut tool_pattern_fingerprint = StringBuilder::new();
    let mut projector_version = UInt32Builder::new();
    let mut projected_at = StringBuilder::new();

    for round in rounds {
        round_id.append_value(&round.round_id);
        generation_id.append_value(&build.generation_id);
        tenant.append_value(&round.tenant);
        append_optional(&mut session_id, round.session_id.as_deref());
        caller_agent.append_value(&round.caller_agent);
        source_adapter.append_value(round.source_adapter.as_db_str());
        transcript_path.append_value(&round.transcript_path);
        start_line_number.append_value(round.start_line_number);
        start_block_index.append_value(round.start_block_index);
        end_line_number.append_value(round.end_line_number);
        end_block_index.append_value(round.end_block_index);
        append_optional(&mut start_message_uuid, round.start_message_uuid.as_deref());
        append_optional(&mut final_message_uuid, round.final_message_uuid.as_deref());
        tool_call_ids_json.append_value(serde_json::to_string(&round.tool_call_ids)?);
        tool_names_json.append_value(serde_json::to_string(&round.tool_names)?);
        tool_call_count.append_value(round.tool_call_count);
        matched_result_count.append_value(round.matched_result_count);
        missing_result_count.append_value(round.missing_result_count);
        orphan_result_count.append_value(round.orphan_result_count);
        error_result_count.append_value(round.error_result_count);
        unknown_result_status_count.append_value(round.unknown_result_status_count);
        append_optional(&mut round_completed_at, round.completed_at.as_deref());
        seal_kind.append_value(round.seal_kind.as_db_str());
        integrity.append_value(round.integrity.as_db_str());
        source_fingerprint.append_value(&round.source_fingerprint);
        append_optional(&mut task_fingerprint, round.task_fingerprint.as_deref());
        task_signal_version.append_value(round.task_signal_version);
        tool_pattern_fingerprint.append_value(&round.tool_pattern_fingerprint);
        projector_version.append_value(round.projector_version);
        projected_at.append_value(build.completed_at.as_deref().unwrap_or(&build.started_at));
    }

    RecordBatch::try_new(
        Arc::new(completed_tool_rounds_schema()),
        vec![
            Arc::new(round_id.finish()),
            Arc::new(generation_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(session_id.finish()),
            Arc::new(caller_agent.finish()),
            Arc::new(source_adapter.finish()),
            Arc::new(transcript_path.finish()),
            Arc::new(start_line_number.finish()),
            Arc::new(start_block_index.finish()),
            Arc::new(end_line_number.finish()),
            Arc::new(end_block_index.finish()),
            Arc::new(start_message_uuid.finish()),
            Arc::new(final_message_uuid.finish()),
            Arc::new(tool_call_ids_json.finish()),
            Arc::new(tool_names_json.finish()),
            Arc::new(tool_call_count.finish()),
            Arc::new(matched_result_count.finish()),
            Arc::new(missing_result_count.finish()),
            Arc::new(orphan_result_count.finish()),
            Arc::new(error_result_count.finish()),
            Arc::new(unknown_result_status_count.finish()),
            Arc::new(round_completed_at.finish()),
            Arc::new(seal_kind.finish()),
            Arc::new(integrity.finish()),
            Arc::new(source_fingerprint.finish()),
            Arc::new(task_fingerprint.finish()),
            Arc::new(task_signal_version.finish()),
            Arc::new(tool_pattern_fingerprint.finish()),
            Arc::new(projector_version.finish()),
            Arc::new(projected_at.finish()),
        ],
    )
    .map_err(|error| StorageError::backend("completed tool round record batch", error))
}

fn build_to_record_batch(
    build: &CompletedToolRoundIndexBuild,
) -> Result<RecordBatch, StorageError> {
    let mut generation_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut projector_version = UInt32Builder::new();
    let mut task_signal_version = UInt32Builder::new();
    let mut status = StringBuilder::new();
    let mut source_block_count = UInt64Builder::new();
    let mut source_fingerprint = StringBuilder::new();
    let mut round_count = UInt64Builder::new();
    let mut started_at = StringBuilder::new();
    let mut completed_at = StringBuilder::new();
    generation_id.append_value(&build.generation_id);
    tenant.append_value(&build.tenant);
    session_id.append_value(&build.session_id);
    projector_version.append_value(build.projector_version);
    task_signal_version.append_value(build.task_signal_version);
    status.append_value(build.status.as_db_str());
    source_block_count.append_value(build.source_block_count);
    source_fingerprint.append_value(&build.source_fingerprint);
    round_count.append_value(build.round_count);
    started_at.append_value(&build.started_at);
    append_optional(&mut completed_at, build.completed_at.as_deref());
    RecordBatch::try_new(
        Arc::new(completed_tool_round_builds_schema()),
        vec![
            Arc::new(generation_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(session_id.finish()),
            Arc::new(projector_version.finish()),
            Arc::new(task_signal_version.finish()),
            Arc::new(status.finish()),
            Arc::new(source_block_count.finish()),
            Arc::new(source_fingerprint.finish()),
            Arc::new(round_count.finish()),
            Arc::new(started_at.finish()),
            Arc::new(completed_at.finish()),
        ],
    )
    .map_err(|error| StorageError::backend("completed tool round build batch", error))
}

fn head_to_record_batch(head: &CompletedToolRoundHead) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut generation_id = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    tenant.append_value(&head.tenant);
    session_id.append_value(&head.session_id);
    generation_id.append_value(&head.generation_id);
    updated_at.append_value(&head.updated_at);
    RecordBatch::try_new(
        Arc::new(completed_tool_round_heads_schema()),
        vec![
            Arc::new(tenant.finish()),
            Arc::new(session_id.finish()),
            Arc::new(generation_id.finish()),
            Arc::new(updated_at.finish()),
        ],
    )
    .map_err(|error| StorageError::backend("completed tool round head batch", error))
}

fn append_optional(builder: &mut StringBuilder, value: Option<&str>) {
    if let Some(value) = value {
        builder.append_value(value);
    } else {
        builder.append_null();
    }
}

impl LanceStore {
    pub async fn publish_completed_tool_round_generation(
        &self,
        build: &CompletedToolRoundIndexBuild,
        rounds: &[CompletedToolRound],
    ) -> Result<(), StorageError> {
        if build.status != RoundIndexBuildStatus::Completed || build.completed_at.is_none() {
            return Err(StorageError::InvalidInput(
                "completed tool round publication requires a completed build".into(),
            ));
        }
        if build.round_count != rounds.len() as u64 {
            return Err(StorageError::InvalidInput(
                "completed tool round build count does not match rows".into(),
            ));
        }
        if rounds.iter().any(|round| {
            round.tenant != build.tenant
                || round.session_id.as_deref() != Some(build.session_id.as_str())
                || round.projector_version != build.projector_version
                || round.task_signal_version != build.task_signal_version
                || round.caller_agent.trim().is_empty()
                || round.caller_agent.len() > 256
                || round.caller_agent.chars().any(char::is_control)
        }) {
            return Err(StorageError::InvalidInput(
                "completed tool round row scope, projector, or agent is invalid".into(),
            ));
        }
        let builds = self
            .conn
            .open_table(BUILDS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let building = CompletedToolRoundIndexBuild {
            status: RoundIndexBuildStatus::Building,
            completed_at: None,
            ..build.clone()
        };
        builds
            .add(build_to_record_batch(&building)?)
            .execute()
            .await
            .map_err(lancedb_err)?;

        if !rounds.is_empty() {
            let rounds_table = self
                .conn
                .open_table(ROUNDS_TABLE)
                .execute()
                .await
                .map_err(lancedb_err)?;
            rounds_table
                .add(rounds_to_record_batch(build, rounds)?)
                .execute()
                .await
                .map_err(lancedb_err)?;
        }

        builds
            .delete(&format!(
                "generation_id = {}",
                sql_quote(&build.generation_id)
            ))
            .await
            .map_err(lancedb_err)?;
        builds
            .add(build_to_record_batch(build)?)
            .execute()
            .await
            .map_err(lancedb_err)?;

        // The head is the only mutable publication pointer. Build and round
        // generations remain immutable: if this final step fails, readers keep
        // seeing the previous head and the newly completed generation is only
        // an unreachable audit row that a later rebuild can supersede.
        let heads = self
            .conn
            .open_table(HEADS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let head_filter = format!(
            "tenant = {} AND session_id = {}",
            sql_quote(&build.tenant),
            sql_quote(&build.session_id)
        );
        let batches: Vec<RecordBatch> = heads
            .query()
            .only_if(head_filter.clone())
            .limit(2)
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("completed tool round head stream", error))?;
        let mut existing = Vec::new();
        for batch in &batches {
            existing.extend(record_batch_to_heads(batch)?);
        }
        if existing.len() > 1 {
            return Err(StorageError::InvalidData(
                "duplicate completed tool round heads",
            ));
        }
        let head = CompletedToolRoundHead {
            tenant: build.tenant.clone(),
            session_id: build.session_id.clone(),
            generation_id: build.generation_id.clone(),
            updated_at: build
                .completed_at
                .clone()
                .unwrap_or_else(|| build.started_at.clone()),
        };
        if existing.is_empty() {
            heads
                .add(head_to_record_batch(&head)?)
                .execute()
                .await
                .map_err(lancedb_err)?;
        } else {
            heads
                .update()
                .only_if(head_filter)
                .column("generation_id", sql_quote(&head.generation_id))
                .column("updated_at", sql_quote(&head.updated_at))
                .execute()
                .await
                .map_err(lancedb_err)?;
        }
        Ok(())
    }

    pub async fn latest_completed_tool_rounds(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<LatestCompletedToolRounds, StorageError> {
        let heads_table = self
            .conn
            .open_table(HEADS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = heads_table
            .query()
            .only_if(format!(
                "tenant = {} AND session_id = {}",
                sql_quote(tenant),
                sql_quote(session_id)
            ))
            .limit(2)
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("completed tool round head stream", error))?;
        let mut heads = Vec::new();
        for batch in &batches {
            heads.extend(record_batch_to_heads(batch)?);
        }
        if heads.len() > 1 {
            return Err(StorageError::InvalidData(
                "duplicate completed tool round heads",
            ));
        }
        let Some(head) = heads.pop() else {
            return Ok(LatestCompletedToolRounds::default());
        };

        let builds_table = self
            .conn
            .open_table(BUILDS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = builds_table
            .query()
            .only_if(format!(
                "generation_id = {} AND tenant = {} AND session_id = {} AND status = 'completed'",
                sql_quote(&head.generation_id),
                sql_quote(tenant),
                sql_quote(session_id)
            ))
            .limit(2)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("completed tool round build stream", error))?;
        let mut builds = Vec::new();
        for batch in &batches {
            builds.extend(record_batch_to_builds(batch)?);
        }
        if builds.len() != 1 {
            return Err(StorageError::InvalidData(
                "completed tool round head does not resolve to one build",
            ));
        }
        let build = builds.remove(0);

        let rounds_table = self
            .conn
            .open_table(ROUNDS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let round_filter = format!(
            "generation_id = {} AND tenant = {} AND session_id = {}",
            sql_quote(&build.generation_id),
            sql_quote(tenant),
            sql_quote(session_id),
        );
        let stored_round_count = rounds_table
            .count_rows(Some(round_filter.clone()))
            .await
            .map_err(lancedb_err)? as u64;
        let stream = rounds_table
            .query()
            .only_if(round_filter)
            .limit(MAX_LATEST_ROUNDS)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("completed tool round stream", error))?;
        let mut rounds = Vec::new();
        for batch in &batches {
            rounds.extend(record_batch_to_rounds(batch)?);
        }
        rounds.sort_by(|left, right| {
            (
                &left.transcript_path,
                left.start_line_number,
                left.start_block_index,
                &left.round_id,
            )
                .cmp(&(
                    &right.transcript_path,
                    right.start_line_number,
                    right.start_block_index,
                    &right.round_id,
                ))
        });
        Ok(LatestCompletedToolRounds {
            build: Some(build),
            stored_round_count,
            rounds,
        })
    }

    /// Returns bounded, content-free candidate evidence from the latest
    /// completed generation of every session. Historical generations remain
    /// immutable for audit but never count twice toward a trigger.
    pub async fn latest_skill_candidate_evidence(
        &self,
        max_builds: usize,
        max_rounds: usize,
    ) -> Result<Vec<SkillCandidateEvidence>, StorageError> {
        if max_builds == 0
            || max_rounds == 0
            || max_builds > MAX_BUILD_POINTER_ROWS
            || max_rounds > 100_000
        {
            return Err(StorageError::InvalidInput(
                "skill candidate scan limits are invalid".into(),
            ));
        }
        let heads_table = self
            .conn
            .open_table(HEADS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = heads_table
            .query()
            .limit(max_builds.saturating_add(1))
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("skill candidate head stream", error))?;
        let mut heads = Vec::new();
        for batch in &batches {
            heads.extend(record_batch_to_heads(batch)?);
        }
        if heads.len() > max_builds {
            crate::metrics::metrics().inc_skill_candidate_capacity_rejection();
            return Err(StorageError::InvalidInput(
                "skill candidate head capacity exceeded; increase MEM_SKILL_CANDIDATE_MAX_BUILDS"
                    .into(),
            ));
        }
        if heads.is_empty() {
            return Ok(Vec::new());
        }

        let builds_table = self
            .conn
            .open_table(BUILDS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut head_scopes = HashSet::with_capacity(heads.len());
        let mut head_generations = HashSet::with_capacity(heads.len());
        if heads.iter().any(|head| {
            !head_scopes.insert((head.tenant.clone(), head.session_id.clone()))
                || !head_generations.insert(head.generation_id.clone())
        }) {
            return Err(StorageError::InvalidData(
                "duplicate completed tool round head scope or generation",
            ));
        }
        let heads_by_generation: HashMap<_, _> = heads
            .into_iter()
            .map(|head| (head.generation_id.clone(), head))
            .collect();
        let mut generation_scopes = HashMap::new();
        let mut ordered_head_ids: Vec<_> = heads_by_generation.keys().cloned().collect();
        ordered_head_ids.sort_unstable();
        for generation_chunk in ordered_head_ids.chunks(100) {
            let ids = generation_chunk
                .iter()
                .map(|generation_id| sql_quote(generation_id))
                .collect::<Vec<_>>()
                .join(", ");
            let batches: Vec<RecordBatch> = builds_table
                .query()
                .only_if(format!("generation_id IN ({ids}) AND status = 'completed'"))
                .limit(generation_chunk.len().saturating_add(1))
                .execute()
                .await
                .map_err(lancedb_err)?
                .try_collect()
                .await
                .map_err(|error| StorageError::backend("skill candidate build stream", error))?;
            let mut builds = Vec::new();
            for batch in &batches {
                builds.extend(record_batch_to_builds(batch)?);
            }
            let requested_generations: HashSet<_> = generation_chunk.iter().cloned().collect();
            let mut seen_generations = HashSet::with_capacity(builds.len());
            if builds.iter().any(|build| {
                !requested_generations.contains(&build.generation_id)
                    || !seen_generations.insert(build.generation_id.clone())
            }) || seen_generations != requested_generations
            {
                return Err(StorageError::InvalidData(
                    "skill candidate head does not resolve to one completed build",
                ));
            }
            for build in builds {
                let Some(head) = heads_by_generation.get(&build.generation_id) else {
                    return Err(StorageError::InvalidData(
                        "skill candidate build is not referenced by a head",
                    ));
                };
                if build.tenant != head.tenant || build.session_id != head.session_id {
                    return Err(StorageError::InvalidData(
                        "skill candidate head scope mismatch",
                    ));
                }
                if build.projector_version == COMPLETED_TOOL_ROUND_PROJECTOR_VERSION
                    && build.task_signal_version == COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION
                {
                    generation_scopes.insert(
                        build.generation_id,
                        (
                            build.tenant,
                            build.session_id,
                            build.projector_version,
                            build.round_count,
                        ),
                    );
                }
            }
        }
        if generation_scopes.is_empty() {
            return Ok(Vec::new());
        }

        let rounds_table = self
            .conn
            .open_table(ROUNDS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut evidence = Vec::new();
        let mut observed_round_counts: HashMap<String, u64> = HashMap::new();
        let mut observed_round_ids = HashSet::new();
        let mut ordered_generation_ids: Vec<_> = generation_scopes.keys().cloned().collect();
        ordered_generation_ids.sort_unstable();
        for generation_chunk in ordered_generation_ids.chunks(100) {
            let ids = generation_chunk
                .iter()
                .map(|generation_id| sql_quote(generation_id))
                .collect::<Vec<_>>()
                .join(", ");
            let remaining = max_rounds.saturating_sub(evidence.len());
            if remaining == 0 {
                crate::metrics::metrics().inc_skill_candidate_capacity_rejection();
                return Err(StorageError::InvalidInput(
                    "skill candidate round scan limit exceeded".into(),
                ));
            }
            let batches: Vec<RecordBatch> = rounds_table
                .query()
                .select(Select::columns(&[
                    "generation_id",
                    "projected_at",
                    "round_id",
                    "tenant",
                    "session_id",
                    "caller_agent",
                    "tool_call_count",
                    "matched_result_count",
                    "missing_result_count",
                    "orphan_result_count",
                    "error_result_count",
                    "unknown_result_status_count",
                    "round_completed_at",
                    "integrity",
                    "source_fingerprint",
                    "task_fingerprint",
                    "task_signal_version",
                    "projector_version",
                ]))
                .only_if(format!(
                    "generation_id IN ({ids}) AND task_signal_version = {}",
                    COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION
                ))
                .limit(remaining.saturating_add(1))
                .execute()
                .await
                .map_err(lancedb_err)?
                .try_collect()
                .await
                .map_err(|error| StorageError::backend("skill candidate round stream", error))?;
            for batch in &batches {
                for item in record_batch_to_skill_candidate_evidence(batch)? {
                    let Some((tenant, session_id, projector_version, _)) =
                        generation_scopes.get(&item.generation_id)
                    else {
                        return Err(StorageError::InvalidData(
                            "skill candidate generation scope missing",
                        ));
                    };
                    if item.round.tenant != *tenant
                        || item.round.session_id.as_deref() != Some(session_id.as_str())
                        || item.round.projector_version != *projector_version
                    {
                        return Err(StorageError::InvalidData(
                            "skill candidate round scope mismatch",
                        ));
                    }
                    if !observed_round_ids
                        .insert((item.generation_id.clone(), item.round.round_id.clone()))
                    {
                        return Err(StorageError::InvalidData(
                            "duplicate skill candidate round evidence",
                        ));
                    }
                    observed_round_counts
                        .entry(item.generation_id.clone())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                    evidence.push(item);
                }
            }
            if evidence.len() > max_rounds {
                crate::metrics::metrics().inc_skill_candidate_capacity_rejection();
                return Err(StorageError::InvalidInput(
                    "skill candidate round scan limit exceeded".into(),
                ));
            }
        }
        if generation_scopes
            .iter()
            .any(|(generation_id, (_, _, _, expected_count))| {
                observed_round_counts
                    .get(generation_id)
                    .copied()
                    .unwrap_or(0)
                    != *expected_count
            })
        {
            return Err(StorageError::InvalidData(
                "skill candidate generation evidence count mismatch",
            ));
        }
        evidence.sort_by(|left, right| {
            (&left.projected_at, &left.round.round_id)
                .cmp(&(&right.projected_at, &right.round.round_id))
        });
        Ok(evidence)
    }
}

fn record_batch_to_skill_candidate_evidence(
    batch: &RecordBatch,
) -> Result<Vec<SkillCandidateEvidence>, StorageError> {
    let generation_id = parse_col::<StringArray>(batch, ROUNDS_TABLE, "generation_id")?;
    let projected_at = parse_col::<StringArray>(batch, ROUNDS_TABLE, "projected_at")?;
    let round_id = parse_col::<StringArray>(batch, ROUNDS_TABLE, "round_id")?;
    let tenant = parse_col::<StringArray>(batch, ROUNDS_TABLE, "tenant")?;
    let session_id = parse_col::<StringArray>(batch, ROUNDS_TABLE, "session_id")?;
    let caller_agent = parse_col::<StringArray>(batch, ROUNDS_TABLE, "caller_agent")?;
    let tool_call_count = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "tool_call_count")?;
    let matched_result_count =
        parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "matched_result_count")?;
    let missing_result_count =
        parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "missing_result_count")?;
    let orphan_result_count = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "orphan_result_count")?;
    let error_result_count = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "error_result_count")?;
    let unknown_result_status_count =
        parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "unknown_result_status_count")?;
    let round_completed_at = parse_col::<StringArray>(batch, ROUNDS_TABLE, "round_completed_at")?;
    let integrity = parse_col::<StringArray>(batch, ROUNDS_TABLE, "integrity")?;
    let source_fingerprint = parse_col::<StringArray>(batch, ROUNDS_TABLE, "source_fingerprint")?;
    let task_fingerprint = parse_col::<StringArray>(batch, ROUNDS_TABLE, "task_fingerprint")?;
    let task_signal_version = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "task_signal_version")?;
    let projector_version = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "projector_version")?;
    let mut evidence = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        evidence.push(SkillCandidateEvidence {
            generation_id: generation_id.value(index).to_string(),
            projected_at: projected_at.value(index).to_string(),
            round: SkillCandidateRoundEvidence {
                round_id: round_id.value(index).to_string(),
                tenant: tenant.value(index).to_string(),
                caller_agent: caller_agent.value(index).to_string(),
                session_id: optional_string(session_id, index),
                tool_call_count: tool_call_count.value(index),
                matched_result_count: matched_result_count.value(index),
                missing_result_count: missing_result_count.value(index),
                orphan_result_count: orphan_result_count.value(index),
                error_result_count: error_result_count.value(index),
                unknown_result_status_count: unknown_result_status_count.value(index),
                completed_at: optional_string(round_completed_at, index),
                integrity: RoundIntegrity::from_db_str(integrity.value(index)).ok_or(
                    StorageError::InvalidData("invalid Skill candidate round integrity"),
                )?,
                source_fingerprint: source_fingerprint.value(index).to_string(),
                task_fingerprint: optional_string(task_fingerprint, index),
                task_signal_version: if task_signal_version.is_null(index) {
                    0
                } else {
                    task_signal_version.value(index)
                },
                projector_version: projector_version.value(index),
            },
        });
    }
    Ok(evidence)
}

fn record_batch_to_heads(batch: &RecordBatch) -> Result<Vec<CompletedToolRoundHead>, StorageError> {
    let tenant = parse_col::<StringArray>(batch, HEADS_TABLE, "tenant")?;
    let session_id = parse_col::<StringArray>(batch, HEADS_TABLE, "session_id")?;
    let generation_id = parse_col::<StringArray>(batch, HEADS_TABLE, "generation_id")?;
    let updated_at = parse_col::<StringArray>(batch, HEADS_TABLE, "updated_at")?;
    let mut heads = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        heads.push(CompletedToolRoundHead {
            tenant: tenant.value(index).to_string(),
            session_id: session_id.value(index).to_string(),
            generation_id: generation_id.value(index).to_string(),
            updated_at: updated_at.value(index).to_string(),
        });
    }
    Ok(heads)
}

fn record_batch_to_builds(
    batch: &RecordBatch,
) -> Result<Vec<CompletedToolRoundIndexBuild>, StorageError> {
    let generation_id = parse_col::<StringArray>(batch, BUILDS_TABLE, "generation_id")?;
    let tenant = parse_col::<StringArray>(batch, BUILDS_TABLE, "tenant")?;
    let session_id = parse_col::<StringArray>(batch, BUILDS_TABLE, "session_id")?;
    let projector_version = parse_col::<UInt32Array>(batch, BUILDS_TABLE, "projector_version")?;
    let task_signal_version = parse_col::<UInt32Array>(batch, BUILDS_TABLE, "task_signal_version")?;
    let status = parse_col::<StringArray>(batch, BUILDS_TABLE, "status")?;
    let source_block_count = parse_col::<UInt64Array>(batch, BUILDS_TABLE, "source_block_count")?;
    let source_fingerprint = parse_col::<StringArray>(batch, BUILDS_TABLE, "source_fingerprint")?;
    let round_count = parse_col::<UInt64Array>(batch, BUILDS_TABLE, "round_count")?;
    let started_at = parse_col::<StringArray>(batch, BUILDS_TABLE, "started_at")?;
    let completed_at = parse_col::<StringArray>(batch, BUILDS_TABLE, "completed_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        out.push(CompletedToolRoundIndexBuild {
            generation_id: generation_id.value(index).to_string(),
            tenant: tenant.value(index).to_string(),
            session_id: session_id.value(index).to_string(),
            projector_version: projector_version.value(index),
            task_signal_version: if task_signal_version.is_null(index) {
                0
            } else {
                task_signal_version.value(index)
            },
            status: RoundIndexBuildStatus::from_db_str(status.value(index))
                .ok_or(StorageError::InvalidData("invalid round build status"))?,
            source_block_count: source_block_count.value(index),
            source_fingerprint: source_fingerprint.value(index).to_string(),
            round_count: round_count.value(index),
            started_at: started_at.value(index).to_string(),
            completed_at: optional_string(completed_at, index),
        });
    }
    Ok(out)
}

fn record_batch_to_rounds(batch: &RecordBatch) -> Result<Vec<CompletedToolRound>, StorageError> {
    let round_id = parse_col::<StringArray>(batch, ROUNDS_TABLE, "round_id")?;
    let tenant = parse_col::<StringArray>(batch, ROUNDS_TABLE, "tenant")?;
    let session_id = parse_col::<StringArray>(batch, ROUNDS_TABLE, "session_id")?;
    let caller_agent = parse_col::<StringArray>(batch, ROUNDS_TABLE, "caller_agent")?;
    let source_adapter = parse_col::<StringArray>(batch, ROUNDS_TABLE, "source_adapter")?;
    let transcript_path = parse_col::<StringArray>(batch, ROUNDS_TABLE, "transcript_path")?;
    let start_line_number = parse_col::<UInt64Array>(batch, ROUNDS_TABLE, "start_line_number")?;
    let start_block_index = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "start_block_index")?;
    let end_line_number = parse_col::<UInt64Array>(batch, ROUNDS_TABLE, "end_line_number")?;
    let end_block_index = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "end_block_index")?;
    let start_message_uuid = parse_col::<StringArray>(batch, ROUNDS_TABLE, "start_message_uuid")?;
    let final_message_uuid = parse_col::<StringArray>(batch, ROUNDS_TABLE, "final_message_uuid")?;
    let tool_call_ids_json = parse_col::<StringArray>(batch, ROUNDS_TABLE, "tool_call_ids_json")?;
    let tool_names_json = parse_col::<StringArray>(batch, ROUNDS_TABLE, "tool_names_json")?;
    let tool_call_count = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "tool_call_count")?;
    let matched_result_count =
        parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "matched_result_count")?;
    let missing_result_count =
        parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "missing_result_count")?;
    let orphan_result_count = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "orphan_result_count")?;
    let error_result_count = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "error_result_count")?;
    let unknown_result_status_count =
        parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "unknown_result_status_count")?;
    let round_completed_at = parse_col::<StringArray>(batch, ROUNDS_TABLE, "round_completed_at")?;
    let seal_kind = parse_col::<StringArray>(batch, ROUNDS_TABLE, "seal_kind")?;
    let integrity = parse_col::<StringArray>(batch, ROUNDS_TABLE, "integrity")?;
    let source_fingerprint = parse_col::<StringArray>(batch, ROUNDS_TABLE, "source_fingerprint")?;
    let task_fingerprint = parse_col::<StringArray>(batch, ROUNDS_TABLE, "task_fingerprint")?;
    let task_signal_version = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "task_signal_version")?;
    let tool_pattern_fingerprint =
        parse_col::<StringArray>(batch, ROUNDS_TABLE, "tool_pattern_fingerprint")?;
    let projector_version = parse_col::<UInt32Array>(batch, ROUNDS_TABLE, "projector_version")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        out.push(CompletedToolRound {
            round_id: round_id.value(index).to_string(),
            tenant: tenant.value(index).to_string(),
            caller_agent: caller_agent.value(index).to_string(),
            source_adapter: SourceAdapter::from_db_str(source_adapter.value(index))
                .ok_or(StorageError::InvalidData("invalid round source adapter"))?,
            session_id: optional_string(session_id, index),
            transcript_path: transcript_path.value(index).to_string(),
            start_line_number: start_line_number.value(index),
            start_block_index: start_block_index.value(index),
            end_line_number: end_line_number.value(index),
            end_block_index: end_block_index.value(index),
            start_message_uuid: optional_string(start_message_uuid, index),
            final_message_uuid: optional_string(final_message_uuid, index),
            tool_call_ids: serde_json::from_str(tool_call_ids_json.value(index))?,
            tool_names: serde_json::from_str(tool_names_json.value(index))?,
            tool_call_count: tool_call_count.value(index),
            matched_result_count: matched_result_count.value(index),
            missing_result_count: missing_result_count.value(index),
            orphan_result_count: orphan_result_count.value(index),
            error_result_count: error_result_count.value(index),
            unknown_result_status_count: unknown_result_status_count.value(index),
            completed_at: optional_string(round_completed_at, index),
            seal_kind: RoundSealKind::from_db_str(seal_kind.value(index))
                .ok_or(StorageError::InvalidData("invalid round seal kind"))?,
            integrity: RoundIntegrity::from_db_str(integrity.value(index))
                .ok_or(StorageError::InvalidData("invalid round integrity"))?,
            source_fingerprint: source_fingerprint.value(index).to_string(),
            task_fingerprint: optional_string(task_fingerprint, index),
            task_signal_version: if task_signal_version.is_null(index) {
                0
            } else {
                task_signal_version.value(index)
            },
            tool_pattern_fingerprint: optional_string(tool_pattern_fingerprint, index)
                .unwrap_or_default(),
            projector_version: projector_version.value(index),
        });
    }
    Ok(out)
}

fn optional_string(array: &StringArray, index: usize) -> Option<String> {
    if array.is_null(index) {
        None
    } else {
        Some(array.value(index).to_string())
    }
}
