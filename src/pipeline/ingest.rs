use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use serde_json::json;

use crate::domain::memory::{IngestMemoryRequest, MemoryStatus, MemoryType, WriteMode};

pub fn initial_status(memory_type: &MemoryType, write_mode: &WriteMode) -> MemoryStatus {
    match (memory_type, write_mode) {
        (MemoryType::Preference | MemoryType::Workflow, _) => MemoryStatus::PendingConfirmation,
        (_, WriteMode::Auto) => MemoryStatus::Active,
        _ => MemoryStatus::Provisional,
    }
}

pub fn compute_content_hash(request: &IngestMemoryRequest) -> String {
    let mut hasher = DefaultHasher::new();
    json!({
        "tenant": request.tenant,
        "memory_type": request.memory_type,
        "content": request.content,
        "evidence": request.evidence,
        "code_refs": request.code_refs,
        "scope": request.scope,
        "visibility": request.visibility,
        "project": request.project,
        "repo": request.repo,
        "module": request.module,
        "task_type": request.task_type,
        "tags": request.tags,
        "source_agent": request.source_agent,
    })
    .to_string()
    .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
