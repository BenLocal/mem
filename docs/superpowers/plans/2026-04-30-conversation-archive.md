# Conversation Archive Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a parallel pipeline to mem that stores every Claude Code transcript block verbatim in a new `conversation_messages` table, with its own embedding queue and HNSW sidecar — completely isolated from the existing `memories` pipeline (no changes to ranking, lifecycle, verbatim guard, or compress).

**Architecture:** A new schema migration (`005_conversation_messages.sql`) introduces two tables. New domain types, repository methods, a copy of the embedding worker, a new HTTP module (`http/transcripts.rs`), and a new service module (`service/transcript_service.rs`) drive a `POST /transcripts/messages` ingest endpoint, a `POST /transcripts/search` semantic search endpoint, and a `GET /transcripts?session_id=…` retrieval endpoint. `cli/mine.rs` is extended so a single transcript scan writes to both `memories` (existing path, unchanged) and `conversation_messages` (new). `mem repair` learns to check/rebuild the second sidecar at `<MEM_DB_PATH>.transcripts.usearch`.

**Tech Stack:** Rust 2021, DuckDB (bundled), `usearch` HNSW, axum HTTP, tokio, `reqwest` (for `mem mine` CLI), `uuid::Uuid::now_v7()`, `serde_json`, integration tests in `tests/` against ephemeral DuckDB.

**Spec:** `docs/superpowers/specs/2026-04-30-conversation-archive-design.md` (commit `ca72cd7`).

---

## Conventions referenced throughout

- **Append-only schema files**: never edit `001_init.sql`–`004_sessions.sql`; new tables go in `005_conversation_messages.sql`.
- **Single-writer DB**: all writes serialize through `Arc<Mutex<Connection>>`. Never split a logical unit (insert message + enqueue job) across two acquisitions.
- **Verbatim**: `conversation_messages.content` stores the block text exactly as it appeared in the transcript; never trim, never reformat.
- **Consistent test pattern**: integration tests use `axum::Router` + ephemeral DuckDB, see `tests/sessions_integration.rs` and `tests/ingest_api.rs` for the existing test-app builder.
- **Commit scope tags**: `feat(transcripts)`, `test(transcripts)`, `fix(transcripts)`, `chore`, etc.

---

## File Structure (locked decisions)

**Created:**
- `db/schema/005_conversation_messages.sql`
- `src/domain/conversation_message.rs`
- `src/service/transcript_service.rs`
- `src/service/transcript_embedding_worker.rs`
- `src/http/transcripts.rs`
- `tests/conversation_archive.rs` (integration suite — schema, ingest, search, get)
- `tests/transcript_embedding_worker.rs`
- `tests/cli_mine_archive.rs` (extends cli_mine coverage to dual-sink behavior)

**Modified:**
- `src/domain/mod.rs` — re-export new types
- `src/storage/duckdb.rs` — new repo methods (or new submodule + delegation)
- `src/storage/mod.rs` — re-export
- `src/storage/vector_index_diagnose.rs` — generalize sidecar diagnose/rebuild to operate on `(table, sidecar_paths)` parameter
- `src/storage/vector_index.rs` — add `transcript_sidecar_paths()` helper
- `src/app.rs` — open second `VectorIndex`, spawn second worker
- `src/http/mod.rs` — mount new router
- `src/cli/mine.rs` — dual-sink (POST `/memories` + POST `/transcripts/messages`)
- `src/cli/repair.rs` — iterate both sidecars
- `src/config.rs` — new env vars (`MEM_TRANSCRIPT_EMBED_BATCH`, `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY`, `MEM_TRANSCRIPT_EMBED_DISABLED`)

**Untouched (verify in self-review):**
- `src/pipeline/{ingest,retrieve,compress,workflow,session}.rs`
- `src/domain/memory.rs`
- `src/service/memory_service.rs`
- `src/service/embedding_worker.rs` (only **read** for pattern; do not modify behavior)
- `src/mcp/server.rs` (MCP surface unchanged in v1)

---

## Task 1: Schema migration `005_conversation_messages.sql`

**Files:**
- Create: `db/schema/005_conversation_messages.sql`
- Test: `tests/conversation_archive.rs`

- [ ] **Step 1: Write the failing schema integration test**

Create `tests/conversation_archive.rs` with this initial test (more cases added in later tasks):

```rust
use mem::storage::DuckDbRepository;
use tempfile::TempDir;

#[tokio::test]
async fn schema_creates_conversation_messages_and_jobs_tables() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let _repo = DuckDbRepository::open(&db).await.unwrap();

    // Re-open a raw connection to introspect.
    let conn = duckdb::Connection::open(&db).unwrap();

    // Verify both tables exist.
    let cm: i64 = conn
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'conversation_messages'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cm, 1, "conversation_messages table should exist");

    let teq: i64 = conn
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'transcript_embedding_jobs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(teq, 1, "transcript_embedding_jobs table should exist");

    // Verify the unique constraint is present (insert two duplicates → second fails).
    conn.execute(
        "INSERT INTO conversation_messages \
         (message_block_id, tenant, caller_agent, transcript_path, line_number, block_index, role, block_type, content, embed_eligible, created_at) \
         VALUES ('m1','t','a','/p',1,0,'user','text','hi',true,'2026-04-30T00:00:00Z')",
        [],
    )
    .unwrap();
    let dup = conn.execute(
        "INSERT INTO conversation_messages \
         (message_block_id, tenant, caller_agent, transcript_path, line_number, block_index, role, block_type, content, embed_eligible, created_at) \
         VALUES ('m2','t','a','/p',1,0,'user','text','hi',true,'2026-04-30T00:00:00Z')",
        [],
    );
    assert!(dup.is_err(), "duplicate (transcript_path,line_number,block_index) should be rejected");
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test --test conversation_archive schema_creates -q
```
Expected: FAIL — "Table conversation_messages does not exist" or similar from DuckDB.

- [ ] **Step 3: Add the migration file**

Create `db/schema/005_conversation_messages.sql`:

```sql
-- Conversation archive: every block of every transcript message,
-- verbatim. Independent from memories table and its ranking/lifecycle.
-- See docs/superpowers/specs/2026-04-30-conversation-archive-design.md.
--
-- Note: DuckDB does not support inline REFERENCES via ALTER, but `CREATE
-- TABLE` does. session_id is intentionally declarative-only; the FK is
-- enforced at application level (mine.rs always passes a session_id that
-- the sessions table already knows about, since it comes from the
-- transcript). Same approach as memories.session_id (see 004_sessions.sql).

create table if not exists conversation_messages (
    message_block_id text primary key,
    session_id text,
    tenant text not null,
    caller_agent text not null,
    transcript_path text not null,
    line_number integer not null,
    block_index integer not null,
    message_uuid text,
    role text not null,
    block_type text not null,
    content text not null,
    tool_name text,
    tool_use_id text,
    embed_eligible boolean not null,
    created_at text not null,
    constraint conv_msg_role_check check (role in ('user','assistant','system')),
    constraint conv_msg_block_type_check check (block_type in ('text','tool_use','tool_result','thinking')),
    constraint conv_msg_uniq unique(transcript_path, line_number, block_index)
);

create index if not exists idx_conv_session_time
    on conversation_messages(session_id, created_at);

create index if not exists idx_conv_tenant_agent_time
    on conversation_messages(tenant, caller_agent, created_at);

create index if not exists idx_conv_tool_use_id
    on conversation_messages(tool_use_id);

-- Embedding queue: mirror of embedding_jobs but keyed to conversation_messages.
create table if not exists transcript_embedding_jobs (
    job_id text primary key,
    tenant text not null,
    message_block_id text not null,
    provider text not null,
    status text not null,
    attempt_count integer not null default 0,
    last_error text,
    available_at text not null,
    created_at text not null,
    updated_at text not null,
    constraint transcript_jobs_status_check check (
        status in ('pending', 'processing', 'completed', 'failed', 'stale')
    )
);

create index if not exists idx_transcript_jobs_poll
    on transcript_embedding_jobs(status, available_at);
create index if not exists idx_transcript_jobs_tenant_block
    on transcript_embedding_jobs(tenant, message_block_id);

-- Embedding storage: mirror of memory_embeddings but keyed to message_block_id.
create table if not exists conversation_message_embeddings (
    message_block_id text primary key,
    tenant text not null,
    embedding_model text not null,
    embedding_dim integer not null,
    embedding blob not null,
    content_hash text not null,
    source_updated_at text not null,
    created_at text not null,
    updated_at text not null
);

create index if not exists idx_conv_msg_emb_tenant on conversation_message_embeddings (tenant);
```

The migration file is automatically picked up by `src/storage/schema.rs` (verify by reading that file — if it embeds files via `include_str!`, add the new file to the list there).

- [ ] **Step 4: Wire migration into schema loader**

Read `src/storage/schema.rs` to find how 001-004 are loaded. Add `005_conversation_messages.sql` to the same list. Example pattern (verify against actual code):

```rust
const MIGRATIONS: &[(&str, &str)] = &[
    ("001_init.sql", include_str!("../../db/schema/001_init.sql")),
    ("002_embeddings.sql", include_str!("../../db/schema/002_embeddings.sql")),
    ("003_graph.sql", include_str!("../../db/schema/003_graph.sql")),
    ("004_sessions.sql", include_str!("../../db/schema/004_sessions.sql")),
    ("005_conversation_messages.sql", include_str!("../../db/schema/005_conversation_messages.sql")),
];
```

