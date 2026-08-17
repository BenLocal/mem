# Agent Instructions

Local-first Rust memory service for multi-agent workflows. `CLAUDE.md` is a symlink to this file.

## Non-negotiable invariants

- Run only one `mem serve` writer per Lance dataset. MCP processes are stdio HTTP forwarders and never open datasets directly.
- Storage is verbatim: never rewrite or truncate `memories.content`. `summary` is an index hint, never a fact source or quotation source. An explicit summary must differ from content.
- Respect lifecycle state (`Provisional | Active | PendingConfirmation`), version chains (`supersedes_memory_id`), confidence, decay, and feedback semantics.
- Any new Lance scan on the transcript read path must soft-degrade on read failure; never add a bare `?` that can turn a recoverable scan error into HTTP 500.
- Lance table changes require schema, record-batch builders, and parsers to change in lockstep.
- Before touching ranking, ingest, or output, name the layer: storage/verbatim, indexing-ranking-lifecycle, or infrastructure/bug-fix.

## Commands and CI

```bash
cargo run                         # default: mem serve on 127.0.0.1:3000
cargo run -- serve
cargo run -- mcp                  # stdio MCP forwarder to MEM_BASE_URL
cargo run -- crystallize          # H4 dry run
cargo run -- crystallize --accept # write synthesized Workflow capsules
cargo run -- crystallize --candidate-jobs # safe candidate preview; no claim/write
cargo run -- crystallize --candidate-jobs --propose # write PendingConfirmation Skill proposals
cargo run -- mcp --profile compiler # dedicated Agent-as-Compiler MCP; compiler tools only

make test-one TEST=search_api       # preferred integration-test iteration
make test-filter FILTER=ingest::compute_content # preferred named/unit iteration
make test-rounds                    # completed_tool_round vertical slice
make test-candidates                # deterministic Skill-candidate + durable queue
make test-skill-compiler            # compiler + lifecycle + bundle/loadout/pin slice
make test-full                      # explicit full-suite gate; capped at 4 build jobs
cargo test --test search_api
cargo test --lib ingest::compute_content
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cross build --release
```

Before every commit, both `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` must pass. CI checks all targets, including integration tests and benches. Fix lints; do not add `#[allow(...)]` without a documented reason.

During implementation, run the narrowest relevant unit or integration target. Do **not** run bare `cargo test` after every edit: this repository has 55 integration crates and each links the full Lance/Arrow/Candle graph. Use `make test-one`, `make test-filter` (lib tests only), or a feature-specific target such as `make test-rounds` / `make test-candidates`; reserve `make test-full` for an explicit pre-commit/release gate or when the user asks for a full regression. The Make targets cap local Cargo build parallelism and test execution at `CARGO_TEST_JOBS=4` / `RUST_TEST_THREADS=2` (override deliberately when running on a dedicated builder).

Tests live in root `tests/`; unit tests live in inline `#[cfg(test)] mod tests`. Do not introduce colocated `*_test.rs` files. Rust edition is 2021; use snake_case. Prefer small focused functions/files, explicit error handling, and no hardcoded secrets.

Commit subjects use `feat(area)`, `fix(area)`, `docs(area)`, `test(area)`, `refactor(area)`, or `chore`. Roadmap closures use `… (closes mempalace-diff §8 #N)`.

## Feedback discipline (calling agent → MCP)

Closing the feedback loop is part of the MCP contract. Retrieval alone does not improve future ranking; callers must send feedback when a retrieved capsule was actually used.

Use `mcp__mem__capability_capsule_feedback` (`POST /capability_capsules/feedback`) with `feedback_kind` and optional verbatim `note`. The removed `capability_capsule_apply_feedback` alias must not be used.

