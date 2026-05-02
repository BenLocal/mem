use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::memory::{
    GraphEdge, IngestMemoryRequest, MemoryRecord, MemoryStatus, MemoryType, WriteMode,
};
use crate::domain::EntityKind;

/// Length of `compute_content_hash` output (sha256 hex). Used by the storage
/// migration to identify legacy rows that still hold the old DefaultHasher
/// 16-char hash.
pub const CONTENT_HASH_LEN: usize = 64;

/// Validate that the request follows verbatim discipline. When the caller
/// supplies a non-empty `summary`, it must not equal `content` (otherwise
/// the agent has copied refined/summarized text into the content field).
pub fn validate_verbatim(content: &str, caller_summary: Option<&str>) -> Result<(), String> {
    if let Some(summary) = caller_summary.filter(|s| !s.is_empty()) {
        if summary == content {
            return Err("summary must not be identical to content (verbatim violation)".into());
        }
    }
    Ok(())
}

pub fn initial_status(memory_type: &MemoryType, write_mode: &WriteMode) -> MemoryStatus {
    match (memory_type, write_mode) {
        (MemoryType::Preference | MemoryType::Workflow, _) => MemoryStatus::PendingConfirmation,
        (_, WriteMode::Auto) => MemoryStatus::Active,
        _ => MemoryStatus::Provisional,
    }
}

pub fn compute_content_hash(request: &IngestMemoryRequest) -> String {
    hash_canonical(&canonical_request_json(request))
}

/// Recompute the content hash from a stored `MemoryRecord`. Equivalent to
/// hashing the original `IngestMemoryRequest`, used by the migration that
/// upgrades pre-sha256 rows.
pub fn compute_content_hash_from_record(record: &MemoryRecord) -> String {
    hash_canonical(&canonical_record_json(record))
}

fn canonical_request_json(request: &IngestMemoryRequest) -> Value {
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
}

fn canonical_record_json(record: &MemoryRecord) -> Value {
    json!({
        "tenant": record.tenant,
        "memory_type": record.memory_type,
        "content": record.content,
        "evidence": record.evidence,
        "code_refs": record.code_refs,
        "scope": record.scope,
        "visibility": record.visibility,
        "project": record.project,
        "repo": record.repo,
        "module": record.module,
        "task_type": record.task_type,
        "tags": record.tags,
        "source_agent": record.source_agent,
    })
}

fn hash_canonical(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn memory_node_id(memory_id: &str) -> String {
    format!("memory:{memory_id}")
}

pub fn project_node_id(project: &str) -> String {
    format!("project:{project}")
}

pub fn repo_node_id(repo: &str) -> String {
    format!("repo:{repo}")
}

pub fn module_node_id(repo: &str, module: &str) -> String {
    format!("module:{repo}:{module}")
}

pub fn workflow_node_id(workflow_id: &str) -> String {
    format!("workflow:{workflow_id}")
}

/// Structured description of the target node in a draft edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToNodeKind {
    EntityRef { kind: EntityKind, alias: String },
    LiteralMemory(String),
}

/// A draft graph edge whose target has not yet been resolved against an
/// `EntityRegistry`. Produced by `extract_graph_edge_drafts`; resolved by
/// `service::memory_service::resolve_drafts_to_edges` (Task 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdgeDraft {
    pub from_node_id: String,
    pub to_kind: ToNodeKind,
    pub relation: String,
}