- [ ] **Step 5: Run the test to verify it passes**

```bash
cargo test --test conversation_archive schema_creates -q
```
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add db/schema/005_conversation_messages.sql src/storage/schema.rs tests/conversation_archive.rs
git commit -m "feat(transcripts): add conversation_messages and embedding queue schema"
```

---

## Task 2: Domain types

**Files:**
- Create: `src/domain/conversation_message.rs`
- Modify: `src/domain/mod.rs`

- [ ] **Step 1: Write the failing unit test**

In `src/domain/conversation_message.rs` (new file), at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_eligible_default_truth_table() {
        assert!(BlockType::Text.embed_eligible_default());
        assert!(BlockType::Thinking.embed_eligible_default());
        assert!(!BlockType::ToolUse.embed_eligible_default());
        assert!(!BlockType::ToolResult.embed_eligible_default());
    }

    #[test]
    fn role_serializes_lowercase() {
        let s = serde_json::to_string(&MessageRole::User).unwrap();
        assert_eq!(s, "\"user\"");
    }

    #[test]
    fn block_type_serializes_snake_case() {
        let s = serde_json::to_string(&BlockType::ToolUse).unwrap();
        assert_eq!(s, "\"tool_use\"");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test --lib conversation_message -q
```
Expected: FAIL with "module not found".

- [ ] **Step 3: Write the domain types**

Top of `src/domain/conversation_message.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationMessage {
    pub message_block_id: String,
    pub session_id: Option<String>,
    pub tenant: String,
    pub caller_agent: String,
    pub transcript_path: String,
    pub line_number: u64,
    pub block_index: u32,
    pub message_uuid: Option<String>,
    pub role: MessageRole,
    pub block_type: BlockType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub embed_eligible: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockType {
    Text,
    ToolUse,
    ToolResult,
    Thinking,
}

impl BlockType {
    pub fn embed_eligible_default(self) -> bool {
        matches!(self, BlockType::Text | BlockType::Thinking)
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            BlockType::Text => "text",
            BlockType::ToolUse => "tool_use",
            BlockType::ToolResult => "tool_result",
            BlockType::Thinking => "thinking",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "text" => Some(BlockType::Text),
            "tool_use" => Some(BlockType::ToolUse),
            "tool_result" => Some(BlockType::ToolResult),
            "thinking" => Some(BlockType::Thinking),
            _ => None,
        }
    }
}

impl MessageRole {
    pub fn as_db_str(self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "user" => Some(MessageRole::User),
            "assistant" => Some(MessageRole::Assistant),
            "system" => Some(MessageRole::System),
            _ => None,
        }
    }
}
```

In `src/domain/mod.rs`, add:

```rust
pub mod conversation_message;
pub use conversation_message::{BlockType, ConversationMessage, MessageRole};
```

(Match the existing re-export style in that file.)

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test --lib conversation_message -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
Expected: PASS, fmt clean, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/domain/conversation_message.rs src/domain/mod.rs
git commit -m "feat(transcripts): add ConversationMessage / BlockType / MessageRole domain types"
```

---

## Task 3: Repository — `create_conversation_message` with idempotent insert + job enqueue

**Files:**
- Modify: `src/storage/duckdb.rs` (add new methods directly; if file is already large, optionally split into `src/storage/transcript_repo.rs` and have `DuckDbRepository` delegate)
- Modify: `src/storage/mod.rs` (re-export new types if needed)
- Test: `tests/conversation_archive.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/conversation_archive.rs`:

```rust
use mem::domain::{BlockType, ConversationMessage, MessageRole};

fn sample_message(suffix: &str, embed: bool, block_type: BlockType) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("mb-{suffix}"),
        session_id: Some("sess-1".to_string()),
        tenant: "local".to_string(),
        caller_agent: "claude-code".to_string(),
        transcript_path: "/tmp/transcript.jsonl".to_string(),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type,
        content: format!("content-{suffix}"),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: embed,
        created_at: "2026-04-30T00:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn create_conversation_message_inserts_row_and_optionally_enqueues_job() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    // Eligible: row + job
    let m1 = sample_message("eligible", true, BlockType::Text);
    repo.create_conversation_message(&m1).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();
    let cm_count: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cm_count, 1);

    let job_count: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs WHERE message_block_id = 'mb-eligible'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(job_count, 1, "embed_eligible=true should enqueue a job");

    // Ineligible: row but no job
    let mut m2 = sample_message("ineligible", false, BlockType::ToolUse);
    m2.line_number = 2;
    repo.create_conversation_message(&m2).await.unwrap();

    let job_count_2: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs WHERE message_block_id = 'mb-ineligible'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(job_count_2, 0, "embed_eligible=false should not enqueue");
}

#[tokio::test]
async fn create_conversation_message_is_idempotent_on_unique_conflict() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let m = sample_message("first", true, BlockType::Text);
    repo.create_conversation_message(&m).await.unwrap();

    // Second call with same (transcript_path, line_number, block_index) but different message_block_id → no error, no second row, no second job
    let mut m2 = m.clone();
    m2.message_block_id = "mb-different-id".to_string();
    repo.create_conversation_message(&m2).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();
    let cm_count: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cm_count, 1);

    let job_count: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(job_count, 1, "no duplicate job on idempotent insert");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --test conversation_archive create_conversation -q
```
Expected: FAIL with "method not found `create_conversation_message`".

- [ ] **Step 3: Implement `create_conversation_message`**

In `src/storage/duckdb.rs`, find the `impl DuckDbRepository` block and add:

```rust
pub async fn create_conversation_message(
    &self,
    msg: &crate::domain::ConversationMessage,
) -> Result<(), StorageError> {
    let msg = msg.clone();
    let conn = self.connection.clone();
    let provider_id = self.embedding_provider_for_jobs.clone(); // see note below
    tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
        let conn = conn.lock().map_err(|_| StorageError::poisoned_lock())?;

        // Step 1: insert message row. Use ON CONFLICT to make idempotent.
        let inserted = conn
            .execute(
                "INSERT INTO conversation_messages (\
                    message_block_id, session_id, tenant, caller_agent, transcript_path, \
                    line_number, block_index, message_uuid, role, block_type, content, \
                    tool_name, tool_use_id, embed_eligible, created_at\
                 ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) \
                 ON CONFLICT (transcript_path, line_number, block_index) DO NOTHING",
                duckdb::params![
                    msg.message_block_id,
                    msg.session_id,
                    msg.tenant,
                    msg.caller_agent,
                    msg.transcript_path,
                    msg.line_number as i64,
                    msg.block_index as i64,
                    msg.message_uuid,
                    msg.role.as_db_str(),
                    msg.block_type.as_db_str(),
                    msg.content,
                    msg.tool_name,
                    msg.tool_use_id,
                    msg.embed_eligible,
                    msg.created_at,
                ],
            )
            .map_err(StorageError::from)?;

        // Step 2: enqueue embedding job ONLY if a new row was inserted AND embed_eligible.
        if inserted == 1 && msg.embed_eligible {
            let job_id = uuid::Uuid::now_v7().to_string();
            let now = current_timestamp_internal();
            conn.execute(
                "INSERT INTO transcript_embedding_jobs (\
                    job_id, tenant, message_block_id, provider, status, attempt_count, \
                    available_at, created_at, updated_at\
                 ) VALUES (?,?,?,?,?,?,?,?,?)",
                duckdb::params![
                    job_id,
                    msg.tenant,
                    msg.message_block_id,
                    provider_id,
                    "pending",
                    0,
                    now,
                    now,
                    now,
                ],
            )
            .map_err(StorageError::from)?;
        }

        Ok(())
    })
    .await
    .map_err(|e| StorageError::Internal(format!("join: {e}")))?
}
```

Notes for the implementer:
- `embedding_provider_for_jobs` is the same string used by `try_enqueue_embedding_job` for memories — read that method to find the exact field name (likely `self.embedding_provider` or passed via config). Match its convention.
- `current_timestamp_internal()` already exists in `duckdb.rs` (used by `try_enqueue_embedding_job`); reuse it. Do **not** duplicate `current_timestamp` from `embedding_worker.rs`.
- `StorageError::poisoned_lock()` — check the actual constructor in `error.rs`; rename if needed.
- DuckDB `ON CONFLICT (...) DO NOTHING` is supported in recent DuckDB versions; verify by reading any existing `INSERT … ON CONFLICT` in `duckdb.rs`. If not supported, fall back to: try INSERT; on `Err(duckdb::Error::DuckDBFailure(_, Some(msg)))` where `msg.contains("Duplicate key")` swallow and treat as `inserted = 0`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --test conversation_archive -q
cargo clippy --all-targets -- -D warnings
```
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/storage/duckdb.rs tests/conversation_archive.rs
git commit -m "feat(transcripts): repository create_conversation_message with idempotent insert + job enqueue"
```

---

## Task 4: Repository — `get_conversation_messages_by_session` and search helpers

**Files:**
- Modify: `src/storage/duckdb.rs`
- Test: `tests/conversation_archive.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/conversation_archive.rs`:

```rust
#[tokio::test]
async fn get_by_session_returns_time_ordered_blocks() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let mut m1 = sample_message("a", true, BlockType::Text);
    m1.created_at = "2026-04-30T00:00:02Z".to_string();
    m1.line_number = 1;

    let mut m2 = sample_message("b", true, BlockType::Text);
    m2.created_at = "2026-04-30T00:00:01Z".to_string();
    m2.line_number = 2;

    let mut m3 = sample_message("c", false, BlockType::ToolUse);
    m3.created_at = "2026-04-30T00:00:03Z".to_string();
    m3.line_number = 3;

    repo.create_conversation_message(&m1).await.unwrap();
    repo.create_conversation_message(&m2).await.unwrap();
    repo.create_conversation_message(&m3).await.unwrap();

    let out = repo
        .get_conversation_messages_by_session("local", "sess-1")
        .await
        .unwrap();

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_block_id, "mb-b"); // earliest
    assert_eq!(out[1].message_block_id, "mb-a");
    assert_eq!(out[2].message_block_id, "mb-c"); // latest
}

