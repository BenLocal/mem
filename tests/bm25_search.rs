use std::sync::Arc;

use mem::{
    domain::{
        capability_capsule::{
            CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
        },
        query::SearchCapabilityCapsuleRequest,
    },
    service::CapabilityCapsuleService,
    storage::Store,
};
use tempfile::tempdir;

fn ingest_request(content: &str, summary: &str) -> IngestCapabilityCapsuleRequest {
    IngestCapabilityCapsuleRequest {
        tenant: "t1".into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        content: content.into(),
        summary: Some(summary.into()),
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Project,
        visibility: Visibility::Private,
        project: Some("p".into()),
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    }
}

/// BM25 must rank the textually-relevant memory above unrelated rows even
/// when all candidates share scope, freshness, and confidence — the lexical
/// signal alone has to push the right one to the top.
#[tokio::test]
async fn bm25_ranks_textual_match_to_top() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("bm25.duckdb");
    let repo = Arc::new(Store::open(&db).await.unwrap());
    let svc = CapabilityCapsuleService::with_providers(repo, "fake".into(), None);

    let target = svc
        .ingest(ingest_request(
            "ClassName field length needs to fit classroomhoststatehistory column width",
            "classroomhoststatehistory ClassName data truncation",
        ))
        .await
        .unwrap();
    svc.ingest(ingest_request(
        "use older 4.x version of plugin in Java 8 builds",
        "git commit id plugin java 8",
    ))
    .await
    .unwrap();
    svc.ingest(ingest_request(
        "starvation pattern in scheduler rotation ledger",
        "scheduler poison pill",
    ))
    .await
    .unwrap();

    let response = svc
        .search(SearchCapabilityCapsuleRequest {
            query: "classroomhoststatehistory ClassName truncation".into(),
            intent: "debugging".into(),
            scope_filters: vec![],
            token_budget: 800,
            caller_agent: "test".into(),
            expand_graph: false,
            tenant: Some("t1".into()),
        })
        .await
        .unwrap();

    let mut surfaced = Vec::new();
    surfaced.extend(
        response
            .directives
            .iter()
            .map(|d| d.capability_capsule_id.clone()),
    );
    surfaced.extend(
        response
            .relevant_facts
            .iter()
            .map(|f| f.capability_capsule_id.clone()),
    );
    surfaced.extend(
        response
            .reusable_patterns
            .iter()
            .map(|p| p.capability_capsule_id.clone()),
    );
    if let Some(w) = response.suggested_workflow.as_ref() {
        surfaced.push(w.capability_capsule_id.clone());
    }

    assert_eq!(
        surfaced.first(),
        Some(&target.capability_capsule_id),
        "BM25-relevant memory must rank to the top, got {surfaced:?}"
    );
}

/// Threshold-driven empty-section behavior: a query with zero relevance to
/// any seeded memory must yield empty sections, not padded low-score
/// garbage. Validates the user-visible contract: "no relevant results =
/// empty section".
#[tokio::test]
async fn unrelated_query_returns_empty_sections() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("empty.duckdb");
    let repo = Arc::new(Store::open(&db).await.unwrap());
    let svc = CapabilityCapsuleService::with_providers(repo, "fake".into(), None);

    svc.ingest(ingest_request(
        "DuckDB stores canonical memory records and indexes locally",
        "duckdb storage",
    ))
    .await
    .unwrap();

    let response = svc
        .search(SearchCapabilityCapsuleRequest {
            query: "completely unrelated topic about distributed consensus algorithms".into(),
            intent: "debugging".into(),
            scope_filters: vec![],
            token_budget: 800,
            caller_agent: "test".into(),
            expand_graph: false,
            tenant: Some("t1".into()),
        })
        .await
        .unwrap();

    assert!(
        response.directives.is_empty(),
        "expected empty directives, got {:?}",
        response.directives
    );
    assert!(
        response.relevant_facts.is_empty(),
        "expected empty relevant_facts, got {:?}",
        response.relevant_facts
    );
    assert!(
        response.reusable_patterns.is_empty(),
        "expected empty reusable_patterns, got {:?}",
        response.reusable_patterns
    );
    assert!(
        response.suggested_workflow.is_none(),
        "expected no suggested_workflow, got {:?}",
        response.suggested_workflow
    );
}
