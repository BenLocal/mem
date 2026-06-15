-- postgres-backend P5 — all remaining `Backend` sub-trait tables.
--
-- One migration creates every P5 table so later batches need no
-- migration change. This batch's traits (MineCursorStore,
-- EvolutionCandidateStore, SessionStore, EntityRegistry,
-- MaintenanceStore) use sessions / episodes / entities /
-- entity_aliases / mine_cursors / evolution_candidates; the next
-- batch's traits (EmbeddingJobStore, GraphStore, TranscriptStore)
-- use embedding_jobs / transcript_embedding_jobs / graph_edges /
-- conversation_messages — created here ahead of time.
--
-- Conventions (mirror 0001): timestamps = TEXT (20-digit zero-padded
-- ms strings); enums = TEXT + CHECK; lists = TEXT[]; i64 = BIGINT;
-- snake_case columns. All statements idempotent.
--
-- The pgvector embedding tables (capability_capsule_embeddings,
-- conversation_message_embeddings) are intentionally NOT created here
-- — they are lazy-created on first upsert with a provider-dependent
-- `vector(<dim>)` column (see postgres_backend.rs EmbeddingVectorStore).

-- ───────────────────────────── sessions ─────────────────────────────
-- Mirrors lance `sessions_schema()`. PK = session_id.
CREATE TABLE IF NOT EXISTS sessions (
    session_id      TEXT PRIMARY KEY,
    tenant          TEXT NOT NULL,
    caller_agent    TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    last_seen_at    TEXT NOT NULL,
    ended_at        TEXT,
    goal            TEXT,
    -- UInt32 on the Lance side; BIGINT here so sqlx's i64 binding
    -- maps cleanly (no unsigned type in Postgres / sqlx).
    memory_count    BIGINT NOT NULL DEFAULT 0
);

-- `latest_active_session(tenant, caller_agent)` traversal: filter the
-- active (ended_at IS NULL) rows per identity, ordered by recency.
CREATE INDEX IF NOT EXISTS idx_sessions_tenant_agent_active
    ON sessions (tenant, caller_agent)
    WHERE ended_at IS NULL;

-- ───────────────────────────── episodes ─────────────────────────────
-- Mirrors lance `episodes_schema()`. PK = episode_id. `scope` /
-- `visibility` are TEXT + CHECK (snake_case wire form, same as 0001).
-- `workflow_candidate` is a JSON-encoded nullable string column (Arrow
-- stored it the same way).
CREATE TABLE IF NOT EXISTS episodes (
    episode_id          TEXT PRIMARY KEY,
    tenant              TEXT NOT NULL,
    goal                TEXT NOT NULL,
    steps               TEXT[] NOT NULL DEFAULT '{}',
    outcome             TEXT NOT NULL,
    evidence            TEXT[] NOT NULL DEFAULT '{}',
    scope               TEXT NOT NULL
        CHECK (scope IN ('global', 'project', 'repo', 'workspace')),
    visibility          TEXT NOT NULL
        CHECK (visibility IN ('private', 'shared', 'system')),
    project             TEXT,
    repo                TEXT,
    module              TEXT,
    tags                TEXT[] NOT NULL DEFAULT '{}',
    source_agent        TEXT NOT NULL,
    idempotency_key     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    workflow_candidate  TEXT
);

-- `list_successful_episodes_for_tenant`: tenant + outcome filter.
CREATE INDEX IF NOT EXISTS idx_episodes_tenant_outcome
    ON episodes (tenant, outcome);

-- ───────────────────────────── entities ─────────────────────────────
-- Mirrors lance `entities_schema()`. PK = entity_id (UUIDv7). `kind`
-- stored via EntityKind::as_db_str (lowercase).
CREATE TABLE IF NOT EXISTS entities (
    entity_id       TEXT PRIMARY KEY,
    tenant          TEXT NOT NULL,
    canonical_name  TEXT NOT NULL,
    kind            TEXT NOT NULL
        CHECK (kind IN
            ('topic', 'project', 'repo', 'module',
             'workflow', 'tag', 'file')),
    created_at      TEXT NOT NULL
);

