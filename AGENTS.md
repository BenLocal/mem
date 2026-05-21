# Agent Instructions

Local-first Rust memory service for multi-agent workflows. Loaded by Claude Code and other agents (`CLAUDE.md` is a symlink to this file).

---

## Common Commands

```bash
cargo run                                      # default = `mem serve` (axum HTTP on 127.0.0.1:3000)
cargo run -- serve                             # explicit HTTP server mode
cargo run -- mcp                               # stdio MCP server; forwards to MEM_BASE_URL

cargo test -q                                  # full suite (integration tests in tests/)
cargo test --test search_api                   # single integration test file
cargo test ingest::compute_content             # single test fn (path or substring)
cargo fmt --check                              # required by CI
cargo clippy --all-targets -- -D warnings      # required by CI

cross build --release                          # cross-compile (reads ./Cross.toml)
```

**Key env vars:** `MEM_DB_PATH` (DuckDB file), `BIND_ADDR` (HTTP bind), `MEM_BASE_URL` / `MEM_TENANT` (MCP forwarder), `MEM_MCP_EXPOSE_EMBEDDINGS=1` (admin tools), `MEM_TRANSCRIPT_EMBED_DISABLED=1` (stop the transcript embedding worker), `MEM_TRANSCRIPT_OVERSAMPLE` (transcript search candidate fan-out factor, default 4; read live in `TranscriptService::search`, invalid values silently fall back to default), `EMBEDDING_BATCH_SIZE` (embedding worker per-tick claim count, **default 8** since 2026-05-21 — flipped from 1 because each tick triggers a ~100ms `DuckDbQuery::refresh()` regardless of how many jobs run; set to `1` to restore per-job failure isolation), `EMBEDDING_WORKER_POLL_INTERVAL_MS` (embedding worker tick cadence, **default 10_000** since 2026-05-21 — flipped from 1000 after measuring 510% idle CPU + 800+ tokio blocking threads at 1 Hz cadence; the spawn_blocking + DuckDB mutex storm dominates idle cost. 10 s tick measured 56% CPU + 217 threads on the same workload. Set to `1000` for sub-second job pickup latency at the cost of 9× CPU baseline), `MEM_VACUUM_DISABLED=1` / `MEM_VACUUM_INTERVAL_SECS` / `MEM_VACUUM_OLDER_THAN_DAYS` (Lance manifest pruning, see `src/worker/vacuum_worker.rs`), `MEM_AUTO_PROMOTE_ENABLED=1` and friends (opt-in `PendingConfirmation → Active` sweep), `MEM_RW_POOL_DISABLED=1` (opt out of the r2d2 read pool that backs `fetch_memories_by_ids`; default is on). The legacy `MEM_VECTOR_INDEX_*` and `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY` env vars were removed in `docs/backend-coupling.md` §4.4 QW-4 — they tuned the usearch sidecar that Lance 0.27's native ANN replaced.

---

## Conventions

- **Rust edition 2021** (see `Cargo.toml`). `cargo fmt` + `cargo clippy --all-targets -- -D warnings` clean is non-negotiable. snake_case for modules / files / functions.
- **Tests:** integration tests live in `tests/` at the repo root (e.g. `tests/search_api.rs`, `tests/hybrid_search.rs`, `tests/vacuum.rs`). Unit tests sit inline as `#[cfg(test)] mod tests` at the bottom of source files. **No** colocated `*_test.rs` convention in this codebase.
- **Schema:** LanceDB tables are defined inline as `Schema::new(vec![Field::new(...)])` in `src/storage/lance_store/{mod,sessions,episodes}.rs`. No external migration files. Adding/changing a table means updating the schema fn + record_batch builders + parsers in lockstep.
- **Commit scope tags:** `feat(area)`, `fix(area)`, `docs(area)`, `test(area)`, `refactor(area)`, `chore`. When closing a roadmap item: `… (closes mempalace-diff §8 #N)`.
- **Pre-commit CI check (mandatory):** Before EVERY commit, run BOTH:
  1. `cargo fmt --check`
  2. `cargo clippy --all-targets -- -D warnings`

  CI runs both gates on the full crate including `tests/` (note `--all-targets`); a clippy lint inside an integration test or bench file (e.g. `tests/bench/runner.rs`) will fail CI just like one in `src/`. Never commit if either check fails. If clippy flags a lint, fix the lint — do not silence with `#[allow(...)]` unless you have a documented reason.

---

