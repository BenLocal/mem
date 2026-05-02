# Entity Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tenant-scoped entity registry that canonicalizes alias strings (`'Rust'` = `'Rust language'` = `'rustlang'`) to a stable `entity_id` (UUIDv7), so graph edges and memory references converge to the same node across sessions.

**Architecture:** Two new tables (`entities` + `entity_aliases`); `MemoryRecord` gains a verbatim caller-supplied `topics: Vec<String>` field; `extract_graph_edges` splits into a pure draft-extraction step + an async resolution step that runs through an `EntityRegistry` trait; existing `graph_edges` rows migrate via a new `mem repair --rebuild-graph` CLI subcommand that re-derives from `memories` using the same production code path. Layering: storage stays verbatim (`canonical_name`), indexing does the normalization (`alias_text` PK is lowercase + whitespace-collapsed), sessions remain orthogonal.

**Tech Stack:** Rust 2021, DuckDB (bundled, single file), `Arc<Mutex<Connection>>` single-writer pattern, `uuid::Uuid::now_v7()` for IDs, axum HTTP, integration tests in `tests/` against ephemeral DuckDB.

**Spec:** `docs/superpowers/specs/2026-05-02-entity-registry-design.md` (commit `8492df7`).

---

## Conventions referenced throughout

- **Append-only schema files**: never edit `001_init.sql` through `007`; new schema goes in `008_entity_registry.sql`.
- **Single-writer DB**: writes serialize through `Arc<Mutex<Connection>>`. Operations that span multiple INSERTs (resolve_or_create, add_alias, etc.) MUST hold one mutex acquisition for both.
- **Verbatim**: `entities.canonical_name` is caller's original spelling; never trim/lowercase/modify. `alias_text` is the normalized PK form (lowercase + whitespace-collapsed).
- **Tenant scoping**: every new table has `tenant TEXT NOT NULL`; composite PK on `entity_aliases` is `(tenant, alias_text)`.
- **Test pattern**: integration tests use `tempfile::TempDir` for ephemeral DuckDB (see `tests/conversation_archive.rs` / `tests/transcript_recall.rs` for the established harness).
- **HTTP error mapping**: use `AppError` for storage errors (per the conversation-archive final-review lesson). Business 404 / 409 are explicit handler-level status returns.
- **Commit scope tags**: `feat(entity)`, `feat(graph)`, `refactor(ingest)`, `test(entity)`, etc.

---

## File Structure (locked decisions)

**Created:**
- `db/schema/008_entity_registry.sql` — two tables + `ALTER TABLE memories ADD COLUMN topics text`
- `src/domain/entity.rs` — `Entity`, `EntityKind`, `EntityWithAliases`, `AddAliasOutcome`
- `src/pipeline/entity_normalize.rs` — pure `normalize_alias(s: &str) -> String` + 7 unit tests
- `src/storage/entity_repo.rs` — `EntityRegistry` trait impl on `DuckDbRepository`
- `src/service/entity_service.rs` — façade for HTTP layer
- `src/http/entities.rs` — 4 HTTP routes
- `tests/entity_registry.rs` — ~15 integration tests

**Modified:**
- `src/storage/duckdb.rs` — declare `pub trait EntityRegistry` (definition only)
- `src/storage/mod.rs` — register module + re-export
- `src/storage/schema.rs` — register `008`, add `apply_entity_registry_schema()` mirroring `apply_sessions_schema()`
- `src/domain/memory.rs` — add `topics: Vec<String>` to `MemoryRecord` and (in `src/http/memory.rs`) to `IngestMemoryRequest`
- `src/domain/mod.rs` — re-export entity types
- `src/pipeline/ingest.rs` — split `extract_graph_edges` → `extract_graph_edge_drafts` (pure, new) + `extract_graph_edges` (deprecated wrapper, BC for `graph_store.rs`)
- `src/pipeline/mod.rs` — register `entity_normalize`
- `src/service/memory_service.rs` — `ingest` resolves drafts via registry before persisting graph edges
- `src/service/mod.rs` — register `entity_service`
- `src/storage/graph_store.rs::sync_memory` — call the new pipeline (drafts + registry resolution) instead of legacy `extract_graph_edges`
- `src/http/mod.rs` — merge `entities::router()`
- `src/http/memory.rs` — add `IngestMemoryRequest.topics`
- `src/cli/repair.rs` — add `--rebuild-graph` flag and subcommand
- `src/app.rs` — construct `EntityService`, attach to `AppState` if HTTP needs it
- `tests/repair_cli.rs` — 3 migration tests for `--rebuild-graph`
- `README.md` + `AGENTS.md` — document the new entity surface

**Untouched (verify in self-review):**
- `db/schema/001`–`007` (append-only)
- `src/storage/transcript_repo.rs`, `src/service/transcript_service.rs`, `src/http/transcripts.rs` (transcripts pipeline unchanged)
- `src/pipeline/{retrieve,ranking,transcript_recall,compress,workflow,session}.rs` (graph extraction is the only pipeline file that changes)
- `src/mcp/server.rs` — MCP surface intentionally unchanged

---

## Task 1: Probe DuckDB composite-PK ON CONFLICT support

**Files:**
- Test: `tests/entity_registry.rs` (new file; only the probe at this point)

The spec's Concerns to Confirm #1 flags that `INSERT … ON CONFLICT (tenant, alias_text) DO NOTHING` on a composite PK needs probing. This task is a 5-minute experiment that decides Task 5's `add_alias` SQL shape. Same pattern as `transcript_recall::fts_predicate_probe`.

- [ ] **Step 1: Create `tests/entity_registry.rs` with the probe**

```rust
//! Integration tests for the entity registry (closes ROADMAP #8). See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.
//!
//! ### Composite-PK ON CONFLICT probe (Task 1, 2026-05-02)
//! The probe below determines whether `INSERT … ON CONFLICT (a, b) DO NOTHING`
//! on a composite PK is supported by the bundled DuckDB version. Outcome is
//! documented as a comment ABOVE the probe after Task 1 runs.

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
    conn.execute_batch("insert into t values ('local', 'rust', 'first');").unwrap();

    let result = conn.execute_batch(
        "insert into t (tenant, alias, payload) values ('local', 'rust', 'second') on conflict (tenant, alias) do nothing;"
    );

    match result {
        Ok(_) => {
            // Verify the original row is preserved.
            let payload: String = conn
                .query_row("select payload from t where tenant='local' and alias='rust'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(payload, "first", "ON CONFLICT DO NOTHING should preserve original");
            println!("Composite-PK ON CONFLICT DO NOTHING SUPPORTED — Task 5 add_alias can use it");
        }
        Err(e) => {
            println!("Composite-PK ON CONFLICT NOT SUPPORTED: {e}");
            println!("Task 5 add_alias must use SELECT-then-INSERT under a single mutex hold");
        }
    }
}
```

- [ ] **Step 2: Run the probe**

```bash
cargo test --test entity_registry composite_pk_on_conflict_probe -- --ignored --nocapture
```

Expected output (one of):
- `Composite-PK ON CONFLICT DO NOTHING SUPPORTED`
- `Composite-PK ON CONFLICT NOT SUPPORTED: <error>`

The probe asserts only the post-conflict invariant (original preserved); it never fails the `ON CONFLICT NOT SUPPORTED` path — that's the informational outcome.

- [ ] **Step 3: Document outcome at top of `tests/entity_registry.rs`**

Update the docstring at the top to reflect the actual result:

