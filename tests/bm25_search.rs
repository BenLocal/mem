use std::sync::Arc;

use mem::{
    config::ReadEngine,
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
        supersedes_capability_capsule_id: None,
        expires_at: None,
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
            min_score: None,
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
            min_score: None,
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

/// Residual #2 (route-B): on the Lance read engine, `bm25_candidate_ids` must
/// return hits even when the vacuum worker NEVER ran `rebuild_query_indexes`
/// (e.g. `MEM_VACUUM_DISABLED=1`). The Tantivy index is built lazily on first
/// query via the `fts_built` latch — this test documents/guards that the
/// lazy-build safety net keeps the route-B FTS bucket working standalone,
/// which is what makes the lance-engine FTS gate (skipping the eager rebuild
/// in DuckDb-default production) safe.
#[tokio::test]
async fn bm25_lazy_builds_on_lance_engine_without_rebuild() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("bm25_lazy.lance");
    let repo = Arc::new(
        Store::open_with_read_engine(&db, ReadEngine::Lance)
            .await
            .unwrap(),
    );
    let svc = CapabilityCapsuleService::with_providers(repo.clone(), "fake".into(), None);

    let target = svc
        .ingest(ingest_request(
            "classroomhoststatehistory ClassName column width truncation bug",
            "classroomhoststatehistory truncation",
        ))
        .await
        .unwrap();
    svc.ingest(ingest_request(
        "unrelated scheduler rotation ledger starvation",
        "scheduler starvation",
    ))
    .await
    .unwrap();

    // NOTE: we deliberately do NOT call `rebuild_query_indexes()` — this is the
    // "vacuum disabled" path. The first BM25 query must lazy-build the Tantivy
    // index from the live corpus and still return the textual match.
    let hits = repo
        .bm25_candidate_ids("t1", "classroomhoststatehistory truncation", 5)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    assert!(
        ids.contains(&target.capability_capsule_id.as_str()),
        "lazy-built BM25 index must surface the textual match without an explicit rebuild; got {ids:?}"
    );

    // A second query reuses the now-built index (the `fts_built` latch is set)
    // and still returns the hit — proving lazy-build is idempotent.
    let again = repo
        .bm25_candidate_ids("t1", "classroomhoststatehistory truncation", 5)
        .await
        .unwrap();
    let again_ids: Vec<&str> = again.iter().map(|(id, _)| id.as_str()).collect();
    assert!(again_ids.contains(&target.capability_capsule_id.as_str()));
}
