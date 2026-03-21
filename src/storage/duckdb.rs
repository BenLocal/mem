use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use duckdb::{params, Connection, OptionalExt};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::domain::{
    episode::EpisodeRecord,
    memory::{FeedbackSummary, MemoryRecord, MemoryStatus, MemoryVersionLink},
};

use super::schema;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackEvent {
    pub feedback_id: String,
    pub memory_id: String,
    pub feedback_kind: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct DuckDbRepository {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("duckdb error: {0}")]
    DuckDb(#[from] duckdb::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid data: {0}")]
    InvalidData(&'static str),
}

impl DuckDbRepository {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        schema::bootstrap(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError> {
        let conn = self.conn()?;
        let stored = memory.clone();
        conn.execute(
            "insert into memories (
                memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                content, evidence_json, code_refs_json, project, repo, module, task_type,
                tags_json, confidence, decay_score, content_hash, idempotency_key,
                supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
            ) values (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20,
                ?21, ?22, ?23, ?24, ?25
            )",
            params![
                stored.memory_id,
                stored.tenant,
                encode_text(&stored.memory_type)?,
                encode_text(&stored.status)?,
                encode_text(&stored.scope)?,
                encode_text(&stored.visibility)?,
                stored.version as i64,
                stored.summary,
                stored.content,
                encode_json(&stored.evidence)?,
                encode_json(&stored.code_refs)?,
                stored.project,
                stored.repo,
                stored.module,
                stored.task_type,
                encode_json(&stored.tags)?,
                f64::from(stored.confidence),
                f64::from(stored.decay_score),
                stored.content_hash,
                stored.idempotency_key,
                stored.supersedes_memory_id,
                stored.source_agent,
                stored.created_at,
                stored.updated_at,
                stored.last_validated_at,
            ],
        )?;

        Ok(memory)
    }

    pub async fn get_memory(
        &self,
        memory_id: String,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
                 from memories
                 where memory_id = ?1",
                params![memory_id],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
                 from memories
                 where tenant = ?1 and memory_id = ?2",
                params![tenant, memory_id],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
                 from memories
                 where tenant = ?1 and memory_id = ?2 and status = ?3",
                params![
                    tenant,
                    memory_id,
                    encode_text(&MemoryStatus::PendingConfirmation)?
                ],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
                 from memories
                 where tenant = ?1
                   and (((?2 is not null and idempotency_key = ?2) or content_hash = ?3))
                 order by
                    case when ?2 is not null and idempotency_key = ?2 then 0 else 1 end,
                    updated_at desc
                 limit 1",
                params![tenant, idempotency_key.as_deref(), content_hash],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select
                memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                content, evidence_json, code_refs_json, project, repo, module, task_type,
                tags_json, confidence, decay_score, content_hash, idempotency_key,
                supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
             from memories
             where tenant = ?1 and status = ?2
             order by created_at desc",
        )?;
        let rows = stmt.query_map(
            params![tenant, encode_text(&MemoryStatus::PendingConfirmation)?],
            map_memory_row,
        )?;
        let collected = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(collected)
    }

    pub async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select
                memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                content, evidence_json, code_refs_json, project, repo, module, task_type,
                tags_json, confidence, decay_score, content_hash, idempotency_key,
                supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
             from memories
             where tenant = ?1
             order by updated_at desc, version desc, memory_id asc",
        )?;
        let rows = stmt.query_map(params![tenant], map_memory_row)?;
        let candidates = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|memory| {
                !matches!(
                    memory.status,
                    MemoryStatus::Rejected | MemoryStatus::Archived
                )
            })
            .collect::<Vec<_>>();
        Ok(candidates)
    }

    pub async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, MemoryStatus::Active)
            .await
    }

    pub async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, MemoryStatus::Rejected)
            .await
    }

    pub async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        {
            let updated_at = current_timestamp();
            let mut conn = self.conn()?;
            let tx = conn.transaction()?;
            let stored = successor.clone();
            let rows_updated = tx.execute(
                "update memories
                 set status = ?1, updated_at = ?2
                 where tenant = ?3 and memory_id = ?4 and status = ?5",
                params![
                    encode_text(&MemoryStatus::Rejected)?,
                    updated_at,
                    tenant,
                    original_memory_id,
                    encode_text(&MemoryStatus::PendingConfirmation)?,
                ],
            )?;

            if rows_updated == 0 {
                return Err(StorageError::InvalidData("pending memory not found"));
            }

            tx.execute(
                "insert into memories (
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
                ) values (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                    ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                    ?16, ?17, ?18, ?19, ?20,
                    ?21, ?22, ?23, ?24, ?25
                )",
                params![
                    stored.memory_id,
                    stored.tenant,
                    encode_text(&stored.memory_type)?,
                    encode_text(&stored.status)?,
                    encode_text(&stored.scope)?,
                    encode_text(&stored.visibility)?,
                    stored.version as i64,
                    stored.summary,
                    stored.content,
                    encode_json(&stored.evidence)?,
                    encode_json(&stored.code_refs)?,
                    stored.project,
                    stored.repo,
                    stored.module,
                    stored.task_type,
                    encode_json(&stored.tags)?,
                    f64::from(stored.confidence),
                    f64::from(stored.decay_score),
                    stored.content_hash,
                    stored.idempotency_key,
                    stored.supersedes_memory_id,
                    stored.source_agent,
                    stored.created_at,
                    stored.updated_at,
                    stored.last_validated_at,
                ],
            )?;
            tx.commit()?;
        }

        Ok(successor)
    }

    pub async fn insert_feedback(
        &self,
        feedback: FeedbackEvent,
    ) -> Result<FeedbackEvent, StorageError> {
        let conn = self.conn()?;
        let stored = feedback.clone();
        conn.execute(
            "insert into feedback_events (feedback_id, memory_id, feedback_kind, created_at)
             values (?1, ?2, ?3, ?4)",
            params![
                stored.feedback_id,
                stored.memory_id,
                stored.feedback_kind,
                stored.created_at
            ],
        )?;
        Ok(feedback)
    }

    pub async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select feedback_id, memory_id, feedback_kind, created_at
             from feedback_events
             where memory_id = ?1
             order by created_at asc",
        )?;
        let rows = stmt.query_map(params![memory_id], |row| {
            Ok(FeedbackEvent {
                feedback_id: row.get(0)?,
                memory_id: row.get(1)?,
                feedback_kind: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        let collected = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(collected)
    }

    pub async fn insert_episode(
        &self,
        episode: EpisodeRecord,
    ) -> Result<EpisodeRecord, StorageError> {
        let conn = self.conn()?;
        let stored = episode.clone();
        conn.execute(
            "insert into episodes (
                episode_id, tenant, goal, steps_json, outcome, evidence_json, scope, visibility,
                project, repo, module, tags_json, source_agent, idempotency_key, created_at,
                updated_at, workflow_candidate_json
             ) values (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17
             )",
            params![
                stored.episode_id,
                stored.tenant,
                stored.goal,
                encode_json(&stored.steps)?,
                stored.outcome,
                encode_json(&stored.evidence)?,
                encode_text(&stored.scope)?,
                encode_text(&stored.visibility)?,
                stored.project,
                stored.repo,
                stored.module,
                encode_json(&stored.tags)?,
                stored.source_agent,
                stored.idempotency_key,
                stored.created_at,
                stored.updated_at,
                encode_optional_json(&stored.workflow_candidate)?,
            ],
        )?;
        Ok(episode)
    }

    pub async fn get_episode(
        &self,
        episode_id: &str,
    ) -> Result<Option<EpisodeRecord>, StorageError> {
        let conn = self.conn()?;
        let episode = conn
            .query_row(
                "select
                    episode_id, tenant, goal, steps_json, outcome, evidence_json, scope,
                    visibility, project, repo, module, tags_json, source_agent, idempotency_key,
                    created_at, updated_at, workflow_candidate_json
                 from episodes
                 where episode_id = ?1",
                params![episode_id],
                |row| {
                    Ok(EpisodeRecord {
                        episode_id: row.get(0)?,
                        tenant: row.get(1)?,
                        goal: row.get(2)?,
                        steps: decode_json(&row.get::<_, String>(3)?).map_err(to_from_sql_error)?,
                        outcome: row.get(4)?,
                        evidence: decode_json(&row.get::<_, String>(5)?)
                            .map_err(to_from_sql_error)?,
                        scope: decode_text(&row.get::<_, String>(6)?).map_err(to_from_sql_error)?,
                        visibility: decode_text(&row.get::<_, String>(7)?)
                            .map_err(to_from_sql_error)?,
                        project: row.get(8)?,
                        repo: row.get(9)?,
                        module: row.get(10)?,
                        tags: decode_json(&row.get::<_, String>(11)?).map_err(to_from_sql_error)?,
                        source_agent: row.get(12)?,
                        idempotency_key: row.get(13)?,
                        created_at: row.get(14)?,
                        updated_at: row.get(15)?,
                        workflow_candidate: decode_optional_json(row.get::<_, Option<String>>(16)?)
                            .map_err(to_from_sql_error)?,
                    })
                },
            )
            .optional()?;

        Ok(episode)
    }

    pub async fn list_memory_versions(
        &self,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let memory = self
            .get_memory(memory_id.to_string())
            .await?
            .ok_or(StorageError::InvalidData("memory not found"))?;

        self.list_memory_versions_for_tenant(&memory.tenant, memory_id)
            .await
    }

    pub async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select memory_id, version, status, updated_at, supersedes_memory_id
             from memories
             where tenant = ?1
             order by version desc, updated_at desc",
        )?;
        let rows = stmt.query_map(params![tenant], |row| {
            Ok(MemoryVersionLink {
                memory_id: row.get(0)?,
                version: to_u64(row.get::<_, i64>(1)?).map_err(to_from_sql_error)?,
                status: decode_text(&row.get::<_, String>(2)?).map_err(to_from_sql_error)?,
                updated_at: row.get(3)?,
                supersedes_memory_id: row.get(4)?,
            })
        })?;
        let all_versions = rows.collect::<Result<Vec<_>, _>>()?;
        let mut by_id = HashMap::new();
        let mut neighbors: HashMap<String, Vec<String>> = HashMap::new();

        for version in all_versions {
            let current_id = version.memory_id.clone();
            if let Some(parent_id) = version.supersedes_memory_id.clone() {
                neighbors
                    .entry(current_id.clone())
                    .or_default()
                    .push(parent_id.clone());
                neighbors
                    .entry(parent_id)
                    .or_default()
                    .push(current_id.clone());
            }
            by_id.insert(current_id, version);
        }

        if !by_id.contains_key(memory_id) {
            return Err(StorageError::InvalidData("memory not found"));
        }

        let mut queue = VecDeque::from([memory_id.to_string()]);
        let mut connected = HashSet::new();

        while let Some(current_id) = queue.pop_front() {
            if !connected.insert(current_id.clone()) {
                continue;
            }

            if let Some(next_ids) = neighbors.get(&current_id) {
                for next_id in next_ids {
                    queue.push_back(next_id.clone());
                }
            }
        }

        let mut collected = connected
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect::<Vec<_>>();
        collected.sort_by(|left, right| {
            right
                .version
                .cmp(&left.version)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });
        Ok(collected)
    }

    pub async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError> {
        let feedback = self.list_feedback_for_memory(memory_id).await?;
        let mut summary = FeedbackSummary::default();
        for event in feedback {
            summary.total += 1;
            match event.feedback_kind.as_str() {
                "useful" => summary.useful += 1,
                "outdated" => summary.outdated += 1,
                "incorrect" => summary.incorrect += 1,
                "applies_here" => summary.applies_here += 1,
                "does_not_apply_here" => summary.does_not_apply_here += 1,
                _ => {}
            }
        }
        Ok(summary)
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.conn
            .lock()
            .map_err(|_| StorageError::InvalidData("duckdb connection mutex poisoned"))
    }

    async fn update_status(
        &self,
        tenant: &str,
        memory_id: &str,
        status: MemoryStatus,
    ) -> Result<MemoryRecord, StorageError> {
        let updated_at = current_timestamp();
        {
            let conn = self.conn()?;
            let rows_updated = conn.execute(
                "update memories
                 set status = ?1, updated_at = ?2
                 where tenant = ?3 and memory_id = ?4 and status = ?5",
                params![
                    encode_text(&status)?,
                    updated_at,
                    tenant,
                    memory_id,
                    encode_text(&MemoryStatus::PendingConfirmation)?,
                ],
            )?;

            if rows_updated == 0 {
                return Err(StorageError::InvalidData("pending memory not found"));
            }
        }

        self.get_memory(memory_id.to_string())
            .await?
            .ok_or(StorageError::InvalidData("updated memory not found"))
    }
}

