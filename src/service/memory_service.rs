use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    domain::memory::{IngestMemoryRequest, MemoryRecord, MemoryStatus},
    pipeline::ingest::{compute_content_hash, initial_status},
    storage::{DuckDbRepository, StorageError},
};

static MEMORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestMemoryResponse {
    pub memory_id: String,
    pub status: MemoryStatus,
}

impl From<MemoryRecord> for IngestMemoryResponse {
    fn from(memory: MemoryRecord) -> Self {
        Self {
            memory_id: memory.memory_id,
            status: memory.status,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryService {
    db_path: Arc<PathBuf>,
}

impl MemoryService {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: Arc::new(db_path.into()),
        }
    }

    pub async fn ingest(
        &self,
        request: IngestMemoryRequest,
    ) -> Result<IngestMemoryResponse, StorageError> {
        let repo = DuckDbRepository::open(self.db_path.as_ref()).await?;
        let content_hash = compute_content_hash(&request);

        if let Some(existing) = repo
            .find_by_idempotency_or_hash(&request.idempotency_key, &content_hash)
            .await?
        {
            return Ok(existing.into());
        }

        let status = initial_status(&request.memory_type, &request.write_mode);
        let now = current_timestamp();
        let memory = MemoryRecord {
            memory_id: next_memory_id(),
            tenant: request.tenant,
            memory_type: request.memory_type,
            status: status.clone(),
            scope: request.scope,
            visibility: request.visibility,
            version: 1,
            summary: summarize(&request.content),
            content: request.content,
            evidence: request.evidence,
            code_refs: request.code_refs,
            project: request.project,
            repo: request.repo,
            module: request.module,
            task_type: request.task_type,
            tags: request.tags,
            confidence: default_confidence(&status),
            decay_score: 0.0,
            content_hash,
            idempotency_key: request.idempotency_key,
            supersedes_memory_id: None,
            source_agent: request.source_agent,
            created_at: now.clone(),
            updated_at: now,
            last_validated_at: None,
        };

        let stored = repo.insert_memory(memory).await?;
        Ok(stored.into())
    }
}

fn summarize(content: &str) -> String {
    const SUMMARY_LIMIT: usize = 80;
    let summary: String = content.chars().take(SUMMARY_LIMIT).collect();
    if summary.is_empty() {
        "memory".to_string()
    } else {
        summary
    }
}

fn default_confidence(status: &MemoryStatus) -> f32 {
    match status {
        MemoryStatus::Active => 0.9,
        MemoryStatus::PendingConfirmation => 0.6,
        MemoryStatus::Provisional => 0.5,
        MemoryStatus::Archived | MemoryStatus::Rejected => 0.0,
    }
}

fn current_timestamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
}

fn next_memory_id() -> String {
    let sequence = MEMORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("mem_{sequence:020}")
}
