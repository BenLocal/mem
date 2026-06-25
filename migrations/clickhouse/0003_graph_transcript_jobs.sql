-- clickhouse-backend P5 — graph + transcript + embedding-job tables.
-- UNVALIDATED scaffold (never run against a real ClickHouse).
--
-- Same conventions as 0001/0002: timestamps are 20-digit zero-padded ms
-- Strings (lexicographic = chronological; aligns the trait `&str` surface),
-- enums LowCardinality(String), optionals are `String` with ''=None (no
-- Nullable), every table is ReplacingMergeTree(row_version) so a logical
-- update is a fresh-row insert and reads take the latest version.

-- graph_edges: bitemporal KG edges. `valid_to` ''=open (active). Per §4(f)
-- BFS is iterative in Rust; this table only stores/queries edges.
CREATE TABLE IF NOT EXISTS graph_edges
(
    from_node_id   String,
    to_node_id     String,
    relation       LowCardinality(String),
    valid_from     String,
    valid_to       String,                       -- ''=active (valid_to IS NULL)
    confidence     Float32,
    extractor      LowCardinality(String),
    strength       Float32,
    stability      Float32,
    last_activated String,
    access_count   Int64,
    row_version    UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (from_node_id, relation, to_node_id, valid_from);

-- conversation_messages: verbatim transcript archive (append-heavy OLAP side).
CREATE TABLE IF NOT EXISTS conversation_messages
(
    message_block_id String,
    session_id       String,                     -- ''=None
    tenant           String,
    caller_agent     String,
    transcript_path  String,
    line_number      UInt64,
    block_index      UInt32,
    message_uuid     String,                     -- ''=None
    role             LowCardinality(String),
    block_type       LowCardinality(String),
    content          String,
    tool_name        String,                     -- ''=None
    tool_use_id      String,                     -- ''=None
    embed_eligible   UInt8,
    created_at       String,
    meta_json        String,                     -- ''=None
    row_version      UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, message_block_id);

-- embedding_jobs: capsule embedding queue.
CREATE TABLE IF NOT EXISTS embedding_jobs
(
    job_id               String,
    tenant               String,
    capability_capsule_id String,
    target_content_hash  String,
    provider             LowCardinality(String),
    status               LowCardinality(String),
    attempt_count        Int64,
    last_error           String,                 -- ''=None
    available_at         String,
    created_at           String,
    updated_at           String,
    row_version          UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (job_id);

-- transcript_embedding_jobs: parallel queue keyed on message_block_id.
CREATE TABLE IF NOT EXISTS transcript_embedding_jobs
(
    job_id           String,
    tenant           String,
    message_block_id String,
    provider         LowCardinality(String),
    status           LowCardinality(String),
    attempt_count    Int64,
    last_error       String,                     -- ''=None
    available_at     String,
    created_at       String,
    updated_at       String,
    row_version      UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (job_id);
