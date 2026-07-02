# Changelog

All notable changes to `mem` are documented here. Format inspired by
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

This project does not yet publish versioned releases; entries below
are organized by feature wave (merge commit ranges on `master`).

## [Unreleased]

### Added — H-series borrowings: ingest linking, note evidence, LoCoMo (2026-07-02)

- **H1 — ingest-time neighbor linking** (zero-LLM slice of A-Mem's link
  network, `closes oss-memory-diff H1`): after embedding a fresh `Active`
  capsule, the embedding worker writes `related_to` edges
  (`extractor="ingest_link"`, cosine riding as edge confidence) to its top-4
  semantic neighbors inside [`MEM_INGEST_LINK_THRESHOLD` (default 0.80),
  near-dup threshold) — at/above that band belongs to the O2/O7(a) supersede
  lane. Pure connectivity for O4's graph boost; opt-in
  `MEM_INGEST_LINK_ENABLED` (default OFF), best-effort after the O2 check.
- **H2 — feedback notes as refine evidence** (review-gated version of
  MemOS's natural-language correction, `closes oss-memory-diff H2`):
  `execute_refine` now pulls the verbatim notes riding `outdated` feedback
  events (new `CapsuleStore::list_feedback_for_memory`, implemented on
  lance / postgres / clickhouse / in-memory) into the refine placeholder's
  conflict-evidence list — the reviewer sees WHY a capsule is stale, not
  just how many times it was flagged.
- **H3 — LoCoMo parity bench harness** (`closes oss-memory-diff H3`):
  `tests/locomo_bench.rs` mirrors `mempalace_bench`'s discipline —
  session-level evidence recall@k (explicitly NOT the LLM-judged LoCoMo QA
  accuracy mem0/Zep quote), categories 1–4 with adversarial excluded,
  `LOCOMO_SAMPLE` stratified sampling, per-conversation shared stores, real
  `locomo10.json` drop-in with a committed synthetic fallback subset.
  Harness validated end-to-end on the subset; the real-dataset public
  number is pending a run.

### Added — evolution E5, ③ refine + ④ split detectors (2026-07-02)

- **③ refine** (`closes evolution-worker E5`): a capsule that is BOTH
  contradicted (hanging `suspected_supersede` edge from O2/O7a, or ≥2
  accumulated `outdated` feedback events) AND still valuable (recalled
  within 30 days — the value gate runs first so per-capsule feedback reads
  stay bounded to the hot set) earns a K-gated `PendingConfirmation`
  review placeholder with `refined_from` lineage. Raw material references
  the source by id + summary + conflict list — never copies content
  (verbatim rule; deliberate deviation from the doc's "旧内容" wording,
  generalize precedent). The fact_check triple-conflict channel is left
  unwired pending real triple corpora.
- **④ split**: a multi-chunk capsule whose chunk vectors form ≥2 groups
  (union-find at `cluster_threshold`) with EVERY cross-group pair at/below
  `split_threshold` (`MEM_EVOLUTION_SPLIT_THRESHOLD`, default 0.5) earns a
  split placeholder carrying the chunk-group plan + `split_from` lineage.
  Mild internal drift (e.g. 45° ≈ 0.707 cosine) never shreds a capsule.
  New `EmbeddingVectorStore::get_capability_capsule_embedding_chunks`
  read on all three backends (lance / postgres / clickhouse); the sweep's
  map vector is the first chunk — per-capsule query count unchanged.
- **SynthesisTask::{Refine, Split}** variants on the review backend;
  `synthesis=off` ↔ `review` produce byte-identical placeholders (the E5
  fold-in acceptance — Phase 1 IS the review backend).
- ③④ inherit the whole E2–E4 machinery: K gate, executed-history
  suppression, reject→`rejected`+re-earn, rollback (shared placeholder
  arm), and `edit_and_accept_pending` now re-owns lineage with the
  placeholder's OWN relation (tag-routed: generalizes / refined_from /
  split_from).