| `feedback_kind` | effect | when to send |
|---|---|---|
| `useful` | confidence +0.10; validates | Directly unblocked or answered the task; strongest positive. |
| `applies_here` | confidence +0.05 | Relevant context, but not load-bearing. |
| `outdated` | decay +0.20 | Correct at ingest, now stale. |
| `does_not_apply_here` | decay +0.10 | Correct elsewhere, not applicable in this scope. |
| `incorrect` | status → `Archived` | Verified factual error; permanent/destructive. |

- Send at most one signal per memory per session, choosing the strongest applicable kind.
- Only send feedback for capsules you actually fetched/read and used. Skipped search hits receive no signal.
- `incorrect` is destructive; use it only for a verified factual error, not disagreement.
- Leave `tenant` unset in MCP calls; the wrapper resolves `MEM_TENANT` (default `local`). HTTP callers must provide tenant.
- Feedback writes immediately affect confidence/decay and therefore the next retrieval; there is no deferred application batch.

## Runtime and configuration quick reference

Core:

- `MEM_DB_PATH`: Lance dataset directory. Legacy directories may still be named `mem.duckdb`; they are not DuckDB files.
- `BIND_ADDR`: HTTP bind address.
- `MEM_BASE_URL` / `MEM_TENANT`: MCP forwarder target/default tenant.
- `MEM_ADMIN_TOKEN`: cross-tenant superuser Bearer for completed-round, review and Skill admin routes. Skill least-privilege roles use distinct 32+ byte `MEM_SKILL_COMPILER_TOKEN`, `MEM_SKILL_REVIEWER_TOKEN`, and `MEM_SKILL_RUNTIME_TOKEN`, each restricted to `MEM_TENANT`.
- `MEM_AGENT_COMPILER_ID`: trusted harness/profile label persisted in Agent-as-Compiler receipts; compiler MCP derives it from process env and never accepts it from tool arguments.
- `MEM_BACKEND`: `lance` (default), `postgres`, or `clickhouse`; alternate backends require `MEM_POSTGRES_URL` or `MEM_CLICKHOUSE_URL`. All backends are compiled into every build.
- `MEM_MCP_EXPOSE_EMBEDDINGS=1`: expose admin embedding tools.

Workers and lifecycle:

- `EMBEDDING_BATCH_SIZE`: capsule embedding claim count, default `8`; use `1` for per-job failure isolation.
- `EMBEDDING_WORKER_POLL_INTERVAL_MS`: default `10000`.
- `MEM_TRANSCRIPT_EMBED_DISABLED=1`: disable transcript embedding worker.
- `MEM_TRANSCRIPT_OVERSAMPLE`: transcript candidate fan-out, default `4`; invalid values use the default.
- `MEM_LAST_USED_FLUSH_SECS`: retrieval reinforcement drain cadence, default `5`. The worker advances `last_used_at`, not `updated_at`; decay anchors on `last_used_at` when present.
- `MEM_AUTO_PROMOTE_DISABLED=1`: opt out of the default-on `PendingConfirmation → Active` sweep. Age default `3` days, decay threshold `0.5`, cadence controlled by `MEM_AUTO_PROMOTE_INTERVAL_SECS`; Preference/Workflow are excluded. Legacy falsy `MEM_AUTO_PROMOTE_ENABLED` also disables it.
- `MEM_MAX_INGEST_PER_SESSION`: positive process-local cap; `0`/unset means unlimited. Rejections are HTTP 429. Idempotent re-ingest does not consume a slot. The soft-bounded counter map resets fail-open at 100k sessions and on restart.

Maintenance and retrieval:

