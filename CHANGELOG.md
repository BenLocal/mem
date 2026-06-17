# Changelog

All notable changes to `mem` are documented here. Format inspired by
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

This project does not yet publish versioned releases; entries below
are organized by feature wave (merge commit ranges on `master`).

## [Unreleased]

## 2026-06-17 — `0.1.6`

### Fixed

- **Transcript search 500 — the real fix** (defense-in-depth). The `0.1.4`
  IVF partition-count fix only *reduced* the ragged-batch 500, and it turned
  out the failing scan was usually the **FTS (BM25) channel**, not the ANN
  one — the lancedb 0.30 / DuckDB lance-extension 4.0 `IO Error: ... all
  columns in a record batch must have the same length` bug fires on *both*
  index-scan kinds, and which queries hit it varies per index rebuild
  (non-deterministic), so no index-tuning fix is reliable. `TranscriptService
  ::search` now soft-degrades **every lance-scan boundary** — BM25, semantic
  ANN, recent-browse, anchor injection, hydrate, and per-primary context
  window: a lance read error on any one logs a `warn` and degrades (the other
  channel carries, or empty windows / primary-only context) instead of
  500ing the request. Verified: the query that 500'd now returns BM25-failed
  → semantic-served results, 20/20 query sweep clean. The partition-count fix
  is kept (it reduces how often the fallback triggers).

## 2026-06-17 — `0.1.5`

### Fixed

- **Transcript semantic-search read-path hardening** (follow-ups from the
  lance-0.30 bug sweep; the HIGH 500 was fixed in `0.1.4`).
  - `semantic_search_transcripts` derives the `best_distance` column index
    from `CONVERSATION_COLS` instead of a hardcoded `row.get(16)` — a column
    add/remove would otherwise silently misread the wrong column.
  - Cap the ANN oversample (`limit*4`) at `MAX_ANN_OVERSAMPLE = 4096` so `k`
    stays bounded if `limit`'s clamp changes.
  - Widen the candidate pool ×4 when any search filter (session / role /
    block_type / time) is set — filters are applied post-fetch, so matching
    rows ranked beyond the oversample cutoff were never pulled, silently
    under-recalling. Adds `TranscriptSearchFilters::is_any_set`.
  - Upsert all chunk embeddings in one `table.add(Vec<RecordBatch>)` instead
    of one add per chunk, which wrote one Lance fragment per chunk and fed
    the fragment explosion the vacuum worker has to compact.

## 2026-06-17 — `0.1.4`

### Fixed

- **Transcript semantic search 500 on LanceDB 0.30** (regression from
  `0.1.3`). `POST /transcripts/search` returned `500 IO Error: ... all
  columns in a record batch must have the same length` for queries whose
  ANN vector landed on a degenerate IVF partition. Root cause: lancedb
  0.30's `IvfPqIndexBuilder::default()` over-partitioned the ~49k-row
  `conversation_message_embeddings` table (256 partitions ⇒ ~190 rows
  each); the DuckDB lance extension (v4.0 read side, vs the lance-7.0
  writer) then produced a ragged record batch materializing ANN results
  across that many small partitions. Verified empirically — 256 partitions
  reproduced it, ~1024 rows/partition (48 partitions) cleared it across a
  broad query sweep; partition *size*, not the (similar) empty-cluster
  ratio, was the trigger. Capsule recall was unaffected (that table is
  flat-scanned, no IVF index). Fix: pin `num_partitions` from row count
  (`ivf_num_partitions`) in `LanceStore::ensure_query_indexes`, and add
  `POST /admin/reindex` (`MaintenanceStore::rebuild_query_indexes`) to
  force-rebuild an index whose shape — not coverage — is stale.

## 2026-06-17 — `0.1.3`