```rust
//! ### Composite-PK ON CONFLICT probe outcome (Task 1, 2026-05-02)
//! `INSERT … ON CONFLICT (tenant, alias_text) DO NOTHING` is **SUPPORTED** by
//! the bundled DuckDB version. Task 5's `add_alias` uses this idiom for the
//! "alias already exists, idempotent re-add" case.
//! Re-run the probe (`#[ignore]`'d below) on DuckDB upgrades.
```

(Or the `NOT SUPPORTED` variant with the actual error message — copy verbatim from the probe printout.)

- [ ] **Step 4: Verify it compiles**

```bash
cargo test --test entity_registry -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: 0 active tests run (probe is `#[ignore]`'d), build clean.

- [ ] **Step 5: Commit**

```bash
git add tests/entity_registry.rs
git commit -m "test(entity): probe DuckDB composite-PK ON CONFLICT support

Determines which SQL shape Task 5's add_alias takes: ON CONFLICT
DO NOTHING vs SELECT-then-INSERT under single mutex hold. Outcome
documented in the file's top doc-comment."
```

---

## Task 2: Schema migration `008_entity_registry.sql`

**Files:**
- Create: `db/schema/008_entity_registry.sql`
- Modify: `src/storage/schema.rs`
- Test: `tests/entity_registry.rs` (append schema test)

- [ ] **Step 1: Write the failing schema test**

Append to `tests/entity_registry.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test entity_registry schema_ -q
```

Expected: FAIL with "Table … does not exist" or similar.

- [ ] **Step 3: Create `db/schema/008_entity_registry.sql`**

```sql
-- Entity registry: canonicalize alias strings to stable entity_id.
-- See docs/superpowers/specs/2026-05-02-entity-registry-design.md.
--
-- Two tables: entities (canonical record) + entity_aliases (lookup).
-- alias_text is the normalized form (lowercase + whitespace-collapsed); see
-- normalize_alias() in src/pipeline/entity_normalize.rs. canonical_name is
-- the caller's verbatim original spelling.
--
-- Tenant-scoped (composite keys include tenant). Entities are NOT linked to
-- sessions: "Rust" written across multiple sessions resolves to the same
-- entity_id within a tenant.

create table if not exists entities (
    entity_id text primary key,
    tenant text not null,
    canonical_name text not null,
    kind text not null,
    created_at text not null,
    constraint entities_kind_check check (
        kind in ('topic', 'project', 'repo', 'module', 'workflow')
    )
);

create index if not exists idx_entities_tenant_kind
    on entities(tenant, kind);

create table if not exists entity_aliases (
    tenant text not null,
    alias_text text not null,
    entity_id text not null,
    created_at text not null,
    primary key (tenant, alias_text)
);

create index if not exists idx_entity_aliases_entity
    on entity_aliases(entity_id);

-- ALTER memories: caller-supplied verbatim topic strings (JSON-encoded
-- Vec<String>; NULL when omitted). Same storage shape as `evidence` field.
-- Note: DuckDB does not support `ADD COLUMN IF NOT EXISTS`. The schema
-- runner applies this file statement-by-statement and swallows the
-- "Column with name 'topics' already exists!" error on re-run, mirroring
-- the 004_sessions.sql ALTER handling.
alter table memories add column topics text;
```

- [ ] **Step 4: Wire the migration into `src/storage/schema.rs`**

Edit `src/storage/schema.rs`. Add the const + apply function + dispatch:

```rust
const ENTITY_REGISTRY_SCHEMA_SQL: &str =
    include_str!("../../db/schema/008_entity_registry.sql");
```

Add to `bootstrap()` after `CONVERSATION_MESSAGES_SCHEMA_SQL`:

```rust
pub fn bootstrap(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(INIT_SCHEMA_SQL)?;
    conn.execute_batch(EMBEDDINGS_SCHEMA_SQL)?;
    conn.execute_batch(GRAPH_SCHEMA_SQL)?;
    apply_sessions_schema(conn)?;
    conn.execute_batch(FTS_SCHEMA_SQL)?;
    conn.execute_batch(CONVERSATION_MESSAGES_SCHEMA_SQL)?;
    apply_entity_registry_schema(conn)?;          // NEW
    let migrated = migrate_content_hash_to_sha256(conn)?;
    if migrated > 0 {
        info!(
            count = migrated,
            "migrated legacy content_hash rows to sha256 (mempalace-diff §8 #1)"
        );
    }
    Ok(())
}

/// Apply `008_entity_registry.sql` statement-by-statement so that the
/// `ALTER TABLE memories ADD COLUMN topics` line is idempotent. Same
/// "swallow already-exists" handling as `apply_sessions_schema` for 004.
fn apply_entity_registry_schema(conn: &Connection) -> Result<(), StorageError> {
    for stmt in ENTITY_REGISTRY_SCHEMA_SQL
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Err(e) = conn.execute_batch(stmt) {
            let msg = e.to_string();
            if stmt.to_ascii_lowercase().contains("alter table")
                && msg.to_ascii_lowercase().contains("already exists")
            {
                continue;
            }
            return Err(StorageError::DuckDb(e));
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --test entity_registry schema_ -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: 2 schema tests pass, build clean.

- [ ] **Step 6: Commit**

```bash
git add db/schema/008_entity_registry.sql src/storage/schema.rs tests/entity_registry.rs
git commit -m "feat(entity): schema 008 — entities + entity_aliases tables

Adds the entity-registry schema and the memories.topics column. The
ALTER TABLE statement is applied via a split-and-swallow runner mirror-
ing apply_sessions_schema; re-bootstrap is idempotent.

CHECK constraint on entities.kind protects against typo writes (e.g.,
'projet' instead of 'project'). Composite PK on entity_aliases gives
O(1) PK lookup on (tenant, alias_text)."
```

---

## Task 3: Domain types — `Entity`, `EntityKind`, `EntityWithAliases`, `AddAliasOutcome`

**Files:**
- Create: `src/domain/entity.rs`
- Modify: `src/domain/mod.rs`

- [ ] **Step 1: Write the failing unit tests**

Create `src/domain/entity.rs` with the test module at the bottom (TDD; tests first, then types):

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entity {
    pub entity_id: String,
    pub tenant: String,
    pub canonical_name: String,
    pub kind: EntityKind,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Topic,
    Project,
    Repo,
    Module,
    Workflow,
}

impl EntityKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            EntityKind::Topic => "topic",
            EntityKind::Project => "project",
            EntityKind::Repo => "repo",
            EntityKind::Module => "module",
            EntityKind::Workflow => "workflow",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "topic" => Some(EntityKind::Topic),
            "project" => Some(EntityKind::Project),
            "repo" => Some(EntityKind::Repo),
            "module" => Some(EntityKind::Module),
            "workflow" => Some(EntityKind::Workflow),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityWithAliases {
    pub entity: Entity,
    /// Normalized alias forms, ordered by created_at ASC. The first-written
    /// alias (added by `resolve_or_create` itself) is at index 0.
    pub aliases: Vec<String>,
}

/// Result of `EntityRegistry::add_alias`. HTTP layer maps these to status
/// codes: Inserted/AlreadyOnSameEntity → 200, ConflictWithDifferentEntity → 409.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAliasOutcome {
    Inserted,
    AlreadyOnSameEntity,
    ConflictWithDifferentEntity(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_kind_round_trip_db_str() {
        for k in [
            EntityKind::Topic,
            EntityKind::Project,
            EntityKind::Repo,
            EntityKind::Module,
            EntityKind::Workflow,
        ] {
            assert_eq!(EntityKind::from_db_str(k.as_db_str()), Some(k));
        }
    }

    #[test]
    fn entity_kind_from_db_str_rejects_unknown() {
        assert_eq!(EntityKind::from_db_str(""), None);
        assert_eq!(EntityKind::from_db_str("Topic"), None); // case-sensitive
        assert_eq!(EntityKind::from_db_str("project "), None); // no trim
        assert_eq!(EntityKind::from_db_str("bogus"), None);
    }

    #[test]
    fn entity_kind_serializes_lowercase() {
        let k = EntityKind::Project;
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, "\"project\"");
    }

    #[test]
    fn add_alias_outcome_matches() {
        let inserted = AddAliasOutcome::Inserted;
        assert_eq!(inserted, AddAliasOutcome::Inserted);
        let conflict = AddAliasOutcome::ConflictWithDifferentEntity("e1".into());
        match conflict {
            AddAliasOutcome::ConflictWithDifferentEntity(id) => assert_eq!(id, "e1"),
            _ => panic!("variant mismatch"),
        }
    }
}
```

- [ ] **Step 2: Wire into `src/domain/mod.rs`**

Add `pub mod entity;` and re-export the public types:

```rust
pub mod entity;
pub use entity::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
```

(Match the existing re-export style and alphabetical ordering in the file.)

- [ ] **Step 3: Run the tests**

```bash
cargo test --lib domain::entity::tests -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: 4 tests pass, build clean.

- [ ] **Step 4: Commit**

```bash
git add src/domain/entity.rs src/domain/mod.rs
git commit -m "feat(entity): domain types Entity / EntityKind / AddAliasOutcome

Five-variant EntityKind matching the schema CHECK constraint
('topic'|'project'|'repo'|'module'|'workflow'). EntityWithAliases
returns aliases in created_at ASC order so the first-written alias
(produced by resolve_or_create) is always at index 0."
```

---

## Task 4: Pure `normalize_alias` function

**Files:**
- Create: `src/pipeline/entity_normalize.rs`
- Modify: `src/pipeline/mod.rs`

- [ ] **Step 1: Write the failing tests + implementation in one file**

Create `src/pipeline/entity_normalize.rs`:

```rust
//! Pure normalize_alias function shared by EntityRegistry and pipeline.
//!
//! Rules (per spec Q3 = C):
//! - Lowercase
//! - Trim leading/trailing whitespace
//! - Collapse internal runs of whitespace to a single space
//! - Preserve punctuation verbatim (C++/C#/.NET/F# keep their identity)
//! - Preserve Unicode verbatim (no NFKC; YAGNI for v1)

/// Normalize an alias string to its canonical lookup form.
///
/// `split_whitespace` is the all-in-one whitespace handler: it splits on
/// any Unicode whitespace, drops empties (collapsing runs), and yields
/// no items if the input is whitespace-only or empty (so the join produces
/// the empty string).
pub fn normalize_alias(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_case_and_whitespace() {
        assert_eq!(normalize_alias("Rust"), "rust");
        assert_eq!(normalize_alias("RUST"), "rust");
        assert_eq!(normalize_alias("  Rust  "), "rust");
        assert_eq!(normalize_alias("Rust  language"), "rust language");
        assert_eq!(normalize_alias("\t Rust\nLanguage \t"), "rust language");
    }

    #[test]
    fn preserves_ascii_punctuation() {
        assert_eq!(normalize_alias("C++"), "c++");
        assert_eq!(normalize_alias("C#"), "c#");
        assert_eq!(normalize_alias(".NET"), ".net");
        assert_eq!(normalize_alias("F#"), "f#");
        assert_eq!(normalize_alias("Node.js"), "node.js");
    }

    #[test]
    fn preserves_unicode_no_nfkc() {
        assert_eq!(normalize_alias("中文"), "中文");
        assert_eq!(normalize_alias("Naïve"), "naïve");
        // No NFKC: full-width chars stay full-width
        assert_eq!(normalize_alias("Ｒｕｓｔ"), "ｒｕｓｔ");
    }

    #[test]
    fn empty_and_whitespace_only() {
        assert_eq!(normalize_alias(""), "");
        assert_eq!(normalize_alias("   "), "");
        assert_eq!(normalize_alias("\t\n  \n\t"), "");
    }

    #[test]
    fn idempotent_on_already_normalized() {
        let inputs = ["rust", "rust language", "c++", "node.js"];
        for input in inputs {
            assert_eq!(normalize_alias(&normalize_alias(input)), normalize_alias(input));
        }
    }
}
```

- [ ] **Step 2: Wire into `src/pipeline/mod.rs`**

Add `pub mod entity_normalize;` to the existing module declarations (alphabetical with `compress`, `entity_normalize`, `ingest`, `ranking`, `retrieve`, etc.).

- [ ] **Step 3: Run the tests**

```bash
cargo test --lib pipeline::entity_normalize -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: 5 unit tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/pipeline/entity_normalize.rs src/pipeline/mod.rs
git commit -m "feat(entity): pipeline::entity_normalize::normalize_alias

Pure function: lowercase + trim + collapse internal whitespace.
Preserves punctuation (C++/C#/.NET/F# keep their identity) and
Unicode (no NFKC; YAGNI for v1). Used by EntityRegistry as the
PK form for entity_aliases lookups."
```

---

## Task 5: `EntityRegistry` trait + `DuckDbRepository` impl

**Files:**
- Modify: `src/storage/duckdb.rs` (trait declaration only)
- Create: `src/storage/entity_repo.rs`
- Modify: `src/storage/mod.rs`
- Test: `tests/entity_registry.rs`

- [ ] **Step 1: Write failing integration tests**

Append to `tests/entity_registry.rs`:

```rust
use mem::domain::{AddAliasOutcome, EntityKind};

const NOW: &str = "00000000020260502000";

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
        .query_row("select count(*) from entities where entity_id = ?1", [&id], |r| r.get(0))
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
        .query_row("select canonical_name from entities where entity_id = ?1", [&id], |r| r.get(0))
        .unwrap();
    assert_eq!(canonical, "Rust");
}

#[tokio::test]
async fn resolve_or_create_is_idempotent_on_alias_hit() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let id1 = repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW).await.unwrap();
    let id2 = repo.resolve_or_create("local", "rust", EntityKind::Topic, NOW).await.unwrap();
    let id3 = repo.resolve_or_create("local", "  RUST  ", EntityKind::Topic, NOW).await.unwrap();
    let id4 = repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW).await.unwrap();

    assert_eq!(id1, id2);
    assert_eq!(id1, id3);
    assert_eq!(id1, id4);

    let conn = duckdb::Connection::open(&db).unwrap();
    let entity_count: i64 = conn.query_row("select count(*) from entities", [], |r| r.get(0)).unwrap();
    assert_eq!(entity_count, 1, "no duplicate entities created");
}

