//! Regression: `accept_pending` / `reject_pending` on a long-lived prod
//! db (created before 2026-05-17 / commit 45c65f4) blew up with
//! `column type mismatch` because the `version` column is `UInt64` in
//! pre-flip dbs but the parser hard-coded `Int64Array`. The lenient
//! `parse_version_column` reader fixes it.
//!
//! Two suites:
//! 1. Fresh-db smoke: ensures the post-fix code still handles `Int64`
//!    (current-schema) tables. accept + reject round-trip.
//! 2. Drift simulation: builds a `RecordBatch` with a `UInt64` `version`
//!    column and feeds it through `record_batch_to_capability_capsules`
//!    directly — proves the lenient reader unblocks the prod scenario
//!    without needing a real legacy db file to be present.

use std::sync::Arc;

use arrow_array::{
    builder::{Float32Builder, ListBuilder, StringBuilder, UInt64Builder},
    Array, RecordBatch,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};
use mem::{
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::{CapsuleStore, Store},
};
use tempfile::tempdir;

fn pending(id: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "local".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::PendingConfirmation,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary-{id}"),
        content: "content".into(),
        evidence: vec![],
        code_refs: vec![],
        project: Some("repro".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.6,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test".into(),
        created_at: "00000000000000000001".into(),
        updated_at: "00000000000000000001".into(),
        last_validated_at: None,
        last_used_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_db_accept_pending_round_trip() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("repro.lance")).await.unwrap());
    store
        .insert_capability_capsule(pending("repro_accept"))
        .await
        .unwrap();
    let accepted = store
        .accept_pending("local", "repro_accept")
        .await
        .expect("accept must not fail on current-schema db");
    assert_eq!(accepted.status, CapabilityCapsuleStatus::Active);
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_db_reject_pending_round_trip() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("repro.lance")).await.unwrap());
    store
        .insert_capability_capsule(pending("repro_reject"))
        .await
        .unwrap();
    let rejected = store
        .reject_pending("local", "repro_reject")
        .await
        .expect("reject must not fail on current-schema db");
    assert_eq!(rejected.status, CapabilityCapsuleStatus::Rejected);
}