fn map_memory_row(row: &duckdb::Row<'_>) -> Result<MemoryRecord, duckdb::Error> {
    Ok(MemoryRecord {
        memory_id: row.get(0)?,
        tenant: row.get(1)?,
        memory_type: decode_text(&row.get::<_, String>(2)?).map_err(to_from_sql_error)?,
        status: decode_text(&row.get::<_, String>(3)?).map_err(to_from_sql_error)?,
        scope: decode_text(&row.get::<_, String>(4)?).map_err(to_from_sql_error)?,
        visibility: decode_text(&row.get::<_, String>(5)?).map_err(to_from_sql_error)?,
        version: to_u64(row.get::<_, i64>(6)?).map_err(to_from_sql_error)?,
        summary: row.get(7)?,
        content: row.get(8)?,
        evidence: decode_json(&row.get::<_, String>(9)?).map_err(to_from_sql_error)?,
        code_refs: decode_json(&row.get::<_, String>(10)?).map_err(to_from_sql_error)?,
        project: row.get(11)?,
        repo: row.get(12)?,
        module: row.get(13)?,
        task_type: row.get(14)?,
        tags: decode_json(&row.get::<_, String>(15)?).map_err(to_from_sql_error)?,
        confidence: row.get::<_, f64>(16)? as f32,
        decay_score: row.get::<_, f64>(17)? as f32,
        content_hash: row.get(18)?,
        idempotency_key: row.get(19)?,
        supersedes_memory_id: row.get(20)?,
        source_agent: row.get(21)?,
        created_at: row.get(22)?,
        updated_at: row.get(23)?,
        last_validated_at: row.get(24)?,
    })
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, StorageError> {
    Ok(serde_json::to_string(value)?)
}

fn encode_optional_json<T: Serialize>(value: &Option<T>) -> Result<Option<String>, StorageError> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn decode_json<T: DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    Ok(serde_json::from_str(value)?)
}

fn decode_optional_json<T: DeserializeOwned>(
    value: Option<String>,
) -> Result<Option<T>, StorageError> {
    value
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(Into::into)
}

fn encode_text<T: Serialize>(value: &T) -> Result<String, StorageError> {
    let value = serde_json::to_value(value)?;
    match value {
        Value::String(value) => Ok(value),
        _ => Err(StorageError::InvalidData(
            "expected string-compatible value",
        )),
    }
}

fn decode_text<T: DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    Ok(serde_json::from_value(Value::String(value.to_owned()))?)
}

fn to_u64(value: i64) -> Result<u64, StorageError> {
    u64::try_from(value)
        .map_err(|_| StorageError::InvalidData("negative integer in unsigned field"))
}

fn to_from_sql_error(error: StorageError) -> duckdb::Error {
    duckdb::Error::FromSqlConversionFailure(0, duckdb::types::Type::Text, Box::new(error))
}

fn current_timestamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
}