#[tokio::test]
async fn fetch_conversation_messages_by_ids_preserves_input_order() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    for (i, suffix) in ["x", "y", "z"].iter().enumerate() {
        let mut m = sample_message(suffix, true, BlockType::Text);
        m.line_number = (i + 1) as u64;
        repo.create_conversation_message(&m).await.unwrap();
    }

    // Search returns ranked by score, so we ask the repo to fetch in a specific order.
    let ids = vec!["mb-z".to_string(), "mb-x".to_string(), "mb-y".to_string()];
    let out = repo
        .fetch_conversation_messages_by_ids("local", &ids)
        .await
        .unwrap();

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_block_id, "mb-z");
    assert_eq!(out[1].message_block_id, "mb-x");
    assert_eq!(out[2].message_block_id, "mb-y");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --test conversation_archive get_by_session fetch_conversation -q
```
Expected: FAIL with "method not found".

- [ ] **Step 3: Implement both methods**

In `src/storage/duckdb.rs`:

```rust
pub async fn get_conversation_messages_by_session(
    &self,
    tenant: &str,
    session_id: &str,
) -> Result<Vec<crate::domain::ConversationMessage>, StorageError> {
    let tenant = tenant.to_string();
    let session_id = session_id.to_string();
    let conn = self.connection.clone();
    tokio::task::spawn_blocking(move || -> Result<Vec<crate::domain::ConversationMessage>, StorageError> {
        let conn = conn.lock().map_err(|_| StorageError::poisoned_lock())?;
        let mut stmt = conn
            .prepare(
                "SELECT message_block_id, session_id, tenant, caller_agent, transcript_path, \
                        line_number, block_index, message_uuid, role, block_type, content, \
                        tool_name, tool_use_id, embed_eligible, created_at \
                 FROM conversation_messages \
                 WHERE tenant = ? AND session_id = ? \
                 ORDER BY created_at ASC, line_number ASC, block_index ASC"
            )
            .map_err(StorageError::from)?;
        let rows = stmt
            .query_map(duckdb::params![tenant, session_id], row_to_conversation_message)
            .map_err(StorageError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)?;
        Ok(rows)
    })
    .await
    .map_err(|e| StorageError::Internal(format!("join: {e}")))?
}

pub async fn fetch_conversation_messages_by_ids(
    &self,
    tenant: &str,
    ids: &[String],
) -> Result<Vec<crate::domain::ConversationMessage>, StorageError> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let tenant = tenant.to_string();
    let ids = ids.to_vec();
    let conn = self.connection.clone();
    tokio::task::spawn_blocking(move || -> Result<Vec<crate::domain::ConversationMessage>, StorageError> {
        let conn = conn.lock().map_err(|_| StorageError::poisoned_lock())?;
        // Build IN clause; DuckDB params support array binding via repeat-? approach.
        let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT message_block_id, session_id, tenant, caller_agent, transcript_path, \
                    line_number, block_index, message_uuid, role, block_type, content, \
                    tool_name, tool_use_id, embed_eligible, created_at \
             FROM conversation_messages \
             WHERE tenant = ? AND message_block_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql).map_err(StorageError::from)?;
        let mut params: Vec<&dyn duckdb::ToSql> = Vec::with_capacity(ids.len() + 1);
        params.push(&tenant);
        for id in &ids {
            params.push(id);
        }
        let rows: Vec<crate::domain::ConversationMessage> = stmt
            .query_map(params.as_slice(), row_to_conversation_message)
            .map_err(StorageError::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)?;

        // Reorder to match input ids.
        let mut by_id: std::collections::HashMap<String, crate::domain::ConversationMessage> =
            rows.into_iter().map(|m| (m.message_block_id.clone(), m)).collect();
        let ordered: Vec<crate::domain::ConversationMessage> =
            ids.iter().filter_map(|id| by_id.remove(id)).collect();
        Ok(ordered)
    })
    .await
    .map_err(|e| StorageError::Internal(format!("join: {e}")))?
}

fn row_to_conversation_message(row: &duckdb::Row<'_>) -> duckdb::Result<crate::domain::ConversationMessage> {
    use crate::domain::{BlockType, ConversationMessage, MessageRole};
    let role_s: String = row.get(8)?;
    let bt_s: String = row.get(9)?;
    Ok(ConversationMessage {
        message_block_id: row.get(0)?,
        session_id: row.get(1)?,
        tenant: row.get(2)?,
        caller_agent: row.get(3)?,
        transcript_path: row.get(4)?,
        line_number: row.get::<_, i64>(5)? as u64,
        block_index: row.get::<_, i64>(6)? as u32,
        message_uuid: row.get(7)?,
        role: MessageRole::from_db_str(&role_s).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(8, duckdb::types::Type::Text, format!("invalid role: {role_s}").into())
        })?,
        block_type: BlockType::from_db_str(&bt_s).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(9, duckdb::types::Type::Text, format!("invalid block_type: {bt_s}").into())
        })?,
        content: row.get(10)?,
        tool_name: row.get(11)?,
        tool_use_id: row.get(12)?,
        embed_eligible: row.get(13)?,
        created_at: row.get(14)?,
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --test conversation_archive -q
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/storage/duckdb.rs tests/conversation_archive.rs
git commit -m "feat(transcripts): repository get-by-session and fetch-by-ids"
```

---

## Task 5: Repository — embedding job state machine + embedding upsert

**Files:**
- Modify: `src/storage/duckdb.rs`
- Test: `tests/conversation_archive.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/conversation_archive.rs`:

```rust
#[tokio::test]
async fn transcript_embedding_job_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let m = sample_message("life", true, BlockType::Text);
    repo.create_conversation_message(&m).await.unwrap();

    // Claim the next pending job.
    let now = "2026-04-30T00:00:00Z";
    let claimed = repo.claim_next_transcript_embedding_job(now, 5).await.unwrap();
    let job = claimed.expect("should have one pending job");
    assert_eq!(job.message_block_id, "mb-life");
    assert_eq!(job.tenant, "local");
    assert_eq!(job.attempt_count, 0);

    // Second claim: nothing pending (the previous one is now 'processing').
    let none = repo.claim_next_transcript_embedding_job(now, 5).await.unwrap();
    assert!(none.is_none());

    // Upsert embedding row.
    let blob = vec![0u8, 0, 128, 63, 0, 0, 0, 64]; // 1.0, 2.0 in LE f32 (sized for dim=2)
    repo.upsert_conversation_message_embedding(
        &job.message_block_id,
        &job.tenant,
        "fake-model",
        2,
        &blob,
        "fake-hash",
        &m.created_at,
        now,
    )
    .await
    .unwrap();

    // Complete the job.
    repo.complete_transcript_embedding_job(&job.job_id, now).await.unwrap();

    let conn = duckdb::Connection::open(&db).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM transcript_embedding_jobs WHERE job_id = ?",
            [&job.job_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "completed");
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test conversation_archive transcript_embedding_job_lifecycle -q
```
Expected: FAIL with missing methods.

- [ ] **Step 3: Implement the methods**

Add to `src/storage/duckdb.rs`. Pattern these on the existing `claim_next_embedding_job` / `complete_embedding_job` / `upsert_memory_embedding` / `permanently_fail_embedding_job` / `reschedule_embedding_job_failure` / `mark_embedding_job_stale` for memories — same SQL skeleton, different table names. Required additions:

```rust
#[derive(Debug, Clone)]
pub struct ClaimedTranscriptEmbeddingJob {
    pub job_id: String,
    pub tenant: String,
    pub message_block_id: String,
    pub provider: String,
    pub attempt_count: i64,
}

pub async fn claim_next_transcript_embedding_job(
    &self,
    now: &str,
    max_retries: u32,
) -> Result<Option<ClaimedTranscriptEmbeddingJob>, StorageError> { /* mirror of claim_next_embedding_job */ }

pub async fn complete_transcript_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> { /* mirror */ }

pub async fn permanently_fail_transcript_embedding_job(
    &self,
    job_id: &str,
    attempt: i64,
    error: &str,
    now: &str,
) -> Result<(), StorageError> { /* mirror */ }

pub async fn reschedule_transcript_embedding_job_failure(
    &self,
    job_id: &str,
    attempt: i64,
    error: &str,
    available_at: &str,
    now: &str,
) -> Result<(), StorageError> { /* mirror */ }

pub async fn mark_transcript_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> { /* mirror */ }

