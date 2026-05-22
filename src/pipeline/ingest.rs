use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, GraphEdge,
    IngestCapabilityCapsuleRequest, WriteMode,
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

/// Validate that the scope / project boundary fields are coherent.
/// Closes `agent-memory-strategy-readiness §4.1 #4`: a capsule with
/// `scope=Project` or `scope=Repo` requires `project` to be set,
/// otherwise the row is filed into the project-scoped pool with no
/// way to filter back out. Rejects ingest at boundary time rather
/// than silently writing a mis-scoped capsule.
///
/// `Global` and `Workspace` scopes are unaffected (Global has no
/// project anchor by design; Workspace is workspace-wide so project
/// is optional metadata, not a filter key).
pub fn validate_scope_boundary(
    scope: &crate::domain::capability_capsule::Scope,
    project: Option<&str>,
) -> Result<(), String> {
    use crate::domain::capability_capsule::Scope;
    let needs_project = matches!(scope, Scope::Project | Scope::Repo);
    let has_project = project.is_some_and(|p| !p.trim().is_empty());
    if needs_project && !has_project {
        return Err(format!(
            "scope={scope:?} requires non-empty `project` field; \
             omit project only when scope is `global` or `workspace`",
        ));
    }
    Ok(())
}

/// Compute the initial `status` for a freshly-ingested capsule.
///
/// Status routing table:
///
/// | type \\ write_mode | `Auto`               | `Propose`            |
/// |--------------------|----------------------|----------------------|
/// | Preference         | PendingConfirmation  | PendingConfirmation  |
/// | Workflow           | PendingConfirmation  | PendingConfirmation  |
/// | Implementation     | Active               | PendingConfirmation  |
/// | Experience         | Active               | PendingConfirmation  |
/// | Episode            | Active               | PendingConfirmation  |
/// | Diary              | Active               | PendingConfirmation  |
///
/// Rationale: `Propose` is the caller's explicit signal that the
/// row should be human-reviewed before joining the active pool —
/// the agent-driven nudge path (PostToolUse hook → SKILL.md trigger
/// (e)) relies on this. The previous matrix routed
/// `(non-Preference/Workflow, Propose)` to `Provisional` instead,
/// which silently put the capsule in the active pool with low
/// confidence but no review hook — agents' propose calls became
/// invisible to `list_pending_review`. `Provisional` is still a
/// valid status the rest of the pipeline understands (retrieve.rs
/// scores it identically to `PendingConfirmation`), but no longer
/// reachable via this entry point.
pub fn initial_status(
    capability_capsule_type: &CapabilityCapsuleType,
    write_mode: &WriteMode,
) -> CapabilityCapsuleStatus {
    match (capability_capsule_type, write_mode) {
        (CapabilityCapsuleType::Preference | CapabilityCapsuleType::Workflow, _) => {
            CapabilityCapsuleStatus::PendingConfirmation
        }
        (_, WriteMode::Propose) => CapabilityCapsuleStatus::PendingConfirmation,
        (_, WriteMode::Auto) => CapabilityCapsuleStatus::Active,
    }
}

pub fn compute_content_hash(request: &IngestCapabilityCapsuleRequest) -> String {
    hash_canonical(&canonical_request_json(request))
}

/// Recompute the content hash from a stored `CapabilityCapsuleRecord`. Equivalent to
/// hashing the original `IngestCapabilityCapsuleRequest`, used by the migration that
/// upgrades pre-sha256 rows.
pub fn compute_content_hash_from_record(record: &CapabilityCapsuleRecord) -> String {
    hash_canonical(&canonical_record_json(record))
}

