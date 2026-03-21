# Hybrid Memory Retrieval (Phase 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use @superpowers/subagent-driven-development (recommended) or @superpowers/executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add asynchronous embeddings, DuckDB-backed job and embedding state, and hybrid lexical+semantic retrieval for `memory` search while preserving existing HTTP contracts and compress pipeline behavior.

**Architecture:** Introduce an `EmbeddingProvider` trait (`fake` | `real`), enqueue `embedding_jobs` on ingest and content-changing flows, run an in-process poller that claims jobs and upserts `memory_embeddings` when `content_hash` still matches, and extend search to fetch lexical + semantic candidates in parallel, merge by `memory_id` with origin metadata, then rerank deterministically before `compress::compress`.

**Tech Stack:** Rust (Axum, Tokio, DuckDB `duckdb` crate), SQL migrations under `db/schema/`, existing modules `src/storage/duckdb.rs`, `src/pipeline/retrieve.rs`, `src/service/memory_service.rs`, `src/http/memory.rs`.

**Spec:** `docs/superpowers/specs/2026-03-21-hybrid-memory-retrieval-design.md`

---

## File map (create / modify)

| Area | Path | Responsibility |
|------|------|----------------|
| Schema | `db/schema/002_embeddings.sql` (new) | `memory_embeddings`, `embedding_jobs` tables, indexes (tenant, status, available_at, memory_id, unique live job) |
| Schema bootstrap | `src/storage/schema.rs` | Include or chain `002_embeddings.sql` after `001_init.sql` (follow existing `include_str!` pattern) |
| Config | `src/config.rs` | `EMBEDDING_*` env vars; fail fast if `real` provider selected without required secrets |
| Domain | `src/domain/embedding.rs` (new) | Job status enum, DTOs for API responses, provider descriptor types |
| Embedding | `src/embedding/mod.rs`, `provider.rs`, `fake.rs`, `openai.rs` (new) | `EmbeddingProvider` trait, fake deterministic vectors, optional OpenAI adapter |
| Pipeline | `src/pipeline/hybrid.rs` (new) | Merge lexical + semantic candidates, `CandidateOrigin`, unified score inputs |
| Pipeline | `src/pipeline/retrieve.rs` | Extend or wrap scoring: lexical_score, semantic_score, hybrid_bonus, keep graph expansion path compatible |
| Storage | `src/storage/duckdb.rs` | CRUD for embeddings/jobs, claim job transaction, semantic k-NN query, rebuild enqueue |
| Storage | `src/storage/mod.rs` | Re-export new types if needed |
| Service | `src/service/memory_service.rs` | Ingest enqueue; search hybrid path; get_memory embedding fields; delegate rebuild/list jobs |
| Worker | `src/service/embedding_worker.rs` (new) | Poll, claim, provider call, write-back / stale / retry backoff |
| HTTP | `src/http/embeddings.rs` (new), `src/http/mod.rs` | `GET /embeddings/jobs`, `POST /embeddings/rebuild`, `GET /embeddings/providers` |
| HTTP | `src/http/memory.rs` | No request shape change for search; optional later |
| App | `src/app.rs`, `src/main.rs` | Wire provider, repository, spawn worker task with shutdown-friendly cancel |
| Lib | `src/lib.rs` | Register new modules |
| Tests | `tests/embedding_*.rs`, extend `tests/search_api.rs`, `tests/ingest_api.rs` | Provider, job lifecycle, hybrid retrieval, smoke for new routes |

---

## Task 1: Schema — `memory_embeddings` and `embedding_jobs`

**Files:**
- Create: `db/schema/002_embeddings.sql`
- Modify: `src/storage/schema.rs` (load new migration batch)
- Test: add integration assertion in a new test or extend `tests/ingest_api.rs` to open DB and `pragma table_info`

- [ ] **Step 1: Write failing test** — Open `DuckDbRepository::open` against a temp path, run bootstrap, query `information_schema.tables` or `pragma table_info('memory_embeddings')` and expect columns including `embedding`, `content_hash`.

```rust
// tests/schema_embeddings.rs (excerpt)
#[tokio::test]
async fn bootstrap_creates_embedding_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.duckdb");
    let _repo = mem::storage::DuckDbRepository::open(&db).await.unwrap();
    // use raw connection or add a small test-only helper to assert table exists
}
```

- [ ] **Step 2: Run test** — `cargo test bootstrap_creates_embedding_tables -- --nocapture`  
  **Expected:** FAIL (missing table)

- [ ] **Step 3: Add SQL** — Tables per spec: `memory_embeddings` (PK `(memory_id)` or `(tenant, memory_id)` per your multi-tenant rule; align with `memories` PK being `memory_id` globally), `embedding_jobs` with statuses `pending|processing|completed|failed|stale`, indexes for worker poll (`status`, `available_at`) and uniqueness for live job `(tenant, memory_id, target_content_hash, provider)`.

- [ ] **Step 4: Wire bootstrap** — Execute `002` in `bootstrap()` same as `001`.

