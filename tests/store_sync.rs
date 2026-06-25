use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::embedding::FakeEmbeddingProvider;
use mem::storage::Store;
use tempfile::TempDir;

async fn temp_lance() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeEmbeddingProvider::new("fake", 64));
    let store = Store::open_with_provider(dir.path().join("store"), provider)
        .await
        .unwrap();
    (dir, store)
}

fn sample_capsule(id: &str, tenant: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: tenant.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Global,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        content_hash: format!("hash-{id}"),
        confidence: 0.5,
        decay_score: 0.0,
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn syncs_capsules_lance_to_lance() {
    let (_sd, src) = temp_lance().await;
    let (_td, dst) = temp_lance().await;

    src.insert_capability_capsules(&[sample_capsule("c1", "local"), sample_capsule("c2", "local")])
        .await
        .unwrap();

    let report = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    assert_eq!(report.copied, 2);

    let ids = dst
        .list_capability_capsule_ids_for_tenant("local")
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"c1".to_string()));

    // Verbatim-content round-trip: fetching c1 from dst preserves content_hash.
    let rows = mem::storage::CapsuleStore::fetch_capability_capsules_by_ids(&dst, "local", &["c1"])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content_hash, "hash-c1");

    // Re-run is idempotent.
    let again = mem::cli::sync::copy_capsules_for_test(&src, &dst, "local", 100).await;
    assert_eq!(again.copied, 0);
    assert_eq!(again.skipped, 2);
}
