# Hybrid Memory Retrieval Design

## Summary

This document defines phase two of the AI agent memory service: hybrid retrieval for `memory` records. The goal is to improve semantic recall without replacing the existing deterministic memory lifecycle, compressed response contract, or DuckDB-centered architecture.

Phase two adds asynchronous embedding generation, a dual-provider abstraction (`fake` and `real`), DuckDB-backed embedding state, semantic candidate recall, and hybrid reranking. The system must continue to function when embeddings are missing, stale, or temporarily unavailable.

## Goals

- Improve semantic recall for `memory` search queries
- Keep the current ingest path fast by generating embeddings asynchronously
- Preserve the existing `POST /memories/search` response shape
- Keep DuckDB as the canonical store and retrieval coordinator
- Support both deterministic local testing and real embedding providers
- Make hybrid retrieval explainable and debuggable

## Non-Goals

- Adding embeddings for `workflow` or `episode` records in this phase
- Replacing DuckDB with a dedicated vector database
- Introducing distributed workers, auth, or multi-node coordination
- Converting ranking to learned-to-rank or model-driven reranking
- Changing the existing review, feedback, or workflow extraction lifecycle

## Architecture

Phase two adds four focused components around the current service:

1. `EmbeddingProvider`
   A stable interface for generating embeddings. The service must support:
   - `FakeEmbeddingProvider` for tests and local deterministic development
   - `RealEmbeddingProvider` for production-like semantic recall

2. `EmbeddingJobPipeline`
   Memory ingest creates asynchronous embedding jobs instead of generating vectors inline.

3. `EmbeddingStore`
   DuckDB stores the currently active embedding for each memory and tracks job lifecycle separately.

4. `HybridRetrievalOrchestrator`
   Search runs lexical recall and semantic recall in parallel, merges candidates, reranks them, and passes the final list into the existing compression pipeline.

## Data Flow

### Ingest flow

1. `POST /memories` writes a canonical memory record exactly as it does today
2. The ingest path creates an embedding job for the new memory content
3. The API returns immediately without waiting for embedding generation
4. A background worker claims pending jobs and calls the configured provider
5. Successful embeddings are written to DuckDB if the target `content_hash` still matches the current memory
6. If the memory changed before write-back, the completed job is marked `stale` and its result is discarded

### Search flow

1. `POST /memories/search` performs current lexical recall
2. The service generates a query embedding through the configured provider
3. The semantic path searches embedded memories in DuckDB
4. The service merges lexical and semantic candidates by `memory_id`
5. Unified reranking combines semantic relevance with existing scope, decay, feedback, freshness, and evidence signals
6. The existing compression layer builds the response payload:
   - `directives`
   - `relevant_facts`
   - `reusable_patterns`
   - optional `suggested_workflow`

## Storage Model

### `memory_embeddings`

This table stores the current active embedding for a memory record.

Required fields:

- `memory_id`
- `tenant`
- `embedding_model`
- `embedding_dim`
- `embedding`
- `content_hash`
- `source_updated_at`
- `created_at`
- `updated_at`

Rules:

- Store only the current valid embedding for a memory record
- Treat embeddings as derived retrieval state, not canonical source-of-truth data
- A stored embedding is valid only when its `content_hash` matches the current memory record

### `embedding_jobs`

This table stores asynchronous embedding generation work.

Required fields:

- `job_id`
- `tenant`
- `memory_id`
- `target_content_hash`
- `provider`
- `status`
- `attempt_count`
- `last_error`
- `available_at`
- `created_at`
- `updated_at`

Allowed status values:

- `pending`
- `processing`
- `completed`
- `failed`
- `stale`

Rules:

- `content_hash` is the sole validity check for whether an embedding is current
- Do not create duplicate live jobs for the same `(tenant, memory_id, target_content_hash, provider)`
- Failed jobs may be retried according to retry policy
- Stale jobs must never overwrite current embedding state

## Worker Design

The first implementation should use an in-process background worker rather than a separate queue system.

### Claim and process

- Poll jobs where status is `pending` or retryable `failed`
- Require `available_at <= now`
- Atomically move the chosen job to `processing`
- Load the target memory and call the provider

### Write-back behavior

- If provider succeeds and `target_content_hash` still matches current memory content:
  - upsert into `memory_embeddings`
  - mark job `completed`
- If provider succeeds but the memory content has changed:
  - mark job `stale`
- If provider fails transiently:
  - increment attempts
  - reschedule with backoff
- If provider fails permanently:
  - mark job `failed`

### Retry policy

The first implementation should use deterministic local retry behavior:

- 1st retry: 1 minute
- 2nd retry: 5 minutes
- 3rd retry: 30 minutes
- after max retries: remain `failed`

## Retrieval Strategy

### Candidate generation

The retrieval path must use two independent candidate sets:

