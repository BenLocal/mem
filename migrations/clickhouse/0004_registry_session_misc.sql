-- clickhouse-backend P5 — entity registry, sessions/episodes, mine cursors,
-- evolution candidates. UNVALIDATED scaffold (never run against a real CH).
-- Conventions as 0001-0003: String timestamps, LowCardinality enums,
-- ''=None, Array(String) for list columns, ReplacingMergeTree(row_version).

CREATE TABLE IF NOT EXISTS entities
(
    entity_id      String,
    tenant         String,
    canonical_name String,
    kind           LowCardinality(String),
    created_at     String,
    row_version    UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, entity_id);

-- entity_aliases: normalized (lowercase + ws-collapsed) alias -> entity.
-- PK is (tenant, alias_text).
CREATE TABLE IF NOT EXISTS entity_aliases
(
    tenant      String,
    alias_text  String,
    entity_id   String,
    created_at  String,
    row_version UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, alias_text);

CREATE TABLE IF NOT EXISTS sessions
(
    session_id   String,
    tenant       String,
    caller_agent String,
    started_at   String,
    last_seen_at String,
    ended_at     String,                         -- ''=None (active)
    goal         String,                         -- ''=None
    memory_count UInt32,
    row_version  UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (session_id);

CREATE TABLE IF NOT EXISTS episodes
(
    episode_id     String,
    tenant         String,
    goal           String,
    steps          Array(String),
    outcome        String,
    evidence       Array(String),
    scope          LowCardinality(String),
    visibility     LowCardinality(String),
    project        String,                       -- ''=None
    repo           String,                       -- ''=None
    module         String,                       -- ''=None
    tags           Array(String),
    source_agent   String,
    idempotency_key String,                      -- ''=None
    created_at     String,
    updated_at     String,
    workflow_candidate String,                   -- ''=None (JSON when present)
    row_version    UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, episode_id);

-- mine_cursors: `mem mine` per-transcript high-water mark (no tenant col).
CREATE TABLE IF NOT EXISTS mine_cursors
(
    transcript_path  String,
    last_line_number Int64,
    updated_at       String,
    row_version      UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (transcript_path);

CREATE TABLE IF NOT EXISTS evolution_candidates
(
    candidate_id       String,
    tenant             String,
    op_kind            LowCardinality(String),
    member_ids         Array(String),
    params             String,
    evidence           Float32,
    consecutive_cycles Int64,
    status             LowCardinality(String),
    first_proposed_at  String,
    last_signal_at     String,
    executed_at        String,                   -- ''=None
    result_capsule_ids Array(String),
    row_version        UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, candidate_id);
