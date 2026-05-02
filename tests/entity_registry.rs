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

// ---------------------------------------------------------------------------
// Ingest → resolve → graph_edges integration (Task 8 + Task 9).
//
// `resolve_drafts_to_edges` is defined in `service::memory_service` (Task 8)
// and wired into `MemoryService::ingest` by Task 9. These exercise the full
// HTTP flow and assert that `graph_edges.to_node_id` carries `entity:<uuid>`
// (resolved via EntityRegistry), not the legacy `topic:<alias>` literal.
// ---------------------------------------------------------------------------

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::util::ServiceExt;

#[tokio::test]
async fn ingest_memory_with_topics_creates_entity_refs_in_graph_edges() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg.clone()).await.unwrap();

    let body = json!({
        "tenant": "local",
        "memory_type": "implementation",
        "content": "Rust borrow checker discussion",
        "scope": "global",
        "source_agent": "test",
        "topics": ["Rust", "ownership"],
        "write_mode": "auto"
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/memories")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify graph_edges has 2 'discusses' edges pointing to entity:<uuid>.
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "select count(*) from graph_edges \
             where relation = 'discusses' and to_node_id like 'entity:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);

    // Verify entities table has 2 entities of kind='topic'.
    let entity_count: i64 = conn
        .query_row(
            "select count(*) from entities where kind='topic' and tenant='local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(entity_count, 2);
}

#[tokio::test]
async fn ingest_with_existing_alias_reuses_entity_id() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg.clone()).await.unwrap();

    async fn post(body: serde_json::Value, app: &axum::Router) -> axum::http::Response<Body> {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/memories")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    let r1 = post(
        json!({
            "tenant": "local",
            "memory_type": "implementation",
            "content": "first mention",
            "scope": "global",
            "source_agent": "test",
            "topics": ["Rust"],
            "write_mode": "auto"
        }),
        &app,
    )
    .await;
    assert_eq!(r1.status(), StatusCode::CREATED);

    let r2 = post(
        json!({
            "tenant": "local",
            "memory_type": "implementation",
            "content": "second mention with case variation",
            "scope": "global",
            "source_agent": "test",
            "topics": ["rust"],
            "write_mode": "auto"
        }),
        &app,
    )
    .await;
    assert_eq!(r2.status(), StatusCode::CREATED);

    // Both memories' 'discusses' edges should point to the SAME entity_id.
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let entity_count: i64 = conn
        .query_row(
            "select count(*) from entities where kind='topic' and tenant='local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        entity_count, 1,
        "case-insensitive alias should not create duplicate entity"
    );

    let edge_count: i64 = conn
        .query_row(
            "select count(*) from graph_edges where relation='discusses'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(edge_count, 2);

    // Both edges should have the same to_node_id (same entity_id).
    let distinct_targets: i64 = conn
        .query_row(
            "select count(distinct to_node_id) from graph_edges where relation='discusses'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct_targets, 1);
}

// ---------------------------------------------------------------------------
// HTTP routes (Task 10): POST/GET /entities, POST /entities/{id}/aliases.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_entities_creates_with_aliases() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg).await.unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/entities")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "local",
                        "canonical_name": "Rust",
                        "kind": "topic",
                        "aliases": ["Rust language", "rustlang"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["entity"]["canonical_name"], "Rust");
    let aliases = v["aliases"].as_array().unwrap();
    assert!(aliases.iter().any(|a| a.as_str() == Some("rust language")));
    assert!(aliases.iter().any(|a| a.as_str() == Some("rustlang")));
}

#[tokio::test]
async fn get_entity_returns_canonical_with_aliases() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg).await.unwrap();

    // Create.
    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/entities")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "local",
                        "canonical_name": "Rust",
                        "kind": "topic"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = v["entity"]["entity_id"].as_str().unwrap().to_string();

    // Get.
    let get = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/entities/{id}?tenant=local"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(get.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["entity"]["entity_id"], id);
}

#[tokio::test]
async fn post_entity_aliases_idempotent_and_409_on_conflict() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg).await.unwrap();

    // Helper: POST /entities, return entity_id.
    async fn create_one(canonical: &str, app: axum::Router) -> String {
        let body = json!({
            "tenant": "local",
            "canonical_name": canonical,
            "kind": "topic"
        })
        .to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/entities")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["entity"]["entity_id"].as_str().unwrap().to_string()
    }

    let rust_id = create_one("Rust", app.clone()).await;
    let py_id = create_one("Python", app.clone()).await;

    // First add — Inserted (200).
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/entities/{rust_id}/aliases"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"tenant": "local", "alias": "Rust language"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);

    // Idempotent re-add — same outcome 200.
    let r2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/entities/{rust_id}/aliases"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"tenant": "local", "alias": "Rust language"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    // Conflict — alias 'rust' already on rust_id; trying on py_id → 409.
    let r3 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/entities/{py_id}/aliases"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"tenant": "local", "alias": "Rust"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn post_entities_returns_409_on_cross_entity_alias_conflict() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg).await.unwrap();

    // 1. Create entity1 with alias "rustlang".
    let r1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/entities")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "local",
                        "canonical_name": "Rust",
                        "kind": "topic",
                        "aliases": ["rustlang"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(r1.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let entity1_id = v["entity"]["entity_id"].as_str().unwrap().to_string();

    // 2. Create entity2 with same alias "rustlang" (under a *different*
    //    canonical_name so the would-be target differs from the alias's
    //    existing owner).
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/entities")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "tenant": "local",
                        "canonical_name": "Python",
                        "kind": "topic",
                        "aliases": ["rustlang"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    // 3. Assert second response is 409 with body referencing entity1.
    assert_eq!(r2.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(r2.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["existing_entity_id"], entity1_id);
    assert_eq!(v["conflicting_alias"], "rustlang");
}

#[tokio::test]
async fn get_entities_list_filters_kind_query_and_clamps_limit() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::app::router_with_config(cfg).await.unwrap();

    // Helper: POST /entities and ignore body.
    async fn create_one(canonical: &str, kind: &str, app: axum::Router) {
        let body = json!({
            "tenant": "local",
            "canonical_name": canonical,
            "kind": kind,
        })
        .to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/entities")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Insert in order: Rust (topic), Python (topic), mem (project), gizmo (project).
    create_one("Rust", "topic", app.clone()).await;
    create_one("Python", "topic", app.clone()).await;
    create_one("mem", "project", app.clone()).await;
    create_one("gizmo", "project", app.clone()).await;

    // Helper: GET /entities and parse the entities array.
    async fn list(uri: &str, app: axum::Router) -> Vec<serde_json::Value> {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["entities"].as_array().cloned().unwrap()
    }

    // kind=topic filter.
    let topics = list("/entities?tenant=local&kind=topic", app.clone()).await;
    assert_eq!(topics.len(), 2);
    for t in &topics {
        assert_eq!(t["kind"], "topic");
    }

    // q= substring filter.
    let just_rust = list("/entities?tenant=local&q=Rust", app.clone()).await;
    assert_eq!(just_rust.len(), 1);
    assert_eq!(just_rust[0]["canonical_name"], "Rust");

    // limit=200 should silently clamp to ≤ 100 per spec.
    let clamped = list("/entities?tenant=local&limit=200", app.clone()).await;
    assert!(
        clamped.len() <= 100,
        "limit=200 should clamp to ≤ 100, got {}",
        clamped.len()
    );

    // Default ordering: created_at desc — gizmo (last inserted) comes first.
    let all = list("/entities?tenant=local", app.clone()).await;
    assert_eq!(all.len(), 4);
    assert_eq!(all[0]["canonical_name"], "gizmo");
}