pub async fn get_transcript_embedding_job_status(&self, job_id: &str) -> Result<Option<String>, StorageError> { /* mirror */ }

#[allow(clippy::too_many_arguments)]
pub async fn upsert_conversation_message_embedding(
    &self,
    message_block_id: &str,
    tenant: &str,
    model: &str,
    dim: i64,
    blob: &[u8],
    content_hash: &str,
    source_updated_at: &str,
    now: &str,
) -> Result<(), StorageError> {
    // mirror of upsert_memory_embedding but writes to conversation_message_embeddings
}
```

Concrete SQL for the claim function (atomic UPDATE…RETURNING pattern; check existing `claim_next_embedding_job` for the exact phrasing — DuckDB supports `UPDATE … RETURNING` since v0.10):

```sql
UPDATE transcript_embedding_jobs
SET status = 'processing', updated_at = ?, attempt_count = attempt_count + 0
WHERE job_id = (
    SELECT job_id FROM transcript_embedding_jobs
    WHERE status = 'pending' AND available_at <= ? AND attempt_count < ?
    ORDER BY available_at ASC
    LIMIT 1
)
RETURNING job_id, tenant, message_block_id, provider, attempt_count
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test --test conversation_archive -q
cargo clippy --all-targets -- -D warnings
```
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/storage/duckdb.rs tests/conversation_archive.rs
git commit -m "feat(transcripts): repository embedding job state machine + embedding upsert"
```

---

## Task 6: Generalize sidecar paths and rebuild source for transcripts

**Files:**
- Modify: `src/storage/vector_index.rs`
- Modify: `src/storage/mod.rs` (re-export)
- Test: `tests/vector_index.rs` (extend)

- [ ] **Step 1: Write the failing test**

Append to `tests/vector_index.rs`:

```rust
use mem::storage::{transcript_sidecar_paths, sidecar_paths};
use std::path::PathBuf;

#[test]
fn transcript_sidecar_paths_uses_dotted_suffix() {
    let db = PathBuf::from("/foo/mem.duckdb");
    let (idx, meta) = transcript_sidecar_paths(&db);
    assert_eq!(idx, PathBuf::from("/foo/mem.duckdb.transcripts.usearch"));
    assert_eq!(meta, PathBuf::from("/foo/mem.duckdb.transcripts.usearch.meta.json"));

    // Memory sidecar paths must be unchanged.
    let (idx2, meta2) = sidecar_paths(&db);
    assert_eq!(idx2, PathBuf::from("/foo/mem.duckdb.usearch"));
    assert_eq!(meta2, PathBuf::from("/foo/mem.duckdb.usearch.meta.json"));
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test vector_index transcript_sidecar -q
```
Expected: FAIL — symbol not found.

- [ ] **Step 3: Implement `transcript_sidecar_paths`**

In `src/storage/vector_index.rs`, alongside the existing `sidecar_paths`:

```rust
/// Compute the sidecar file paths for the **transcript** vector index.
///
/// `<db>.transcripts.usearch` holds the binary index;
/// `<db>.transcripts.usearch.meta.json` holds the metadata.
pub fn transcript_sidecar_paths(db_path: &Path) -> (PathBuf, PathBuf) {
    let mut idx: OsString = db_path.as_os_str().to_owned();
    idx.push(".transcripts.usearch");
    let mut meta: OsString = db_path.as_os_str().to_owned();
    meta.push(".transcripts.usearch.meta.json");
    (PathBuf::from(idx), PathBuf::from(meta))
}
```

In `src/storage/mod.rs`, add to the `vector_index` re-export:

```rust
pub use vector_index::{
    sidecar_paths, transcript_sidecar_paths, EmbeddingRowSource, VectorIndex,
    VectorIndexError, VectorIndexFingerprint, VectorIndexMeta,
};
```

Also add a separate trait `TranscriptEmbeddingRowSource` (parallel to `EmbeddingRowSource`):

```rust
pub trait TranscriptEmbeddingRowSource {
    fn count_total_transcript_embeddings(&self) -> Result<i64, StorageError>;
    #[allow(clippy::type_complexity)]
    fn for_each_transcript_embedding(
        &self,
        batch: usize,
        f: &mut dyn FnMut(&str, &[u8]) -> Result<(), StorageError>,
    ) -> Result<(), StorageError>;
}
```

Implement this trait on `DuckDbRepository` in `src/storage/duckdb.rs` — the implementation is identical in shape to the memories `EmbeddingRowSource` impl but selects from `conversation_message_embeddings`. Read the existing impl to match exactly.

- [ ] **Step 4: Run all storage tests**