fn canonical_request_json(request: &IngestCapabilityCapsuleRequest) -> Value {
    json!({
        "tenant": request.tenant,
        "capability_capsule_type": request.capability_capsule_type,
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

fn canonical_record_json(record: &CapabilityCapsuleRecord) -> Value {
    json!({
        "tenant": record.tenant,
        "capability_capsule_type": record.capability_capsule_type,
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

pub fn memory_node_id(capability_capsule_id: &str) -> String {
    format!("capability_capsule:{capability_capsule_id}")
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

pub fn topic_node_id(topic: &str) -> String {
    format!("topic:{topic}")
}

pub fn tag_node_id(tag: &str) -> String {
    format!("tag:{tag}")
}

pub fn session_node_id(session_id: &str) -> String {
    format!("session:{session_id}")
}

/// Node id for a file entity. Composite when `repo` is provided so
/// `src/foo.rs` in repo `mem` is a distinct node from the same path
/// in another repo. ROADMAP #19.
pub fn file_node_id(repo: Option<&str>, path: &str) -> String {
    match repo {
        Some(r) => format!("file:{r}:{path}"),
        None => format!("file:{path}"),
    }
}

/// Normalize a `code_refs` entry into a file path suitable for use
/// as an `EntityKind::File` alias. Strips:
///
/// 1. Leading / trailing whitespace.
/// 2. A trailing `:<digits>` suffix (the `path:line_number` convention
///    documented in `database-schema.md`). Detection uses the last `:`
///    so Windows drive letters like `C:\foo\bar.rs` are preserved
///    (their suffix is `\foo\bar.rs`, not digits).
/// 3. Trailing `/`.
///
/// Returns `None` when the result is empty.
pub fn normalize_file_ref(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = match trimmed.rsplit_once(':') {
        Some((prefix, suffix))
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            prefix
        }
        _ => trimmed,
    };
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// Composite alias for the `EntityKind::File` entity registry — same
/// shape as `Module`'s `<repo>:<module>`. ROADMAP #19.
pub(crate) fn file_alias(repo: Option<&str>, path: &str) -> String {
    match repo {
        Some(r) => format!("{r}:{path}"),
        None => path.to_string(),
    }
}

/// Structured description of the target node in a draft edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToNodeKind {
    EntityRef {
        kind: EntityKind,
        alias: String,
    },
    LiteralMemory(String),
    /// Verbatim `session:<session_id>` node — session ids are already
    /// canonical UUIDv7 strings, so we bypass `EntityRegistry`
    /// (which exists for alias normalization the session_id doesn't
    /// need) and write the node id directly. Added in ROADMAP #18.
    LiteralSession(String),
}

/// A draft graph edge whose target has not yet been resolved against an
/// `EntityRegistry`. Produced by `extract_graph_edge_drafts`; resolved by
/// `service::capability_capsule_service::resolve_drafts_to_edges` (Task 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdgeDraft {
    pub from_node_id: String,
    pub to_kind: ToNodeKind,
    pub relation: String,
}

/// Pure: produce drafts that downstream code resolves against an
/// `EntityRegistry`. Used by `service::capability_capsule_service::ingest`
/// on live writes.
///
/// Skips empty/whitespace-only field values.
pub fn extract_graph_edge_drafts(memory: &CapabilityCapsuleRecord) -> Vec<GraphEdgeDraft> {
    let mut drafts = Vec::new();
    let from_node_id = memory_node_id(&memory.capability_capsule_id);

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
    } else if matches!(
        memory.capability_capsule_type,
        CapabilityCapsuleType::Workflow
    ) {
        // Self-referencing workflow: alias = the capability_capsule_id itself.
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Workflow,
                alias: memory.capability_capsule_id.clone(),
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
    // ROADMAP #16: tags route through `EntityRegistry` the same way
    // topics do — dedup variants ("Rust" / "rust" / " RUST ") to one
    // canonical entity_id, then write a `tagged` edge. The
    // `contradicts:<other_id>` tag prefix is the one special case:
    // it's a memory→memory pointer encoded in tag space, not a real
    // tag. Skip it here (legacy `extract_graph_edges` handled it; new
    // pipeline drops it because no live caller emits this prefix on
    // ingest paths — it's a historical artifact).
    for tag in memory
        .tags
        .iter()
        .filter(|v| !v.trim().is_empty())
        .filter(|v| !v.starts_with("contradicts:"))
    {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Tag,
                alias: tag.clone(),
            },
            relation: "tagged".into(),
        });
    }
    if let Some(prev) = memory
        .supersedes_capability_capsule_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
    {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::LiteralMemory(prev.to_string()),
            relation: "supersedes".into(),
        });
    }
    // ROADMAP #19: each `code_refs[]` entry becomes a
    // `mentions_file` edge through `EntityKind::File`. Normalization
    // drops the `path:line_number` suffix so `src/foo.rs:42` and
    // `src/foo.rs` collapse to one file entity. When the memory has
    // a `repo` field, the alias is composite (`<repo>:<path>`) so
    // the same path in different repos is distinct — matches the
    // `Module` entity's `<repo>:<module>` shape.
    let repo_for_file_alias = memory.repo.as_deref().filter(|v| !v.trim().is_empty());
    for raw_ref in memory.code_refs.iter() {
        let Some(path) = normalize_file_ref(raw_ref) else {
            continue;
        };
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::File,
                alias: file_alias(repo_for_file_alias, &path),
            },
            relation: "mentions_file".into(),
        });
    }
    // ROADMAP #18: every memory derived from a transcript session
    // (the `mem mine` path stamps `session_id`) gets a
    // `memory:<id> --extracted_from--> session:<sid>` edge. Direction
    // is memory→session (NOT session→memory) so the graph cleanup
    // mechanism `close_edges_for_capability_capsule` still
    // auto-closes the edge when the memory is hard-deleted —
    // session→memory would leave a dangling edge pointing at a
    // deleted node. Reverse-direction reads ("which memories came
    // from this session") still work via `neighbors`, which already
    // matches `from_node_id = X OR to_node_id = X`.
    if let Some(sid) = memory
        .session_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
    {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::LiteralSession(sid.to_string()),
            relation: "extracted_from".into(),
        });
    }
    drafts
}

