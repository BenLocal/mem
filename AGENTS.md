# Agent Instructions

Local-first Rust memory service for multi-agent workflows. Loaded by Claude Code and other agents (`CLAUDE.md` is a symlink to this file).

---

## Common Commands

```bash
cargo run                                      # default = `mem serve` (axum HTTP on 127.0.0.1:3000)
cargo run -- serve                             # explicit HTTP server mode
cargo run -- mcp                               # stdio MCP server; forwards to MEM_BASE_URL
cargo run -- repair --check                    # diagnose vector index sidecar (read-only)
cargo run -- repair --rebuild                  # force-rebuild sidecar (offline; stop `mem serve` first)

cargo test -q                                  # full suite (integration tests in tests/)
cargo test --test search_api                   # single integration test file
cargo test ingest::compute_content             # single test fn (path or substring)
cargo fmt --check                              # required by CI
cargo clippy --all-targets -- -D warnings      # required by CI

cross build --release                          # cross-compile (reads ./Cross.toml)
```

**Key env vars:** `MEM_DB_PATH` (DuckDB file), `BIND_ADDR` (HTTP bind), `MEM_BASE_URL` / `MEM_TENANT` (MCP forwarder), `MEM_MCP_EXPOSE_EMBEDDINGS=1` (admin tools), `MEM_VECTOR_INDEX_FLUSH_EVERY` / `_OVERSAMPLE` / `_USE_LEGACY` (HNSW sidecar tuning), `MEM_TRANSCRIPT_EMBED_DISABLED=1` / `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY` (transcript-archive worker + sidecar tuning).

---

## Conventions

- **Rust edition 2021** (see `Cargo.toml`). `cargo fmt` + `cargo clippy --all-targets -- -D warnings` clean is non-negotiable. snake_case for modules / files / functions.
- **Tests:** integration tests live in `tests/` at the repo root (e.g. `tests/search_api.rs`, `tests/vector_index.rs`). Unit tests sit inline as `#[cfg(test)] mod tests` at the bottom of source files. **No** colocated `*_test.rs` convention in this codebase.
- **Schema migrations:** `db/schema/*.sql` files are append-only; never edit historical files in-place. Add a new numbered file for new tables / columns.
- **Commit scope tags:** `feat(area)`, `fix(area)`, `docs(area)`, `test(area)`, `refactor(area)`, `chore`. When closing a roadmap item: `… (closes mempalace-diff §8 #N)`.
- **Pre-commit CI check:** Before every commit, run `cargo fmt --check && cargo clippy --all-targets -- -D warnings` to ensure CI will pass. Never commit code that fails these checks.

---

## Architecture (non-obvious bits)