/// Pure: produce drafts that downstream code resolves against an
/// `EntityRegistry`. Used by both `service::memory_service::ingest`
/// (live writes) and `cli::repair::rebuild_graph` (historical re-derive).
///
/// Skips empty/whitespace-only field values.
pub fn extract_graph_edge_drafts(memory: &MemoryRecord) -> Vec<GraphEdgeDraft> {
    let mut drafts = Vec::new();
    let from_node_id = memory_node_id(&memory.memory_id);

    if let Some(p) = memory.project.as_deref().filter(|v| !v.trim().is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Project,
                alias: p.to_string(),
            },
            relation: "applies_to".into(),
        });
    }
    if let Some(r) = memory.repo.as_deref().filter(|v| !v.trim().is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Repo,
                alias: r.to_string(),
            },
            relation: "observed_in".into(),
        });
    }
    if let (Some(r), Some(m)) = (
        memory.repo.as_deref().filter(|v| !v.trim().is_empty()),
        memory.module.as_deref().filter(|v| !v.trim().is_empty()),
    ) {
        // Module is keyed as "<repo>:<module>" to match the legacy
        // module_node_id format. resolve_or_create will treat this as a
        // single alias string.
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Module,
                alias: format!("{r}:{m}"),
            },
            relation: "relevant_to".into(),
        });
    }
    if let Some(wf) = memory.task_type.as_deref().filter(|v| !v.trim().is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Workflow,
                alias: wf.to_string(),
            },
            relation: "uses_workflow".into(),
        });
    } else if matches!(memory.memory_type, MemoryType::Workflow) {
        // Self-referencing workflow: alias = the memory_id itself.
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Workflow,
                alias: memory.memory_id.clone(),
            },
            relation: "uses_workflow".into(),
        });
    }
    for topic in memory.topics.iter().filter(|v| !v.trim().is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Topic,
                alias: topic.clone(),
            },
            relation: "discusses".into(),
        });
    }
    if let Some(prev) = memory
        .supersedes_memory_id
        .as_deref()
        .filter(|v| !v.is_empty())
    {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::LiteralMemory(prev.to_string()),
            relation: "supersedes".into(),
        });
    }
    drafts
}

fn legacy_to_node_id(kind: &ToNodeKind) -> String {
    match kind {
        ToNodeKind::LiteralMemory(id) => memory_node_id(id),
        ToNodeKind::EntityRef {
            kind: EntityKind::Project,
            alias,
        } => project_node_id(alias),
        ToNodeKind::EntityRef {
            kind: EntityKind::Repo,
            alias,
        } => repo_node_id(alias),
        ToNodeKind::EntityRef {
            kind: EntityKind::Module,
            alias,
        } => {
            // alias is "<repo>:<module>"; module_node_id rebuilds the same string.
            if let Some((r, m)) = alias.split_once(':') {
                module_node_id(r, m)
            } else {
                format!("module:{alias}")
            }
        }
        ToNodeKind::EntityRef {
            kind: EntityKind::Workflow,
            alias,
        } => workflow_node_id(alias),
        ToNodeKind::EntityRef {
            kind: EntityKind::Topic,
            alias,
        } => format!("topic:{alias}"),
    }
}

