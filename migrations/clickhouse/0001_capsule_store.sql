-- clickhouse-backend P1 — capability_capsules + feedback_events.
--
-- UNVALIDATED scaffold: this DDL has NOT been run against a real ClickHouse.
--
-- ReplacingMergeTree(row_version): every logical "update" (status / decay /
-- last_used / supersede / soft-delete) is written as a *versioned re-insert*;
-- reads take the latest version per ORDER BY key (`FINAL` for correctness,
-- `argMax(...) GROUP BY pk` on the hot path — see clickhouse-backend.md §4a).
-- Optional columns are plain `String` with `''` = absent (no `Nullable`, per
-- the design's open point #2). Timestamps are 20-digit zero-padded ms `String`s
-- (aligns the trait's `&str` surface; lexicographic compare = chronological).
-- Enums are `LowCardinality(String)` holding the serde snake_case form.

CREATE TABLE IF NOT EXISTS capability_capsules
(
    capability_capsule_id            String,
    tenant                           String,
    capability_capsule_type          LowCardinality(String),
    status                           LowCardinality(String),
    scope                            LowCardinality(String),
    visibility                       LowCardinality(String),
    version                          Int64,
    summary                          String,
    content                          String,
    evidence                         Array(String),
    code_refs                        Array(String),
    project                          String,
    repo                             String,
    module                           String,
    task_type                        String,
    tags                             Array(String),
    topics                           Array(String),
    confidence                       Float32,
    decay_score                      Float32,
    content_hash                     String,
    idempotency_key                  String,
    session_id                       String,
    supersedes_capability_capsule_id String,
    source_agent                     String,
    created_at                       String,
    updated_at                       String,
    last_validated_at                String,
    last_used_at                     String,
    last_recalled_at                 String,
    expires_at                       String,
    row_version                      UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, capability_capsule_id);

CREATE TABLE IF NOT EXISTS feedback_events
(
    feedback_id           String,
    capability_capsule_id String,
    feedback_kind         LowCardinality(String),
    created_at            String,
    note                  String
)
ENGINE = MergeTree
ORDER BY (capability_capsule_id, created_at);