- [ ] **Step 5: Run test** — **Expected:** PASS

- [ ] **Step 6: Commit** — `git add db/schema/002_embeddings.sql src/storage/schema.rs tests/schema_embeddings.rs && git commit -m "feat(db): add embedding tables for hybrid retrieval"`

---

## Task 2: Configuration and explicit provider mode

**Files:**
- Modify: `src/config.rs`
- Modify: `src/app.rs` (read config; pass into service factory)
- Test: `tests/config_embedding.rs` or unit test in `config.rs` with `temp_env`

- [ ] **Step 1: Write failing test** — With `EMBEDDING_PROVIDER=real` and no API key (if you define one), `Config::from_env()` returns error; with `fake`, succeeds and `embedding_dim` matches.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Fields: `embedding_provider` (`fake`|`real`), `embedding_model`, `embedding_dim`, `embedding_worker_poll_interval_ms`, `embedding_max_retries`, `embedding_batch_size`. **No silent fallback** from `real` to `fake`.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(config): add embedding provider settings"`

---

## Task 3: `EmbeddingProvider` trait + `FakeEmbeddingProvider`

**Files:**
- Create: `src/embedding/mod.rs`, `src/embedding/provider.rs`, `src/embedding/fake.rs`
- Modify: `src/lib.rs`
- Test: `tests/embedding_fake_provider.rs`

- [ ] **Step 1: Write failing test** — Same input → same `Vec<f32>`; different inputs → not equal; length == `embedding_dim` from config.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Trait roughly:

```rust
#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn model(&self) -> &str;
    fn dim(&self) -> usize;
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;
}
```

Use deterministic hashing / seeded projection for fake (no network).

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(embedding): add provider trait and fake implementation"`

---

## Task 4: DuckDB repository — job enqueue and idempotency

**Files:**
- Modify: `src/storage/duckdb.rs`
- Modify: `src/error.rs` or `StorageError` variants if needed
- Test: `tests/embedding_jobs.rs`

- [ ] **Step 1: Write failing test** — After `insert_memory`, call new `enqueue_embedding_job(...)` twice with same `(tenant, memory_id, target_content_hash, provider)` → single pending row (insert-or-ignore or upsert semantics per spec).

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — `enqueue_embedding_job`, `get_memory_content_hash_for_update` (for worker), types mirroring SQL columns.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(storage): enqueue embedding jobs with deduplication"`

---

## Task 5: Ingest path enqueues job (no latency regression)

**Files:**
- Modify: `src/service/memory_service.rs` (`ingest` after successful `insert_memory`)
- Modify: `src/service/memory_service.rs` (supersede / `replace_pending_with_successor` success path — new content_hash → enqueue)
- Test: `tests/ingest_api.rs` or dedicated test using repository directly

- [ ] **Step 1: Write failing test** — POST `/memories` then query `embedding_jobs` for that `memory_id` → one `pending` job with matching `target_content_hash`.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Call repository enqueue with provider name from config; do **not** await embedding in request path.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(ingest): enqueue embedding jobs asynchronously"`

---

## Task 6: Embedding worker — claim, embed, write-back, stale, retry

**Files:**
- Create: `src/service/embedding_worker.rs`
- Modify: `src/storage/duckdb.rs` — transactional `claim_next_job`, `complete_job`, `fail_job_with_backoff`, `mark_stale`, `upsert_memory_embedding`
- Modify: `src/app.rs` — `tokio::spawn` worker with `interval`, `CancellationToken` or shutdown on drop
- Test: `tests/embedding_worker.rs` (use `FakeEmbeddingProvider` + in-memory/temp DB)

- [ ] **Step 1: Write failing test** — Seed pending job + memory row; run worker tick; `memory_embeddings` row exists and job `completed`; if memory `content_hash` changed before write-back, job `stale` and no upsert.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Backoff: 1m, 5m, 30m then `failed`; `attempt_count` increment; `available_at` scheduling. Claim: single-winner `UPDATE ... WHERE job_id = ? AND status = 'pending'` pattern.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(worker): process embedding jobs in-process"`

---

## Task 7: Store and query vectors — semantic candidate recall

**Files:**
- Modify: `db/schema/002_embeddings.sql` if index needed (e.g. no HNSW in phase 2 — acceptable to scan tenant-filtered subset with cosine similarity in SQL)
- Modify: `src/storage/duckdb.rs` — `search_semantic_candidates(tenant, query_embedding, limit)` joining `memory_embeddings` to `memories` where `memories.content_hash = memory_embeddings.content_hash` and status filters match lexical path (exclude archived/rejected, same tenant)
- Test: `tests/semantic_recall.rs`

- [ ] **Step 1: Write failing test** — Insert two memories with embeddings; query vector closer to B; expect B in semantic results with higher score than A.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Serialize embedding as DuckDB-friendly type (`LIST<FLOAT>` or `BLOB`); use DuckDB built-in distance if available in your version, else dot product in SQL. Normalize vectors in fake provider if you use cosine.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(storage): semantic candidate search in DuckDB"`