/// **Deprecated.** Legacy wrapper that produces edges with the OLD
/// `"project:..."` / `"repo:..."` etc. string `to_node_id` format. New
/// code should call `extract_graph_edge_drafts` and resolve through
/// `EntityRegistry` (see `service::memory_service::resolve_drafts_to_edges`).
///
/// This wrapper exists only so the in-tree `graph_store::sync_memory`
/// caller and any historical tests keep compiling until they are migrated.
#[deprecated(note = "Use extract_graph_edge_drafts + EntityRegistry resolution")]
pub fn extract_graph_edges(memory: &MemoryRecord) -> Vec<GraphEdge> {
    let from_node_id = memory_node_id(&memory.memory_id);

    // Convert drafts via the legacy node-id scheme.
    let mut edges: Vec<GraphEdge> = extract_graph_edge_drafts(memory)
        .into_iter()
        .map(|draft| GraphEdge {
            from_node_id: draft.from_node_id,
            to_node_id: legacy_to_node_id(&draft.to_kind),
            relation: draft.relation,
            valid_from: String::new(),
            valid_to: None,
        })
        .collect();

    // The `contradicts:` tag pattern has no field-level draft equivalent; it
    // lives solely in this legacy wrapper until the new pipeline handles it.
    for contradicted in memory
        .tags
        .iter()
        .filter_map(|tag| tag.strip_prefix("contradicts:"))
        .filter(|value| !value.is_empty())
    {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: memory_node_id(contradicted),
            relation: "contradicts".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    }

    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryStatus, Scope, Visibility};

    fn baseline_memory(id: &str) -> MemoryRecord {
        MemoryRecord {
            memory_id: id.to_string(),
            tenant: "local".to_string(),
            memory_type: MemoryType::Implementation,
            status: MemoryStatus::Active,
            scope: Scope::Global,
            visibility: Visibility::Private,
            version: 1,
            summary: "x".to_string(),
            content: "x".to_string(),
            source_agent: "test".to_string(),
            content_hash: "00".repeat(32),
            ..MemoryRecord::default()
        }
    }

    #[test]
    fn extract_graph_edge_drafts_emits_entity_refs_for_all_field_types() {
        let memory = MemoryRecord {
            memory_id: "m1".to_string(),
            project: Some("mem".to_string()),
            repo: Some("foo/bar".to_string()),
            module: Some("storage".to_string()),
            task_type: Some("debug".to_string()),
            topics: vec!["Rust".to_string(), "ownership".to_string()],
            ..baseline_memory("m1")
        };
        let drafts = extract_graph_edge_drafts(&memory);

        let entity_refs: Vec<_> = drafts
            .iter()
            .filter_map(|d| match &d.to_kind {
                ToNodeKind::EntityRef { kind, alias } => {
                    Some((*kind, alias.clone(), d.relation.clone()))
                }
                _ => None,
            })
            .collect();

        assert!(entity_refs.contains(&(EntityKind::Project, "mem".into(), "applies_to".into())));
        assert!(entity_refs.contains(&(EntityKind::Repo, "foo/bar".into(), "observed_in".into())));
        assert!(entity_refs
            .iter()
            .any(|(k, _, r)| *k == EntityKind::Module && r == "relevant_to"));
        assert!(entity_refs.contains(&(
            EntityKind::Workflow,
            "debug".into(),
            "uses_workflow".into()
        )));
        assert!(entity_refs.contains(&(EntityKind::Topic, "Rust".into(), "discusses".into())));
        assert!(entity_refs.contains(&(EntityKind::Topic, "ownership".into(), "discusses".into())));
    }

    #[test]
    fn extract_graph_edge_drafts_emits_literal_memory_for_supersedes() {
        let memory = MemoryRecord {
            memory_id: "m2".to_string(),
            supersedes_memory_id: Some("m1".to_string()),
            ..baseline_memory("m2")
        };
        let drafts = extract_graph_edge_drafts(&memory);
        assert!(drafts.iter().any(|d| matches!(
            &d.to_kind,
            ToNodeKind::LiteralMemory(id) if id == "m1"
        )));
    }

    #[test]
    fn extract_graph_edge_drafts_skips_empty_topic_strings() {
        let memory = MemoryRecord {
            memory_id: "m3".to_string(),
            topics: vec!["".to_string(), "Rust".to_string(), "  ".to_string()],
            ..baseline_memory("m3")
        };
        let drafts = extract_graph_edge_drafts(&memory);
        let topic_drafts: Vec<_> = drafts
            .iter()
            .filter(|d| {
                matches!(
                    &d.to_kind,
                    ToNodeKind::EntityRef {
                        kind: EntityKind::Topic,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(
            topic_drafts.len(),
            1,
            "empty/whitespace-only topics filtered out"
        );
    }

    #[test]
    fn validate_verbatim_no_caller_summary_ok() {
        assert!(validate_verbatim("any content here", None).is_ok());
    }

    #[test]
    fn validate_verbatim_empty_caller_summary_ok() {
        // Empty string normalized to "no summary supplied" → no validation.
        assert!(validate_verbatim("any content here", Some("")).is_ok());
    }

    #[test]
    fn validate_verbatim_caller_summary_differs_ok() {
        assert!(validate_verbatim("hello world", Some("greeting")).is_ok());
    }

    #[test]
    fn validate_verbatim_caller_summary_equals_content_rejected() {
        let err = validate_verbatim("the same text", Some("the same text"))
            .expect_err("should reject identical caller summary");
        assert!(
            err.contains("verbatim"),
            "error must mention verbatim: {}",
            err
        );
    }
}