- **Single binary, three subcommands**: `mem serve` (HTTP), `mem mcp` (stdio MCP forwarder over `MEM_BASE_URL`), `mem repair --check|--rebuild` (one-shot diagnostic / sidecar rebuild). All three share `Config::from_env`. MCP is *not* a separate process talking to DuckDB directly — it speaks JSON-RPC and proxies to the HTTP service. Two HTTP services pointed at the same DB will fight; DuckDB is single-writer.
- **Storage layer (`src/storage/`)**: DuckDB (bundled, single file) wrapped in `Arc<Mutex<Connection>>` — every DB call serializes through one mutex. This is the de-facto concurrency boundary; treat it as "one transaction at a time." Graph layer (`src/storage/graph_store.rs`): `DuckDbGraphStore` writes edges to a `graph_edges` table (same `Arc<Mutex<Connection>>` as the rest of DuckDB). Edges carry `valid_from`/`valid_to` timestamps; `sync_memory` writes active edges, `close_edges_for_memory` is called by supersede flows, and queries default to `valid_to IS NULL` (active only). `neighbors_at(node, ts)` supports point-in-time lookups. ANN sidecar lives in `vector_index.rs` (usearch HNSW, single-file `<MEM_DB_PATH>.usearch` + meta JSON; rebuildable from DuckDB on every startup mismatch).
- **Pipeline (`src/pipeline/`)** is the heart of behavior, not `service/`. Four stages: `ingest.rs` (status assignment, content_hash via sha2, graph edge extraction) → `retrieve.rs` (additive integer scoring: semantic + lexical + scope + intent + confidence + freshness − decay + graph) → `compress.rs` (token-budgeted four-section output: directives / relevant_facts / reusable_patterns / suggested_workflow) → `workflow.rs` (episode → workflow extraction).
- **Embedding pipeline is async + persistent**: writes enqueue rows in `embedding_jobs` (DuckDB table); `service/embedding_worker.rs` consumes with retry/backoff and mirrors successful upserts into `VectorIndex`. Provider trait under `src/embedding/` (embed_anything local / OpenAI / fake). Failure does not block ingest — jobs go `pending → processing → completed | failed | stale`.
- **Memory has lifecycle, not just CRUD**: `MemoryStatus = Provisional | Active | PendingConfirmation`, `supersedes_memory_id` forms version chains, `feedback_events` mutate `confidence` / `decay_score`. New code touching memories must respect status transitions — see `domain/memory.rs`.
- **CLI layer (`src/cli/`)**: home of subcommand handlers other than `serve` / `mcp`. Currently houses `cli/repair.rs`. Pattern: handler returns `i32` (process exit code); `main.rs` dispatches and `std::process::exit`s.
- **Transcript archive (parallel pipeline to memories)**: a verbatim conversation archive lives alongside `memories` with **zero shared state**. Separate table `conversation_messages` (storage entry: `src/storage/transcript_repo.rs`), separate queue `transcript_embedding_jobs`, separate HNSW sidecar at `<MEM_DB_PATH>.transcripts.usearch`, separate worker `src/service/transcript_embedding_worker.rs`. HTTP entry is `src/http/transcripts.rs` (`POST /transcripts/messages`, `POST /transcripts/search`, `GET /transcripts?session_id=…&tenant=…`). `mem mine` is **dual-sink**: one transcript scan writes both extracted memories (unchanged) and every block (text / tool_use / tool_result / thinking) to the archive. `mem repair` covers both sidecars. MCP surface is intentionally untouched — transcript search is HTTP-only by design. Spec: `docs/superpowers/specs/2026-04-30-conversation-archive-design.md`.
- **Entity registry**: `entities` + `entity_aliases` tables canonicalize alias strings to stable `entity_id` (UUIDv7). `MemoryRecord.topics: Vec<String>` is the caller-supplied input; ingest pipeline (`extract_graph_edge_drafts` + `resolve_drafts_to_edges` in `service::memory_service`) routes through `EntityRegistry` so `graph_edges.to_node_id` is `"entity:<uuid>"`. Aliases are normalized (lowercase + whitespace-collapsed) at the PK; canonical_name preserves caller verbatim. Tenant-scoped, session-orthogonal. Migration command: `mem repair --rebuild-graph`.

---

## Design Discipline

- **Verbatim rule**: `memories.content` is the **fact source** — never rewrite, never truncate at storage. `memories.summary` is **index / hint only** — never use it as the basis for an answer or quote. Output-layer compression (`pipeline/compress.rs`) operates on `content`, never replaces it. The ingest pipeline enforces that, when a caller provides an explicit `summary` field, it must not equal `content` — agents must not copy refined/summarized text into the `content` field. When no caller summary is supplied, the server derives one from `content[:80]` for indexing purposes only.
- **Two-axis layering** (see `docs/mempalace-diff.md` §8): 📦 storage stays verbatim, 🔍 indexing / ranking / lifecycle is where structured signals live, ⚙️ infra / bug-fix is its own track. Before touching ranking, ingest, or output, name which layer you're in.

---

## Where to find design context

- **`docs/mempalace-diff.md`** — comparison with MemPalace + roadmap (§8 numbered items #1–#13). Completed items have ✅; commit messages reference them (e.g. `closes mempalace-diff §8 #3`). Read before non-trivial design changes.
- **`docs/superpowers/specs/`** — design specs from brainstorming sessions (e.g. `2026-04-27-vector-index-sidecar-design.md`, `2026-04-28-mem-repair-cli-design.md`, `2026-04-30-conversation-archive-design.md`, `2026-05-01-transcript-recall-design.md`).
- **`docs/superpowers/plans/`** — TDD-style implementation plans paired with the specs above; useful for "why is it like this" archaeology.
- **`docs/superpowers/skills/mem-mcp-codex/SKILL.md`** — how caller agents (Codex / Cursor) are expected to use the MCP tools; check before changing tool surface.