#[tokio::test]
async fn resolve_or_create_creates_separate_entities_for_distinct_aliases() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust = repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW).await.unwrap();
    let lang = repo.resolve_or_create("local", "Rust language", EntityKind::Topic, NOW).await.unwrap();
    assert_ne!(rust, lang, "caller did not declare these as synonyms");
}

#[tokio::test]
async fn add_alias_links_to_existing_entity() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust_id = repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW).await.unwrap();
    let outcome = repo.add_alias("local", &rust_id, "Rust language", NOW).await.unwrap();
    assert_eq!(outcome, AddAliasOutcome::Inserted);

    // After add_alias, resolving "rust language" hits the existing rust_id.
    let lang_resolved = repo.resolve_or_create("local", "rust language", EntityKind::Topic, NOW).await.unwrap();
    assert_eq!(lang_resolved, rust_id);
}

#[tokio::test]
async fn add_alias_returns_already_on_same_entity_when_idempotent() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust_id = repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW).await.unwrap();
    repo.add_alias("local", &rust_id, "rustlang", NOW).await.unwrap();
    let outcome = repo.add_alias("local", &rust_id, "rustlang", NOW).await.unwrap();
    assert_eq!(outcome, AddAliasOutcome::AlreadyOnSameEntity);
}

#[tokio::test]
async fn add_alias_returns_conflict_when_alias_belongs_to_different_entity() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let rust_id = repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW).await.unwrap();
    let py_id = repo.resolve_or_create("local", "Python", EntityKind::Topic, NOW).await.unwrap();
    let outcome = repo.add_alias("local", &py_id, "rust", NOW).await.unwrap();
    assert_eq!(outcome, AddAliasOutcome::ConflictWithDifferentEntity(rust_id));
}

#[tokio::test]
async fn tenant_isolation_distinct_registries() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let a = repo.resolve_or_create("alice", "Rust", EntityKind::Topic, NOW).await.unwrap();
    let b = repo.resolve_or_create("bob", "Rust", EntityKind::Topic, NOW).await.unwrap();
    assert_ne!(a, b, "different tenants must produce different entities");
}

#[tokio::test]
async fn list_entities_filters_by_kind_and_query() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    repo.resolve_or_create("local", "Rust", EntityKind::Topic, NOW).await.unwrap();
    repo.resolve_or_create("local", "Python", EntityKind::Topic, NOW).await.unwrap();
    repo.resolve_or_create("local", "mem", EntityKind::Project, NOW).await.unwrap();

    let topics = repo.list_entities("local", Some(EntityKind::Topic), None, 100).await.unwrap();
    assert_eq!(topics.len(), 2);

    let rust_only = repo.list_entities("local", None, Some("Rust"), 100).await.unwrap();
    assert_eq!(rust_only.len(), 1);
    assert_eq!(rust_only[0].canonical_name, "Rust");
}

#[tokio::test]
async fn get_entity_returns_canonical_with_aliases_in_creation_order() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let id = repo.resolve_or_create("local", "Rust", EntityKind::Topic, "00000000020260502000").await.unwrap();
    repo.add_alias("local", &id, "Rust language", "00000000020260502001").await.unwrap();
    repo.add_alias("local", &id, "rustlang", "00000000020260502002").await.unwrap();

    let with_aliases = repo.get_entity("local", &id).await.unwrap().unwrap();
    assert_eq!(with_aliases.entity.canonical_name, "Rust");
    assert_eq!(with_aliases.aliases, vec!["rust", "rust language", "rustlang"]);
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --test entity_registry resolve_or_create_ add_alias_ tenant_ list_ get_entity_ -q
```

Expected: FAIL with "method not found" / "trait not in scope".

- [ ] **Step 3: Declare the trait in `src/storage/duckdb.rs`**

Find the existing `pub trait EmbeddingRowSource` (or similar trait declarations near the top of the file's `impl` blocks). Add a new public trait:

```rust
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};

#[async_trait::async_trait]
pub trait EntityRegistry: Send + Sync {
    /// Resolve `alias` (caller's verbatim input) under `tenant` to a stable
    /// `entity_id`. If the normalized form is unknown, atomically create a
    /// new entity (with `kind` and `canonical_name = alias`) plus its first
    /// alias row. Both INSERTs run under a single mutex acquisition.
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError>;

    /// Fetch the entity (by id, scoped to tenant) plus all its aliases in
    /// `created_at ASC` order. Returns `Ok(None)` if no such entity exists.
    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError>;

    /// Add `alias` to an existing entity. Three outcomes:
    /// - `Inserted`: alias was new, now linked to `entity_id`.
    /// - `AlreadyOnSameEntity`: alias was already on this entity (idempotent).
    /// - `ConflictWithDifferentEntity(other_id)`: alias is owned by another entity.
    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError>;

    /// List entities under `tenant`, optionally filtered by `kind` and a
    /// SQL `LIKE`-substring on `canonical_name`. Order is `created_at DESC`,
    /// capped by `limit`.
    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError>;
}
```

(`async_trait` is already a dependency — used by `EmbeddingProvider` and others. Verify by `grep async_trait Cargo.toml`.)

- [ ] **Step 4: Implement on `DuckDbRepository` in new file `src/storage/entity_repo.rs`**

```rust
//! `EntityRegistry` impl for `DuckDbRepository`. See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.

use async_trait::async_trait;

use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::pipeline::entity_normalize::normalize_alias;
use super::duckdb::{DuckDbRepository, EntityRegistry, StorageError};

#[async_trait]
impl EntityRegistry for DuckDbRepository {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        let normalized = normalize_alias(alias);
        let conn = self.conn()?;

        // Lookup first.
        let existing: Option<String> = conn
            .query_row(
                "select entity_id from entity_aliases \
                 where tenant = ?1 and alias_text = ?2",
                duckdb::params![tenant, normalized],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)?;
        if let Some(id) = existing {
            return Ok(id);
        }