- Phase 1 of ③④ never touches the source capsule — after accepting the
  rewritten/split content, the reviewer supersedes or archives the source
  explicitly (the "edit another capsule" review action stays an open
  design item, same as ⑤'s deferred topics relocation).

### Added — evolution E4, ⑤ reweight + ⑥ Hebbian (2026-07-02)

- **⑤ reweight** (`closes evolution-worker E4`): stable, highly-recalled
  clusters (>0.5 of members recalled within one sweep interval) gain +0.02
  confidence per signal cycle (emission stops at the 0.9 cap); K-cycle map
  orphans with zero recalls gain +0.05 decay per cycle (archiving stays
  `idle_archive_worker`'s job). Every nudge is an auditable
  `feedback_events` row via two new typed system kinds
  `system_reweight_up` / `system_reweight_decay` (§12.2 settled) with their
  own `FeedbackSummary` + `/metrics` buckets. Reweight candidates are
  RECURRING — they never park in `executed`; the open gate re-fires each
  cycle the signal holds, silence decays them as usual.
- **⑥ Hebbian co-recall edges**: capsules stamped by the same
  `last_used_worker` flush (identical `last_used_at`) are one co-recall
  batch; a pair earns a `co_recalled_with` edge (`extractor="evolution"`,
  feeding O4's 1-hop graph boost) after K FRESH batches — the candidate's
  `params` carries the batch timestamp so a stale batch re-observed across
  sweeps is silence, not evidence. Weak-edge retirement closes idle
  evolution-owned co_recalled_with edges after `prune_idle_cycles`
  (`MEM_EVOLUTION_PRUNE_IDLE_CYCLES`, default 3) sweep intervals, measured
  conservatively against max(edge birth, potentiation stamp, either
  endpoint's own `last_used_at`); caller edges and `user_tunnel:*` are
  never touched. Corecall rollback closes the edge via the E2 rollback
  surface.
- **Public feedback API hardening**: `FeedbackKind::is_system()` —
  `submit_feedback` (HTTP + MCP) now rejects system-emitted kinds,
  enforcing `AutoPromoted`'s long-documented "never sent by
  submit_feedback" for the first time.
- Deferred with notes: ⑤'s topics-relocation review proposal (needs an
  "edit another capsule" review action type — revisit with E5); ⑤ rollback
  stays note-based inversion per §11 (recurring ops have no single
  executed state).

### Added — evolution E2/E3, review-loop closure (2026-07-02)

- **Rollback of one executed evolution candidate** (doc §11):
  `POST /reviews/evolution/rollback {tenant, candidate_id}` +
  `evolution_worker::rollback_candidate`. merge: losers → `Active`,
  `merged_into` lineage edges closed; generalize: proposal capsule →
  `Archived`, all its edges closed. The candidate row survives as a
  `rolled_back` tombstone — nothing is ever physically deleted. Done as
  service + HTTP (not the design's interim CLI): `mem serve` is the Lance
  dataset's single writer, a second direct-store process would conflict.
- **Reject closes the loop** (E3): `reject_pending` now closes the rejected
  capsule's incident graph edges (terminal-transition hygiene — supersede
  and hard-delete already did this) and, for evolution proposals, flips the
  producing candidate row `executed` → `rejected`. The cluster may then
  re-propose, but as a fresh candidate it must re-earn the K-cycle gate
  before another placeholder reaches the review queue.
- **Accept carries lineage** (E3): `edit_and_accept_pending` re-writes
  `generalizes` edges from the accepted successor (a NEW capsule id) to the
  sources recorded in the placeholder's `evidence` — proposal-time edges on
  the placeholder are closed by the accept and would otherwise orphan the
  lineage.
- Acceptance tests: `tests/evolution_merge.rs` (post-merge retrieval sees
  only the canonical; dry-run preview set == live execution set; rollback
  round-trips incl. HTTP) + `tests/evolution_review.rs` (both review legs).

### Changed

- **Dedup narrowed to mirror duplicates** (§12.1 settled with E2):
  `MEM_DEDUP_THRESHOLD` default 0.92 → **0.95**. The 0.88–0.95
  near-duplicate band is the evolution ① merge operator's territory now, so
  the two workers never make competing archive-vs-merge calls on one pair.

## 2026-06-29 — `0.2.4`

The **OSS-comparison line** (`docs/oss-memory-diff.md` O1–O7) plus its
broader-track follow-ons (G-series G4/G5) and an online-observability layer.
Two parallel themes: catch up to mem0 / agentmemory / Zep-Graphiti on
write-time hygiene + recall quality, and make the running service measurable.

### Added

- **O5 — output-layer secret redaction** (default ON, opt out with
  `MEM_REDACT_SECRETS_DISABLED=1`). `pipeline/redact.rs` masks high-confidence
  secrets in *derived* output only — storage stays verbatim. Four seams: capsule
  compress + capsule embedding worker, and the transcript pre-embed + transcript
  **search** output (verbatim-fetch paths `capability_capsule_get` /
  `transcripts_range` are intentionally not redacted). White-list: `sk-`, AWS
  `AKIA`, private-key blocks, `<private>`, GitHub classic + fine-grained
  (`github_pat_`), JWT, `Bearer`, Stripe (`sk_live_`/`rk_live_`), Slack
  (`xox[baprs]-`), Google (`AIza…`); `\b`-guarded against in-word false hits.
- **O6 — recall-quality eval framework.** Gold-set recall regression gate
  (`tests/golden_recall/`, a hermetic non-ignored CI step) + LongMemEval parity
  harness (`tests/mempalace_bench.rs`, `#[ignore]`; real-set public number
  pending a faster machine).
- **O6d — online observability** (`src/metrics.rs` + `GET /metrics`): a
  process-local atomic-counter registry exposed as JSON. Pipeline-scoped names
  (`capsule_*` / `transcript_*` / `episode_*` ingest+search) + `redaction_hits`
  + `neardup_flags` + `kg_auto_invalidated` + per-`FeedbackKind` counts. The
  online complement to O6's offline eval.
- **O7 — Mem0-style auto-extraction, zero-LLM by default.** (a) cluster-canonical
  near-duplicate supersede proposal in the embedding worker; (b) heuristic
  high-signal extraction lane in `mem mine` (`MEM_MINE_HEURISTIC_EXTRACT`,
  default off, review-gated); (c) opt-in generative-LLM lane
  (`MEM_MINE_LLM_EXTRACT` + gateway, default off, fail-safe — silently falls
  back to (a)/(b) when no LLM).
- **O1 — retrieve freshness bonus.** The freshness score now anchors on
  `last_used_at` (else `updated_at`), symmetric with the decay clock — a
  recently *used* capsule ranks fresher, not just a recently *written* one.
- **G4 — zero-LLM contradiction auto-invalidation.** Asserting a new
  `(from, predicate, to)` whose predicate is configured functional
  (`MEM_KG_FUNCTIONAL_PREDICATES`, default empty = off) auto-closes conflicting
  active `(from, predicate, other_to)` edges (Graphiti's pattern, structured
  triples only). Closures count in `/metrics::kg_auto_invalidated`.
- **G5 — user/project profile.** `POST /capability_capsules/profile` aggregates
  the in-scope active `Preference` + `Workflow` capsules and tenant entities into
  one queryable "conventions" view — read-side, no new storage.
- **Transcript ANN self-heal.** On the stale-index ragged-batch error, the
  semantic channel now force-reindexes and retries once (guarded against
  stampede) before falling back to the BM25-only soft-degrade.

### Fixed

- **Per-session ingest cap → HTTP 429** (was 400): the throttle now returns the
  dedicated `StorageError::RateLimited`, and its counter map is soft-bounded
  (100k sessions, fail-open reset) so it can't grow unbounded.

### Changed

- Recall headline spells out section counts in full words (`0 directives,
  3 facts, …`) instead of `0d 3f`.
- `mem mine` / pre-compact hook banner glyph `✦` → `🧠`.

### Docs

- New `docs/oss-memory-diff.md` §8 records the G-series (G1→O6, G4✅, G5✅, and
  the deferred G2/G3/G6/G7 with assessments).
- `AGENTS.md` "Key env vars" mega-paragraph split into a per-var bullet list.

## 2026-06-26 — `0.2.3`

### Fixed

- **Auto-recall noise reduced** — two precision fixes for the UserPromptSubmit
  recall banner:
  - *Scoped guidance no longer leaks across projects.* `finalize`'s
    Preference/Workflow floor-exemption is now scope-aware: `Global`/`Workspace`
    guidance always surfaces, but `Project`/`Repo`-scoped guidance must match the
    active `scope_filters` (an empty filter set preserves the original
    always-surface behavior, so raw MCP searches are unchanged). The
    `recall-prompt` hook now derives `scope_filters` from the payload `cwd`, so a
    `project:NVR-APP` preference stops surfacing while working in another repo.
    `error-recall` stays global on purpose (incidents are cross-repo).
  - *Low-relevance transcript windows are dropped.* The banner floors injected
    windows by RRF score via `MEM_RECALL_TRANSCRIPT_MIN_SCORE` (default 20),
    cutting loose semantic-match noise.

## 2026-06-26 — `0.2.2`

CI-only patch — no binary or behavior change vs `0.2.1`.

### Fixed

- **CI runner disk exhaustion.** Now that postgres + clickhouse are default
  dependencies, the `rust` job's `cargo test` compiles a much larger target tree
  (sqlx / clickhouse / pgvector + lance) and exhausted the ~14 GB ubuntu-latest
  disk → `No space left on device` while lance wrote test datasets (a marginal
  flake — it failed `0.2.1`'s tag CI). The `rust` / `postgres` / `clickhouse`
  jobs now reclaim ~25 GB via `jlumbroso/free-disk-space` as their first step.

## 2026-06-25 — `0.2.1`

### Added

- **Visible hook headlines.** Every `mem` hook that previously injected only
  model-facing `additionalContext` (invisible to the user) now ALSO emits a
  one-line user-visible `systemMessage` headline, so you can see when mem fires:
  - UserPromptSubmit recall → `🧠 mem · recalled N (Nd Nf Np Nw)`
  - PostToolUseFailure recall → `🧠 mem · N incident hit(s) for the last failure`
  - PostToolUse commit-nudge → `💡 mem · committed \`<subject>\` — consider propose_experience`
  - SessionStart wake-up → `🧠 mem · session-start memories loaded`

  The `additionalContext` payloads are unchanged (their format is still parsed
  back by `cli/feedback.rs::scan_transcript`).

## 2026-06-25 — `0.2.0`

Backend-expansion release. Adds a third storage backend (ClickHouse) and two
new CLI subcommands (`mem import`, `mem sync`); makes the Postgres + ClickHouse
backends **default dependencies** (always compiled, selected at runtime).

### Added

- **ClickHouse storage backend** (`MEM_BACKEND=clickhouse` + `MEM_CLICKHOUSE_URL`).
  A `ClickHouseBackend` (clickhouse-rs client) implements all 11 storage
  sub-traits, so the blanket `Backend` impl makes it a runtime-selectable peer
  to Lance (default) and Postgres. ANN via `Array(Float32)` + `cosineDistance`,
  lexical via substring/token candidates, RRF fused Rust-side; the update-heavy
  lifecycle (decay / status / supersede) is modeled as versioned re-inserts into
  `ReplacingMergeTree`. Built P1–P6 and **e2e-validated against ClickHouse 26.5**
  (all parity scenarios pass). See `docs/clickhouse-backend.md`.
- **`mem import` CLI** — bulk **archive-only** transcript import, extensible per
  source agent (`mem import claude-code` walks `~/.claude/projects/**/*.jsonl`).
  Reuses the `mine` parser's block half (no memory extraction), idempotent via
  the server-side `(transcript_path, line_number, block_index)` dedup. The
  rebuild path for the verbatim transcript archive.
- **`mem sync` CLI** — verbatim **any → any** store-to-store migration across
  Lance / Postgres / ClickHouse (`--from <kind>:<locator> --to … --tenant …`).
  Copies capsules (+ version chains), transcripts, entities, episodes, and
  active graph edges through existing trait reads/writes — original ids /
  timestamps / lifecycle state preserved, not re-ingested. Idempotent
  (re-run = resume), embeddings rebuilt on the target, `--domains` subset +
  `--dry-run`. See README «Migrating between backends».

### Changed

- **Postgres + ClickHouse are now default dependencies** — the `postgres` /
  `clickhouse` cargo features were **removed**. Both backends are compiled into
  every build and selected purely at runtime via `MEM_BACKEND` +
  `MEM_POSTGRES_URL` / `MEM_CLICKHOUSE_URL`; `sqlx` / `pgvector` / `clickhouse`
  are always in the dep graph. The feature-gated parity suites now self-skip
  without `MEM_TEST_{POSTGRES,CLICKHOUSE}_URL`, so a plain `cargo test` compiles
  and runs them.

### Fixed

- **Transcript import/mine `413 Payload Too Large`** on tool-result-heavy
  sessions — replaced fixed 100-block batching with **size-aware batching**
  (bounded by both block count and a 1.5 MiB serialized-byte budget) so a heavy
  session's large blocks no longer overflow the server's 2 MiB request limit.
- **Docker release build** — `COPY migrations ./migrations` into the builder.
  With postgres/clickhouse always compiled, their stores `include_str!` the
  migration SQL at compile time; the image lacked the files.
- **`make install`** — `cargo install --path . --locked`. Without `--locked`,
  `cargo install` re-resolves and pulls pgvector's sqlx onto sqlx-core 0.9.0
  while our `sqlx = 0.8` stays 0.8.6 → two sqlx-core versions → `pgvector::Vector`
  fails its sqlx bounds. The locked resolution unifies on 0.8.6.

### Docs

- Route-B documentation sweep (DuckDB → lance-native) across README,
  `docs/api-data-flow.md`, and src comments; README «Storage backends» reworked
  for three default-compiled backends + `mem import` / `mem sync` usage.

## 2026-06-24 — `0.1.9`

Route-B release. **The DuckDB read engine is removed** — `mem` is now
lance-native only: Lance datasets on disk + lancedb-Rust native reads +
a Tantivy BM25 full-text subsystem. ~8200 lines deleted; the `duckdb`
dependency is gone.

### Changed

- **Reads are lancedb-Rust native.** All Store read methods that went
  through the in-process DuckDB lance-extension (`SELECT … FROM ns.main.*`)
  were reimplemented on the LanceDB Rust query API: `only_if` SQL filters,
  `nearest_to` ANN, `fetch_*_by_ids` hydration, with ranking / aggregation /
  RRF compose done Rust-side (`pipeline/retrieve.rs`). Read freshness comes
  from opening the read connection with `read_consistency_interval(0)`
  (Strong), replacing the old `refresh` / `mark_dirty` / `ensure_fresh`
  connection-rebuild machinery (also removed).
- **Full-text search is a self-built Tantivy index** (`src/storage/fts.rs`,
  jieba tokenizer), replacing the DuckDB `lance_fts(...)` path that hit the
  upstream `lance scanner.rs` ragged-batch bug. In-RAM, full-rebuilt at
  startup + each maintenance sweep (cheap: ~1s at the real ~31k-doc scale).
  CJK queries are term-split before search.
- **Decay writes moved to lancedb Rust `table.update()`** (Phase 2,
  irreversible gate): `apply_time_decay` + `bump_last_used_at` no longer
  write through the DuckDB extension. The old dual-writer commit race is
  gone (single Rust-API writer); lance's native `execute_with_retry` handles
  optimistic-concurrency conflicts. Decay output is verified bit-identical
  to the old path.

### Removed

- The `duckdb` crate dependency; `src/storage/duckdb_query/` (~6000 lines);
  the `ReadEngine` / `MEM_READ_ENGINE` read-engine switch; `MEM_DUCKDB_THREADS`
  and the r2d2 DuckDB read pool (`MEM_RW_POOL_DISABLED`); `StorageError::DuckDb`;
  the DuckDB-ATTACH snapshot-visibility probe + the two duckdb PoC examples.

### Notes

- The on-disk dataset directory keeps its legacy name `mem.duckdb` for
  path compatibility — it holds Lance datasets, not a DuckDB file.
- FTS parity is **soft** (overlap@10 ≥ 0.8) by design — Tantivy BM25 is a
  different engine than the old `lance_fts`; non-FTS read parity is exact,
  asserted against frozen DuckDB-generated goldens (`tests/golden/`).
- Design + execution record: `docs/remove-duckdb-keep-lance.md`.

## 2026-06-22 — `0.1.8`

Memory-footprint release. Swaps the Rust global allocator from glibc malloc
to jemalloc so a long-lived `mem serve` returns idle memory to the OS instead
of ratcheting RSS upward.

### Changed

- **jemalloc as the global allocator** (`src/main.rs`, `Cargo.toml`): glibc's
  per-thread arenas (8×nproc = 768 on the 96-core deployment box) retained
  freed memory and never returned it to the OS, ratcheting RSS 5→8 GB over
  days even with `MALLOC_ARENA_MAX` capped. `tikv-jemallocator` is now wired
  as `#[global_allocator]`, and `main()` enables `background_thread` so
  jemalloc's decay returns idle pages on a timer rather than only on
  allocation activity. Governs all Rust-side allocations (the dominant churn
  is local embedding inference via Candle/gemm); DuckDB's bundled-C
  allocations still hit the system allocator and stay capped via
  `MALLOC_ARENA_MAX`. Local A/B: ~57–94% of the idle rise returned to the OS
  vs. glibc's 0%. `background_thread` is best-effort — unsupported on some
  musl builds, where it no-ops and decay still runs lazily.

## 2026-06-18 — `0.1.7`

Consolidation release. Supersedes `0.1.5` and `0.1.6` (both deprecated):
their changelog/release notes carried a transcript-bug root-cause that was
later disproven, and `0.1.5` was an unreleased intermediate. `0.1.7` is the
clean reference point — same runtime behavior as `0.1.6` plus a green CI
and corrected docs.

### Fixed

- **CI `postgres` job** (`fix(test)` 8f1d413): `connect_fresh` now creates
  the pgvector extension under a transaction-scoped advisory lock + `SCHEMA
  public`, so parallel integration tests on a fresh DB no longer race
  `CREATE EXTENSION` (`duplicate key … pg_extension_name_index` →
  `type "vector" does not exist`). Test-only; production unaffected.

### Docs

- **Transcript ragged-batch root cause corrected** (a5df25a): it is a Lance
  **stale/partially-covering-index** scan bug (present in both the DuckDB
  lance-extension 4.0 reader AND the lance-7.0 Rust reader — verified, so it
  is NOT a reader/writer version skew, and NOT IVF over-partitioning). A
  fresh `POST /admin/reindex` clears it; it recurs as the unindexed delta
  regrows. Mitigation is unchanged from `0.1.6`: `TranscriptService::search`
  soft-degrades every lance-scan boundary (never 500s). The `0.1.7` attempt
  to reroute reads through the lance Rust API was reverted — the lance 7.0
  reader hit the same bug.

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
