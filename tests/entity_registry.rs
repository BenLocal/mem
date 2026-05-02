//! Integration tests for the entity registry (closes ROADMAP #8). See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.
//!
//! ### Composite-PK ON CONFLICT probe outcome (Task 1, 2026-05-02)
//! `INSERT … ON CONFLICT (tenant, alias_text) DO NOTHING` is **SUPPORTED** by
//! the bundled DuckDB version. Task 5's `add_alias` uses this idiom for the
//! "alias already exists, idempotent re-add" case.
//! Re-run the probe (`#[ignore]`'d below) on DuckDB upgrades.

#[test]
#[ignore]
fn composite_pk_on_conflict_probe() {
    // Run: cargo test --test entity_registry composite_pk_on_conflict_probe -- --ignored --nocapture
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("probe.duckdb");
    let conn = duckdb::Connection::open(&db).unwrap();
    conn.execute_batch(
        "create table t (tenant text not null, alias text not null, payload text not null, primary key (tenant, alias));"
    ).unwrap();
    conn.execute_batch("insert into t values ('local', 'rust', 'first');")
        .unwrap();

    let result = conn.execute_batch(
        "insert into t (tenant, alias, payload) values ('local', 'rust', 'second') on conflict (tenant, alias) do nothing;"
    );

    match result {
        Ok(_) => {
            // Verify the original row is preserved.
            let payload: String = conn
                .query_row(
                    "select payload from t where tenant='local' and alias='rust'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                payload, "first",
                "ON CONFLICT DO NOTHING should preserve original"
            );
            println!("Composite-PK ON CONFLICT DO NOTHING SUPPORTED — Task 5 add_alias can use it");
        }
        Err(e) => {
            println!("Composite-PK ON CONFLICT NOT SUPPORTED: {e}");
            println!("Task 5 add_alias must use SELECT-then-INSERT under a single mutex hold");
        }
    }
}

use mem::storage::DuckDbRepository;
use tempfile::TempDir;

#[tokio::test]
async fn schema_creates_entities_aliases_and_topics_column() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let _repo = DuckDbRepository::open(&db).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();

    let entities_count: i64 = conn
        .query_row(
            "select count(*) from information_schema.tables where table_name='entities'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(entities_count, 1, "entities table should exist");

    let aliases_count: i64 = conn
        .query_row(
            "select count(*) from information_schema.tables where table_name='entity_aliases'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(aliases_count, 1, "entity_aliases table should exist");

    // memories.topics column added by the ALTER in 008.
    let topics_col: i64 = conn
        .query_row(
            "select count(*) from information_schema.columns where table_name='memories' and column_name='topics'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(topics_col, 1, "memories.topics column should exist");

    // CHECK constraint on entities.kind: invalid kind rejected.
    let bad = conn.execute(
        "insert into entities (entity_id, tenant, canonical_name, kind, created_at) \
         values ('e1', 't', 'X', 'bogus', '00000000020260502000')",
        [],
    );
    assert!(bad.is_err(), "kind='bogus' should violate CHECK constraint");
}

#[tokio::test]
async fn schema_bootstrap_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let _repo1 = DuckDbRepository::open(&db).await.unwrap();
    drop(_repo1);
    let _repo2 = DuckDbRepository::open(&db).await.unwrap();
    // No panic: re-opening must not fail on duplicate ALTER.
}

use mem::domain::memory::{MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility};
use mem::domain::{AddAliasOutcome, EntityKind};
use mem::storage::EntityRegistry;

const NOW: &str = "00000000020260502000";

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
        evidence: vec![],
        code_refs: vec![],
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        confidence: 0.5,
        decay_score: 0.0,
        content_hash: "00".repeat(32),
        idempotency_key: None,
        session_id: None,
        supersedes_memory_id: None,
        source_agent: "test".to_string(),
        created_at: NOW.to_string(),
        updated_at: NOW.to_string(),
        last_validated_at: None,
    }
}

#[tokio::test]
async fn memory_record_topics_round_trip() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let memory = MemoryRecord {
        memory_id: "mem-test".to_string(),
        topics: vec!["Rust".into(), "ownership".into()],
        content_hash: "deadbeef".repeat(8), // 64 chars
        content: "discussion of language ownership".to_string(),
        summary: "ownership notes".to_string(),
        ..baseline_memory("mem-test")
    };
    repo.insert_memory(memory).await.unwrap();

    let fetched = repo
        .get_memory_for_tenant("local", "mem-test")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fetched.topics,
        vec!["Rust".to_string(), "ownership".to_string()]
    );
}

#[tokio::test]
async fn memory_record_empty_topics_round_trips_as_empty_vec() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let memory = MemoryRecord {
        memory_id: "mem-empty".to_string(),
        topics: vec![],
        ..baseline_memory("mem-empty")
    };
    repo.insert_memory(memory).await.unwrap();

    let fetched = repo
        .get_memory_for_tenant("local", "mem-empty")
        .await
        .unwrap()
        .unwrap();
    assert!(fetched.topics.is_empty());
}