- `MEM_VACUUM_DISABLED=1`, `MEM_VACUUM_INTERVAL_SECS`, `MEM_VACUUM_OLDER_THAN_DAYS`: manifest maintenance.
- `MEM_VACUUM_AGGRESSIVE=1`: opt into `delete_unverified=true`; default is OFF because concurrent writers may still reference an unverified manifest. `MEM_VACUUM_PRESERVE_UNVERIFIED=1` forces the safe behavior and wins over aggressive mode.
- `MEM_RECALL_PER_SOURCE_CAP`: soft diversity cap, default `3`; `0` disables. Overflow is deferred, not dropped.
- `MEM_RECALL_POOL_LIMIT`: optional positive lifecycle-pool cap; unset/`0`/invalid is unbounded. Preference/Workflow guidance and hybrid hits remain included.
- `MEM_RECALL_SEMANTIC_TIMEOUT_MS`: semantic-leg deadline, default `1500`; `0` disables. Timeout degrades to BM25-only and increments `recall_semantic_timeouts`. During index builds, a process-wide RAII flag preemptively skips semantic work and increments `recall_semantic_skips`; slow timeout is not treated as a broken index. Embedding lazy init must remain cancellation-safe.
- `MEM_RECALL_STYLE`: auto-recall banner style, default `index`; `snippet` restores legacy snippets. Directives stay full-text. Banner renderer and `cli/feedback.rs::scan_transcript` parser are coupled; preserve their round-trip tests.

Extraction, governance, and safety:

- `MEM_INGEST_NEARDUP_ENABLED=1` / `MEM_INGEST_NEARDUP_THRESHOLD` (default OFF / `0.92`): after embedding an Active ingest, identify its near-duplicate cluster, choose the longest/earliest canonical, and propose `suspected_supersede`. It must remain review-gated and verbatim-safe; never auto-merge/archive.
- `MEM_MINE_HEURISTIC_EXTRACT=1`: default-off zero-LLM extraction for untagged assistant text. Candidates use `write_mode:"propose"` and must never become Active directly.
- `MEM_MINE_LLM_EXTRACT=1` plus `LLM_API_BASE`, `LLM_MODEL`, optional `LLM_API_KEY`: default-off generative extraction. Missing base/model disables the lane; all LLM errors degrade to empty/fallback; output is propose-only; client uses `.no_proxy()`. Limit remains ≤5 candidates/block and ≤40 blocks/run.
- The same LLM variables serve `mem crystallize`. Invoking the subcommand is its enable gate. Legacy H4 writes only with `--accept`; candidate mode previews by default and writes only `PendingConfirmation` proposals with `--candidate-jobs --propose`. Synthesis errors never activate an asset.
- `MEM_REDACT_SECRETS_DISABLED=1`: opt out of default-on secret redaction. Redact prompt/index outputs and both embedding inputs, but keep stored capsule/transcript content verbatim. Explicit verbatim fetches (`capability_capsule_get`, `transcripts_range`, `get_by_session`) intentionally remain unredacted.
- `MEM_KG_FUNCTIONAL_PREDICATES`: comma-separated, default empty/OFF. Only list genuinely single-valued predicates; a new `(from,predicate,to)` closes active conflicting targets. Never configure multi-valued predicates such as `uses`.
- `MEM_RERANK_OFFLINE_ENABLED=1`, `MEM_RERANK_PROVIDER`, `MEM_RERANK_MODEL_DIR`, `MEM_RERANK_MERGE_FLOOR` (default OFF; floor `0.5`): gate evolution merges with the offline reranker. Low score cancels; errors fail-closed/HOLD. `fake` is the deterministic test provider. Do not add interactive query-path reranking. Procedural-sibling detection remains the primary merge defense. See `docs/offline-reranker-lane.md`.

Removed configuration (`MEM_VECTOR_INDEX_*`, `MEM_TRANSCRIPT_VECTOR_INDEX_FLUSH_EVERY`, `MEM_DUCKDB_THREADS`, `MEM_READ_ENGINE`, `MEM_RW_POOL_DISABLED`) must not be reintroduced. See `docs/backend-coupling.md`.

## Architecture invariants

### Processes and HTTP