---

## Task 8: Hybrid merge + unified reranking + search integration

**Files:**
- Create: `src/pipeline/hybrid.rs`
- Modify: `src/pipeline/retrieve.rs` or call hybrid from service layer
- Modify: `src/service/memory_service.rs` — `search`: parallel `search_candidates` (lexical) + `embed_query` + `search_semantic`; merge by `memory_id` with `CandidateOrigin::{LexicalOnly, SemanticOnly, Hybrid}`; rerank; then existing `rank_with_graph` on ordered list **or** integrate graph boost inside hybrid scorer (choose one consistent ordering; document in code)
- Test: extend `tests/search_api.rs`

- [ ] **Step 1: Write failing test** — Memory with text that does not match query tokens but matches semantically (fake vectors crafted in test) appears in search results; missing embeddings → same as current lexical-only behavior.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Scoring components per spec: map existing `text_match_score` to `lexical_score`; semantic from SQL similarity; add `hybrid_bonus`; keep scope, memory type, confidence, decay, freshness, evidence (evidence: e.g. bonus from `!memory.evidence.is_empty()`); **feedback:** prefer using existing `confidence` / `decay_score` on `MemoryRecord` (already updated by `apply_feedback`) plus optional `feedback_summary` penalty if you add a batched fetch — minimum is decay/confidence weighting so incorrect/outdated memories stay suppressed.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(search): hybrid lexical and semantic retrieval"`

---

## Task 9: `GET /memories/{id}` embedding metadata

**Files:**
- Modify: `src/domain/memory.rs` — extend `MemoryDetailResponse` with optional `embedding_*` fields (or nested struct)
- Modify: `src/storage/duckdb.rs` — `get_embedding_meta_for_memory`
- Modify: `src/service/memory_service.rs` — `get_memory` fills metadata
- Test: `tests/ingest_api.rs` or new file — GET after worker completes

- [ ] **Step 1: Write failing test** — Expect `embedding_status`, `embedding_model`, `embedding_updated_at`, `embedding_content_hash` keys in JSON; no raw vector.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(api): expose embedding metadata on memory detail"`

---

## Task 10: Operational APIs — jobs, rebuild, providers

**Files:**
- Create: `src/http/embeddings.rs`
- Modify: `src/http/mod.rs` — merge router
- Modify: `src/service/memory_service.rs` — list jobs, rebuild, provider info
- Test: `tests/embeddings_api.rs`

- [ ] **Step 1: Write failing test** — `GET /embeddings/providers` returns provider name, model, dim; `POST /embeddings/rebuild` enqueues jobs; `GET /embeddings/jobs?tenant=&status=` filters.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Rebuild: by tenant, optional memory id list, optional scope, `force` to re-embed even if hash unchanged (per spec intent); use same deduping rules.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(api): embeddings jobs, rebuild, and provider metadata"`

---

## Task 11: Real provider adapter (OpenAI) behind tests with mocks

**Files:**
- Create: `src/embedding/openai.rs`
- Add dev-dependency or use `wiremock` / `httpmock` in `tests/`
- Modify: `src/config.rs` — `OPENAI_API_KEY` when `EMBEDDING_PROVIDER=real`

- [ ] **Step 1: Write failing test** — Mock HTTP 200 with embedding array; adapter returns `Vec<f32>` of correct dim; error paths for 4xx/5xx classified transient vs permanent per your policy.

- [ ] **Step 2: Run test** — **Expected:** FAIL

- [ ] **Step 3: Implement** — Production code uses `reqwest`; tests never hit network.

- [ ] **Step 4: Run test** — **Expected:** PASS

- [ ] **Step 5: Commit** — `git commit -m "feat(embedding): OpenAI embedding provider adapter"`

---

## Task 12: Observability and acceptance checklist

**Files:**
- Modify: as needed — `tracing` fields for `candidate_origin`, score components at `debug` level (optional, YAGNI if not already using tracing)

- [ ] **Step 1: Manual / test verification** — Run through spec **Acceptance Criteria** (ingest latency, worker stale handling, degraded search without embeddings, operator endpoints).

- [ ] **Step 2: Run full suite** — `cargo test`  
  **Expected:** all PASS

- [ ] **Step 3: Commit** — if only logging: `git commit -m "chore: trace hybrid retrieval scoring"`

---

## Plan review

After implementation, use @superpowers/verification-before-completion before claiming done. Optional: run the plan-document review loop described in @superpowers/writing-plans (reviewer prompt file) if you maintain one in-repo.

---

## Execution handoff

**Plan complete and saved to `docs/superpowers/plans/2026-03-21-hybrid-memory-retrieval.md`. Two execution options:**

1. **Subagent-Driven (recommended)** — Dispatch a fresh subagent per task, review between tasks, fast iteration. **REQUIRED SUB-SKILL:** @superpowers/subagent-driven-development

2. **Inline Execution** — Execute tasks in one session with checkpoints. **REQUIRED SUB-SKILL:** @superpowers/executing-plans

**Which approach do you want?**