#[tokio::test]
async fn memory_record_null_topics_in_legacy_row_decodes_as_empty_vec() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let memory = baseline_memory("mem-null-topics");
    repo.insert_memory(memory).await.unwrap();

    // Simulate a legacy row written before the topics column was added by
    // forcing topics to NULL via raw SQL.
    {
        let conn = duckdb::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE memories SET topics = NULL WHERE memory_id = ?1",
            duckdb::params!["mem-null-topics"],
        )
        .unwrap();
    }

    let fetched = repo
        .get_memory_for_tenant("local", "mem-null-topics")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fetched.topics,
        Vec::<String>::new(),
        "NULL topics in a legacy row must deserialize as empty Vec, not an error"
    );
}

#[tokio::test]
async fn resolve_or_create_inserts_entity_and_alias_on_first_call() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let id = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    assert!(!id.is_empty());

    // entity row + alias row both present.
    let conn = duckdb::Connection::open(&db).unwrap();
    let entity_count: i64 = conn
        .query_row(
            "select count(*) from entities where entity_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(entity_count, 1);

    let alias_count: i64 = conn
        .query_row(
            "select count(*) from entity_aliases where tenant='local' and alias_text='rust'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(alias_count, 1);

    // canonical_name preserves caller's verbatim input.
    let canonical: String = conn
        .query_row(
            "select canonical_name from entities where entity_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(canonical, "Rust");
}

#[tokio::test]
async fn resolve_or_create_is_idempotent_on_alias_hit() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let id1 = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let id2 = repo
        .resolve_or_create("local", "rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let id3 = repo
        .resolve_or_create("local", "  RUST  ", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let id4 = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();

    assert_eq!(id1, id2);
    assert_eq!(id1, id3);
    assert_eq!(id1, id4);

    let conn = duckdb::Connection::open(&db).unwrap();
    let entity_count: i64 = conn
        .query_row("select count(*) from entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(entity_count, 1, "no duplicate entities created");
}

#[tokio::test]
async fn resolve_or_create_creates_separate_entities_for_distinct_aliases() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let lang = repo
        .resolve_or_create("local", "Rust language", EntityKind::Topic, NOW)
        .await
        .unwrap();
    assert_ne!(rust, lang, "caller did not declare these as synonyms");
}

#[tokio::test]
async fn add_alias_links_to_existing_entity() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust_id = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let outcome = repo
        .add_alias("local", &rust_id, "Rust language", NOW)
        .await
        .unwrap();
    assert_eq!(outcome, AddAliasOutcome::Inserted);

    // After add_alias, resolving "rust language" hits the existing rust_id.
    let lang_resolved = repo
        .resolve_or_create("local", "rust language", EntityKind::Topic, NOW)
        .await
        .unwrap();
    assert_eq!(lang_resolved, rust_id);
}

#[tokio::test]
async fn add_alias_returns_already_on_same_entity_when_idempotent() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust_id = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    repo.add_alias("local", &rust_id, "rustlang", NOW)
        .await
        .unwrap();
    let outcome = repo
        .add_alias("local", &rust_id, "rustlang", NOW)
        .await
        .unwrap();
    assert_eq!(outcome, AddAliasOutcome::AlreadyOnSameEntity);
}

#[tokio::test]
async fn add_alias_returns_conflict_when_alias_belongs_to_different_entity() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust_id = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let py_id = repo
        .resolve_or_create("local", "Python", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let outcome = repo.add_alias("local", &py_id, "rust", NOW).await.unwrap();
    assert_eq!(
        outcome,
        AddAliasOutcome::ConflictWithDifferentEntity(rust_id)
    );
}

#[tokio::test]
async fn tenant_isolation_distinct_registries() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let a = repo
        .resolve_or_create("alice", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    let b = repo
        .resolve_or_create("bob", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    assert_ne!(a, b, "different tenants must produce different entities");
}

#[tokio::test]
async fn list_entities_filters_by_kind_and_query() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW)
        .await
        .unwrap();
    repo.resolve_or_create("local", "Python", EntityKind::Topic, NOW)
        .await
        .unwrap();
    repo.resolve_or_create("local", "mem", EntityKind::Project, NOW)
        .await
        .unwrap();

    let topics = repo
        .list_entities("local", Some(EntityKind::Topic), None, 100)
        .await
        .unwrap();
    assert_eq!(topics.len(), 2);

    let rust_only = repo
        .list_entities("local", None, Some("Rust"), 100)
        .await
        .unwrap();
    assert_eq!(rust_only.len(), 1);
    assert_eq!(rust_only[0].canonical_name, "Rust");
}

#[tokio::test]
async fn get_entity_returns_canonical_with_aliases_in_creation_order() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let id = repo
        .resolve_or_create("local", "Rust", EntityKind::Topic, "00000000020260502000")
        .await
        .unwrap();
    repo.add_alias("local", &id, "Rust language", "00000000020260502001")
        .await
        .unwrap();
    repo.add_alias("local", &id, "rustlang", "00000000020260502002")
        .await
        .unwrap();

    let with_aliases = repo.get_entity("local", &id).await.unwrap().unwrap();
    assert_eq!(with_aliases.entity.canonical_name, "Rust");
    assert_eq!(
        with_aliases.aliases,
        vec!["rust", "rust language", "rustlang"]
    );
}
