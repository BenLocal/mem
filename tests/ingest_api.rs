#[path = "../src/domain/mod.rs"]
mod domain;

use domain::memory::{IngestMemoryRequest, MemoryType, Scope, WriteMode};

#[test]
fn ingest_request_serializes_expected_shape() {
    let request = IngestMemoryRequest {
        memory_type: MemoryType::Implementation,
        content: "cache invalidation rule".into(),
        scope: Scope::Repo,
        write_mode: WriteMode::Auto,
        ..Default::default()
    };

    let value = serde_json::to_value(request).unwrap();

    assert_eq!(value["scope"], "repo");
    assert_eq!(value["write_mode"], "auto");
    assert_eq!(value["memory_type"], "implementation");
    assert!(value.get("project").is_none());
    assert!(value.get("idempotency_key").is_none());
}