- One binary has two long-running modes: `mem serve` and `mem mcp`; one-shot handlers live under `src/cli/` and return an exit code.
- `mem mcp` speaks JSON-RPC over stdio and proxies HTTP to `MEM_BASE_URL`. It has no storage connection.
- `/metrics` exposes process-local `AtomicU64` counters that reset on restart. Increment only at behavior choke points, never for validation-rejected requests. Keep pipeline-explicit names (`capsule_*`, `transcript_*`, `episode_*`) and cross-surface counters such as `redaction_hits`, `feedback_*`, `recall_semantic_*`, and reranker metrics.

### Storage and retrieval

- `LanceStore` uses lancedb's Rust API for both reads and writes with `read_consistency_interval(0)` strong freshness. There is no DuckDB connection, SQL read engine, read pool, or usearch sidecar.
- Filters use `query().only_if(...)`; ANN uses `nearest_to(...).nprobes().refine_factor()`; Rust hydrates IDs and performs ranking, aggregation, graph BFS, version dedup, and RRF fusion.
- BM25 is the in-memory Tantivy subsystem in `src/storage/fts.rs`, with jieba precision tokenization. It rebuilds at startup/maintenance only when source Lance versions changed; forced `/admin/reindex` always rebuilds.
- All writes, including decay, go through lancedb. Lance native retry plus `LanceStore::with_lance_commit_retry` handles contention.
- ANN indexes are maintained explicitly by `ensure_vector_indexes`; Lance does not auto-build them. Tables below 5k rows remain flat-scanned.
- `graph_edges` carries `valid_from`/`valid_to`; default reads return active edges. Point-in-time traversal uses `neighbors_within` with `MAX_HOPS_CAP=3`. Entity aliases normalize to tenant-scoped UUIDv7 entity IDs while preserving canonical display names.

### Pipelines and lifecycle

- Core pipeline is `pipeline/ingest.rs → retrieve.rs → compress.rs → workflow.rs`; behavior belongs there rather than in thin service wrappers.
- Ingest persists embedding jobs asynchronously. Jobs transition `pending → processing → completed | failed | stale`; embedding failures never block ingest.
- Search combines semantic, lexical, scope, intent, confidence, freshness, decay, and graph signals. Compression operates on verbatim `content` under a token budget.
- `supersedes_memory_id` forms auditable version chains. Code touching memories must preserve lifecycle status transitions and graph-edge closure.

### Transcript archive

- Transcript storage is parallel to capsules: `conversation_messages`, `transcript_embedding_jobs`, and `conversation_message_embeddings` share no capsule state. `mem mine` writes both extracted memories and every transcript block. Transcript search remains HTTP-only.
- Lance may fail stale/partially-covered transcript ANN scans with unequal record-batch column lengths. Every transcript read boundary—semantic ANN, recent browse, anchor injection, hydration, and context windows—must catch read errors and degrade rather than return 500.
- The semantic ANN boundary may single-flight force-rebuild indexes and retry once on the ragged-batch error; if rebuild/retry cannot serve ANN, fall back to BM25. Other scan boundaries degrade without rebuilding. Keep `ReindexGuard` stampede protection.

## Design discipline

- `memories.content` is the fact source. `memories.summary` is only an index/headline hint. Output compression and redaction must never rewrite stored facts.
- Storage schema is defined inline in `src/storage/lance_store/{mod,sessions,episodes}.rs`; update schema fields, builders, and row parsers together.
- Preferences and workflows require review gating where specified. New extraction/governance capabilities default off unless the documented contract explicitly says otherwise.
- Secret handling: never hardcode credentials. Validate required environment variables at startup and avoid leaking secrets in errors/logs.

## Design context

- `docs/architecture.md`: detailed current architecture and transcript failure handling.
- `docs/remove-duckdb-keep-lance.md`: route-B storage migration rationale.
- `docs/backend-coupling.md`: backend boundaries and removed configuration.
- `docs/offline-reranker-lane.md`: reranker design and deployment.
- `docs/mempalace-diff.md` §8: roadmap and numbered closure references.
- `CHANGELOG.md`: feature history and rationale.