Storage upgrade + a month of lifecycle / governance / backend work.
Headline: LanceDB 0.27 → 0.30 (arrow-array 57 → 58) completed as a
proper migration — supersedes the bare dependabot bump (#30) that left
arrow pinned at 57 and would not typecheck across the duckdb →
`Table::add` boundary. Also lands the Postgres storage backend
(opt-in via `MEM_BACKEND`), the self-evolution worker, retrieval
reinforcement + idle-archive + ingest quality gate governance, the
progressive-disclosure recall banner, and the Lance vacuum worker.

### Added

- **Vacuum worker** (`src/worker/vacuum_worker.rs`) — daily Lance
  manifest pruning across every managed table. Lance is copy-on-write
  so high-churn tables (`transcript_embedding_jobs`,
  `conversation_message_embeddings`) accumulate gigabytes of historical
  `_versions/` manifests within days even though the actual row data
  is tens of MB. The worker calls `Table::optimize(OptimizeAction::Prune)`
  via the new `LanceStore::vacuum_old_versions` and aggregates the
  per-table `RemovalStats` into a `VacuumStats { bytes_removed,
  old_versions_removed, tables_pruned, tables_skipped }`. Always-on
  maintenance (matches `decay_worker`'s shape) — opt out with
  `MEM_VACUUM_DISABLED=1`. Tunables: `_INTERVAL_SECS` (default 86_400),
  `_OLDER_THAN_DAYS` (default 7; `0` rejected at worker config but
  permitted via the HTTP override below). On-demand entry:
  `POST /admin/vacuum` (new `src/http/maintenance.rs` module), optional
  body `{"older_than_days": N}` overrides the configured cutoff for
  one call. The Lance 7-day in-flight-transaction safety margin
  (`delete_unverified=false`) is always applied, regardless of the
  `older_than_days` override.

- **Auto-promote sweep** (`src/worker/auto_promote_worker.rs`) — opt-in
  background worker that moves long-idle `PendingConfirmation` capsules
  to `Active` after they sit untouched past `MEM_AUTO_PROMOTE_AGE_DAYS`
  (default 7). Audited via a `feedback_events` row with new
  `feedback_kind = "auto_promoted"` (status side-effect → `Active`, no
  confidence/decay delta). Default OFF; opt in with
  `MEM_AUTO_PROMOTE_ENABLED=1`. Tunables: `_AGE_DAYS`,
  `_INTERVAL_SECS`, `_TYPES` (CSV; default
  `experience,implementation,episode,diary` — `preference` and
  `workflow` deliberately excluded), `_DECAY_THRESHOLD` (default 0.5;
  capsules already flagged stale by `outdated` /
  `does_not_apply_here` feedback won't be auto-promoted). New HTTP
  endpoint `POST /reviews/auto_promote` for manual / cron-driven runs,
  supports `dry_run=true` (default) to preview candidate ids without
  writing.

### Changed

- `FeedbackKind::archived_status() -> bool` replaced by
  `FeedbackKind::status_after() -> Option<CapabilityCapsuleStatus>` so
  the new `AutoPromoted` variant can map to `Active` alongside
  `Incorrect`'s existing mapping to `Archived`. Internal API only —
  no JSON shape change on `feedback_events` or
  `POST /capability_capsules/feedback`.

### Backend abstraction — Phase 5 (`docs/backend-coupling.md` §6)

Phase 5 closed the backend-abstraction roadmap: services / workers /
pipeline no longer touch `Store` directly. The composition `Store =
LanceStore + DuckDbQuery` is now an implementation detail.

#### Added

- **`Backend` umbrella trait** (`src/storage/backend.rs`) — supertrait
  aggregating the 9 sub-traits extracted in Phases 2 + 3 (`CapsuleStore
  + CapsuleSearchStore + EmbeddingJobStore + EmbeddingVectorStore +
  GraphStore + TranscriptStore + EntityRegistry + SessionStore +
  MaintenanceStore + Send + Sync + 'static`). Blanket
  `impl<T> Backend for T where T: <9 sub-traits>` so `Store`
  automatically satisfies it; any future single-backend type that
  wires the same 9 traits (e.g. a hypothetical `PostgresBackend`)
  drops in without touching the umbrella.
- **`CapsuleStore` extended with 4 capsule-pool read methods**
  (`list_wings`, `capsule_stats`, `get_taxonomy`,
  `list_capability_capsules_in_scope`) that services used directly
  off `Store` but weren't on any trait. Implemented on all 3
  backends (Store / InMemoryCapsuleStore / PostgresCapsuleStore).
- **Parity test for `FeedbackSummary.auto_promoted`**
  (`feedback_summary_counts_auto_promoted`) — 28/28 capsule_store_parity
  scenarios across Lance + InMemory.
- `storage::VacuumStats` re-export so external callers don't reach
  into `storage::lance_store::`.

#### Changed

- **All service constructors take `Arc<dyn Backend>` instead of
  `Arc<Store>`**: `CapabilityCapsuleService::{new, new_with_settings,
  with_providers}`, `EntityService::new`, `TranscriptService::new`.
  Same for the 5 worker `run` / `tick` / `sweep_once` entry points
  (`embedding`, `transcript_embedding`, `vacuum`, `decay`,
  `auto_promote`). `app.rs::from_config` upcasts at construction —
  the concrete `Arc<Store>` only lives for the one Lance-only call
  (`set_transcript_job_provider`).
- **`CapabilityCapsuleRecord.version`: `u64 → i64`** (Phase 5 pain #1).
  Lance schema `DataType::UInt64 → Int64`, DuckDB read
  `row.get::<_, i64>(N)`, Postgres bind direct `.bind(memory.version)`
  (no more `try_from(u64)` guards). Aligns with every signed-integer
  column type across backends (Postgres BIGINT, DuckDB BIGINT, sqlite
  INTEGER). Lance schema change requires existing dev DBs to be
  rebuilt — acceptable for the local-first posture.
- **`FeedbackSummary.auto_promoted: u64` slot added** (pain #5).
  Routes `AutoPromoted` events through all 3 backend aggregators
  instead of dropping them into the catch-all. `#[serde(default)]`
  keeps old wire payloads valid.
- **`CapsuleStore` trait doc**: explicit atomicity contracts on
  `apply_feedback` + `replace_pending_with_successor` (pain #4) —
  both spec'd as **NOT atomic across backends**. Backends MAY use
  real transactions (Postgres does); the Lance backend cannot.
  Callers MUST be prepared for partial-state observation on crash.
  Per §3.3 rejection of trait-level `transaction()`.
- **Postgres `apply_feedback`** (`src/storage/postgres_capsule_store.rs`)
  collapsed from 4 SQL string variants to 1 statement using
  `SET col = COALESCE($N::TEXT, col)` with `Option<String>` binds
  (pain #3). Always 6 bindings, no dispatch on which combination of
  optional fields is being updated.
- **Pipeline narrow traits unified with storage sub-traits**:
  `pipeline::store_traits::{GraphRead, SessionStore}` (QW-5 era)
  deleted. Rust 1.86+ trait upcasting lets `&dyn Backend` coerce
  directly to `&dyn storage::GraphStore` / `&dyn storage::SessionStore`
  via supertrait bounds, so the pipeline now consumes the canonical
  storage-layer sub-traits without the indirection.
- **`lance_store` / `duckdb_query` modules: `pub` → `pub(crate)`**.
  External callers cannot reach the concrete halves; all access goes
  through `Backend` or one of the 9 sub-traits.

#### Removed

- **~22 LanceStore READ methods that were orphaned** when DuckDbQuery
  took over reads — discovered when the `pub(crate)` flip let
  clippy's dead-code lint finally fire. Deleted from
  `lance_store/capability_capsules.rs` (9 readers:
  `list_capability_capsules_for_tenant`, `get_pending`,
  `find_by_idempotency_or_hash`, `list_pending_review`,
  `search_candidates`, `recent_active_capability_capsules`,
  `fetch_capability_capsules_by_ids`,
  `list_capability_capsule_versions_for_tenant`,
  `list_capability_capsule_ids_for_tenant`),
  `lance_store/entities.rs` (2: `get_entity`, `list_entities`),
  `lance_store/graph.rs` (3: `neighbors`,
  `related_capability_capsule_ids`, `query_graph_edges`),
  `lance_store/transcripts.rs` (8: `get_conversation_messages_by_session`,
  `get_conversation_messages_by_session_paged`,
  `list_transcript_sessions`, `fetch_conversation_messages_by_ids`,
  `context_window_for_block`, `anchor_session_candidates`,
  `recent_conversation_messages`, `bm25_transcript_candidates`).
- Now-dead helpers in `lance_store/mod.rs`:
  `record_batch_to_graph_edges`, `record_batch_to_entities`,
  `sort_messages_chronological_asc`.
- **3 lance-side round-trip integration tests** that exercised only
  the deleted readers: `lancedb_graph_store_round_trip`,
  `lancedb_filter_methods_round_trip`,
  `lancedb_transcript_repository_round_trip`. The write+read
  invariant is canonically tested in `duckdb_query/*/tests`, which
  seeds with the same `lance.write_*` calls and asserts through the
  canonical DuckDB read path.
- `pipeline/store_traits.rs` (deleted module): the narrow `GraphRead`
  + `SessionStore` traits from QW-5 are obsolete now that the
  storage-layer canonical traits are used directly.

Net: ~+580 lines (umbrella trait + new CapsuleStore method impls +
sqlx queries + 4 pain-fix capsules) vs ~−1430 lines (dead lance reads
+ dead helpers + dead tests). cargo fmt --check + cargo clippy
--all-targets clean on both default and `--features postgres`. 179
lib unit tests + 125 integration tests across 17 suites green
(capsule_store_parity now 28/28 across Lance + InMemory).

#### Docs

- `docs/backend-coupling.md` — Phase 5 closeout: §6.6 marked ✅,
  §5.1 LT-1 ✅, all 5 Phase 4 pain points marked resolved with
  commit hashes, tail-item table updated through the `pub(crate)`
  flip.
- `docs/database-schema.md` — §0 architecture diagram updated for
  the Backend umbrella; §1 table list tags each table with its
  sub-trait; capability_capsules.version: UInt64 → Int64;
  feedback_kind: 5 → 6 (with auto_promoted row); §6 maintenance
  cleaned of stale HNSW / `mem repair` references.
- `docs/ROADMAP.MD` — status date bumped to 2026-05-18; #2/#3
  marked superseded; #16/#17 planning rows updated to not assume
  `mem repair` CLI exists; cross-reference added to
  `backend-coupling.md` as parallel workstream.
- `docs/mempalace-diff.md` — new §15.6 documents the Backend
  abstraction as a parallel workstream; §15.2 + §14 conversation
  archive section updated to reflect LanceDB-native ANN replacing
  the old usearch sidecar.

### KG ingestion expansion (ROADMAP #16 + #17 + #18 + #19)

Batch H of the MemPalace-alignment ROADMAP — wires more sources of
graph edges into ingest. Net result: knowledge-graph edge density
3-5× higher per memory, lookups like "find all memories with this
tag / from this session / that touched this file" become single
graph traversals instead of array-scan SQL.

#### Added

- **`EntityKind::Tag`** + new `pipeline::ingest::tag_node_id`
  helper. Every non-empty `tags[]` entry on a capsule emits a
  `memory:<id> --tagged--> entity:<uuid>` edge through
  `EntityRegistry::resolve_or_create`, so casing / whitespace
  variants ("Rust" / " rust " / "RUST") collapse to one canonical
  entity. `contradicts:` prefixed tags are skipped (historical
  artifact, never produced by live ingest paths).
- **`EntityKind::File`** + new helpers `file_node_id`,
  `file_alias`, `normalize_file_ref` in `pipeline::ingest`. Every
  `code_refs[]` entry emits a `memory:<id> --mentions_file-->
  entity:<uuid>` edge. The normalizer strips `:<digits>` line
  suffixes (last-`:` based — Windows drive letters like `C:\foo.rs`
  survive verbatim because their suffix is `\foo.rs`, not digits)
  and trims trailing slashes. Composite alias `<repo>:<path>` when
  the memory has a `repo` field so the same path in different
  repos is distinct; bare `<path>` otherwise.
- **`ToNodeKind::LiteralSession(String)`** variant + new
  `pipeline::ingest::session_node_id` helper. Every memory carrying
  a non-blank `session_id` (every ingest auto-buckets into a
  session via `pipeline::session::resolve_session`) emits a
  `memory:<id> --extracted_from--> session:<sid>` edge. Direction
  is memory→session (not session→memory) so
  `close_edges_for_capability_capsule` auto-closes the edge on
  capsule hard-delete instead of leaving a dangling pointer.
  Session ids bypass `EntityRegistry` because they're already
  canonical UUIDv7 — no alias normalization needed.
- Three new unit tests in `pipeline::ingest::tests` covering
  tagged + mentions_file + extracted_from edges, plus a path
  normalization unit test covering line-suffix stripping, Windows
  paths, trailing slashes, empty inputs.

#### Audit

- **ROADMAP #17 (`supersedes` graph edge)** was already shipped at
  `pipeline/ingest.rs:236-246` — emits the edge through
  `ToNodeKind::LiteralMemory` when `supersedes_capability_capsule_id`
  is set. ROADMAP row was stale; marked ✅ retroactively.

#### Changed

- `tests/ingest_api.rs::get_memory_returns_full_record` —
  previously asserted `graph_links == []` for a fresh capsule with
  no scope fields. With #18 every ingest auto-buckets into a
  session and writes the `extracted_from` edge, so the assertion
  loosened to "exactly one `extracted_from` edge whose target
  starts with `session:`".

### Storage cleanup pass (audit A + B + C + D1 + D2)

Targeted sweep across `src/storage/` after the Phase 5+
`pub(crate)` flip surfaced accumulated cruft. Two doc-only / pure
refactor commits + two real bug fixes.

#### Added

- `LanceStore::delete_feedback_events_by_capability_capsule_id` —
  cascade helper mirroring the existing
  `delete_embedding_jobs_by_capability_capsule_id`. Used by the
  D2 cascade fix below.

#### Changed

- **`CapsuleStore::delete_capability_capsule_hard`** trait doc
  now specs cascade contract explicitly: implementations MUST
  cascade to `feedback_events` / `embedding_jobs` /
  `capability_capsule_embeddings`; `graph_edges` SHOULD be
  CLOSED (`valid_to = now`) not deleted, preserving the
  time-travel `valid_from / valid_to` schema. Atomicity contract
  inherited from Phase 5 pain #4 — NOT atomic across backends,
  callers MUST be prepared for partial state, retry is safe
  because every cascade helper is idempotent on empty-set input.
- Lance, Postgres, and InMemory backends all updated to honor the
  new cascade contract. Postgres wraps the entire cascade in
  `BEGIN/COMMIT` (real transaction, tightens guarantee on that
  backend without changing the caller surface).
- **`Store` inherent methods** — 7 pure-delegate methods that
  carried stale "TODO: route to DuckDbQuery once added" comments
  (`feedback_summary`, `get_capability_capsule`,
  `latest_active_session`, `list_successful_episodes_for_tenant`,
  `list_embedding_jobs`, `get_capability_capsule_embedding_row`,
  `latest_embedding_job_status_for_hash`) inlined into their
  corresponding trait impl bodies and removed from the inherent
  surface. Phase 5 made the routing decision moot (services
  dispatch through traits, not inherent methods) so the TODO was
  inviting work that wouldn't change observable behavior.
- **Module-level doc blocks** rewritten in
  `src/storage/lance_store/mod.rs` (50 lines of "skeleton",
  "alternate backend to DuckDbRepository", "Status: read path
  fully working", "Mutating methods are still
  `unimplemented!()`", "Schema mapping planned, not yet
  enforced", "behind a `lancedb` Cargo feature") and
  `src/storage/duckdb_query/mod.rs` (60 lines of "Coverage so
  far" lists + "The next commit introduces the Store composition
  layer") to reflect current architecture: implementation halves
  behind the `Backend` umbrella, write/read split documented, no
  more "in progress" framing.
- **11 `DuckDbRepository` / "legacy DuckDB" comment references**
  scrubbed across `types.rs`, `lance_store/{episodes,sessions,
  transcripts,capability_capsules,mod}.rs`, and
  `duckdb_query/{mod,transcripts,entities}.rs`. The
  `DuckDbRepository` type was deleted months ago; comments
  describing the code as "mirroring" it were misleading. Updated
  to describe the code's current shape directly.

#### Fixed

- **`DuckDbQuery::list_capability_capsule_versions_for_tenant`
  (D1)** — took `capability_capsule_id` as a parameter but
  ignored it (`let _ = capability_capsule_id;` at the top of the
  body), returning version links for EVERY capsule in the tenant.
  Service-layer `get_memory_detail` expected the chain rooted at
  the requested id, so callers got over-broad results when the
  tenant had more than one chain. The bug hid behind a passing
  integration test (`get_memory_returns_full_version_chain_for_successor_ids`)
  whose fixture happened to seed a tenant with only one chain —
  broken impl and correct impl returned the same set. Rewrote as
  a single recursive CTE that anchors on `(tenant, id)` and
  recurses both directions of the supersedes link with the tenant
  filter applied at every step. New unit tests assert chain
  isolation from unrelated rows in the same tenant + cross-tenant
  isolation.
- **`*::delete_capability_capsule_hard` cascade (D2)** —
  previously deleted only the `capability_capsules` row,
  leaving orphans in `feedback_events`, `embedding_jobs`,
  `capability_capsule_embeddings`, and outgoing `graph_edges`.
  Because capsule ids are uuidv7 (never reused), orphan rows
  accumulated monotonically — no future capsule could ever
  collide with a deleted id to incidentally clean up. Lance
  impl now sequences DELETE on the 3 satellites + CLOSE on
  outgoing graph_edges; Postgres wraps the cascade in
  `BEGIN/COMMIT`; InMemory clears the matching feedback events
  from the state vec. New parity test
  `delete_hard_cascades_feedback_events` runs against both Lance
  and InMemory.

Net storage-cleanup diff: -94 lines (A+B+C, pure refactor) +
~250 lines / -33 lines (D1 + D2, real bug fixes with new tests).
185 lib unit tests + 30 capsule_store_parity tests (+2 cascade)
+ 95 other integration tests across 19 suites green. `cargo fmt
--check` + `cargo clippy --all-targets` clean on both default and
`--features postgres`.

---

## 2026-05-07 — `0.1.1`

Storage-layer overhaul of the BM25 path, cursor-paged transcripts, and
auto-feedback from transcripts. Foundation for the KG ingestion-surface
work tracked in ROADMAP #16–#20 (still 🚧).

### Added

- **Tantivy-backed BM25** (`src/storage/fts.rs`) — incremental writes
  per segment; no rebuild worker, no DuckDB dep-tracker bug. Lazy
  `IndexWriter` so read-only repo opens (e.g. `mem repair
  --rebuild-graph`) don't fight a running `mem serve` for the per-dir
  lockfile. Bootstrap from existing rows on first open.
- **`POST /transcripts` cursor pagination** (`limit` + structured
  `cursor` + `since` / `until`). Composite cursor
  `(created_at, line_number, block_index)` so ms-collisions don't drop
  rows. `next_cursor` + `has_more` in response.
- **Admin web — transcript drawer**: quick filter bar
  (全部 / 24h / 今天 / 昨天) + IntersectionObserver-driven infinite
  scroll, default page size 200.
- **`mem feedback-from-transcript`** — scans a transcript for
  `mcp__mem__memory_search*` calls, finds memory_ids whose returned
  text reappears in subsequent assistant blocks, POSTs `applies_here`.
  Wired into `Stop` and `PreCompact` hooks; closes the lifecycle loop
  even when the agent forgets to call `memory_feedback` itself.
- **`AGENTS.md` "Feedback discipline"** section — five `feedback_kind`
  values, when to fire, the two MCP entry points
  (`memory_feedback` / `memory_apply_feedback`).

### Changed

- `GET /transcripts?session_id=…` → `POST /transcripts` with JSON
  body. Old GET form's `:`-separated string cursor shredded ISO-8601
  timestamps; structured object cursor avoids the collision and the
  URL-length issues.
- `synthetic_recall_bench` regression bound widened from 0.06 to 0.25
  for hybrid-vs-BM25: tantivy's BM25 alone now ≈0.98 NDCG@10, so
  fake-embedding hybrid naturally trails it.

### Fixed

- **`mem mine` was dropping ~80 % of user-typed messages**: Claude
  Code emits `message.content` as either an array of blocks
  (tool-uses / attachments present) or a plain string (raw user text).
  `parse_transcript_full` only handled the array form — string-shaped
  payloads were silently skipped. After the fix, re-mining a 5097-block
  session pulled 275 previously-lost rows in (191 user/text). Same
  shape fix applied to `feedback-from-transcript`.

### Removed

- `src/worker/fts_worker.rs`, `fts_dirty` AtomicBool, `ensure_fts_index_fresh`,
  `is_fts_dependency_error` recovery path. All obsolete with tantivy:
  no rebuild cycle, no background catch-up task, no DuckDB FTS
  dep-tracker bug to detect.

### Docs

- `docs/ROADMAP.MD` `#16–#20` — KG ingestion-surface widening (`tags`,
  `supersedes`, transcript→memory, `code_refs`, content extraction).
- `docs/mempalace-diff.md` `§5.1` — empirical evidence for the gap
  (10 memories / 8 entities / ~3 edges-per-memory in the live tenant).

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