        // Auto-promote. Both INSERTs run while the mutex is held.
        let entity_id = uuid::Uuid::now_v7().to_string();
        conn.execute(
            "insert into entities (entity_id, tenant, canonical_name, kind, created_at) \
             values (?1, ?2, ?3, ?4, ?5)",
            duckdb::params![entity_id, tenant, alias, kind.as_db_str(), now],
        )?;
        conn.execute(
            "insert into entity_aliases (tenant, alias_text, entity_id, created_at) \
             values (?1, ?2, ?3, ?4)",
            duckdb::params![tenant, normalized, entity_id, now],
        )?;
        Ok(entity_id)
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let conn = self.conn()?;
        let entity = conn
            .query_row(
                "select entity_id, tenant, canonical_name, kind, created_at \
                 from entities where tenant = ?1 and entity_id = ?2",
                duckdb::params![tenant, entity_id],
                |r| -> duckdb::Result<Entity> {
                    let kind_s: String = r.get(3)?;
                    Ok(Entity {
                        entity_id: r.get(0)?,
                        tenant: r.get(1)?,
                        canonical_name: r.get(2)?,
                        kind: EntityKind::from_db_str(&kind_s).ok_or_else(|| {
                            duckdb::Error::FromSqlConversionFailure(
                                3,
                                duckdb::types::Type::Text,
                                format!("invalid kind: {kind_s}").into(),
                            )
                        })?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::DuckDb)?;
        let Some(entity) = entity else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "select alias_text from entity_aliases \
             where tenant = ?1 and entity_id = ?2 \
             order by created_at asc, alias_text asc",
        )?;
        let aliases: Vec<String> = stmt
            .query_map(duckdb::params![tenant, entity_id], |r| r.get::<_, String>(0))?
            .collect::<duckdb::Result<Vec<_>>>()?;
        Ok(Some(EntityWithAliases { entity, aliases }))
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        let normalized = normalize_alias(alias);
        let conn = self.conn()?;

        let existing_owner: Option<String> = conn
            .query_row(
                "select entity_id from entity_aliases \
                 where tenant = ?1 and alias_text = ?2",
                duckdb::params![tenant, normalized],
                |r| r.get(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)?;

        match existing_owner {
            None => {
                conn.execute(
                    "insert into entity_aliases (tenant, alias_text, entity_id, created_at) \
                     values (?1, ?2, ?3, ?4)",
                    duckdb::params![tenant, normalized, entity_id, now],
                )?;
                Ok(AddAliasOutcome::Inserted)
            }
            Some(owner) if owner == entity_id => Ok(AddAliasOutcome::AlreadyOnSameEntity),
            Some(other) => Ok(AddAliasOutcome::ConflictWithDifferentEntity(other)),
        }
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let conn = self.conn()?;
        let mut sql = String::from(
            "select entity_id, tenant, canonical_name, kind, created_at \
             from entities where tenant = ?1",
        );
        let mut params: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant.to_string())];

        if let Some(k) = kind_filter {
            sql.push_str(" and kind = ?");
            sql.push_str(&format!("{}", params.len() + 1));
            params.push(Box::new(k.as_db_str().to_string()));
        }
        if let Some(q) = query {
            sql.push_str(" and canonical_name like ?");
            sql.push_str(&format!("{}", params.len() + 1));
            params.push(Box::new(format!("%{q}%")));
        }
        sql.push_str(" order by created_at desc limit ?");
        sql.push_str(&format!("{}", params.len() + 1));
        params.push(Box::new(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |r| -> duckdb::Result<Entity> {
            let kind_s: String = r.get(3)?;
            Ok(Entity {
                entity_id: r.get(0)?,
                tenant: r.get(1)?,
                canonical_name: r.get(2)?,
                kind: EntityKind::from_db_str(&kind_s).ok_or_else(|| {
                    duckdb::Error::FromSqlConversionFailure(
                        3,
                        duckdb::types::Type::Text,
                        format!("invalid kind: {kind_s}").into(),
                    )
                })?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<duckdb::Result<Vec<_>>>()?)
    }
}
```

`OptionalExt::optional()` is from `duckdb` — already imported by other files in this codebase. If a `use duckdb::OptionalExt;` line is needed at the top of `entity_repo.rs`, add it.

- [ ] **Step 5: Wire into `src/storage/mod.rs`**

Add:

```rust
pub mod entity_repo;
pub use duckdb::EntityRegistry;
```

(Match the file's existing `pub use` lines for `transcript_repo`.)

- [ ] **Step 6: Run all tests**

```bash
cargo test --test entity_registry -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: all `entity_registry` tests pass (probe still ignored).

- [ ] **Step 7: Commit**

```bash
git add src/storage/duckdb.rs src/storage/entity_repo.rs src/storage/mod.rs tests/entity_registry.rs
git commit -m "feat(entity): EntityRegistry trait + DuckDbRepository impl

Async trait with resolve_or_create / get_entity / add_alias /
list_entities. resolve_or_create lookup-then-INSERT-INSERT runs
under a single mutex hold (no race for auto-promote). add_alias
returns one of {Inserted, AlreadyOnSameEntity, ConflictWithDifferentEntity}.
canonical_name preserves caller verbatim (first-writer-wins);
alias_text is normalized for PK lookup."
```

---

## Task 6: `MemoryRecord.topics` field + DTO + storage round-trip

**Files:**
- Modify: `src/domain/memory.rs`
- Modify: `src/http/memory.rs` (`IngestMemoryRequest`)
- Modify: `src/storage/duckdb.rs` (`create_memory` insert; `row_to_memory` read; etc. — wherever `evidence` is handled)
- Test: append to `tests/entity_registry.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/entity_registry.rs`:

```rust
use mem::domain::memory::{MemoryRecord, MemoryStatus, MemoryType};

#[tokio::test]
async fn memory_record_topics_round_trip() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    // Seed a memory directly via the repo, with topics set.
    let memory = MemoryRecord {
        memory_id: "mem-test".to_string(),
        tenant: "local".to_string(),
        memory_type: MemoryType::Observation,
        content: "discussion of language ownership".to_string(),
        summary: "ownership notes".to_string(),
        evidence: vec![],
        code_refs: vec![],
        scope: "global".to_string(),
        visibility: "private".to_string(),
        confidence: 0.7,
        decay_score: 0.0,
        version: 1,
        created_at: NOW.to_string(),
        updated_at: NOW.to_string(),
        last_accessed_at: NOW.to_string(),
        status: MemoryStatus::Active,
        source_agent: "test".to_string(),
        idempotency_key: None,
        content_hash: "deadbeef".repeat(8), // 64 chars
        supersedes_memory_id: None,
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec!["Rust".into(), "ownership".into()],
        session_id: None,
    };
    repo.create_memory(&memory).await.unwrap();

    let fetched = repo.get_memory_for_tenant("local", "mem-test").await.unwrap().unwrap();
    assert_eq!(fetched.topics, vec!["Rust".to_string(), "ownership".to_string()]);
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
    repo.create_memory(&memory).await.unwrap();

    let fetched = repo.get_memory_for_tenant("local", "mem-empty").await.unwrap().unwrap();
    assert!(fetched.topics.is_empty());
}

// Helper: baseline MemoryRecord with all required fields.
fn baseline_memory(id: &str) -> MemoryRecord {
    MemoryRecord {
        memory_id: id.to_string(),
        tenant: "local".to_string(),
        memory_type: MemoryType::Observation,
        content: "x".to_string(),
        summary: "x".to_string(),
        evidence: vec![],
        code_refs: vec![],
        scope: "global".to_string(),
        visibility: "private".to_string(),
        confidence: 0.5,
        decay_score: 0.0,
        version: 1,
        created_at: NOW.to_string(),
        updated_at: NOW.to_string(),
        last_accessed_at: NOW.to_string(),
        status: MemoryStatus::Active,
        source_agent: "test".to_string(),
        idempotency_key: None,
        content_hash: "00".repeat(32),
        supersedes_memory_id: None,
        project: None,
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
        topics: vec![],
        session_id: None,
    }
}
```

(If `MemoryRecord` has additional fields not enumerated above — read `src/domain/memory.rs` carefully and adjust the baseline_memory helper.)

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test entity_registry memory_record_ -q
```

Expected: FAIL with "no field `topics`".

- [ ] **Step 3: Add `topics` to `MemoryRecord`**

In `src/domain/memory.rs`, find the `MemoryRecord` struct. Add the field next to `tags` (analogous list-of-strings field):

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub topics: Vec<String>,
```

- [ ] **Step 4: Add `topics` to `IngestMemoryRequest`**

In `src/http/memory.rs`, find `IngestMemoryRequest`. Add:

```rust
#[serde(default)]
pub topics: Vec<String>,
```

The handler that turns the request into a `MemoryRecord` carries `topics: req.topics` through.

- [ ] **Step 5: Update storage layer `create_memory` and row mapping**

In `src/storage/duckdb.rs`, find `create_memory` (or whatever function INSERTs into `memories`). It already serializes `evidence`, `code_refs`, `tags` as JSON-encoded strings. Add `topics` to that pattern:

```rust
let topics_json = serde_json::to_string(&memory.topics)
    .map_err(StorageError::Serde)?;
// ... add `topics_json` to the INSERT params list and add `topics` to the
// column list in the SQL.
```

In the row-to-`MemoryRecord` mapping function (likely `row_to_memory` or `map_memory_row`):

```rust
let topics: Vec<String> = row
    .get::<_, Option<String>>("topics")?
    .as_deref()
    .and_then(|s| serde_json::from_str(s).ok())
    .unwrap_or_default();
```

(NULL or invalid JSON → empty Vec; matches the verbatim spec's robustness requirement.)

Verify by reading existing `evidence` / `code_refs` / `tags` handling and mirroring it. If the storage layer uses a builder or struct, add `topics` to the same builder.

- [ ] **Step 6: Run the tests**

```bash
cargo test --test entity_registry memory_record_ -q
cargo test --test ingest_api -q
cargo test --test search_api -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: new tests pass; existing memory-touching tests still pass.

If existing tests break because `MemoryRecord` literal constructions need a new field: add `topics: vec![]` to those literals. Search for `MemoryRecord {` in tests and update.

- [ ] **Step 7: Commit**

```bash
git add src/domain/memory.rs src/http/memory.rs src/storage/duckdb.rs tests/entity_registry.rs <test files modified for new field>
git commit -m "feat(entity): MemoryRecord.topics + storage round-trip

Verbatim caller-supplied topic strings, JSON-encoded in
memories.topics column. Mirrors evidence/tags storage shape:
NULL or unparseable column → empty Vec on read; serde_json::to_string
always produces valid JSON on write. Default empty when caller
omits the field."
```

---

## Task 7: Split `extract_graph_edges` into pure drafts + deprecated wrapper

**Files:**
- Modify: `src/pipeline/ingest.rs`
- Test: append to `src/pipeline/ingest.rs::tests` (or wherever existing tests live)

- [ ] **Step 1: Write failing tests for the new `extract_graph_edge_drafts`**

In `src/pipeline/ingest.rs::tests` (read existing tests first; they likely test `extract_graph_edges`):

```rust
#[test]
fn extract_graph_edge_drafts_emits_entity_refs_for_all_field_types() {
    let memory = MemoryRecord {
        memory_id: "m1".to_string(),
        project: Some("mem".to_string()),
        repo: Some("foo/bar".to_string()),
        module: Some("storage".to_string()),
        task_type: Some("debug".to_string()),
        topics: vec!["Rust".to_string(), "ownership".to_string()],
        ..baseline_memory("m1")
    };
    let drafts = extract_graph_edge_drafts(&memory);

    let entity_refs: Vec<_> = drafts.iter()
        .filter_map(|d| match &d.to_kind {
            ToNodeKind::EntityRef { kind, alias } => Some((*kind, alias.clone(), d.relation.clone())),
            _ => None,
        })
        .collect();

    assert!(entity_refs.contains(&(EntityKind::Project, "mem".into(), "applies_to".into())));
    assert!(entity_refs.contains(&(EntityKind::Repo, "foo/bar".into(), "observed_in".into())));
    // module relation uses the existing repo+module keying — see implementation
    assert!(entity_refs.iter().any(|(k, _, r)| *k == EntityKind::Module && r == "relevant_to"));
    assert!(entity_refs.contains(&(EntityKind::Workflow, "debug".into(), "uses_workflow".into())));
    assert!(entity_refs.contains(&(EntityKind::Topic, "Rust".into(), "discusses".into())));
    assert!(entity_refs.contains(&(EntityKind::Topic, "ownership".into(), "discusses".into())));
}

#[test]
fn extract_graph_edge_drafts_emits_literal_memory_for_supersedes() {
    let memory = MemoryRecord {
        memory_id: "m2".to_string(),
        supersedes_memory_id: Some("m1".to_string()),
        ..baseline_memory("m2")
    };
    let drafts = extract_graph_edge_drafts(&memory);
    assert!(drafts.iter().any(|d| matches!(
        &d.to_kind,
        ToNodeKind::LiteralMemory(id) if id == "m1"
    )));
}

#[test]
fn extract_graph_edge_drafts_skips_empty_topic_strings() {
    let memory = MemoryRecord {
        memory_id: "m3".to_string(),
        topics: vec!["".to_string(), "Rust".to_string(), "  ".to_string()],
        ..baseline_memory("m3")
    };
    let drafts = extract_graph_edge_drafts(&memory);
    let topic_drafts: Vec<_> = drafts.iter()
        .filter(|d| matches!(&d.to_kind, ToNodeKind::EntityRef { kind: EntityKind::Topic, .. }))
        .collect();
    assert_eq!(topic_drafts.len(), 1, "empty/whitespace-only topics filtered out");
}

fn baseline_memory(id: &str) -> MemoryRecord {
    // Minimal MemoryRecord; same shape as in tests/entity_registry.rs.
    // (Inline a copy or move to a shared test util.)
    todo!("inline minimal record matching the struct shape; remove this todo before commit")
}
```

(Replace the `todo!()` with a real baseline before committing — same pattern as in tests/entity_registry.rs Step 1's `baseline_memory`.)

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --lib pipeline::ingest::tests::extract_graph_edge_drafts -q
```

Expected: FAIL with "function not defined".

- [ ] **Step 3: Implement `extract_graph_edge_drafts` and the BC wrapper**

In `src/pipeline/ingest.rs`, add the new types and function above the existing `extract_graph_edges`:

```rust
use crate::domain::EntityKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToNodeKind {
    EntityRef {
        kind: EntityKind,
        alias: String,
    },
    LiteralMemory(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdgeDraft {
    pub from_node_id: String,
    pub to_kind: ToNodeKind,
    pub relation: String,
}

/// Pure: produce drafts that downstream code resolves against an
/// `EntityRegistry`. Used by both `service::memory_service::ingest`
/// (live writes) and `cli::repair::rebuild_graph` (historical re-derive).
///
/// Skips empty/whitespace-only field values.
pub fn extract_graph_edge_drafts(memory: &MemoryRecord) -> Vec<GraphEdgeDraft> {
    let mut drafts = Vec::new();
    let from_node_id = memory_node_id(&memory.memory_id);

    if let Some(p) = memory.project.as_deref().filter(|v| !v.trim().is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef { kind: EntityKind::Project, alias: p.to_string() },
            relation: "applies_to".into(),
        });
    }
    if let Some(r) = memory.repo.as_deref().filter(|v| !v.trim().is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef { kind: EntityKind::Repo, alias: r.to_string() },
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
            to_kind: ToNodeKind::EntityRef { kind: EntityKind::Workflow, alias: wf.to_string() },
            relation: "uses_workflow".into(),
        });
    } else if matches!(memory.memory_type, MemoryType::Workflow) {
        // Self-referencing workflow: alias = the memory_id itself.
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef {
                kind: EntityKind::Workflow,
                alias: memory.memory_id.clone(),
            },
            relation: "uses_workflow".into(),
        });
    }
    for topic in memory.topics.iter().filter(|v| !v.trim().is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::EntityRef { kind: EntityKind::Topic, alias: topic.clone() },
            relation: "discusses".into(),
        });
    }
    if let Some(prev) = memory.supersedes_memory_id.as_deref().filter(|v| !v.is_empty()) {
        drafts.push(GraphEdgeDraft {
            from_node_id: from_node_id.clone(),
            to_kind: ToNodeKind::LiteralMemory(prev.to_string()),
            relation: "supersedes".into(),
        });
    }
    drafts
}
```

Then update the existing `extract_graph_edges` to be a deprecated wrapper:

```rust
/// **Deprecated.** Legacy wrapper that produces edges with the OLD
/// `"project:..."` / `"repo:..."` etc. string `to_node_id` format. New
/// code should call `extract_graph_edge_drafts` and resolve through
/// `EntityRegistry` (see `service::memory_service::resolve_drafts_to_edges`).
///
/// This wrapper exists only so the in-tree `graph_store::sync_memory`
/// caller and any historical tests keep compiling until they are migrated.
#[deprecated(note = "Use extract_graph_edge_drafts + EntityRegistry resolution")]
pub fn extract_graph_edges(memory: &MemoryRecord) -> Vec<GraphEdge> {
    extract_graph_edge_drafts(memory)
        .into_iter()
        .map(|draft| GraphEdge {
            from_node_id: draft.from_node_id,
            to_node_id: legacy_to_node_id(&draft.to_kind),
            relation: draft.relation,
            valid_from: String::new(),
            valid_to: None,
        })
        .collect()
}

fn legacy_to_node_id(kind: &ToNodeKind) -> String {
    match kind {
        ToNodeKind::LiteralMemory(id) => memory_node_id(id),
        ToNodeKind::EntityRef { kind: EntityKind::Project, alias } => project_node_id(alias),
        ToNodeKind::EntityRef { kind: EntityKind::Repo, alias } => repo_node_id(alias),
        ToNodeKind::EntityRef { kind: EntityKind::Module, alias } => {
            // alias is "<repo>:<module>"; module_node_id rebuilds the same string.
            if let Some((r, m)) = alias.split_once(':') {
                module_node_id(r, m)
            } else {
                format!("module:{alias}")
            }
        }
        ToNodeKind::EntityRef { kind: EntityKind::Workflow, alias } => workflow_node_id(alias),
        ToNodeKind::EntityRef { kind: EntityKind::Topic, alias } => format!("topic:{alias}"),
    }
}
```

The `legacy_to_node_id` for `Topic` uses `"topic:..."` prefix as a placeholder; no production caller relies on it (legacy tests don't seed memories with topics).

- [ ] **Step 4: Run all tests**

```bash
cargo test --lib pipeline::ingest -q
cargo test --test graph_temporal -q  # uses legacy extract_graph_edges
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: all pass. Clippy may emit a `deprecated` warning at the legacy callsite in `graph_store.rs` (Task 9 silences it via `#[allow(deprecated)]` if needed).

If clippy with `-D warnings` errors on the deprecation warning, add `#[allow(deprecated)]` at the call site in `src/storage/graph_store.rs:56` as a tactical fix until Task 9 properly migrates it.

- [ ] **Step 5: Commit**

```bash
git add src/pipeline/ingest.rs <any test files updated for new struct fields>
git commit -m "refactor(ingest): split extract_graph_edges into pure drafts + BC wrapper

extract_graph_edge_drafts is a pure function producing
Vec<GraphEdgeDraft> with structured ToNodeKind (EntityRef vs
LiteralMemory). Topics now produce Topic-kind drafts with
relation='discusses'.

extract_graph_edges is preserved as #[deprecated] wrapper
for the existing graph_store.rs caller; Task 9 migrates the
caller to the new path."
```

---

## Task 8: Service-layer resolution (`resolve_drafts_to_edges`) + ingest integration

**Files:**
- Modify: `src/service/memory_service.rs`
- Modify: `src/service/mod.rs` (only if a new helper module is needed; otherwise no change)
- Test: append to `tests/entity_registry.rs`

- [ ] **Step 1: Write the failing integration test**

Append to `tests/entity_registry.rs`:

```rust
use mem::http::router_with_config;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn ingest_memory_with_topics_creates_entity_refs_in_graph_edges() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = router_with_config(cfg.clone()).await.unwrap();

    let body = json!({
        "tenant": "local",
        "memory_type": "observation",
        "content": "Rust borrow checker discussion",
        "scope": "global",
        "source_agent": "test",
        "topics": ["Rust", "ownership"],
        "write_mode": "auto"
    });
    let resp = app.oneshot(
        Request::builder().method("POST").uri("/memories")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify graph_edges has 2 'discusses' edges pointing to entity:<uuid>.
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let count: i64 = conn.query_row(
        "select count(*) from graph_edges \
         where relation = 'discusses' and to_node_id like 'entity:%'",
        [],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(count, 2);

    // Verify entities table has 2 entities of kind='topic'.
    let entity_count: i64 = conn.query_row(
        "select count(*) from entities where kind='topic' and tenant='local'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(entity_count, 2);
}

#[tokio::test]
async fn ingest_with_existing_alias_reuses_entity_id() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = router_with_config(cfg.clone()).await.unwrap();

    let post = |body: serde_json::Value, app: &axum::Router| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder().method("POST").uri("/memories")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string())).unwrap()
            ).await.unwrap()
        }
    };

    let r1 = post(json!({
        "tenant": "local",
        "memory_type": "observation",
        "content": "first mention",
        "scope": "global",
        "source_agent": "test",
        "topics": ["Rust"],
        "write_mode": "auto"
    }), &app).await;
    assert_eq!(r1.status(), StatusCode::OK);

    let r2 = post(json!({
        "tenant": "local",
        "memory_type": "observation",
        "content": "second mention with case variation",
        "scope": "global",
        "source_agent": "test",
        "topics": ["rust"],
        "write_mode": "auto"
    }), &app).await;
    assert_eq!(r2.status(), StatusCode::OK);

    // Both memories' 'discusses' edges should point to the SAME entity_id.
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let entity_count: i64 = conn.query_row(
        "select count(*) from entities where kind='topic' and tenant='local'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(entity_count, 1, "case-insensitive alias should not create duplicate entity");

    let edge_count: i64 = conn.query_row(
        "select count(*) from graph_edges where relation='discusses'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(edge_count, 2);

    // Both edges should have the same to_node_id (same entity_id).
    let distinct_targets: i64 = conn.query_row(
        "select count(distinct to_node_id) from graph_edges where relation='discusses'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(distinct_targets, 1);
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test entity_registry ingest_memory_with_topics ingest_with_existing_alias -q
```

Expected: FAIL — currently `topics` is stored on `MemoryRecord` but ingest doesn't extract `discusses` edges through registry.

- [ ] **Step 3: Add `resolve_drafts_to_edges` and call it from ingest**

In `src/service/memory_service.rs`, find `MemoryService::ingest`. It currently calls `extract_graph_edges(&memory)` somewhere (or `graph_store::sync_memory` does). The change:

```rust
use crate::domain::GraphEdge;
use crate::pipeline::ingest::{extract_graph_edge_drafts, GraphEdgeDraft, ToNodeKind};
use crate::storage::EntityRegistry;

async fn resolve_drafts_to_edges(
    drafts: Vec<GraphEdgeDraft>,
    registry: &impl EntityRegistry,
    tenant: &str,
    now: &str,
) -> Result<Vec<GraphEdge>, StorageError> {
    let mut out = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let to_node_id = match draft.to_kind {
            ToNodeKind::LiteralMemory(memory_id) => format!("memory:{memory_id}"),
            ToNodeKind::EntityRef { kind, alias } => {
                let id = registry.resolve_or_create(tenant, &alias, kind, now).await?;
                format!("entity:{id}")
            }
        };
        out.push(GraphEdge {
            from_node_id: draft.from_node_id,
            to_node_id,
            relation: draft.relation,
            valid_from: now.to_string(),
            valid_to: None,
        });
    }
    Ok(out)
}
```

In `MemoryService::ingest`, replace the existing graph-edge extraction step. The current ingest probably does something like:

```rust
let edges = extract_graph_edges(&memory);
self.graph_store.sync_memory(&memory.memory_id, &edges, &now).await?;
```

Change to:

```rust
let drafts = extract_graph_edge_drafts(&memory);
let edges = resolve_drafts_to_edges(drafts, &self.repo, &request.tenant, &now).await?;
self.graph_store.sync_memory_edges(&memory.memory_id, &edges, &now).await?;
```

If `graph_store::sync_memory` does the extraction itself (likely — see Task 9), the path differs. **Read `MemoryService::ingest` and `DuckDbGraphStore::sync_memory` first**; the goal is that ingest uses the new pipeline (drafts + resolve), not the legacy `extract_graph_edges` wrapper.

If `MemoryService::ingest` doesn't currently call `extract_graph_edges` directly (it delegates to `graph_store::sync_memory`), the integration happens in Task 9 (modify `sync_memory` to take drafts) — leave Task 8 to just define `resolve_drafts_to_edges` as a service-module helper, and rely on Task 9 to wire it in.

**Decision**: Define `resolve_drafts_to_edges` as `pub(crate) async fn` in `src/service/memory_service.rs` (or a new `src/service/graph_resolution.rs` if cleaner). Task 9 calls it. The two integration tests above will FAIL after this Task 8 commit, and PASS after Task 9 — that's expected; mark them `#[ignore = "wired in Task 9"]` if the failures block CI in between, OR commit Tasks 8+9 together as a single logical unit.

**Recommended**: combine Tasks 8 + 9 into one commit (the function + the wiring). The plan keeps them as separate tasks for clarity; the implementer commits at the end of Task 9.

- [ ] **Step 4: Don't commit yet** — combine with Task 9.

---

## Task 9: Migrate `graph_store::sync_memory` to use the new pipeline

**Files:**
- Modify: `src/storage/graph_store.rs`
- Modify: `src/service/memory_service.rs` (call site)
- Test: existing `tests/graph_temporal.rs` must still pass

- [ ] **Step 1: Read existing `graph_store::sync_memory`**

Open `src/storage/graph_store.rs`. Find `sync_memory`. Current shape (per the grep at line 56):

```rust
pub async fn sync_memory(&self, memory: &MemoryRecord, now: &str) -> Result<(), GraphError> {
    let edges = extract_graph_edges(memory);  // legacy wrapper
    // ... INSERT edges with valid_from = now
}
```

It calls the legacy extractor that produces `"project:..."` strings. After Task 7's deprecation, this is the LAST in-tree caller of the legacy path.

- [ ] **Step 2: Migrate `sync_memory` to the new pipeline**

Change `sync_memory` to take a `&dyn EntityRegistry` (or take the resolved edges directly). Two options:

**Option A: `sync_memory` stays method on `DuckDbGraphStore`; takes registry reference**

```rust
pub async fn sync_memory(
    &self,
    memory: &MemoryRecord,
    registry: &dyn EntityRegistry,  // NEW parameter
    now: &str,
) -> Result<(), GraphError> {
    let drafts = extract_graph_edge_drafts(memory);
    let edges = resolve_drafts_to_edges(drafts, registry, &memory.tenant, now).await
        .map_err(GraphError::from)?;
    // ... existing INSERT logic, but using `edges` directly
}
```

**Option B: Caller resolves edges and passes them in**

```rust
pub async fn sync_memory_edges(
    &self,
    memory_id: &str,
    edges: &[GraphEdge],
    now: &str,
) -> Result<(), GraphError> {
    // ... INSERT edges with from_node_id = "memory:<id>", valid_from = now
}
```

(Caller — `MemoryService::ingest` — does the draft + resolve.)

**Recommend Option B**: keeps `graph_store` ignorant of registry; ingest service is the orchestrator. Cleaner layering.

Migrate:

1. Rename `sync_memory` → `sync_memory_edges`; signature takes pre-resolved edges.
2. Remove the legacy `extract_graph_edges` call from `graph_store.rs`.
3. Remove the `use crate::pipeline::ingest::extract_graph_edges;` import.
4. In `MemoryService::ingest`, do drafts + resolve + call `sync_memory_edges`:

```rust
let drafts = extract_graph_edge_drafts(&memory);
let edges = resolve_drafts_to_edges(drafts, &self.repo, &memory.tenant, &now).await?;
self.graph_store.sync_memory_edges(&memory.memory_id, &edges, &now).await?;
```

5. Update `tests/graph_temporal.rs`: it imports `extract_graph_edges` directly. Two options:
   - Migrate the test to use the new draft path with a fake registry / `DuckDbRepository` setup
   - Keep the test using the deprecated wrapper with `#[allow(deprecated)]` (acceptable; the wrapper exists for exactly this BC purpose)

   Recommend **the latter** — minimal disturbance. Add `#[allow(deprecated)]` annotations where the test calls `extract_graph_edges`. The wrapper's `legacy_to_node_id` continues to produce the old `"project:..."` strings, so the test's existing assertions about `to_node_id` content pass unchanged.

- [ ] **Step 3: Run all tests**

```bash
cargo test --test entity_registry ingest_memory_with_topics ingest_with_existing_alias -q
cargo test --test graph_temporal -q
cargo test --test ingest_api -q
cargo test --test search_api -q
cargo test -q --no-fail-fast
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected:
- The 2 new entity_registry ingest tests now PASS.
- `tests/graph_temporal.rs` continues to pass (with `#[allow(deprecated)]` annotations as needed).
- Other suites unchanged.
- Clippy may warn about `#[deprecated]` use in tests; the `#[allow(deprecated)]` annotations silence those.

- [ ] **Step 4: Commit (combined with Task 8)**

```bash
git add src/service/memory_service.rs src/storage/graph_store.rs tests/entity_registry.rs <other test files>
git commit -m "feat(graph): wire ingest through EntityRegistry; migrate sync_memory

MemoryService::ingest now extracts pure drafts (extract_graph_edge_drafts),
resolves entity refs through DuckDbRepository (which implements
EntityRegistry), and persists pre-resolved edges via the renamed
DuckDbGraphStore::sync_memory_edges (taking edges directly, not a
MemoryRecord).

graph_store no longer imports the legacy extractor. tests/graph_temporal.rs
keeps the deprecated wrapper via #[allow(deprecated)] for BC (legitimate
wrapper consumer; the wrapper exists for exactly this purpose).

Closes Task 8 + 9 of the entity-registry plan as a single commit
(Task 8 alone would leave the new tests failing)."
```

---

## Task 10: HTTP routes — `entity_service` + `http::entities`

**Files:**
- Create: `src/service/entity_service.rs`
- Create: `src/http/entities.rs`
- Modify: `src/service/mod.rs`
- Modify: `src/http/mod.rs`
- Modify: `src/app.rs` (attach EntityService to AppState if needed)
- Test: append to `tests/entity_registry.rs`

- [ ] **Step 1: Write failing HTTP tests**

Append to `tests/entity_registry.rs`:

```rust
#[tokio::test]
async fn post_entities_creates_with_aliases() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = router_with_config(cfg).await.unwrap();

    let resp = app.oneshot(
        Request::builder().method("POST").uri("/entities")
            .header("content-type", "application/json")
            .body(Body::from(json!({
                "tenant": "local",
                "canonical_name": "Rust",
                "kind": "topic",
                "aliases": ["Rust language", "rustlang"]
            }).to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
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
    let app = router_with_config(cfg).await.unwrap();

    // Create.
    let create = app.clone().oneshot(
        Request::builder().method("POST").uri("/entities")
            .header("content-type", "application/json")
            .body(Body::from(json!({
                "tenant": "local",
                "canonical_name": "Rust",
                "kind": "topic"
            }).to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(create.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = v["entity"]["entity_id"].as_str().unwrap().to_string();

    // Get.
    let get = app.oneshot(
        Request::builder().method("GET").uri(format!("/entities/{id}?tenant=local"))
            .body(Body::empty()).unwrap()
    ).await.unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(get.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["entity"]["entity_id"], id);
}

#[tokio::test]
async fn post_entity_aliases_idempotent_and_409_on_conflict() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = router_with_config(cfg).await.unwrap();

    // Create two entities.
    let create_one = |canonical: &str, app: axum::Router| {
        let body = json!({"tenant": "local", "canonical_name": canonical, "kind": "topic"}).to_string();
        async move {
            let resp = app.oneshot(
                Request::builder().method("POST").uri("/entities")
                    .header("content-type", "application/json")
                    .body(Body::from(body)).unwrap()
            ).await.unwrap();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            v["entity"]["entity_id"].as_str().unwrap().to_string()
        }
    };
    let rust_id = create_one("Rust", app.clone()).await;
    let py_id = create_one("Python", app.clone()).await;

    // First add — Inserted (200).
    let r1 = app.clone().oneshot(
        Request::builder().method("POST").uri(format!("/entities/{rust_id}/aliases"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"tenant": "local", "alias": "Rust language"}).to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);

    // Idempotent re-add — same outcome 200.
    let r2 = app.clone().oneshot(
        Request::builder().method("POST").uri(format!("/entities/{rust_id}/aliases"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"tenant": "local", "alias": "Rust language"}).to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);

    // Conflict — alias 'rust' already on rust_id; trying on py_id → 409.
    let r3 = app.oneshot(
        Request::builder().method("POST").uri(format!("/entities/{py_id}/aliases"))
            .header("content-type", "application/json")
            .body(Body::from(json!({"tenant": "local", "alias": "Rust"}).to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(r3.status(), StatusCode::CONFLICT);
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo test --test entity_registry post_entities get_entity post_entity_aliases -q
```

Expected: FAIL — routes 404.

- [ ] **Step 3: Implement `EntityService` and `http::entities`**

Create `src/service/entity_service.rs`:

```rust
//! Façade for the entity-registry HTTP layer. See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.

use std::sync::Arc;

use crate::domain::{AddAliasOutcome, EntityKind, EntityWithAliases, Entity};
use crate::storage::{DuckDbRepository, EntityRegistry, StorageError};

#[derive(Clone)]
pub struct EntityService {
    repo: DuckDbRepository,
}

impl EntityService {
    pub fn new(repo: DuckDbRepository) -> Self {
        Self { repo }
    }

    pub async fn create_with_aliases(
        &self,
        tenant: &str,
        canonical_name: &str,
        kind: EntityKind,
        aliases: &[String],
        now: &str,
    ) -> Result<EntityWithAliases, StorageError> {
        let entity_id = self
            .repo
            .resolve_or_create(tenant, canonical_name, kind, now)
            .await?;
        for alias in aliases {
            self.repo.add_alias(tenant, &entity_id, alias, now).await?;
            // Note: we ignore Inserted/AlreadyOnSameEntity outcomes here;
            // ConflictWithDifferentEntity propagates as InvalidInput at HTTP.
        }
        self.repo
            .get_entity(tenant, &entity_id)
            .await?
            .ok_or_else(|| StorageError::InvalidInput(
                "entity disappeared after creation".into(),
            ))
    }

    pub async fn get(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        self.repo.get_entity(tenant, entity_id).await
    }

    pub async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        self.repo.add_alias(tenant, entity_id, alias, now).await
    }

    pub async fn list(
        &self,
        tenant: &str,
        kind: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        self.repo.list_entities(tenant, kind, query, limit).await
    }
}
```

Create `src/http/entities.rs`:

```rust
//! HTTP routes for entity registry. See spec
//! docs/superpowers/specs/2026-05-02-entity-registry-design.md.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::domain::{AddAliasOutcome, EntityKind, EntityWithAliases, Entity};
use crate::error::AppError;
use crate::storage::time::current_timestamp;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/entities", post(post_entity).get(list_entities))
        .route("/entities/{entity_id}", get(get_entity))
        .route("/entities/{entity_id}/aliases", post(post_alias))
}

// ────────── POST /entities ──────────

#[derive(Debug, Deserialize)]
struct CreateEntityRequest {
    tenant: String,
    canonical_name: String,
    kind: EntityKind,
    #[serde(default)]
    aliases: Vec<String>,
}

async fn post_entity(
    State(state): State<AppState>,
    Json(req): Json<CreateEntityRequest>,
) -> Result<(StatusCode, Json<EntityWithAliases>), AppError> {
    let now = current_timestamp();
    let result = state.entity_service
        .create_with_aliases(&req.tenant, &req.canonical_name, req.kind, &req.aliases, &now)
        .await?;
    Ok((StatusCode::CREATED, Json(result)))
}

// ────────── GET /entities/{id} ──────────

#[derive(Debug, Deserialize)]
struct GetEntityQuery { tenant: String }

async fn get_entity(
    State(state): State<AppState>,
    Path(entity_id): Path<String>,
    Query(q): Query<GetEntityQuery>,
) -> Result<Json<EntityWithAliases>, (StatusCode, String)> {
    match state.entity_service.get(&q.tenant, &entity_id).await {
        Ok(Some(e)) => Ok(Json(e)),
        Ok(None) => Err((StatusCode::NOT_FOUND, format!("entity not found: {entity_id}"))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ────────── POST /entities/{id}/aliases ──────────

#[derive(Debug, Deserialize)]
struct AddAliasRequest { tenant: String, alias: String }

#[derive(Debug, Serialize)]
struct AddAliasResponse {
    outcome: &'static str,
    existing_entity_id: Option<String>,
}

async fn post_alias(
    State(state): State<AppState>,
    Path(entity_id): Path<String>,
    Json(req): Json<AddAliasRequest>,
) -> Result<(StatusCode, Json<AddAliasResponse>), AppError> {
    let now = current_timestamp();
    let outcome = state.entity_service
        .add_alias(&req.tenant, &entity_id, &req.alias, &now)
        .await?;
    let (status, payload) = match outcome {
        AddAliasOutcome::Inserted => (StatusCode::OK, AddAliasResponse {
            outcome: "inserted",
            existing_entity_id: None,
        }),
        AddAliasOutcome::AlreadyOnSameEntity => (StatusCode::OK, AddAliasResponse {
            outcome: "already_on_same_entity",
            existing_entity_id: Some(entity_id.clone()),
        }),
        AddAliasOutcome::ConflictWithDifferentEntity(other) => (
            StatusCode::CONFLICT,
            AddAliasResponse {
                outcome: "conflict_with_different_entity",
                existing_entity_id: Some(other),
            },
        ),
    };
    Ok((status, Json(payload)))
}

// ────────── GET /entities ──────────

#[derive(Debug, Deserialize)]
struct ListEntitiesQuery {
    tenant: String,
    #[serde(default)]
    kind: Option<EntityKind>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize { 50 }

#[derive(Debug, Serialize)]
struct ListEntitiesResponse { entities: Vec<Entity> }

async fn list_entities(
    State(state): State<AppState>,
    Query(q): Query<ListEntitiesQuery>,
) -> Result<Json<ListEntitiesResponse>, AppError> {
    let entities = state.entity_service
        .list(&q.tenant, q.kind, q.q.as_deref(), q.limit.min(100))
        .await?;
    Ok(Json(ListEntitiesResponse { entities }))
}
```

Update `src/service/mod.rs`:

```rust
pub mod entity_service;
pub use entity_service::EntityService;
```

Update `src/http/mod.rs`:

```rust
pub mod entities;
// In the router builder, add:
//   .merge(entities::router())
```

Update `src/app.rs` to construct `EntityService` and attach to `AppState`:

```rust
// In AppState:
pub entity_service: EntityService,
// In AppState::from_config:
let entity_service = EntityService::new(repo.clone());
// Then include `entity_service` in the AppState literal.
```

- [ ] **Step 4: Run tests**

```bash
cargo test --test entity_registry -q
cargo build --tests
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: all tests pass, clippy + fmt clean.

- [ ] **Step 5: Commit**

```bash
git add src/service/entity_service.rs src/service/mod.rs src/http/entities.rs src/http/mod.rs src/app.rs tests/entity_registry.rs
git commit -m "feat(entity): EntityService + 4 HTTP routes

POST /entities (create with optional aliases) - 201 + EntityWithAliases.
GET /entities/{id}?tenant - 200 / 404.
POST /entities/{id}/aliases - 200 (inserted/idempotent) / 409 (conflict
with different entity owner).
GET /entities?tenant&kind&q&limit - 200 + entities list (created_at desc).

MCP unchanged. Errors flow through AppError where applicable; 404/409
are explicit handler-level status returns."
```

---

## Task 11: `mem repair --rebuild-graph` CLI subcommand

**Files:**
- Modify: `src/cli/repair.rs`
- Modify: `src/storage/duckdb.rs` (helper methods if needed: `list_distinct_tenants`, `list_memories_for_tenant`, `delete_memory_origin_graph_edges`, `bulk_insert_graph_edges`)
- Test: append to `tests/repair_cli.rs`

- [ ] **Step 1: Write failing migration tests**

Append to `tests/repair_cli.rs`:

```rust
use mem::cli::repair::{rebuild_graph_for_test, RebuildGraphOutcome};

#[tokio::test]
async fn rebuild_graph_converts_legacy_to_entity_refs() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");

    // Bootstrap and seed a memory through the production path.
    let app = mem::http::router_with_config(cfg.clone()).await.unwrap();
    let body = json!({
        "tenant": "local",
        "memory_type": "observation",
        "content": "x",
        "scope": "global",
        "source_agent": "test",
        "project": "mem",
        "topics": ["Rust"],
        "write_mode": "auto"
    });
    let resp = app.oneshot(
        Request::builder().method("POST").uri("/memories")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap()
    ).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Inject a legacy-format graph edge directly.
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    conn.execute(
        "insert into graph_edges (from_node_id, to_node_id, relation, valid_from, valid_to) \
         values ('memory:legacy1', 'project:legacy-project', 'applies_to', '00000000020260501000', null)",
        [],
    ).unwrap();
    conn.execute(
        "insert into memories (memory_id, tenant, memory_type, content, summary, scope, visibility, \
         confidence, decay_score, version, created_at, updated_at, last_accessed_at, status, \
         source_agent, content_hash, project) \
         values ('legacy1', 'local', 'observation', 'old', 'old', 'global', 'private', \
                 0.5, 0.0, 1, '00000000020260501000', '00000000020260501000', '00000000020260501000', \
                 'active', 'test', 'a' || repeat('a', 63), 'legacy-project')",
        [],
    ).unwrap();

    let outcome = rebuild_graph_for_test(&cfg).await.unwrap();
    assert!(matches!(outcome, RebuildGraphOutcome::Rebuilt { .. }));

    // After rebuild: NO legacy 'project:...' edges remain.
    let legacy_count: i64 = conn.query_row(
        "select count(*) from graph_edges where to_node_id like 'project:%'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(legacy_count, 0);

    // All memory→entity edges use 'entity:<uuid>' format.
    let entity_count: i64 = conn.query_row(
        "select count(*) from graph_edges \
         where from_node_id like 'memory:%' and to_node_id like 'entity:%'",
        [], |r| r.get(0),
    ).unwrap();
    assert!(entity_count >= 2, "got {entity_count}; expected at least project + topic edges");
}

#[tokio::test]
async fn rebuild_graph_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let app = mem::http::router_with_config(cfg.clone()).await.unwrap();
    app.oneshot(
        Request::builder().method("POST").uri("/memories")
            .header("content-type", "application/json")
            .body(Body::from(json!({
                "tenant": "local",
                "memory_type": "observation",
                "content": "x",
                "scope": "global",
                "source_agent": "test",
                "topics": ["Rust"],
                "write_mode": "auto"
            }).to_string())).unwrap()
    ).await.unwrap();

    rebuild_graph_for_test(&cfg).await.unwrap();
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let count1: i64 = conn.query_row("select count(*) from graph_edges", [], |r| r.get(0)).unwrap();

    rebuild_graph_for_test(&cfg).await.unwrap();
    let count2: i64 = conn.query_row("select count(*) from graph_edges", [], |r| r.get(0)).unwrap();

    assert_eq!(count1, count2, "rebuild must be idempotent");
}

#[tokio::test]
async fn rebuild_graph_handles_empty_database() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let _app = mem::http::router_with_config(cfg.clone()).await.unwrap();

    let outcome = rebuild_graph_for_test(&cfg).await.unwrap();
    assert!(matches!(outcome, RebuildGraphOutcome::Rebuilt { rebuilt_memory_count: 0, .. }));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test repair_cli rebuild_graph -q
```

Expected: FAIL — `rebuild_graph_for_test` and `RebuildGraphOutcome` not defined.

- [ ] **Step 3: Implement `--rebuild-graph` in `src/cli/repair.rs`**

Add a `RebuildGraphOutcome`:

```rust
#[derive(Debug, Clone)]
pub enum RebuildGraphOutcome {
    Rebuilt {
        rebuilt_memory_count: usize,
        new_edge_count: usize,
        elapsed_ms: u64,
    },
    Failed {
        reason: String,
    },
}

impl RebuildGraphOutcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            RebuildGraphOutcome::Rebuilt { .. } => 0,
            RebuildGraphOutcome::Failed { .. } => 2,
        }
    }
}
```

Add to the `RepairMode` group:

```rust
#[derive(Debug, Args)]
#[group(multiple = false)]
pub struct RepairMode {
    #[arg(long)]
    pub check: bool,
    #[arg(long)]
    pub rebuild: bool,
    #[arg(long)]
    pub rebuild_graph: bool,  // NEW
}
```

Implement the rebuild logic:

```rust
pub async fn rebuild_graph_for_test(config: &Config) -> Result<RebuildGraphOutcome, anyhow::Error> {
    use crate::pipeline::ingest::extract_graph_edge_drafts;
    use crate::storage::time::current_timestamp;

    let started = std::time::Instant::now();
    let repo = DuckDbRepository::open(&config.db_path).await?;
    let now = current_timestamp();

    let tenants = repo.list_distinct_memory_tenants().await?;
    let mut total_memories = 0;
    let mut total_edges = 0;

    for tenant in tenants {
        // Delete all memory-originating edges; supersedes / topic / project /
        // etc. all originate from "memory:..." ids.
        repo.delete_graph_edges_from_memories(&tenant).await?;

        let memories = repo.list_memories_for_tenant(&tenant).await?;
        for memory in &memories {
            let drafts = extract_graph_edge_drafts(memory);
            let edges = resolve_drafts_to_edges(drafts, &repo, &tenant, &now).await?;
            for edge in &edges {
                repo.insert_graph_edge(edge).await?;
            }
            total_edges += edges.len();
        }
        total_memories += memories.len();
    }

    Ok(RebuildGraphOutcome::Rebuilt {
        rebuilt_memory_count: total_memories,
        new_edge_count: total_edges,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}
```

(`resolve_drafts_to_edges` is the function from Task 8/9. If it lives in `service::memory_service`, expose it `pub(crate)` so `cli::repair` can call it. Or move it to a `service::graph_resolution` module and re-export.)

The new repo helpers (`list_distinct_memory_tenants`, `list_memories_for_tenant`, `delete_graph_edges_from_memories`, `insert_graph_edge`) — add to `src/storage/duckdb.rs` if they don't already exist. Each is a thin SQL wrapper.

Update `run` to dispatch:

```rust
pub async fn run(args: RepairArgs) -> i32 {
    let config = match Config::from_env() { /* ... */ };
    if args.mode.rebuild_graph {
        match rebuild_graph_for_test(&config).await {
            Ok(outcome) => {
                println!("{outcome:?}");
                outcome.exit_code()
            }
            Err(e) => {
                eprintln!("rebuild-graph failed: {e}");
                2
            }
        }
    } else if args.mode.rebuild {
        run_rebuild(&config, &fp, args.json).await
    } else {
        run_check(&config, &fp, args.json).await
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test --test repair_cli rebuild_graph -q
cargo test --test entity_registry -q
cargo test -q --no-fail-fast
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Expected: all 3 new migration tests pass; existing tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add src/cli/repair.rs src/storage/duckdb.rs <other modified> tests/repair_cli.rs
git commit -m "feat(cli): mem repair --rebuild-graph subcommand

Re-derives all memory→{entity,memory} edges from the memories table
using the same production extract_graph_edge_drafts +
resolve_drafts_to_edges path. After running, all to_node_id values
are in the new 'entity:<uuid>' format; legacy 'project:...'/'repo:...'
strings are gone.

Idempotent: re-running produces the same result (registry lookups
hit existing entity_ids).

Per spec risk #2: rebuild does not preserve historical valid_to
ranges from pre-migration edges (those rows are deleted). Acceptable
for first-run upgrade migration; documented in CHANGELOG."
```

---

## Task 12: README + AGENTS.md + smoke verification

**Files:**
- Modify: `README.md` — document the new entity surface
- Modify: `AGENTS.md` — add bullet under Architecture
- Smoke: manual checklist in commit message; full test suite verification

- [ ] **Step 1: Update README**

Find the existing transcript-archive section in README.md. Append a new section:

```markdown
## Entity Registry (entities + entity_aliases)

Tenant-scoped registry that canonicalizes alias strings (`"Rust"` = `"Rust language"` = `"rustlang"`) to a stable `entity_id`. Three mechanisms feed it:

1. **`mem mine` / `POST /memories`** — caller-supplied `topics: Vec<String>` field plus existing `project` / `repo` / `module` / `task_type` strings auto-promote to entities on first ingest.
2. **`POST /entities`** — explicit creation with optional aliases.
3. **`POST /entities/{id}/aliases`** — add a synonym to an existing entity; idempotent; returns 409 on conflict.

After ingest, `graph_edges.to_node_id` is `"entity:<uuid>"` for every entity-typed edge. Memory→memory edges (`supersedes`) keep the `"memory:<id>"` prefix.

**Migration**: existing `graph_edges` rows from before the registry shipped retain their legacy `"project:..."` / `"repo:..."` strings. Run `cargo run -- repair --rebuild-graph` to re-derive all memory-originating edges through the registry. Idempotent.

**Aliases & normalization**: alias matching is lowercase + whitespace-collapsed; punctuation preserved (`C++` ≠ `c`). Caller's verbatim spelling lives on `entities.canonical_name`.

**MCP**: the registry is HTTP-only; no MCP surface (matches the conversation-archive / transcript-recall convention).

Spec: [`docs/superpowers/specs/2026-05-02-entity-registry-design.md`](docs/superpowers/specs/2026-05-02-entity-registry-design.md).
```

- [ ] **Step 2: Update AGENTS.md**

Find the "Architecture" section. Add a bullet about the registry, mirroring the conversation-archive bullet style:

```markdown
- **Entity registry**: `entities` + `entity_aliases` tables canonicalize alias strings to stable `entity_id` (UUIDv7). `MemoryRecord.topics: Vec<String>` is the caller-supplied input; ingest pipeline (`extract_graph_edge_drafts` + `resolve_drafts_to_edges` in `service::memory_service`) routes through `EntityRegistry` so `graph_edges.to_node_id` is `"entity:<uuid>"`. Aliases are normalized (lowercase + whitespace-collapsed) at the PK; canonical_name preserves caller verbatim. Tenant-scoped, session-orthogonal. Migration command: `mem repair --rebuild-graph`.
```

- [ ] **Step 3: Run the full suite**

```bash
cargo test -q --no-fail-fast
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Expected:
- 0 failures (target: 18+ new entity_registry tests; existing graph_temporal / ingest_api / search_api / etc. unchanged)
- 1 ignored (FTS predicate probe baseline, plus optionally the new composite-PK probe — count both)
- `cargo build --release` clean

- [ ] **Step 4: Manual smoke (operator pre-merge)**

Run on a fresh DB; document the checklist in the commit message:

```bash
rm -f $MEM_DB_PATH
cargo run -- serve &
SERVE_PID=$!
sleep 2

# 1. Explicit entity creation with aliases.
curl -s -X POST localhost:3000/entities \
  -H 'content-type: application/json' \
  -d '{"tenant":"local","canonical_name":"Rust","kind":"topic","aliases":["Rust language","rustlang"]}' | jq

# 2. Ingest with topic alias variant — should resolve to the same entity_id.
curl -s -X POST localhost:3000/memories \
  -H 'content-type: application/json' \
  -d '{"tenant":"local","memory_type":"observation","content":"...","scope":"global","source_agent":"smoke","topics":["RUSTLANG"],"write_mode":"auto"}' | jq

# 3. Verify graph_edges target is the existing entity_id.
duckdb $MEM_DB_PATH "select to_node_id from graph_edges where relation='discusses'"

# 4. Migration smoke.
kill $SERVE_PID
cargo run -- repair --rebuild-graph
duckdb $MEM_DB_PATH "select count(*) from graph_edges where to_node_id like 'project:%'"  # must be 0
duckdb $MEM_DB_PATH "select count(*) from graph_edges where to_node_id like 'entity:%'"   # must be > 0
```

- [ ] **Step 5: Commit**

```bash
git add README.md AGENTS.md
git commit -m "docs(entity): document the registry surface

README gets a new section explaining entity_id resolution, the topics
field, alias normalization rules, the migration command, and the
HTTP-only / no-MCP convention. AGENTS.md gets a one-bullet summary
under Architecture mirroring the conversation-archive entry."
```

---

## Self-Review Checklist (run before declaring plan complete)

Run this checklist yourself; not a subagent dispatch.

**1. Spec coverage:**
- §Schema → Task 2 ✓
- §Domain Types → Task 3 ✓
- §Pipeline `entity_normalize` → Task 4 ✓
- §Storage `EntityRegistry` trait + impl → Task 5 ✓
- §Domain `MemoryRecord.topics` + storage round-trip → Task 6 ✓
- §Pipeline `extract_graph_edge_drafts` split → Task 7 ✓
- §Service `resolve_drafts_to_edges` + ingest wiring → Tasks 8 + 9 ✓
- §HTTP 4 routes → Task 10 ✓
- §CLI `mem repair --rebuild-graph` → Task 11 ✓
- §Verification + README + AGENTS.md → Task 12 ✓
- Concerns to Confirm #1 (composite-PK ON CONFLICT probe) → Task 1 ✓
- Concerns to Confirm #2 (schema_runner ALTER tolerance) → Task 2 ✓
- Concerns to Confirm #3 (legacy `extract_graph_edges` callers) → Task 7 (deprecated wrapper) + Task 9 (graph_store migration) ✓
- Risk #2 (`--rebuild-graph` loses valid_to history) → documented in Task 11 commit message ✓
- Risk #4 (legacy wrapper semantic drift) → documented at Task 7's `legacy_to_node_id` ✓

**2. Placeholder scan**: search for "TBD", "TODO", "implement later". Replace any with concrete content.

**3. Type consistency**: cross-check method signatures across tasks. Specifically:
- `EntityRegistry::resolve_or_create(&self, tenant: &str, alias: &str, kind: EntityKind, now: &str) -> Result<String, StorageError>` — used in Tasks 5, 8, 11. Same signature each time? Yes.
- `extract_graph_edge_drafts(memory: &MemoryRecord) -> Vec<GraphEdgeDraft>` — Tasks 7, 8, 11. Consistent.
- `resolve_drafts_to_edges(drafts, registry, tenant, now)` — Tasks 8, 11 (cli::repair calls it). Consistent.
- `EntityKind` variants — 5 each in Tasks 2 (CHECK constraint), 3 (enum), 4 (used in normalize tests, none directly), 5 (registry trait), 7 (drafts). Consistent: `Topic, Project, Repo, Module, Workflow`.

If any drift found, fix inline before committing.

---

## Execution Handoff

**Plan complete and saved to `docs/superpowers/plans/2026-05-02-entity-registry.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration. Same model as conversation-archive / transcript-recall.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

**Which approach?**
