use std::sync::Arc;

use mem::{
    config::EmbeddingSettings,
    domain::{
        capability_capsule::{
            CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope,
            Visibility,
        },
        query::SearchCapabilityCapsuleRequest,
        EntityKind,
    },
    embedding::{arc_embedding_provider, deterministic_embedding},
    service::CapabilityCapsuleService,
    storage::Store,
};
use tempfile::tempdir;

fn f32_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

#[tokio::test]
async fn hybrid_search_surfaces_semantic_match_without_lexical_overlap() {
    let dim = 64;
    let mut settings = EmbeddingSettings::development_defaults();
    settings.dim = dim;
    let provider = arc_embedding_provider(&settings).unwrap();

    let dir = tempdir().unwrap();
    let db = dir.path().join("hybrid.duckdb");
    let repo = Arc::new(Store::open(&db).await.unwrap());

    let query_text = "query-unique-semantic-anchor-xyz";
    let query_vec = deterministic_embedding(query_text, dim);

    let mem_noise = CapabilityCapsuleRecord {
        capability_capsule_id: "mem_noise".into(),
        tenant: "t1".into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "noise".into(),
        content: "alpha beta gamma delta".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: "hash-noise".into(),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test".into(),
        created_at: "1".into(),
        updated_at: "1".into(),
        last_validated_at: None,
    };

    let mem_hit = CapabilityCapsuleRecord {
        capability_capsule_id: "mem_hit".into(),
        tenant: "t1".into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: "other".into(),
        content: "unrelated body text".into(),
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.9,
        decay_score: 0.0,
        content_hash: "hash-hit".into(),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test".into(),
        created_at: "2".into(),
        updated_at: "2".into(),
        last_validated_at: None,
    };

    repo.insert_capability_capsule(mem_noise.clone())
        .await
        .unwrap();
    repo.insert_capability_capsule(mem_hit.clone())
        .await
        .unwrap();

    let now = "00000000000000000001";
    repo.upsert_capability_capsule_embedding(
        &mem_noise.capability_capsule_id,
        &mem_noise.tenant,
        "fake",
        dim as i64,
        &f32_to_blob(&deterministic_embedding("noise-source", dim)),
        &mem_noise.content_hash,
        &mem_noise.updated_at,
        now,
    )
    .await
    .unwrap();
    repo.upsert_capability_capsule_embedding(
        &mem_hit.capability_capsule_id,
        &mem_hit.tenant,
        "fake",
        dim as i64,
        &f32_to_blob(&query_vec),
        &mem_hit.content_hash,
        &mem_hit.updated_at,
        now,
    )
    .await
    .unwrap();

    let service = CapabilityCapsuleService::with_providers(repo, "fake".into(), Some(provider));

    let response = service
        .search(SearchCapabilityCapsuleRequest {
            query: query_text.into(),
            intent: "debugging".into(),
            scope_filters: vec![],
            token_budget: 800,
            caller_agent: "test".into(),
            expand_graph: false,
            tenant: Some("t1".into()),
        })
        .await
        .unwrap();

    let mut ids = Vec::new();
    ids.extend(
        response
            .directives
            .iter()
            .map(|d| d.capability_capsule_id.as_str()),
    );
    ids.extend(
        response
            .relevant_facts
            .iter()
            .map(|f| f.capability_capsule_id.as_str()),
    );
    ids.extend(
        response
            .reusable_patterns
            .iter()
            .map(|p| p.capability_capsule_id.as_str()),
    );
    if let Some(w) = response.suggested_workflow.as_ref() {
        ids.push(w.capability_capsule_id.as_str());
    }

    assert!(
        ids.contains(&"mem_hit"),
        "expected semantic hit mem_hit in compressed response, got {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Helper shared by graph_boost test below
// ---------------------------------------------------------------------------

async fn ingest_for_e2e(
    svc: &CapabilityCapsuleService,
    content: &str,
    project: Option<&str>,
    repo_name: Option<&str>,
) -> mem::service::IngestCapabilityCapsuleResponse {
    use mem::domain::capability_capsule::{
        CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
    };
    svc.ingest(IngestCapabilityCapsuleRequest {
        tenant: "t".into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: project.map(String::from),
        repo: repo_name.map(String::from),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn graph_boost_excludes_superseded_memory_from_related_memory_ids() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("graph-boost.duckdb");
    let repo = Arc::new(Store::open(&db).await.unwrap());
    let svc = CapabilityCapsuleService::new(repo.clone());

    // Ingest two memories sharing project=foo so graph edges link them via
    // the resolved entity node (entity:<uuid>) — Task 9 routed ingest through
    // EntityRegistry::resolve_or_create.
    let r1 = ingest_for_e2e(&svc, "alpha", Some("foo"), Some("mem")).await;
    let r2 = ingest_for_e2e(&svc, "beta", Some("foo"), Some("mem")).await;

    // Look up the entity_id that ingest auto-promoted for project="foo" so we
    // can drive the graph query against the same node both edges point at.
    let project_entity_id = repo
        .resolve_or_create("t", "foo", EntityKind::Project, "00000000020260502000")
        .await
        .unwrap();
    let project_node = format!("entity:{project_entity_id}");

    // Pre-condition: both memories are reachable from the shared project node.
    let pre = repo
        .related_capability_capsule_ids(std::slice::from_ref(&project_node))
        .await
        .unwrap();
    let mut pre_sorted = pre.clone();
    pre_sorted.sort();
    assert_eq!(
        pre_sorted.len(),
        2,
        "both memories should be related before close: {pre_sorted:?}"
    );

    // Simulate the supersede side-effect: close r1's outbound edges.
    repo.close_edges_for_capability_capsule(&r1.capability_capsule_id)
        .await
        .unwrap();

    // Post-condition: only r2 is reachable — r1 is excluded by graph_boost.
    let post = repo
        .related_capability_capsule_ids(std::slice::from_ref(&project_node))
        .await
        .unwrap();
    assert_eq!(
        post.len(),
        1,
        "after close_edges_for_capability_capsule, only r2 should be related: {post:?}"
    );
    assert_eq!(post[0], r2.capability_capsule_id);
}