fn legacy_to_node_id(kind: &ToNodeKind) -> String {
    match kind {
        ToNodeKind::LiteralMemory(id) => memory_node_id(id),
        ToNodeKind::LiteralSession(id) => session_node_id(id),
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
        } => topic_node_id(alias),
        ToNodeKind::EntityRef {
            kind: EntityKind::Tag,
            alias,
        } => tag_node_id(alias),
        ToNodeKind::EntityRef {
            kind: EntityKind::File,
            alias,
        } => {
            // Same composite-alias split as Module: first `:` splits
            // the repo prefix from the path. Bare paths (no-repo case)
            // become `file:<path>` without a repo component.
            if let Some((r, p)) = alias.split_once(':') {
                file_node_id(Some(r), p)
            } else {
                file_node_id(None, alias)
            }
        }
    }
}

/// **Deprecated.** Legacy wrapper that produces edges with the OLD
/// `"project:..."` / `"repo:..."` etc. string `to_node_id` format. New
/// code should call `extract_graph_edge_drafts` and resolve through
/// `EntityRegistry` (see `service::capability_capsule_service::resolve_drafts_to_edges`).
///
/// This wrapper exists only so the in-tree `graph_store::sync_memory`
/// caller and any historical tests keep compiling until they are migrated.
#[deprecated(note = "Use extract_graph_edge_drafts + EntityRegistry resolution")]
pub fn extract_graph_edges(memory: &CapabilityCapsuleRecord) -> Vec<GraphEdge> {
    let from_node_id = memory_node_id(&memory.capability_capsule_id);

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
    use crate::domain::capability_capsule::{CapabilityCapsuleStatus, Scope, Visibility};

    fn baseline_memory(id: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.to_string(),
            tenant: "local".to_string(),
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            status: CapabilityCapsuleStatus::Active,
            scope: Scope::Global,
            visibility: Visibility::Private,
            version: 1,
            summary: "x".to_string(),
            content: "x".to_string(),
            source_agent: "test".to_string(),
            content_hash: "00".repeat(32),
            ..CapabilityCapsuleRecord::default()
        }
    }

    #[test]
    fn extract_graph_edge_drafts_emits_entity_refs_for_all_field_types() {
        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: "m1".to_string(),
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
        assert!(entity_refs.contains(&(
            EntityKind::Module,
            "foo/bar:storage".into(),
            "relevant_to".into()
        )));
        assert!(entity_refs.contains(&(
            EntityKind::Workflow,
            "debug".into(),
            "uses_workflow".into()
        )));
        assert!(entity_refs.contains(&(EntityKind::Topic, "Rust".into(), "discusses".into())));
        assert!(entity_refs.contains(&(EntityKind::Topic, "ownership".into(), "discusses".into())));
    }

    #[test]
    fn extract_graph_edge_drafts_emits_literal_session_when_session_id_set() {
        // ROADMAP #18: memory carrying a session_id (e.g. from
        // `mem mine`) emits a `memory:<id> --extracted_from--> session:<sid>`
        // edge so the reverse lookup "which memories came from this
        // session" is one graph hop instead of an SQL scan.
        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: "m_sess".to_string(),
            session_id: Some("019e3900-aaaa-bbbb-cccc-dddd".to_string()),
            ..baseline_memory("m_sess")
        };
        let drafts = extract_graph_edge_drafts(&memory);
        let session_drafts: Vec<_> = drafts
            .iter()
            .filter(|d| matches!(d.to_kind, ToNodeKind::LiteralSession(_)))
            .collect();
        assert_eq!(session_drafts.len(), 1);
        assert_eq!(session_drafts[0].relation, "extracted_from");
        assert_eq!(
            session_drafts[0].from_node_id,
            "capability_capsule:m_sess",
            "edge direction is memory → session so close_edges_for_capability_capsule cleans it on hard-delete",
        );
        assert_eq!(
            session_drafts[0].to_kind,
            ToNodeKind::LiteralSession("019e3900-aaaa-bbbb-cccc-dddd".to_string()),
        );
    }

    #[test]
    fn extract_graph_edge_drafts_skips_blank_session_id() {
        for sid in ["", "   "] {
            let memory = CapabilityCapsuleRecord {
                capability_capsule_id: "m_blank".to_string(),
                session_id: Some(sid.to_string()),
                ..baseline_memory("m_blank")
            };
            let drafts = extract_graph_edge_drafts(&memory);
            assert!(
                !drafts
                    .iter()
                    .any(|d| matches!(d.to_kind, ToNodeKind::LiteralSession(_))),
                "blank session_id {sid:?} must not emit an edge",
            );
        }
    }

    #[test]
    fn extract_graph_edge_drafts_emits_literal_memory_for_supersedes() {
        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: "m2".to_string(),
            supersedes_capability_capsule_id: Some("m1".to_string()),
            ..baseline_memory("m2")
        };
        let drafts = extract_graph_edge_drafts(&memory);
        assert!(drafts.iter().any(|d| matches!(
            &d.to_kind,
            ToNodeKind::LiteralMemory(id) if id == "m1"
        )));
    }

    #[test]
    fn normalize_file_ref_strips_line_suffix_and_whitespace() {
        assert_eq!(
            normalize_file_ref("src/foo.rs:42"),
            Some("src/foo.rs".into()),
        );
        assert_eq!(
            normalize_file_ref("  src/foo.rs:42  "),
            Some("src/foo.rs".into()),
        );
        assert_eq!(normalize_file_ref("src/foo.rs"), Some("src/foo.rs".into()));
        assert_eq!(normalize_file_ref("src/"), Some("src".into()));
        // Windows-style drive letter — last `:` suffix is `\foo.rs`,
        // not digits, so the path is preserved verbatim.
        assert_eq!(normalize_file_ref(r"C:\foo.rs"), Some(r"C:\foo.rs".into()),);
        // Empty / whitespace input.
        assert_eq!(normalize_file_ref(""), None);
        assert_eq!(normalize_file_ref("   "), None);
        assert_eq!(normalize_file_ref("/"), None);
        // Only `:line` (no path) is normalized away too — a degenerate
        // input that strips to empty string.
        assert_eq!(normalize_file_ref(":42"), None);
    }

    #[test]
    fn extract_graph_edge_drafts_emits_mentions_file_edges_with_repo_composite() {
        // ROADMAP #19: each code_refs entry becomes a `mentions_file`
        // edge through `EntityKind::File`. Composite alias prefixed
        // with repo so the same path in different repos is distinct.
        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: "m_file".to_string(),
            repo: Some("mem".to_string()),
            code_refs: vec![
                "src/storage/lance_store/mod.rs".to_string(),
                "src/storage/lance_store/mod.rs:1359".to_string(), // same file, line-stripped
                "src/cache.rs".to_string(),
                "".to_string(),    // skipped
                "   ".to_string(), // skipped
            ],
            ..baseline_memory("m_file")
        };
        let drafts = extract_graph_edge_drafts(&memory);
        let file_drafts: Vec<_> = drafts
            .iter()
            .filter(|d| {
                matches!(
                    &d.to_kind,
                    ToNodeKind::EntityRef {
                        kind: EntityKind::File,
                        ..
                    }
                )
            })
            .collect();
        // 3 drafts: src/storage/lance_store/mod.rs (×2 emitted, but
        // both normalize to same alias), src/cache.rs. Note we emit
        // duplicates at draft level — sync_memory_edges dedupes at
        // insert time via the (from, to, relation) existence check.
        assert_eq!(
            file_drafts.len(),
            3,
            "empty / whitespace skipped, duplicates allowed at draft level",
        );
        let aliases: Vec<&str> = file_drafts
            .iter()
            .filter_map(|d| match &d.to_kind {
                ToNodeKind::EntityRef { alias, .. } => Some(alias.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            aliases,
            vec![
                "mem:src/storage/lance_store/mod.rs",
                "mem:src/storage/lance_store/mod.rs", // line-stripped → same alias
                "mem:src/cache.rs",
            ],
        );
        assert!(file_drafts.iter().all(|d| d.relation == "mentions_file"));
        assert!(file_drafts
            .iter()
            .all(|d| d.from_node_id == "capability_capsule:m_file"));
    }

    #[test]
    fn extract_graph_edge_drafts_mentions_file_bare_alias_when_no_repo() {
        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: "m_no_repo".to_string(),
            repo: None,
            code_refs: vec!["docs/README.md".to_string()],
            ..baseline_memory("m_no_repo")
        };
        let drafts = extract_graph_edge_drafts(&memory);
        let file_alias_only: Vec<&str> = drafts
            .iter()
            .filter_map(|d| match &d.to_kind {
                ToNodeKind::EntityRef {
                    kind: EntityKind::File,
                    alias,
                } => Some(alias.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(file_alias_only, vec!["docs/README.md"]);
    }

    #[test]
    fn extract_graph_edge_drafts_emits_tagged_edges_for_tags() {
        // ROADMAP #16: each non-empty, non-`contradicts:`-prefixed tag
        // becomes a `tagged` edge through `EntityKind::Tag`.
        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: "m_tag".to_string(),
            tags: vec![
                "rust".to_string(),
                "lifecycle".to_string(),
                "".to_string(),
                "  ".to_string(),
                "contradicts:m_old".to_string(),
            ],
            ..baseline_memory("m_tag")
        };
        let drafts = extract_graph_edge_drafts(&memory);
        let tag_drafts: Vec<_> = drafts
            .iter()
            .filter(|d| {
                matches!(
                    &d.to_kind,
                    ToNodeKind::EntityRef {
                        kind: EntityKind::Tag,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(
            tag_drafts.len(),
            2,
            "empty / whitespace tags skipped; `contradicts:` prefix skipped (not a real tag)",
        );
        let aliases: Vec<&str> = tag_drafts
            .iter()
            .filter_map(|d| match &d.to_kind {
                ToNodeKind::EntityRef { alias, .. } => Some(alias.as_str()),
                _ => None,
            })
            .collect();
        assert!(aliases.contains(&"rust"));
        assert!(aliases.contains(&"lifecycle"));
        assert!(tag_drafts.iter().all(|d| d.relation == "tagged"));
        assert!(tag_drafts
            .iter()
            .all(|d| d.from_node_id == "capability_capsule:m_tag"));
    }

    #[test]
    fn extract_graph_edge_drafts_skips_empty_topic_strings() {
        let memory = CapabilityCapsuleRecord {
            capability_capsule_id: "m3".to_string(),
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
        assert_eq!(
            topic_drafts[0].to_kind,
            ToNodeKind::EntityRef {
                kind: EntityKind::Topic,
                alias: "Rust".into()
            }
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

    #[test]
    fn validate_scope_project_requires_project_field() {
        use crate::domain::capability_capsule::Scope;
        let err = validate_scope_boundary(&Scope::Project, None)
            .expect_err("scope=Project + project=None should reject");
        assert!(err.contains("project"), "error must mention project: {err}");
    }

    #[test]
    fn validate_scope_repo_requires_project_field() {
        use crate::domain::capability_capsule::Scope;
        let err = validate_scope_boundary(&Scope::Repo, None)
            .expect_err("scope=Repo + project=None should reject");
        assert!(err.contains("project"), "error must mention project: {err}");
    }

    #[test]
    fn validate_scope_project_empty_string_also_rejected() {
        use crate::domain::capability_capsule::Scope;
        for blank in ["", "   ", "\t\n"] {
            validate_scope_boundary(&Scope::Project, Some(blank))
                .expect_err("blank-string project under scope=Project should reject");
        }
    }

    #[test]
    fn validate_scope_project_with_project_ok() {
        use crate::domain::capability_capsule::Scope;
        assert!(validate_scope_boundary(&Scope::Project, Some("phoenix")).is_ok());
        assert!(validate_scope_boundary(&Scope::Repo, Some("mem")).is_ok());
    }

    #[test]
    fn validate_scope_global_and_workspace_allow_no_project() {
        use crate::domain::capability_capsule::Scope;
        // Global / Workspace scopes don't anchor to a project — None is fine.
        assert!(validate_scope_boundary(&Scope::Global, None).is_ok());
        assert!(validate_scope_boundary(&Scope::Workspace, None).is_ok());
        // And of course a project is still allowed (treated as metadata).
        assert!(validate_scope_boundary(&Scope::Global, Some("any")).is_ok());
    }
}