- `lexical_candidates`
- `semantic_candidates`

Semantic recall only considers:

- matching tenant
- active memories
- memories whose embedding `content_hash` still matches canonical memory content
- memories allowed by current scope filtering

### Candidate merge

Merge by `memory_id` and retain origin metadata:

- `lexical_only`
- `semantic_only`
- `hybrid`

This origin metadata must be preserved through reranking for observability and tuning.

### Unified reranking

The first implementation should remain deterministic and parameterized. At minimum the ranker should consider:

- `lexical_score`
- `semantic_score`
- `hybrid_bonus`
- `scope_match_score`
- `memory_type_weight`
- `confidence`
- `decay_penalty`
- `feedback_penalty_or_bonus`
- `freshness_score`
- `evidence_bonus`

Design rules:

- lexical matches must still protect against semantic noise
- semantic-only matches should recover meaningfully similar records that lexical recall would miss
- hybrid matches should receive an explicit ranking bonus
- archived, incorrect, or strongly decayed memories must not be promoted by semantic similarity alone

## API Changes

Phase two should preserve current agent-facing contracts as much as possible.

### Existing APIs

- `POST /memories`
  - behavior changes internally to enqueue embedding work
  - response may optionally include lightweight embedding status metadata, but this is not required for phase two

- `POST /memories/search`
  - request shape should remain stable by default
  - the service should use hybrid retrieval internally
  - an optional debug flag or retrieval mode switch may be added later if useful for comparison

- `GET /memories/{id}`
  - should expose embedding metadata, but not raw vectors
  - recommended fields:
    - `embedding_status`
    - `embedding_model`
    - `embedding_updated_at`
    - `embedding_content_hash`

### New operational APIs

- `GET /embeddings/jobs`
  - list jobs by `tenant`, `status`, or `memory_id`

- `POST /embeddings/rebuild`
  - queue rebuild jobs by `tenant`, optional scope, explicit memory IDs, provider, and force mode

- `GET /embeddings/providers`
  - return configured provider metadata such as provider name, mode, model, and dimension

## Configuration

Phase two should add explicit embedding configuration:

- `EMBEDDING_PROVIDER`
- `EMBEDDING_MODEL`
- `EMBEDDING_DIM`
- `EMBEDDING_WORKER_POLL_INTERVAL_MS`
- `EMBEDDING_MAX_RETRIES`
- `EMBEDDING_BATCH_SIZE`

When a real provider is used, phase two may additionally require provider-specific secrets such as `OPENAI_API_KEY`.

The service must not silently fall back from a real provider to a fake provider. Provider mode should always be explicit.

## Testing Strategy

### Provider tests

- fake provider returns stable vectors for identical input
- fake provider respects configured dimensions
- different inputs generate distinguishable outputs
- real provider adapter behavior is tested behind mocks or error-focused integration boundaries

### Job pipeline tests

- ingest creates an embedding job
- worker writes back completed embeddings
- updated memory content makes old jobs stale
- transient failures retry with backoff
- permanent failures stop retrying
- rebuild endpoints enqueue appropriate jobs

### Hybrid retrieval tests

- semantic recall finds a relevant memory lexical recall misses
- hybrid matches rank above lexical-only or semantic-only candidates where expected
- missing embeddings degrade cleanly to lexical-only retrieval
- tenant, scope, and status filters hold on semantic candidates too
- negative feedback and decay still suppress otherwise similar memories

### Smoke tests

- `GET /memories/{id}` returns embedding metadata
- `GET /embeddings/jobs` reflects job lifecycle
- `POST /embeddings/rebuild` queues rebuild work correctly
- service startup launches worker cleanly
- invalid provider configuration fails explicitly

## Risks

- embedding quality may introduce retrieval noise if the real provider is weak for engineering memory
- asynchronous indexing creates a temporary window where new memories are lexical-only
- hybrid weights may be difficult to tune without observability
- rebuilds become necessary whenever provider, dimension, or model changes
- debugging retrieval becomes harder unless origin metadata and score components remain visible

## Rollout Order

The recommended implementation order is:

1. provider abstraction and deterministic fake provider
2. embedding job schema and worker lifecycle
3. current embedding storage in DuckDB
4. semantic query embedding and semantic candidate recall
5. hybrid merge and unified reranking
6. operational APIs and rebuild tooling

## Acceptance Criteria

Phase two is complete when:

- memory ingest enqueues embedding work without increasing API latency significantly
- the worker can populate embeddings asynchronously and safely handle stale writes
- hybrid search improves semantic recall while preserving current lexical fallbacks
- the service remains operational when embeddings are missing or temporarily unavailable
- operators can inspect jobs, inspect provider configuration, and trigger rebuilds
- tests cover provider behavior, job lifecycle, and hybrid retrieval correctness