## Architecture (non-obvious bits)

- **Single binary, two long-running subcommands**: `mem serve` (HTTP) and `mem mcp` (stdio MCP forwarder over `MEM_BASE_URL`), plus the one-shot CLI utilities under `src/cli/` (`mine`, `wake-up`, `feedback`). All share `Config::from_env`. MCP is *not* a separate process talking to DuckDB directly — it speaks JSON-RPC and proxies to the HTTP service. Two HTTP services pointed at the same DB will fight; DuckDB is single-writer.
- **Storage layer (`src/storage/`)**: Lance datasets on disk (`LanceStore`, writes) + an in-process DuckDB that ATTACHes the same lance dir via the lance core extension (`DuckDbQuery`, reads). The DuckDB connection is wrapped in `Arc<Mutex<Connection>>` — every read serializes through one mutex; `tokio::task::spawn_blocking` bridges sync `duckdb-rs` 1.x into async. Writes go through LanceDB's Rust API; the `Store` composition layer (`src/storage/store.rs`) calls `DuckDbQuery::refresh()` after every write because the lance DuckDB extension caches dataset versions on a per-connection basis. Graph layer (writes in `src/storage/lance_store/graph.rs`, reads in `src/storage/duckdb_query/graph.rs`): the `graph_edges` Lance table stores edges with `valid_from` / `valid_to` timestamps. `sync_memory_edges` writes active edges, `close_edges_for_capability_capsule` runs on supersede; reads default to `valid_to IS NULL` (active only). Point-in-time lookups go through `neighbors_within(node, max_hops, as_of)` (BFS, `MAX_HOPS_CAP = 3`). Caller-supplied edges write via `add_edge_direct` (preserves caller's `valid_from`); explicit fact closure via `invalidate_edge(from, predicate, to, ended_at)`. Whole-graph aggregate via `graph_stats()`. ANN and BM25 indexes live inside Lance — FTS index is built at table-open time via `ensure_fts_index` in `lance_store/mod.rs` (the lance DuckDB extension's `lance_fts(...)` table function silently returns empty for un-indexed columns, so the index has to exist before the first query); vector indexes are handled by Lance internally. **No external sidecar** (the old usearch `<MEM_DB_PATH>.usearch` path is gone).
- **Pipeline (`src/pipeline/`)** is the heart of behavior, not `service/`. Four stages: `ingest.rs` (status assignment, content_hash via sha2, graph edge extraction) → `retrieve.rs` (additive integer scoring: semantic + lexical + scope + intent + confidence + freshness − decay + graph) → `compress.rs` (token-budgeted four-section output: directives / relevant_facts / reusable_patterns / suggested_workflow) → `workflow.rs` (episode → workflow extraction).
- **Embedding pipeline is async + persistent**: writes enqueue rows in `embedding_jobs` (DuckDB table); `service/embedding_worker.rs` consumes with retry/backoff and mirrors successful upserts into `VectorIndex`. Provider trait under `src/embedding/` (embed_anything local / OpenAI / fake). Failure does not block ingest — jobs go `pending → processing → completed | failed | stale`.
- **Memory has lifecycle, not just CRUD**: `MemoryStatus = Provisional | Active | PendingConfirmation`, `supersedes_memory_id` forms version chains, `feedback_events` mutate `confidence` / `decay_score`. New code touching memories must respect status transitions — see `domain/memory.rs`.
- **CLI layer (`src/cli/`)**: home of subcommand handlers other than `serve` / `mcp` — `mine`, `wake-up`, `feedback` are the current one-shot utilities. Pattern: handler returns `i32` (process exit code); `main.rs` dispatches and `std::process::exit`s.
- **Transcript archive (parallel pipeline to memories)**: a verbatim conversation archive lives alongside `memories` with **zero shared state**. Separate table `conversation_messages` (CRUD entry: `src/storage/lance_store/transcripts.rs`, reads: `src/storage/duckdb_query/transcripts.rs`), separate queue `transcript_embedding_jobs`, separate embedding table `conversation_message_embeddings` (also Lance-native vectors, no sidecar), separate worker `src/worker/transcript_embedding_worker.rs`. HTTP entry is `src/http/transcripts.rs` (`POST /transcripts/messages`, `POST /transcripts/search`, `GET /transcripts?session_id=…&tenant=…`). `mem mine` is **dual-sink**: one transcript scan writes both extracted memories (unchanged) and every block (text / tool_use / tool_result / thinking) to the archive. MCP surface is intentionally untouched — transcript search is HTTP-only by design.
- **Entity registry**: `entities` + `entity_aliases` tables canonicalize alias strings to stable `entity_id` (UUIDv7). `MemoryRecord.topics: Vec<String>` is the caller-supplied input; ingest pipeline (`extract_graph_edge_drafts` + `resolve_drafts_to_edges` in `service::memory_service`) routes through `EntityRegistry` so `graph_edges.to_node_id` is `"entity:<uuid>"`. Aliases are normalized (lowercase + whitespace-collapsed) at the PK; canonical_name preserves caller verbatim. Tenant-scoped, session-orthogonal.

---

## Design Discipline

- **Verbatim rule**: `memories.content` is the **fact source** — never rewrite, never truncate at storage. `memories.summary` is **index / hint only** — never use it as the basis for an answer or quote. Output-layer compression (`pipeline/compress.rs`) operates on `content`, never replaces it. The ingest pipeline enforces that, when a caller provides an explicit `summary` field, it must not equal `content` — agents must not copy refined/summarized text into the `content` field. When no caller summary is supplied, the server derives one from `content[:80]` for indexing purposes only.
- **Two-axis layering** (see `docs/mempalace-diff.md` §8): 📦 storage stays verbatim, 🔍 indexing / ranking / lifecycle is where structured signals live, ⚙️ infra / bug-fix is its own track. Before touching ranking, ingest, or output, name which layer you're in.

---

## Feedback discipline (calling agent → MCP)

When using `mem`'s MCP tools, **closing the feedback loop is part of the contract** — the lifecycle (`confidence` ↑, `decay_score` ↑, status → `Archived`) only moves when callers send signals back. A read-only consumer that never calls back means every memory's score is frozen at ingest, and ranking quality stops compounding.

The two MCP entry points are equivalent over the same `POST /memories/feedback` backend:
- `mcp__mem__memory_feedback` — canonical name; argument is `feedback_kind: string`.
- `mcp__mem__memory_apply_feedback` — same body, argument renamed `kind`. Use either; pick one per session for consistency.

The five `feedback_kind` values and what each does (`src/domain/memory.rs::FeedbackKind`):

| `feedback_kind`         | confidence Δ | decay Δ | side effect           | when to send                                                                                       |
|-------------------------|--------------|---------|-----------------------|----------------------------------------------------------------------------------------------------|
| `useful`                | +0.10        | 0       | marks validated       | A retrieved memory **directly** unblocked / answered the current task. The strongest positive.    |
| `applies_here`          | +0.05        | 0       | —                     | Memory was relevant context but not the load-bearing fact. The mild positive.                     |
| `outdated`              | 0            | +0.20   | —                     | Memory was right at ingest but is now stale (renamed file, reverted decision, expired credential). |
| `does_not_apply_here`   | 0            | +0.10   | —                     | Correct elsewhere but doesn't fit this scope/project. Don't archive — just deprioritize.          |
| `incorrect`             | 0            | 0       | **status → Archived** | Memory contains a factual error. Permanent — same path as the admin UI's "delete".                 |

### When to fire

- Send **at most one** signal per memory per session — the strongest one. Don't spam the queue with `applies_here` for every search hit.
- Only fire on memories you actually **read and used** — search-hit-but-skimmed-and-ignored is not feedback. Silence is a valid signal too.
- `incorrect` is destructive (archives the row); reserve it for "I verified this is wrong," not "I disagree."
- The `tenant` field is required by the HTTP layer but the MCP wrapper fills it from `MEM_TENANT` automatically — leave it out of the call and the resolver picks `local`.

### What this does to ranking

Per-memory scoring (`pipeline/retrieve.rs`) sums an integer-weighted blend of semantic + lexical + scope + intent + confidence + freshness − decay + graph signals. Feedback tweaks two of those (`confidence`, `decay_score`) on the underlying record, so the next retrieval that surfaces the same memory ranks differently. There is no offline batch that "applies" feedback later; the write is immediate and visible to the next `memory_search` call.

---

## Where to find design context

- **`docs/mempalace-diff.md`** — comparison with MemPalace + roadmap (§8 numbered items #1–#13). Completed items have ✅; commit messages reference them (e.g. `closes mempalace-diff §8 #3`). Read before non-trivial design changes.
- **`CHANGELOG.md`** — per-feature historical context (what landed when, why); useful for "why is it like this" archaeology.