-- `list_entities(tenant, kind, query, limit)`: tenant + optional kind.
CREATE INDEX IF NOT EXISTS idx_entities_tenant_kind
    ON entities (tenant, kind);

-- ───────────────────────── entity_aliases ──────────────────────────
-- Mirrors lance `entity_aliases_schema()`. Composite PK =
-- (tenant, alias_text) — Postgres enforces the uniqueness LanceDB
-- couldn't, which is exactly the alias→entity binding invariant the
-- registry relies on. `alias_text` is the normalized form.
CREATE TABLE IF NOT EXISTS entity_aliases (
    tenant      TEXT NOT NULL,
    alias_text  TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (tenant, alias_text)
);

-- `get_entity` alias list: lookup aliases by owning entity.
CREATE INDEX IF NOT EXISTS idx_entity_aliases_entity
    ON entity_aliases (tenant, entity_id);

-- ───────────────────────────── mine_cursors ─────────────────────────
-- Mirrors lance `mine_cursors_schema()`. PK = transcript_path.
CREATE TABLE IF NOT EXISTS mine_cursors (
    transcript_path     TEXT PRIMARY KEY,
    last_line_number    BIGINT NOT NULL,
    updated_at          TEXT NOT NULL
);

-- ──────────────────────── evolution_candidates ──────────────────────
-- Mirrors lance `evolution_candidates_schema()`. PK = candidate_id.
-- `member_ids` / `result_capsule_ids` are JSON-encoded string arrays
-- (Vec<String> serialized as a JSON text), matching the Lance backend
-- (which stored them as JSON strings, not Arrow lists). `params` is a
-- JSON object stored verbatim as text.
CREATE TABLE IF NOT EXISTS evolution_candidates (
    candidate_id        TEXT PRIMARY KEY,
    tenant              TEXT NOT NULL,
    op_kind             TEXT NOT NULL,
    member_ids          TEXT NOT NULL,
    params              TEXT NOT NULL,
    evidence            REAL NOT NULL,
    consecutive_cycles  BIGINT NOT NULL,
    status              TEXT NOT NULL,
    first_proposed_at   TEXT NOT NULL,
    last_signal_at      TEXT NOT NULL,
    executed_at         TEXT,
    result_capsule_ids  TEXT NOT NULL
);

-- `list_evolution_candidates(tenant, status)`: tenant + optional status.
CREATE INDEX IF NOT EXISTS idx_evolution_candidates_tenant_status
    ON evolution_candidates (tenant, status);

-- ═══════════════════ NEXT-BATCH TABLES (created early) ═══════════════

-- ──────────────────────────── embedding_jobs ────────────────────────
-- Mirrors lance `embedding_jobs_schema()`. PK = job_id.
CREATE TABLE IF NOT EXISTS embedding_jobs (
    job_id                  TEXT PRIMARY KEY,
    tenant                  TEXT NOT NULL,
    capability_capsule_id   TEXT NOT NULL,
    target_content_hash     TEXT NOT NULL,
    provider                TEXT NOT NULL,
    status                  TEXT NOT NULL,
    attempt_count           BIGINT NOT NULL DEFAULT 0,
    last_error              TEXT,
    available_at            TEXT NOT NULL,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL
);

-- Queue claim probe (status + availability) and capsule-keyed cleanup.
CREATE INDEX IF NOT EXISTS idx_embedding_jobs_status_available
    ON embedding_jobs (status, available_at);
CREATE INDEX IF NOT EXISTS idx_embedding_jobs_capsule
    ON embedding_jobs (capability_capsule_id);