/// Build a `RecordBatch` whose `version` column is `UInt64` (the
/// pre-45c65f4 layout) and confirm the Lance parser still returns a
/// `CapabilityCapsuleRecord` with the expected `i64` version. Before the
/// lenient reader landed, this batch made the parser return `column
/// type mismatch` and the HTTP layer return 500.
///
/// We exercise the parser by writing this batch into a fresh Lance table
/// (the table's auto-inferred schema picks up `UInt64`) and then reading
/// it back through `Store::get_capability_capsule_for_tenant` — i.e.
/// the same code path `accept_pending` runs after the UPDATE.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_uint64_version_column_round_trips_via_lance_parser() {
    use lancedb::{connect, query::ExecutableQuery, query::QueryBase};

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("legacy.lance");
    let conn = connect(db_path.to_str().unwrap()).execute().await.unwrap();

    // Schema mirrors pre-45c65f4: `version: UInt64`, everything else
    // identical to current `capability_capsules_schema()`.
    let str_list = DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)));
    let legacy_schema = Arc::new(Schema::new(vec![
        Field::new("capability_capsule_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("capability_capsule_type", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("scope", DataType::Utf8, false),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("version", DataType::UInt64, false), // ← the drift
        Field::new("summary", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("evidence", str_list.clone(), false),
        Field::new("code_refs", str_list.clone(), false),
        Field::new("project", DataType::Utf8, true),
        Field::new("repo", DataType::Utf8, true),
        Field::new("module", DataType::Utf8, true),
        Field::new("task_type", DataType::Utf8, true),
        Field::new("tags", str_list.clone(), false),
        Field::new("topics", str_list, false),
        Field::new("confidence", DataType::Float32, false),
        Field::new("decay_score", DataType::Float32, false),
        Field::new("content_hash", DataType::Utf8, false),
        Field::new("idempotency_key", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("supersedes_capability_capsule_id", DataType::Utf8, true),
        Field::new("source_agent", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
        Field::new("last_validated_at", DataType::Utf8, true),
    ]));

    let mut id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut typ = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut scope = StringBuilder::new();
    let mut visibility = StringBuilder::new();
    let mut version = UInt64Builder::new();
    let mut summary = StringBuilder::new();
    let mut content = StringBuilder::new();
    let mut evidence = ListBuilder::new(StringBuilder::new());
    let mut code_refs = ListBuilder::new(StringBuilder::new());
    let mut project = StringBuilder::new();
    let mut repo = StringBuilder::new();
    let mut module = StringBuilder::new();
    let mut task_type = StringBuilder::new();
    let mut tags = ListBuilder::new(StringBuilder::new());
    let mut topics = ListBuilder::new(StringBuilder::new());
    let mut confidence = Float32Builder::new();
    let mut decay_score = Float32Builder::new();
    let mut content_hash = StringBuilder::new();
    let mut idempotency_key = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut supersedes = StringBuilder::new();
    let mut source_agent = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    let mut last_validated_at = StringBuilder::new();

    id.append_value("legacy_001");
    tenant.append_value("local");
    typ.append_value("experience");
    status.append_value("active");
    scope.append_value("repo");
    visibility.append_value("shared");
    version.append_value(7u64); // ← stored as u64, parser must coerce to i64=7
    summary.append_value("legacy summary");
    content.append_value("legacy content");
    evidence.append(true);
    code_refs.append(true);
    project.append_value("repro");
    repo.append_value("mem");
    module.append_null();
    task_type.append_null();
    tags.append(true);
    topics.append(true);
    confidence.append_value(0.9);
    decay_score.append_value(0.0);
    content_hash.append_value("hash-legacy");
    idempotency_key.append_null();
    session_id.append_null();
    supersedes.append_null();
    source_agent.append_value("test");
    created_at.append_value("00000000000000000001");
    updated_at.append_value("00000000000000000001");
    last_validated_at.append_null();

    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(typ.finish()),
        Arc::new(status.finish()),
        Arc::new(scope.finish()),
        Arc::new(visibility.finish()),
        Arc::new(version.finish()),
        Arc::new(summary.finish()),
        Arc::new(content.finish()),
        Arc::new(evidence.finish()),
        Arc::new(code_refs.finish()),
        Arc::new(project.finish()),
        Arc::new(repo.finish()),
        Arc::new(module.finish()),
        Arc::new(task_type.finish()),
        Arc::new(tags.finish()),
        Arc::new(topics.finish()),
        Arc::new(confidence.finish()),
        Arc::new(decay_score.finish()),
        Arc::new(content_hash.finish()),
        Arc::new(idempotency_key.finish()),
        Arc::new(session_id.finish()),
        Arc::new(supersedes.finish()),
        Arc::new(source_agent.finish()),
        Arc::new(created_at.finish()),
        Arc::new(updated_at.finish()),
        Arc::new(last_validated_at.finish()),
    ];
    let batch = RecordBatch::try_new(legacy_schema.clone(), columns).unwrap();

    // Create the table with the legacy schema + insert the row.
    let table = conn
        .create_table("capability_capsules", batch.clone())
        .execute()
        .await
        .unwrap();

    // Read it back via the Lance API directly — same path
    // `update_status`'s post-write read uses.
    let stream = table
        .query()
        .only_if("capability_capsule_id = 'legacy_001'")
        .limit(1)
        .execute()
        .await
        .unwrap();
    use futures::TryStreamExt;
    let batches: Vec<RecordBatch> = stream.try_collect().await.unwrap();
    assert_eq!(batches.len(), 1);

    // Confirm column is genuinely UInt64 (not silently coerced on insert).
    assert_eq!(
        batches[0]
            .schema()
            .field_with_name("version")
            .unwrap()
            .data_type(),
        &DataType::UInt64,
        "test setup must keep the column as UInt64 to exercise the legacy path",
    );

    // The parser is `pub(super)` (in-crate only), so we exercise it
    // indirectly via the equivalent end-to-end path the production
    // accept_pending flow uses. Open a Store rooted at this directory
    // and read the row; the parser must not return "column type
    // mismatch".
    drop(table);
    drop(conn);
    let store = Arc::new(Store::open(&db_path).await.unwrap());
    // Reads route through DuckDbQuery which coerces UInt64 silently,
    // so this check is mostly a smoke test that the file is intact.
    // The real signal is the lance-side parse below.
    let from_duckdb = store
        .get_capability_capsule_for_tenant("local", "legacy_001")
        .await
        .unwrap()
        .expect("row must be visible to DuckDB read path");
    assert_eq!(from_duckdb.version, 7);

    // The lance-side parser (the one update_status uses) lives behind
    // `Store::get_capability_capsule` which routes to LanceStore on
    // cross-tenant lookups. That's the path that was failing on prod.
    let from_lance = store
        .get_capability_capsule("legacy_001".to_string())
        .await
        .expect("lance read must not blow up on UInt64 version column")
        .expect("row must be visible to Lance read path");
    assert_eq!(from_lance.version, 7);
}