```bash
cargo test --test vector_index -q
cargo clippy --all-targets -- -D warnings
```
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index.rs src/storage/duckdb.rs src/storage/mod.rs tests/vector_index.rs
git commit -m "feat(transcripts): add transcript sidecar paths and TranscriptEmbeddingRowSource"
```

---

## Task 7: Transcript embedding worker

**Files:**
- Create: `src/service/transcript_embedding_worker.rs`
- Modify: `src/service/mod.rs` (re-export `pub mod transcript_embedding_worker;`)
- Test: `tests/transcript_embedding_worker.rs` (new)

- [ ] **Step 1: Write the failing integration test**

Create `tests/transcript_embedding_worker.rs`:

```rust
use mem::config::EmbeddingSettings;
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::FakeProvider;
use mem::service::transcript_embedding_worker;
use mem::storage::{transcript_sidecar_paths, DuckDbRepository, VectorIndex, VectorIndexFingerprint};
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn worker_processes_pending_transcript_jobs_and_writes_to_index() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("mem.duckdb");
    let repo = DuckDbRepository::open(&db).await.unwrap();

    let provider = Arc::new(FakeProvider::with_dim_and_value(3, vec![0.1, 0.2, 0.3]));
    let fp = VectorIndexFingerprint {
        provider: provider.name().to_string(),
        model: provider.model().to_string(),
        dim: provider.dim(),
    };
    let (idx_path, meta_path) = transcript_sidecar_paths(&db);
    let index = Arc::new(
        VectorIndex::open_or_rebuild_transcripts(&repo, &db, &fp).await.unwrap(),
    );

    // Ingest one eligible message.
    let msg = ConversationMessage {
        message_block_id: "mb-1".to_string(),
        session_id: Some("sess".to_string()),
        tenant: "local".to_string(),
        caller_agent: "claude-code".to_string(),
        transcript_path: "/tmp/t.jsonl".to_string(),
        line_number: 1,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type: BlockType::Text,
        content: "hello world".to_string(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: true,
        created_at: "2026-04-30T00:00:00Z".to_string(),
    };
    repo.create_conversation_message(&msg).await.unwrap();

    // Run one tick.
    let settings = EmbeddingSettings {
        worker_poll_interval_ms: 50,
        max_retries: 5,
        vector_index_flush_every: 1,
        ..EmbeddingSettings::default_for_test()
    };
    transcript_embedding_worker::tick(&repo, &*provider, &settings, &index).await.unwrap();

    // Job is completed and index has 1 row.
    let conn = duckdb::Connection::open(&db).unwrap();
    let status: String = conn
        .query_row("SELECT status FROM transcript_embedding_jobs LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(index.size(), 1);
}

#[tokio::test]
async fn worker_failure_does_not_affect_memories_pipeline() {
    // Sketch: ingest one memory (via existing repo path) AND one transcript message.
    // Inject a transcript provider that errors.
    // Run one tick of the memories worker (succeeds) and one of the transcript worker (fails).
    // Assert: memories embedding_jobs row reaches 'completed';
    //         transcript_embedding_jobs reaches 'pending' with attempt_count=1 OR 'failed' if max_retries=1.
    // Implementation cribs the test-app pattern from tests/embedding_worker.rs.
    // … see tests/embedding_worker.rs for the exact harness …
}
```

(`open_or_rebuild_transcripts` and `FakeProvider::with_dim_and_value` may need small companion additions — see steps 3 and notes below.)

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test transcript_embedding_worker -q
```
Expected: FAIL — symbols missing.

- [ ] **Step 3: Add `VectorIndex::open_or_rebuild_transcripts`**

In `src/storage/vector_index.rs`, add a parallel constructor that takes a `TranscriptEmbeddingRowSource` and uses transcript sidecar paths. The body is identical in shape to `open_or_rebuild`, only:
- `transcript_sidecar_paths(db_path)` instead of `sidecar_paths`
- Iterates `for_each_transcript_embedding` from the new trait

Refactor opportunity (low priority — only if you're touching the function anyway): factor the common rebuild loop into a private generic helper. If the existing function is already long and adding a clone makes it longer, the factor is worth it. If it's a clean copy, leave it for a future refactor PR.

- [ ] **Step 4: Implement the worker**

Create `src/service/transcript_embedding_worker.rs`:

```rust
use std::sync::Arc;

use crate::config::EmbeddingSettings;
use crate::embedding::EmbeddingProvider;
use crate::storage::{DuckDbRepository, StorageError, VectorIndex, VectorIndexError};
use tracing::{error, info, warn};

pub async fn run(
    repo: DuckDbRepository,
    provider: Arc<dyn EmbeddingProvider>,
    settings: EmbeddingSettings,
    index: Arc<VectorIndex>,
) {
    info!(
        provider = provider.name(),
        model = provider.model(),
        dim = provider.dim(),
        poll_interval_ms = settings.worker_poll_interval_ms,
        "transcript embedding worker started"
    );
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        settings.worker_poll_interval_ms.max(1),
    ));
    loop {
        interval.tick().await;
        if let Err(err) = tick(&repo, provider.as_ref(), &settings, &index).await {
            error!(error = %err, "transcript embedding worker tick failed");
        }
    }
}

pub async fn tick(
    repo: &DuckDbRepository,
    provider: &dyn EmbeddingProvider,
    settings: &EmbeddingSettings,
    index: &VectorIndex,
) -> Result<(), StorageError> {
    let now = current_timestamp();
    let Some(job) = repo
        .claim_next_transcript_embedding_job(&now, settings.max_retries)
        .await?
    else {
        return Ok(());
    };

    if job.provider != settings.job_provider_id() {
        let now = current_timestamp();
        repo.permanently_fail_transcript_embedding_job(
            &job.job_id,
            job.attempt_count + 1,
            "transcript embedding job provider does not match runtime configuration",
            &now,
        )
        .await?;
        return Ok(());
    }

    let Some(message) = repo
        .get_conversation_message_by_id(&job.tenant, &job.message_block_id)
        .await?
    else {
        let now = current_timestamp();
        repo.permanently_fail_transcript_embedding_job(
            &job.job_id,
            job.attempt_count + 1,
            "conversation message row missing for embedding job",
            &now,
        )
        .await?;
        return Ok(());
    };

    let text = message.content.clone();
    let embedding = match provider.embed_text(&text).await {
        Ok(v) => v,
        Err(err) => {
            record_failure(repo, &job, settings, &err.to_string()).await?;
            return Ok(());
        }
    };

    if embedding.len() != provider.dim() {
        record_failure(
            repo,
            &job,
            settings,
            &format!(
                "provider returned length {} (expected {})",
                embedding.len(),
                provider.dim()
            ),
        )
        .await?;
        return Ok(());
    }

    if repo.get_transcript_embedding_job_status(&job.job_id).await?.as_deref() != Some("processing") {
        return Ok(());
    }

    let blob = f32_slice_to_blob(&embedding);
    let now = current_timestamp();
    let content_hash = sha2_hex(&text);
    repo.upsert_conversation_message_embedding(
        &job.message_block_id,
        &job.tenant,
        provider.model(),
        provider.dim() as i64,
        &blob,
        &content_hash,
        &message.created_at,
        &now,
    )
    .await?;

    match index.upsert(&job.message_block_id, &embedding).await {
        Ok(()) => {
            let count = index.dirty_count_increment();
            if count >= settings.vector_index_flush_every {
                if let Err(err) = index.save_at_default_paths().await {
                    warn!(error = %err, "transcript vector index periodic save failed");
                } else {
                    index.dirty_count_reset();
                }
            }
        }
        Err(VectorIndexError::HashCollision { existing, incoming }) => {
            let now = current_timestamp();
            let msg = format!("transcript vector_index hash collision: {existing} vs {incoming}");
            error!(message_block_id = %job.message_block_id, error = %msg, "hash collision; permanently failing");
            repo.permanently_fail_transcript_embedding_job(&job.job_id, job.attempt_count + 1, &msg, &now).await?;
            return Ok(());
        }
        Err(err) => {
            warn!(
                job_id = %job.job_id,
                message_block_id = %job.message_block_id,
                error = %err,
                "transcript vector index upsert failed; embedding row already written"
            );
        }
    }

    repo.complete_transcript_embedding_job(&job.job_id, &now).await?;
    info!(job_id = %job.job_id, "transcript embedding worker completed job");
    Ok(())
}

async fn record_failure(
    repo: &DuckDbRepository,
    job: &crate::storage::ClaimedTranscriptEmbeddingJob,
    settings: &EmbeddingSettings,
    message: &str,
) -> Result<(), StorageError> {
    // Pattern-identical to the memories worker; see service/embedding_worker.rs::record_failure.
    // (Inline the same backoff schedule: 60s, 300s, 1800s.)
    todo!("paste-equivalent of memories record_failure, but call transcript variants")
}

fn failure_backoff_ms(attempt_after_fail: i64) -> u128 {
    match attempt_after_fail {
        1 => 60_000,
        2 => 300_000,
        _ => 1_800_000,
    }
}

fn truncate_error(message: &str) -> String {
    const MAX: usize = 2000;
    if message.len() <= MAX {
        message.to_string()
    } else {
        message.chars().take(MAX).collect()
    }
}

fn f32_slice_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_ne_bytes());
    }
    out
}

fn sha2_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn current_timestamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
}
```

Important: the `todo!()` in `record_failure` MUST be replaced before the test passes — paste the body from `service/embedding_worker.rs::record_failure`, swap method names to the transcript variants. No actual `todo!` should ship.

Also add `repo.get_conversation_message_by_id(&tenant, &message_block_id)` — a singleton fetch that returns `Result<Option<ConversationMessage>, StorageError>`. SQL is straightforward: `SELECT … FROM conversation_messages WHERE tenant = ? AND message_block_id = ?`. Reuse `row_to_conversation_message`.

In `src/service/mod.rs` add: `pub mod transcript_embedding_worker;`.

- [ ] **Step 5: Run the worker tests to verify they pass**

```bash
cargo test --test transcript_embedding_worker -q
cargo clippy --all-targets -- -D warnings
```
Expected: PASS, clippy clean. The "isolation from memories" test sketched in step 1 must be filled in based on the `tests/embedding_worker.rs` harness — port its setup and assert that running both workers concurrently with one provider failing only affects its own queue.

- [ ] **Step 6: Commit**

```bash
git add src/service/transcript_embedding_worker.rs src/service/mod.rs src/storage/duckdb.rs src/storage/vector_index.rs tests/transcript_embedding_worker.rs
git commit -m "feat(transcripts): add transcript embedding worker"
```

---

## Task 8: Wire second worker + vector index in `app.rs`

**Files:**
- Modify: `src/app.rs`
- Modify: `src/config.rs` (add `MEM_TRANSCRIPT_EMBED_DISABLED`, `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY`)

- [ ] **Step 1: Read existing `app.rs::AppState::from_config` to find injection points**

Already explored. The two injection points:
- after `let vector_index = Arc::new(VectorIndex::open_or_rebuild(…))` → also open the transcript index
- after the existing `tokio::spawn(... embedding_worker::run …)` → also spawn the transcript worker (unless disabled)

- [ ] **Step 2: Extend `AppState::from_config`**

Insert this block after the existing memories vector_index initialization:

```rust
let transcript_index = Arc::new(
    VectorIndex::open_or_rebuild_transcripts(&repository, &config.db_path, &fp).await?,
);
info!(
    size = transcript_index.size(),
    "transcript vector index ready"
);
```

After the existing `tokio::spawn(... embedding_worker::run …)`, append:

```rust
if !config.embedding.transcript_disabled {
    let provider_transcript = provider.clone();
    let repo_transcript = repository.clone();
    let transcript_settings = config.embedding.clone(); // same provider config
    let transcript_index_for_worker = transcript_index.clone();
    tokio::spawn(async move {
        crate::service::transcript_embedding_worker::run(
            repo_transcript,
            provider_transcript,
            transcript_settings,
            transcript_index_for_worker,
        )
        .await;
    });
}
```

Add `transcript_index: Arc<VectorIndex>` to `AppState` struct so the search route handler can reach it:

```rust
#[derive(Clone)]
pub struct AppState {
    pub memory_service: MemoryService,
    pub transcript_service: TranscriptService,
    pub config: crate::config::Config,
}
```

(`TranscriptService` is the new service we'll wire in Task 9.)

- [ ] **Step 3: Add config fields**

In `src/config.rs`, find `EmbeddingSettings`. Add:

```rust
pub transcript_disabled: bool,
pub transcript_vector_index_flush_every: usize,
```

In its `from_env` builder, read:

```rust
transcript_disabled: std::env::var("MEM_TRANSCRIPT_EMBED_DISABLED")
    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    .unwrap_or(false),
transcript_vector_index_flush_every: std::env::var("MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY")
    .ok()
    .and_then(|s| s.parse().ok())
    .filter(|&n: &usize| n > 0)
    .unwrap_or(256),
```

The transcript worker should consume `transcript_vector_index_flush_every`, not the memories `vector_index_flush_every`. Update `transcript_embedding_worker::tick` accordingly: instead of `settings.vector_index_flush_every`, use a transcript-specific field. The simplest approach is to clone the settings for the worker and override that one field:

```rust
let mut transcript_settings = config.embedding.clone();
transcript_settings.vector_index_flush_every = config.embedding.transcript_vector_index_flush_every;
```

- [ ] **Step 4: Run all tests**

```bash
cargo test -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
Expected: PASS. (`AppState`'s `transcript_service` field is `pub` but unused yet; clippy may warn — leave a `#[allow(dead_code)]` on the field temporarily, or wire Task 9 first if you prefer.)

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/config.rs
git commit -m "feat(transcripts): spawn transcript worker and open transcript vector index"
```

---

## Task 9: Service + HTTP routes

**Files:**
- Create: `src/service/transcript_service.rs`
- Create: `src/http/transcripts.rs`
- Modify: `src/service/mod.rs` (add `pub mod transcript_service; pub use transcript_service::TranscriptService;`)
- Modify: `src/http/mod.rs` (mount router)
- Test: `tests/conversation_archive.rs` (HTTP integration tests)

- [ ] **Step 1: Write the failing HTTP integration test**

Append to `tests/conversation_archive.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use mem::app::router_with_config;
use mem::config::Config;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn build_test_app(db_dir: &TempDir) -> axum::Router {
    let mut cfg = Config::local();
    cfg.db_path = db_dir.path().join("mem.duckdb");
    router_with_config(cfg).await.unwrap()
}

#[tokio::test]
async fn post_transcripts_messages_creates_a_row() {
    let dir = TempDir::new().unwrap();
    let app = build_test_app(&dir).await;

    let body = json!({
        "session_id": "sess-1",
        "tenant": "local",
        "caller_agent": "claude-code",
        "transcript_path": "/tmp/t.jsonl",
        "line_number": 1,
        "block_index": 0,
        "role": "assistant",
        "block_type": "text",
        "content": "hello",
        "embed_eligible": true,
        "created_at": "2026-04-30T00:00:00Z"
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcripts/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v["message_block_id"].is_string());
}

#[tokio::test]
async fn get_transcripts_by_session_returns_blocks() {
    let dir = TempDir::new().unwrap();
    let app = build_test_app(&dir).await;

    // Seed two blocks via POST.
    for i in 0..2 {
        let body = json!({
            "session_id": "sess-X",
            "tenant": "local",
            "caller_agent": "claude-code",
            "transcript_path": "/tmp/t.jsonl",
            "line_number": i + 1,
            "block_index": 0,
            "role": "user",
            "block_type": "text",
            "content": format!("msg-{i}"),
            "embed_eligible": false,
            "created_at": format!("2026-04-30T00:00:0{i}Z")
        });
        let resp = app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/transcripts/messages")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Fetch.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/transcripts?session_id=sess-X&tenant=local")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let messages = v["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"], "msg-0");
}

#[tokio::test]
async fn post_transcripts_search_filters_by_role_and_block_type() {
    // Seed: 1 user/text, 1 assistant/text, 1 assistant/tool_use.
    // POST /transcripts/search with role="user" → expect 1 hit (user/text).
    // POST with block_type="tool_use" → expect 1 hit.
    // Use empty query string + tenant filter to avoid needing real embeddings;
    // the service falls back to recent-time ordering when query is empty.
    // (See implementation note in step 3.)
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --test conversation_archive post_transcripts get_transcripts -q
```
Expected: FAIL — routes 404.

- [ ] **Step 3: Implement service and HTTP**

`src/service/transcript_service.rs`:

```rust
use std::sync::Arc;

use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::embedding::EmbeddingProvider;
use crate::storage::{DuckDbRepository, StorageError, VectorIndex};

#[derive(Clone)]
pub struct TranscriptService {
    repo: DuckDbRepository,
    index: Arc<VectorIndex>,
    provider: Option<Arc<dyn EmbeddingProvider>>,
}

#[derive(Debug, Clone)]
pub struct TranscriptSearchHit {
    pub message: ConversationMessage,
    pub score: f32,
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptSearchFilters {
    pub session_id: Option<String>,
    pub role: Option<MessageRole>,
    pub block_type: Option<BlockType>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
}

impl TranscriptService {
    pub fn new(
        repo: DuckDbRepository,
        index: Arc<VectorIndex>,
        provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self { repo, index, provider }
    }

    pub async fn ingest(&self, msg: ConversationMessage) -> Result<(), StorageError> {
        self.repo.create_conversation_message(&msg).await
    }

    pub async fn get_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.repo.get_conversation_messages_by_session(tenant, session_id).await
    }

    pub async fn search(
        &self,
        tenant: &str,
        query: &str,
        filters: &TranscriptSearchFilters,
        limit: usize,
    ) -> Result<Vec<TranscriptSearchHit>, StorageError> {
        // Phase 1: candidates from HNSW (semantic) OR from recent-time SQL (lexical/empty-query path).
        let candidates: Vec<(String, f32)> = if !query.trim().is_empty() {
            if let Some(provider) = &self.provider {
                let q_vec = provider
                    .embed_text(query)
                    .await
                    .map_err(|e| StorageError::Internal(format!("query embed: {e}")))?;
                self.index
                    .search(&q_vec, limit.max(1) * 4) // oversample to leave headroom for filters
                    .await
                    .map_err(|e| StorageError::VectorIndex(e.to_string()))?
            } else {
                vec![]
            }
        } else {
            // Empty query path: pull recent messages from SQL.
            self.repo
                .recent_conversation_messages(tenant, limit.max(1) * 4)
                .await?
                .into_iter()
                .map(|m| (m.message_block_id, 0.0))
                .collect()
        };

        // Phase 2: hydrate by id (preserves rank order from HNSW).
        let ids: Vec<String> = candidates.iter().map(|(id, _)| id.clone()).collect();
        let hydrated = self
            .repo
            .fetch_conversation_messages_by_ids(tenant, &ids)
            .await?;

        // Phase 3: filter and zip with score.
        let scores: std::collections::HashMap<String, f32> =
            candidates.into_iter().collect();
        let hits: Vec<TranscriptSearchHit> = hydrated
            .into_iter()
            .filter(|m| filters.session_id.as_ref().is_none_or(|s| m.session_id.as_deref() == Some(s)))
            .filter(|m| filters.role.is_none_or(|r| m.role == r))
            .filter(|m| filters.block_type.is_none_or(|b| m.block_type == b))
            .filter(|m| filters.time_from.as_ref().is_none_or(|t| m.created_at.as_str() >= t.as_str()))
            .filter(|m| filters.time_to.as_ref().is_none_or(|t| m.created_at.as_str() <= t.as_str()))
            .take(limit)
            .map(|m| {
                let score = *scores.get(&m.message_block_id).unwrap_or(&0.0);
                TranscriptSearchHit { message: m, score }
            })
            .collect();
        Ok(hits)
    }
}
```

Add `recent_conversation_messages(tenant, limit)` in `duckdb.rs` — `SELECT … ORDER BY created_at DESC LIMIT ?`.

`src/http/transcripts.rs`:

```rust
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::service::transcript_service::{TranscriptSearchFilters, TranscriptSearchHit};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/transcripts/messages", post(post_message))
        .route("/transcripts/search", post(post_search))
        .route("/transcripts", get(get_by_session))
}

#[derive(Debug, Deserialize)]
pub struct IngestRequest {
    pub session_id: Option<String>,
    pub tenant: String,
    pub caller_agent: String,
    pub transcript_path: String,
    pub line_number: u64,
    pub block_index: u32,
    pub message_uuid: Option<String>,
    pub role: MessageRole,
    pub block_type: BlockType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub embed_eligible: bool,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct IngestResponse {
    pub message_block_id: String,
}

async fn post_message(
    State(state): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> Result<Json<IngestResponse>, (StatusCode, String)> {
    let id = uuid::Uuid::now_v7().to_string();
    let msg = ConversationMessage {
        message_block_id: id.clone(),
        session_id: req.session_id,
        tenant: req.tenant,
        caller_agent: req.caller_agent,
        transcript_path: req.transcript_path,
        line_number: req.line_number,
        block_index: req.block_index,
        message_uuid: req.message_uuid,
        role: req.role,
        block_type: req.block_type,
        content: req.content,
        tool_name: req.tool_name,
        tool_use_id: req.tool_use_id,
        embed_eligible: req.embed_eligible,
        created_at: req.created_at,
    };
    state.transcript_service
        .ingest(msg)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(IngestResponse { message_block_id: id }))
}

#[derive(Debug, Deserialize)]
pub struct GetBySessionQuery {
    pub session_id: String,
    pub tenant: String,
}

#[derive(Debug, Serialize)]
pub struct GetBySessionResponse {
    pub messages: Vec<ConversationMessage>,
}

async fn get_by_session(
    State(state): State<AppState>,
    Query(q): Query<GetBySessionQuery>,
) -> Result<Json<GetBySessionResponse>, (StatusCode, String)> {
    let messages = state.transcript_service
        .get_by_session(&q.tenant, &q.session_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(GetBySessionResponse { messages }))
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub tenant: String,
    pub session_id: Option<String>,
    pub role: Option<MessageRole>,
    pub block_type: Option<BlockType>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize { 20 }

#[derive(Debug, Serialize)]
pub struct SearchHitDto {
    pub message_block_id: String,
    pub session_id: Option<String>,
    pub role: MessageRole,
    pub block_type: BlockType,
    pub content: String,
    pub created_at: String,
    pub score: f32,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHitDto>,
}

async fn post_search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, String)> {
    let filters = TranscriptSearchFilters {
        session_id: req.session_id,
        role: req.role,
        block_type: req.block_type,
        time_from: req.time_from,
        time_to: req.time_to,
    };
    let hits = state.transcript_service
        .search(&req.tenant, &req.query, &filters, req.limit)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let dtos = hits
        .into_iter()
        .map(|h| SearchHitDto {
            message_block_id: h.message.message_block_id,
            session_id: h.message.session_id,
            role: h.message.role,
            block_type: h.message.block_type,
            content: h.message.content,
            created_at: h.message.created_at,
            score: h.score,
        })
        .collect();
    Ok(Json(SearchResponse { hits: dtos }))
}
```

Update `src/http/mod.rs`:

```rust
pub mod embeddings;
pub mod graph;
pub mod health;
pub mod logging;
pub mod memory;
pub mod review;
pub mod transcripts;     // <-- new

use axum::{middleware, Router};
use crate::app::AppState;

pub fn router() -> Router<AppState> {
    Router::<AppState>::new()
        .merge(health::router::<AppState>())
        .merge(memory::router())
        .merge(embeddings::router())
        .merge(review::router())
        .merge(graph::router())
        .merge(transcripts::router())  // <-- new
        .layer(middleware::from_fn(logging::log_request_response))
}
```

Update `src/service/mod.rs` — add `pub mod transcript_service; pub use transcript_service::TranscriptService;`.

Update `AppState::from_config` to actually construct `TranscriptService`:

```rust
let transcript_service = TranscriptService::new(
    repository.clone(),
    transcript_index.clone(),
    Some(provider.clone()),
);
```

…and pass it into `AppState { … transcript_service, … }`.

- [ ] **Step 4: Run the HTTP integration tests**

```bash
cargo test --test conversation_archive -q
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
Expected: PASS, clippy clean, fmt clean.

- [ ] **Step 5: Commit**

```bash
git add src/service/transcript_service.rs src/service/mod.rs src/http/transcripts.rs src/http/mod.rs src/app.rs src/storage/duckdb.rs tests/conversation_archive.rs
git commit -m "feat(transcripts): service + HTTP routes (POST /transcripts/messages, /transcripts/search, GET /transcripts)"
```

---

## Task 10: Extend `cli/mine.rs` for dual-sink

**Files:**
- Modify: `src/cli/mine.rs`
- Test: `tests/cli_mine_archive.rs` (new), or extend `tests/cli_mine.rs`

- [ ] **Step 1: Write the failing test**

Create `tests/cli_mine_archive.rs`:

```rust
use mem::app::router_with_config;
use mem::config::Config;
use std::io::Write;
use tempfile::{NamedTempFile, TempDir};

fn write_transcript_fixture(file: &mut NamedTempFile) {
    // Three lines: 1 user text, 1 assistant text+tool_use, 1 user tool_result
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"please read README.md"}]},"sessionId":"S1","timestamp":"2026-04-30T00:00:00Z"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll remember: this is a Rust project"},{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"README.md"}}]},"sessionId":"S1","timestamp":"2026-04-30T00:00:01Z"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"…file contents…"}]},"sessionId":"S1","timestamp":"2026-04-30T00:00:02Z"}"#,
    ];
    for l in lines {
        writeln!(file, "{l}").unwrap();
    }
}

#[tokio::test]
async fn mine_writes_to_both_memories_and_conversation_messages() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");
    let _app = router_with_config(cfg.clone()).await.unwrap();
    // Spin up an axum server on a random port for `mem mine` to POST to.
    let app = router_with_config(cfg.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    let mut transcript = NamedTempFile::new().unwrap();
    write_transcript_fixture(&mut transcript);

    let args = mem::cli::mine::MineArgs {
        transcript_path: transcript.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url: format!("http://{}", addr),
    };
    let code = mem::cli::mine::run(args).await;
    assert_eq!(code, 0);

    // Assertions: 5 conversation_messages rows (1 user-text + 1 assistant-text + 1 assistant-tool_use + 1 user-tool_result; tool_use is one block; tool_result is one block).
    let conn = duckdb::Connection::open(&cfg.db_path).unwrap();
    let cm: i64 = conn
        .query_row("SELECT count(*) FROM conversation_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cm, 4, "should have 4 blocks");

    // memories should have at least 1 row (the "I'll remember:" extracted phrase).
    let m: i64 = conn
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert!(m >= 1, "extraction path still works");

    // transcript_embedding_jobs: 2 (1 user text + 1 assistant text). tool_use / tool_result are NOT embed-eligible.
    let jobs: i64 = conn
        .query_row("SELECT count(*) FROM transcript_embedding_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jobs, 2);
}

#[tokio::test]
async fn mine_is_idempotent_at_block_level() {
    // Same setup as above, run mine twice, assert counts unchanged.
    // (Cribs the spin-up code; factor into a helper if duplication grows.)
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test --test cli_mine_archive -q
```
Expected: FAIL — `mem mine` does not yet write to `conversation_messages`.

- [ ] **Step 3: Extend `cli/mine.rs`**

Refactor the existing `parse_transcript` + `run`:

```rust
// New struct: every block of every message becomes one of these.
pub struct ArchivedBlock {
    pub session_id: String,
    pub timestamp: String,
    pub line_number: usize,
    pub block_index: usize,
    pub message_uuid: Option<String>,
    pub role: String,
    pub block_type: String,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
}

pub fn parse_transcript_full(path: &Path) -> Result<(Vec<ExtractedMemory>, Vec<ArchivedBlock>)> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut memories = Vec::new();
    let mut blocks = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let line_number = line_num + 1;
        let session_id = value["sessionId"].as_str().unwrap_or("").to_string();
        let timestamp = value["timestamp"].as_str().unwrap_or("").to_string();
        let message_uuid = value["uuid"].as_str().map(str::to_string);
        let role = value["message"]["role"].as_str().unwrap_or("user").to_string();

        let Some(content_array) = value["message"]["content"].as_array() else { continue; };

        for (block_index, item) in content_array.iter().enumerate() {
            let block_type = item["type"].as_str().unwrap_or("text").to_string();
            let (content, tool_name, tool_use_id) = match block_type.as_str() {
                "text" => (item["text"].as_str().unwrap_or("").to_string(), None, None),
                "thinking" => (item["thinking"].as_str().unwrap_or("").to_string(), None, None),
                "tool_use" => (
                    serde_json::to_string(&item["input"]).unwrap_or_default(),
                    item["name"].as_str().map(str::to_string),
                    item["id"].as_str().map(str::to_string),
                ),
                "tool_result" => (
                    item["content"].as_str().unwrap_or("").to_string(),
                    None,
                    item["tool_use_id"].as_str().map(str::to_string),
                ),
                _ => (line.clone(), None, None), // unknown; verbatim store the whole line
            };

            // Existing extract path: only assistant + text gets fed to extract_memory.
            if role == "assistant" && block_type == "text" {
                if let Some(extracted) = extract_memory(&content) {
                    memories.push(ExtractedMemory {
                        content: extracted,
                        session_id: session_id.clone(),
                        timestamp: timestamp.clone(),
                        line_number,
                    });
                }
            }

            blocks.push(ArchivedBlock {
                session_id: session_id.clone(),
                timestamp: timestamp.clone(),
                line_number,
                block_index,
                message_uuid: message_uuid.clone(),
                role: role.clone(),
                block_type,
                content,
                tool_name,
                tool_use_id,
            });
        }
    }

    Ok((memories, blocks))
}
```

Update the legacy `parse_transcript` to wrap `parse_transcript_full` and discard `blocks` (preserves backward compat for any caller that still uses it; OR delete the old fn and migrate callers).

In `run`:

```rust
pub async fn run(args: MineArgs) -> i32 {
    let (memories, blocks) = match parse_transcript_full(&args.transcript_path) {
        Ok(t) => t,
        Err(e) => { eprintln!("Failed to parse transcript: {}", e); return 1; }
    };

    let client = reqwest::Client::new();
    let mut mem_ok = 0;
    let mut mem_fail = 0;
    let mut block_ok = 0;
    let mut block_fail = 0;

    // [既有] 抽取关键句 → /memories
    for memory in memories {
        let idempotency_key = format!("{}:{}", args.transcript_path.display(), memory.line_number);
        let payload = serde_json::json!({
            "tenant": args.tenant,
            "memory_type": "experience",
            "content": memory.content,
            "scope": "global",
            "source_agent": args.agent,
            "idempotency_key": idempotency_key,
            "write_mode": "auto",
        });
        match client.post(format!("{}/memories", args.base_url)).json(&payload).send().await {
            Ok(r) if r.status().is_success() || r.status() == 409 => mem_ok += 1,
            Ok(r) => { eprintln!("memory POST {}", r.status()); mem_fail += 1; }
            Err(e) => { eprintln!("memory POST {}", e); mem_fail += 1; }
        }
    }

    // [新增] 全量 block → /transcripts/messages
    for b in blocks {
        let block_type_str = b.block_type.clone();
        let embed_eligible = matches!(block_type_str.as_str(), "text" | "thinking");
        let payload = serde_json::json!({
            "session_id": b.session_id,
            "tenant": args.tenant,
            "caller_agent": args.agent,
            "transcript_path": args.transcript_path.display().to_string(),
            "line_number": b.line_number,
            "block_index": b.block_index,
            "message_uuid": b.message_uuid,
            "role": b.role,
            "block_type": block_type_str,
            "content": b.content,
            "tool_name": b.tool_name,
            "tool_use_id": b.tool_use_id,
            "embed_eligible": embed_eligible,
            "created_at": b.timestamp,
        });
        match client.post(format!("{}/transcripts/messages", args.base_url)).json(&payload).send().await {
            Ok(r) if r.status().is_success() => block_ok += 1,
            Ok(r) => { eprintln!("transcript POST {}", r.status()); block_fail += 1; }
            Err(e) => { eprintln!("transcript POST {}", e); block_fail += 1; }
        }
    }

    println!(
        "Mined: memories={}/{} blocks={}/{}",
        mem_ok, mem_ok + mem_fail, block_ok, block_ok + block_fail
    );
    if mem_fail > 0 || block_fail > 0 { 1 } else { 0 }
}
```

Note: the `role` strings produced here (`"user" | "assistant" | "system"`) and the `block_type` strings (`"text" | "tool_use" | "tool_result" | "thinking"`) match the `MessageRole` and `BlockType` `serde(rename_all)` outputs from Task 2 — verify by running the existing role serialization test.

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test --test cli_mine_archive -q
cargo test --test cli_mine -q   # ensure existing test suite still green
cargo clippy --all-targets -- -D warnings
```
Expected: PASS for both, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/cli/mine.rs tests/cli_mine_archive.rs
git commit -m "feat(transcripts): mem mine writes every block to /transcripts/messages"
```

---

## Task 11: `mem repair` covers transcripts sidecar

**Files:**
- Modify: `src/storage/vector_index_diagnose.rs`
- Modify: `src/cli/repair.rs`
- Test: `tests/repair_cli.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/repair_cli.rs`:

```rust
#[tokio::test]
async fn repair_check_reports_status_for_both_sidecars() {
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp);
    let _ = router_with_config(cfg.clone()).await.unwrap(); // populates DB + writes both sidecars

    let report = mem::cli::repair::run_check_for_test(&cfg, /* json */ false).await;
    // Should describe BOTH sidecars (memories + transcripts).
    assert!(report.contains(".usearch"));
    assert!(report.contains(".transcripts.usearch"));
    // Expected exit codes are aggregated (worst of the two).
}
```

(`run_check_for_test` is a thin shim around the existing `run_check`, exposing the formatted text and exit code for tests. Add it to `cli/repair.rs` if it doesn't already exist.)

- [ ] **Step 2: Run to verify it fails**

```bash
cargo test --test repair_cli repair_check_reports_status_for_both -q
```
Expected: FAIL — current `run_check` only diagnoses the memories sidecar.

- [ ] **Step 3: Generalize the diagnose/rebuild surface**

In `src/storage/vector_index_diagnose.rs`, add transcript-aware variants:

```rust
pub async fn diagnose_transcripts(
    repo: &DuckDbRepository,
    db_path: &Path,
    fp: &VectorIndexFingerprint,
) -> Result<DiagnosticReport, StorageError> {
    // Same body as `diagnose`, but:
    //   - sidecar paths from `transcript_sidecar_paths`
    //   - row count from `count_total_transcript_embeddings`
}

pub async fn rebuild_transcripts_index(
    repo: &DuckDbRepository,
    db_path: &Path,
    fp: &VectorIndexFingerprint,
) -> Result<VectorIndex, VectorIndexError> {
    // Same as `rebuild_index` but for transcripts.
}
```

Refactor opportunity: factor the common parts of `diagnose` and `diagnose_transcripts` into a private helper parameterized by `(sidecar_paths_fn, count_fn)`. Worth doing since we now have two callsites.

In `src/cli/repair.rs::run_check`, after calling `diagnose`, also call `diagnose_transcripts`. Format both reports — for `--json`, return an object `{"memories": …, "transcripts": …, "exit_code": worst_of_both}`. For text mode, print two sections labelled "Memories" and "Transcripts". The aggregate exit code is `max(memories.exit_code, transcripts.exit_code)`.

Same change for `run_rebuild`.

- [ ] **Step 4: Run the test**

```bash
cargo test --test repair_cli -q
cargo clippy --all-targets -- -D warnings
```
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/storage/vector_index_diagnose.rs src/cli/repair.rs tests/repair_cli.rs
git commit -m "feat(transcripts): mem repair covers transcripts sidecar alongside memories"
```

---

## Task 12: End-to-end smoke + final verification

**Files:**
- Modify: `tests/integration_claude_code.rs` (add a smoke that combines mine + search + get)

- [ ] **Step 1: Write a final end-to-end smoke**

Append to `tests/integration_claude_code.rs`:

```rust
#[tokio::test]
async fn end_to_end_mine_then_search_then_get() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = mem::config::Config::local();
    cfg.db_path = tmp.path().join("mem.duckdb");

    // Spin up the full app (workers + HTTP).
    let app = mem::app::router_with_config(cfg.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    // 1. Run mine on a fixture.
    let mut transcript = tempfile::NamedTempFile::new().unwrap();
    write_transcript_fixture(&mut transcript);
    let code = mem::cli::mine::run(mem::cli::mine::MineArgs {
        transcript_path: transcript.path().to_path_buf(),
        tenant: "local".to_string(),
        agent: "claude-code".to_string(),
        base_url: format!("http://{}", addr),
    }).await;
    assert_eq!(code, 0);

    // 2. Wait briefly for the transcript embedding worker to drain.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 3. GET by session_id returns ordered blocks.
    let client = reqwest::Client::new();
    let resp = client.get(format!("http://{addr}/transcripts?session_id=S1&tenant=local")).send().await.unwrap();
    assert!(resp.status().is_success());
    let v: serde_json::Value = resp.json().await.unwrap();
    let messages = v["messages"].as_array().unwrap();
    assert!(messages.len() >= 4);

    // 4. POST search returns at least one hit (semantic match against fixture text).
    let body = serde_json::json!({
        "query": "Rust project",
        "tenant": "local",
        "limit": 5
    });
    let resp = client.post(format!("http://{addr}/transcripts/search")).json(&body).send().await.unwrap();
    assert!(resp.status().is_success());
    let v: serde_json::Value = resp.json().await.unwrap();
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "expected at least one semantic hit");
}
```

- [ ] **Step 2: Run the full suite**

```bash
cargo test -q
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release
```
Expected: all green, including pre-existing tests untouched.

- [ ] **Step 3: Manual smoke (per spec verification checklist)**

```bash
# 1. Start serve
cargo run -- serve &
SERVE_PID=$!

# 2. Run mine on a real transcript
cargo run -- mine ~/.claude/projects/<some-project>/<some-session>.jsonl --agent claude-code

# 3. Verify rows
duckdb $MEM_DB_PATH "select count(*) from conversation_messages"
duckdb $MEM_DB_PATH "select count(*) from transcript_embedding_jobs where status = 'completed'"

# 4. Search
curl -s -X POST localhost:3000/transcripts/search \
  -H 'content-type: application/json' \
  -d '{"query":"vector index", "tenant":"local", "limit":3}' | jq

# 5. Get by session
SESSION=$(duckdb $MEM_DB_PATH "select session_id from conversation_messages limit 1" -csv -noheader)
curl -s "localhost:3000/transcripts?session_id=$SESSION&tenant=local" | jq '.messages | length'

# 6. Repair both sidecars
kill $SERVE_PID
cargo run -- repair --check
```

Expected output documented in spec verification checklist.

- [ ] **Step 4: Commit**

```bash
git add tests/integration_claude_code.rs
git commit -m "test(transcripts): end-to-end mine + search + get smoke"
```

---

## Self-Review

Run before declaring the plan complete:

**Spec coverage check** — every spec section maps to a task:
- §Schema (`conversation_messages`, `transcript_embedding_jobs`, `conversation_message_embeddings`) → Task 1 ✓
- §Domain Types → Task 2 ✓
- §Repository Surface (create, get-by-session, fetch-by-ids, search-by-vector, embedding queue ops) → Tasks 3, 4, 5, 6 (TranscriptEmbeddingRowSource), 9 (`recent_conversation_messages` for empty-query path) ✓
- §`cli/mine.rs` Extension → Task 10 ✓
- §`service/transcript_service.rs` → Task 9 ✓
- §`service/transcript_embedding_worker.rs` → Task 7 ✓
- §HTTP Routes → Task 9 ✓
- §`vector_index.rs` (transcript sidecar paths + open_or_rebuild_transcripts) → Task 6 (paths), Task 7 step 3 (open_or_rebuild_transcripts) ✓
- §`mem repair` extension → Task 11 ✓
- §Configuration env vars → Task 8 ✓
- §Testing Strategy unit + integration tests → Tasks 2, 3, 4, 5, 6, 7, 9, 10, 11, 12 ✓

**Open spec concerns deliberately not implemented** (carry as tickets):
- Retention / TTL — explicit Non-Goal
- `mem wake-up` reading transcripts — explicit Non-Goal
- MCP transcript tools — explicit Non-Goal
- `MEM_TRANSCRIPT_EMBED_DISABLED` is implemented (Task 8); other concerns deferred.

**Type-consistency spot-check:**
- `MessageRole::as_db_str` ↔ `from_db_str` ↔ `serde(rename_all = "lowercase")` all produce `"user" | "assistant" | "system"`.
- `BlockType::as_db_str` ↔ `from_db_str` ↔ `serde(rename_all = "snake_case")` all produce `"text" | "tool_use" | "tool_result" | "thinking"`.
- `cli/mine.rs` POST payload uses these exact strings.
- `005_conversation_messages.sql` CHECK constraints use these exact strings.

If a check fails during execution, fix in place and re-run the suite.

---

## Execution Handoff

**Plan complete and saved to `docs/superpowers/plans/2026-04-30-conversation-archive.md`. Two execution options:**

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration. Best for this plan because each task is largely self-contained and the spec is precise.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints. Better if you want to see every detail and intervene mid-task.

**Which approach?**
