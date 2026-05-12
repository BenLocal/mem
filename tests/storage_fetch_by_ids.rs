use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
};
use mem::service::CapabilityCapsuleService;
use mem::storage::Store;
use tempfile::tempdir;

#[tokio::test]
async fn fetch_by_ids_filters_tenant_and_status() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("fetch.lance");
    let store = Arc::new(Store::open(&db).await.unwrap());
    let svc = CapabilityCapsuleService::new(store.clone());

    let make = |tenant: &str, content: &str| IngestCapabilityCapsuleRequest {
        tenant: tenant.into(),
        capability_capsule_type: CapabilityCapsuleType::Implementation,
        content: content.into(),
        summary: None,
        evidence: vec![],
        code_refs: vec![],
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        project: None,
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        source_agent: "test".into(),
        idempotency_key: None,
        write_mode: WriteMode::Auto,
        supersedes_capability_capsule_id: None,
    };
    let a = svc.ingest(make("ten-a", "alpha")).await.unwrap();
    let b = svc.ingest(make("ten-b", "beta")).await.unwrap();
    let c = svc.ingest(make("ten-a", "gamma")).await.unwrap();

    let ids = vec![
        a.capability_capsule_id.as_str(),
        b.capability_capsule_id.as_str(),
        c.capability_capsule_id.as_str(),
    ];
    let rows = store
        .fetch_capability_capsules_by_ids("ten-a", &ids)
        .await
        .unwrap();

    let returned: std::collections::HashSet<_> = rows
        .iter()
        .map(|m| m.capability_capsule_id.as_str())
        .collect();
    assert!(returned.contains(a.capability_capsule_id.as_str()));
    assert!(returned.contains(c.capability_capsule_id.as_str()));
    assert!(!returned.contains(b.capability_capsule_id.as_str())); // wrong tenant
}
