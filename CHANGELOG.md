# Changelog

All notable changes to `mem` are documented here. Format inspired by
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

This project does not yet publish versioned releases; entries below
are organized by feature wave (merge commit ranges on `master`).

## [Unreleased]

### Added
- _Nothing yet — add new entries here as they land._

---

## 2026-05-05 — MemPalace LongMemEval Parity Bench

### Added

- `tests/mempalace_bench.rs` — `#[ignore]`'d entry that runs LongMemEval against
  mem's stack and emits results JSON in mempalace-mirror shape
- `tests/bench/longmemeval.rs` — runner: 3 rungs (raw / rooms / full), per-Q
  ingest + re-rank, aggregate, output formatters
- `tests/bench/longmemeval_dataset.rs` — JSON loader with env-var skip semantics
- `src/pipeline/eval_metrics.rs` — `recall_any_at_k`, `recall_all_at_k` (mempalace-style binary indicators)

### Notes

- Manual decision tool, not in CI (matches mempalace's manual operation flow)
- Uses production embedding stack (configured via `EMBEDDING_*` env vars);
  warns if `EMBEDDING_PROVIDER=fake`
- `MEM_LONGMEMEVAL_PATH` env var points to a pre-downloaded dataset
- Mempalace's AAAK and llmrerank modes skipped (mem has no analog)
- `1.5-3 hour` wall-clock for full 500-question run; `MEM_LONGMEMEVAL_LIMIT=50`
  for smoke

---

## 2026-05-03 — Transcript Recall Quality Bench

**Range:** `070f900..16a0afd`.

### Added

- 10-rung ablation harness for the transcript recall pipeline (`tests/recall_bench.rs`)
- `src/pipeline/eval_metrics.rs` — pure NDCG@k / MRR / Recall@k / Precision@k
- `tests/bench/` — fixture / synthetic generator / real loader / judgment / oracle / runner modules
- Synthetic CI guard: monotone-improvement assertions across rungs
- Real fixture loader: `MEM_BENCH_FIXTURE_PATH` env-var, JSON schema v1, `#[ignore]`'d

### Changed

- `pipeline::transcript_recall::ScoringOpts` extended with `disable_session_cooc /
  disable_anchor / disable_freshness` bools (default false → zero behavior change)

### Notes

- Bench answers two questions: (1) does each existing signal carry weight?
  (2) is a real cross-encoder worth pursuing? Oracle rerank rung gives the
  binary-reranker upper bound.
- Co-mention + entity-alias auto-judgment biases toward lexical hits;
  absolute scores not directly comparable across BM25/HNSW, but Δ across rungs
  is reliable.

---

## 2026-05-02 — Entity Registry

**Range:** `8f8d2af..68395a0`.

Adds a tenant-scoped entity registry that canonicalizes alias strings
(`"Rust"` = `"Rust language"` = `"rustlang"`) to a stable UUIDv7
`entity_id`. The ingest pipeline auto-promotes caller-supplied `topics`
(and legacy `project` / `repo` / `module` / `task_type` fields) into
registry entries on first write, so `graph_edges.to_node_id` is now
`"entity:<uuid>"` for all entity-typed edges.

### Added
- `entities` + `entity_aliases` tables (schema 008) with composite-PK
  ON CONFLICT for idempotent alias upserts.
- `domain::entity`: `Entity`, `EntityKind` (snake_case serde), `EntityWithAliases`,
  `AddAliasOutcome`.
- `pipeline::entity_normalize::normalize_alias` — pure case-fold +
  whitespace-collapse; punctuation preserved (`C++` ≠ `c`).
- `EntityRegistry` trait + `DuckDbRepository` impl with single-mutex discipline.
- `MemoryRecord.topics: Vec<String>` field, round-tripping via JSON column.
- `extract_graph_edge_drafts` (pure) + `#[deprecated]` legacy wrapper.
- `resolve_drafts_to_edges` service helper — routes drafts through the
  registry and returns typed `GraphEdge` values.
- Production ingest wired to produce `"entity:<uuid>"` graph edges.
- 4 HTTP routes: `POST /entities`, `GET /entities/{id}`, `POST /entities/{id}/aliases`,
  `GET /entities` (list). `POST /entities` returns 409 with `existing_entity_id`
  on cross-entity alias conflict.
- `mem repair --rebuild-graph` CLI subcommand — re-derives all
  memory-originating edges through the registry per-tenant atomically.

### Notes
- MCP surface unchanged by design (HTTP-only, matching conversation-archive
  / transcript-recall convention).
- Migration: run `mem repair --rebuild-graph` to upgrade legacy
  `"project:..."` / `"repo:..."` graph edge targets. Idempotent.
- Spec: `docs/superpowers/specs/2026-05-02-entity-registry-design.md`.

---

## 2026-04-30 — Conversation Archive

**Merge:** `aa6eab1`. **Range:** `1ac2d6b..49b652f`.

Adds a parallel "transcript archive" pipeline alongside `memories`. Every
Claude Code transcript block is now stored verbatim with its own embedding
queue and HNSW sidecar, so transcript recall is decoupled from the
curated-memory pipeline.

### Added
- `conversation_messages` table storing transcript blocks verbatim, with a
  dedicated embedding queue.
- Independent HNSW sidecar at `<MEM_DB_PATH>.transcripts.usearch` (rebuildable
  from DuckDB on startup mismatch, like the memories sidecar).
- Three HTTP routes: `POST /transcripts/messages`, `POST /transcripts/search`,
  `GET /transcripts?session_id=…`.
- `mem mine` becomes dual-sink — writes to both `memories` and
  `conversation_messages`.

### Notes
- MCP surface unchanged by design.
- Spec: `docs/superpowers/specs/2026-04-30-conversation-archive-design.md`.

---

## 2026-05-01 — Transcript Recall

**Merge:** `cec4984`. **Range:** `b3f7d64..5a81f28`.

Lifts `POST /transcripts/search` recall quality to memories-pipeline parity:
adds a BM25 lexical channel fused with HNSW via RRF, plus session / anchor /
recency bonuses that act as freshness/decay substitutes for transcripts.

### Added
- BM25 lexical channel for transcript search, fused with HNSW via RRF
  (shared with memories via `pipeline/ranking.rs`).
- Session co-occurrence + anchor + recency bonuses (freshness/decay
  substitutes for transcripts).
- ±k context-window hydration; same-session windows merge into conversation
  snippets.

### Changed
- **Breaking**: `POST /transcripts/search` response shape changes from
  `{hits: [...]}` to `{windows: [...]}`. Two in-tree callers migrated.

### Notes
- Spec: `docs/superpowers/specs/2026-05-01-transcript-recall-design.md`.

---

## 2026-05-02 — Polish & Test Isolation

**Merges:** `677c9fa` (`chore/transcript-recall-polish`) and `c58cc4c`
(`chore/test-isolation-and-provider-passthrough`).
**Ranges:** `fbed933..e7fdfa4` and `d6ecfe2..4b2bd69`.

Two adjacent cleanup waves on top of the conversation-archive +
transcript-recall stack: tunables and doc/test polish, then a real fix to
the `EmbeddingSettings` plumbing that lets previously-`#[ignore]`'d worker
tests run.

### Added
- `MEM_TRANSCRIPT_OVERSAMPLE` env override for transcript HNSW oversampling.
- `MemoryService::new_with_settings(repo, &EmbeddingSettings)` constructor —
  derives `embedding_job_provider` from settings instead of relying on
  process-wide env at worker spin-up.
- AGENTS.md spec pointers; `rrf_contribution` doc note.

### Changed
- Test files split / tightened around the transcript-recall and lifecycle
  paths.
- 4 shared-DB flaky tests migrated to per-test temp DBs.

### Fixed
- `embedding_defaults_when_empty` assertions aligned with EmbedAnything
  defaults (drift after 47aff1e).
- 6 worker-driven tests un-ignored after `MemoryService::new_with_settings`
  fix and now pass.

### Notes
- After this wave, `cargo test -q --no-fail-fast` is fully green:
  219 passed, 0 failed, 1 ignored (only the FTS predicate probe remains,
  which is design-time).
