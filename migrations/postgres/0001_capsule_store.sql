-- Phase 4 Postgres spike (backend-coupling.md §6.5) — schema for the
-- `CapsuleStore` trait surface. Mirrors the Lance backend's row
-- shape so the same `CapabilityCapsuleRecord` deserialization path
-- works without backend-specific data converters.
--
-- Design notes (drift from Lance backend):
--
-- 1. Timestamps stay as TEXT (20-digit zero-padded ms strings)
--    rather than TIMESTAMPTZ. The trait surface deals in `&str`
--    timestamps and the codebase's `storage::current_timestamp` /
--    `timestamp_add_ms` helpers operate on strings. A future
--    Postgres-native impl could prefer TIMESTAMPTZ + an inbound
--    conversion layer at the trait boundary, but Phase 4 spike
--    stays with strings to minimize trait churn.
--
-- 2. Enums stored as TEXT with CHECK constraints rather than
--    Postgres ENUM types. Reason: the domain enums use
--    `serde(rename_all = "snake_case")` so the wire form is already
--    snake_case strings; mapping to Postgres ENUM would add a
--    serde layer at every binding. CHECK constraints give the same
--    validation guarantee at insert/update time without the binding
--    cost.
--
-- 3. `version` is BIGINT (i64), matching the domain
--    `CapabilityCapsuleRecord.version: i64`. The domain type was
--    `u64` during the Phase 4 spike and required an
--    `i64::try_from(u64)` cast at every bind / fetch; Phase 5
--    pain #1 rewidened to `i64` so the impl boundary is
--    `.bind(memory.version)` with no conversion.
--
-- 4. List columns (evidence, code_refs, tags, topics) use TEXT[].
--    sqlx maps `Vec<String>` ↔ TEXT[] natively (sqlx `postgres`
--    feature ships with that adapter).
--
-- 5. No vector / embedding column on this table — that lives on
--    a separate `capability_capsule_embeddings` table per
--    `EmbeddingVectorStore`. Phase 4 spike does NOT implement that
--    trait yet; doing pgvector requires a separate extension setup
--    that's outside the "smallest validation" scope per doc §6.5.

CREATE TABLE IF NOT EXISTS capability_capsules (
    capability_capsule_id            TEXT PRIMARY KEY,
    tenant                           TEXT NOT NULL,
    capability_capsule_type          TEXT NOT NULL
        CHECK (capability_capsule_type IN
            ('implementation', 'experience', 'preference',
             'episode', 'workflow', 'diary')),
    status                           TEXT NOT NULL
        CHECK (status IN
            ('pending_confirmation', 'provisional', 'active',
             'archived', 'rejected')),
    scope                            TEXT NOT NULL
        CHECK (scope IN ('global', 'project', 'repo', 'workspace')),
    visibility                       TEXT NOT NULL
        CHECK (visibility IN ('private', 'shared', 'system')),
    version                          BIGINT NOT NULL,
    summary                          TEXT NOT NULL,
    content                          TEXT NOT NULL,
    evidence                         TEXT[] NOT NULL DEFAULT '{}',
    code_refs                        TEXT[] NOT NULL DEFAULT '{}',
    project                          TEXT,
    repo                             TEXT,
    module                           TEXT,
    task_type                        TEXT,
    tags                             TEXT[] NOT NULL DEFAULT '{}',
    topics                           TEXT[] NOT NULL DEFAULT '{}',
    confidence                       REAL NOT NULL,
    decay_score                      REAL NOT NULL,
    content_hash                     TEXT NOT NULL,
    idempotency_key                  TEXT,
    session_id                       TEXT,
    supersedes_capability_capsule_id TEXT,
    source_agent                     TEXT NOT NULL,
    created_at                       TEXT NOT NULL,
    updated_at                       TEXT NOT NULL,
    last_validated_at                TEXT,
    -- roadmap O1: last time this capsule was emitted into a retrieval
    -- response; anchors the decay clock via COALESCE(last_used_at, updated_at).
    last_used_at                     TEXT,
    -- Step-1 governance fix: durable, sweep-proof recall signal. Written
    -- ONLY by the real recall path (never by the decay sweep), so
    -- `last_recalled_at IS NULL` == "never recalled since creation".
    last_recalled_at                 TEXT
);

-- Common query patterns used by CapsuleStore reads.
CREATE INDEX IF NOT EXISTS idx_capsules_tenant_status
    ON capability_capsules (tenant, status);

-- Idempotency probe: `(tenant, idempotency_key)` lookup. Partial
-- index so rows without a key don't bloat the b-tree.
CREATE INDEX IF NOT EXISTS idx_capsules_idempotency
    ON capability_capsules (tenant, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- Content-hash dedup probe paired with the idempotency one in
-- `find_by_idempotency_or_hash`.
CREATE INDEX IF NOT EXISTS idx_capsules_content_hash
    ON capability_capsules (tenant, content_hash);

-- Version-chain walk via `supersedes_capability_capsule_id`.
CREATE INDEX IF NOT EXISTS idx_capsules_supersedes
    ON capability_capsules (supersedes_capability_capsule_id)
    WHERE supersedes_capability_capsule_id IS NOT NULL;

-- Feedback audit log — keyed on `(capability_capsule_id, created_at)`
-- for `feedback_summary` aggregate.
CREATE TABLE IF NOT EXISTS feedback_events (
    feedback_id             TEXT PRIMARY KEY,
    capability_capsule_id   TEXT NOT NULL,
    feedback_kind           TEXT NOT NULL
        CHECK (feedback_kind IN
            ('useful', 'outdated', 'incorrect',
             'applies_here', 'does_not_apply_here', 'auto_promoted')),
    created_at              TEXT NOT NULL,
    note                    TEXT
);

CREATE INDEX IF NOT EXISTS idx_feedback_capsule
    ON feedback_events (capability_capsule_id);