-- ───────────────────── transcript_embedding_jobs ────────────────────
-- Mirrors lance `transcript_embedding_jobs_schema()`. PK = job_id.
CREATE TABLE IF NOT EXISTS transcript_embedding_jobs (
    job_id              TEXT PRIMARY KEY,
    tenant              TEXT NOT NULL,
    message_block_id    TEXT NOT NULL,
    provider            TEXT NOT NULL,
    status              TEXT NOT NULL,
    attempt_count       BIGINT NOT NULL DEFAULT 0,
    last_error          TEXT,
    available_at        TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_transcript_embedding_jobs_status_available
    ON transcript_embedding_jobs (status, available_at);
CREATE INDEX IF NOT EXISTS idx_transcript_embedding_jobs_block
    ON transcript_embedding_jobs (message_block_id);

-- ───────────────────────────── graph_edges ──────────────────────────
-- Mirrors lance `graph_edges_schema()`. NO tenant column (graph is
-- tenant-orthogonal; node ids are namespaced like `entity:<uuid>` /
-- `capability_capsule:<id>`). Composite PK = (from_node_id, to_node_id,
-- relation, valid_from) — the temporal-edge identity (a re-asserted
-- edge gets a new valid_from, so it's distinct). K1/K3/K9 columns
-- (confidence, extractor, strength, stability, last_activated,
-- access_count) are all nullable.
CREATE TABLE IF NOT EXISTS graph_edges (
    from_node_id    TEXT NOT NULL,
    to_node_id      TEXT NOT NULL,
    relation        TEXT NOT NULL,
    valid_from      TEXT NOT NULL,
    valid_to        TEXT,
    confidence      REAL,
    extractor       TEXT,
    strength        REAL,
    stability       REAL,
    last_activated  TEXT,
    access_count    BIGINT,
    PRIMARY KEY (from_node_id, to_node_id, relation, valid_from)
);

-- Active-edge BFS traversal in both directions. Partial indexes on
-- `valid_to IS NULL` keep the active-only reads (the default) cheap.
CREATE INDEX IF NOT EXISTS idx_graph_edges_from_active
    ON graph_edges (from_node_id)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS idx_graph_edges_to_active
    ON graph_edges (to_node_id)
    WHERE valid_to IS NULL;
-- `query_predicate(relation, as_of)` traversal.
CREATE INDEX IF NOT EXISTS idx_graph_edges_relation
    ON graph_edges (relation);

-- ─────────────────────── conversation_messages ──────────────────────
-- Mirrors lance `conversation_messages_schema()`. PK = message_block_id.
-- `line_number` (UInt64) / `block_index` (UInt32) widen to BIGINT here.
-- `role` / `block_type` stored as TEXT (lowercase wire form). A GIN
-- tsvector index over `content` backs the next batch's BM25 transcript
-- search (the analog of the Lance FTS index).
CREATE TABLE IF NOT EXISTS conversation_messages (
    message_block_id    TEXT PRIMARY KEY,
    session_id          TEXT,
    tenant              TEXT NOT NULL,
    caller_agent        TEXT NOT NULL,
    transcript_path     TEXT NOT NULL,
    line_number         BIGINT NOT NULL,
    block_index         BIGINT NOT NULL,
    message_uuid        TEXT,
    role                TEXT NOT NULL,
    block_type          TEXT NOT NULL,
    content             TEXT NOT NULL,
    tool_name           TEXT,
    tool_use_id         TEXT,
    embed_eligible      BOOLEAN NOT NULL DEFAULT FALSE,
    created_at          TEXT NOT NULL,
    meta_json           TEXT,
    -- Generated tsvector for BM25 (next batch). 'simple' config matches
    -- the capability_capsules content_tsv column in 0003.
    content_tsv         TSVECTOR
        GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED
);

-- Session-scoped reads, time-range scans, and the BM25 GIN index.
CREATE INDEX IF NOT EXISTS idx_conversation_messages_tenant_session
    ON conversation_messages (tenant, session_id);
CREATE INDEX IF NOT EXISTS idx_conversation_messages_created
    ON conversation_messages (tenant, created_at);
CREATE INDEX IF NOT EXISTS idx_conversation_messages_content_tsv
    ON conversation_messages USING GIN (content_tsv);
