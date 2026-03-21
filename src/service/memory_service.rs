use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use thiserror::Error;

use crate::{
    domain::memory::{EditPendingRequest, EditPendingResponse, IngestMemoryRequest, MemoryRecord, MemoryStatus},
    pipeline::ingest::{compute_content_hash, initial_status},
    storage::{DuckDbRepository, StorageError},
};
use crate::domain::memory::MemoryDetailResponse;

static MEMORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct IngestMemoryResponse {
    pub memory_id: String,
    pub status: MemoryStatus,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("memory not found")]
    NotFound,
    #[error(transparent)]
    Storage(#[from] StorageError),
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
    repository: DuckDbRepository,
}

impl MemoryService {
    pub fn new(repository: DuckDbRepository) -> Self {
        Self { repository }
    }

    pub async fn ingest(
        &self,
        request: IngestMemoryRequest,
    ) -> Result<IngestMemoryResponse, StorageError> {
        let content_hash = compute_content_hash(&request);

        if let Some(existing) = self
            .repository
            .find_by_idempotency_or_hash(&request.tenant, &request.idempotency_key, &content_hash)
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

        let stored = self.repository.insert_memory(memory).await?;
        Ok(stored.into())
    }

    pub async fn list_pending_review(&self, tenant: &str) -> Result<Vec<MemoryRecord>, ServiceError> {
        Ok(self.repository.list_pending_review(tenant).await?)
    }

    pub async fn get_memory(
        &self,
        tenant: Option<&str>,
        memory_id: &str,
    ) -> Result<MemoryDetailResponse, ServiceError> {
        let memory = match tenant {
            Some(tenant) => self.repository.get_memory_for_tenant(tenant, memory_id).await?,
            None => self.repository.get_memory(memory_id.to_string()).await?,
        }
        .ok_or(ServiceError::NotFound)?;

        Ok(MemoryDetailResponse {
            version_chain: self
                .repository
                .list_memory_versions_for_tenant(&memory.tenant, memory_id)
                .await?,
            graph_links: Vec::new(),
            feedback_summary: self.repository.feedback_summary(memory_id).await?,
            memory,
        })
    }

    pub async fn accept_pending(&self, tenant: &str, memory_id: &str) -> Result<MemoryRecord, ServiceError> {
        self.repository
            .get_pending(tenant, memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        Ok(self.repository.accept_pending(tenant, memory_id).await?)
    }

    pub async fn reject_pending(&self, tenant: &str, memory_id: &str) -> Result<MemoryRecord, ServiceError> {
        self.repository
            .get_pending(tenant, memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;
        Ok(self.repository.reject_pending(tenant, memory_id).await?)
    }

    pub async fn edit_and_accept_pending(
        &self,
        tenant: &str,
        patch: EditPendingRequest,
    ) -> Result<EditPendingResponse, ServiceError> {
        let original_memory_id = patch.memory_id.clone();
        let original = self
            .repository
            .get_pending(tenant, &original_memory_id)
            .await?
            .ok_or(ServiceError::NotFound)?;

        let superseding = self
            .repository
            .replace_pending_with_successor(
                tenant,
                &original_memory_id,
                self.superseding_active_version(&original, patch),
            )
            .await?;

        Ok(EditPendingResponse {
            original_memory_id: original.memory_id,
            memory: superseding,
        })
    }

    fn superseding_active_version(
        &self,
        original: &MemoryRecord,
        patch: EditPendingRequest,
    ) -> MemoryRecord {
        let request = IngestMemoryRequest {
            tenant: original.tenant.clone(),
            memory_type: original.memory_type.clone(),
            content: patch.content.clone(),
            evidence: patch.evidence.clone(),
            code_refs: patch.code_refs.clone(),
            scope: original.scope.clone(),
            visibility: original.visibility.clone(),
            project: original.project.clone(),
            repo: original.repo.clone(),
            module: original.module.clone(),
            task_type: original.task_type.clone(),
            tags: patch.tags.clone(),
            source_agent: original.source_agent.clone(),
            idempotency_key: None,
            write_mode: crate::domain::memory::WriteMode::Auto,
        };
        let now = current_timestamp();

        MemoryRecord {
            memory_id: next_memory_id(),
            tenant: original.tenant.clone(),
            memory_type: original.memory_type.clone(),
            status: MemoryStatus::Active,
            scope: original.scope.clone(),
            visibility: original.visibility.clone(),
            version: original.version + 1,
            summary: patch.summary,
            content: patch.content,
            evidence: patch.evidence,
            code_refs: patch.code_refs,
            project: original.project.clone(),
            repo: original.repo.clone(),
            module: original.module.clone(),
            task_type: original.task_type.clone(),
            tags: patch.tags,
            confidence: default_confidence(&MemoryStatus::Active),
            decay_score: 0.0,
            content_hash: compute_content_hash(&request),
            idempotency_key: None,
            supersedes_memory_id: Some(original.memory_id.clone()),
            source_agent: original.source_agent.clone(),
            created_at: now.clone(),
            updated_at: now,
            last_validated_at: None,
        }
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
