-- clickhouse-backend P3 — embedding vector tables. UNVALIDATED scaffold
-- (never run against a real ClickHouse).
--
-- Engine: ReplacingMergeTree(row_version). Each upsert appends its rows with
-- one shared, fresh `row_version` (a "generation"); `chunk_index` discriminates
-- the N chunk-vectors of a single id so ReplacingMergeTree does NOT collapse
-- them into one row. Reads take the latest row by `ORDER BY row_version DESC`
-- (the capsule side is single-row in practice). `embedding Array(Float32)` is
-- variable-length — the dim lives in `embedding_dim`, so no dim-aware
-- table creation is needed (unlike Lance's lazy-create). The experimental
-- `vector_similarity` (HNSW) index for scale is a P4/validation concern, not
-- added here (P3 only stores/retrieves vectors; the ANN query is P4/P5).
--
-- See docs/clickhouse-backend.md §4(a)/§4(d) + the §10 P3 pain inventory.

CREATE TABLE IF NOT EXISTS capability_capsule_embeddings
(
    capability_capsule_id String,
    tenant                String,
    embedding_model       LowCardinality(String),
    embedding_dim         Int64,
    embedding             Array(Float32),
    content_hash          String,
    source_updated_at     String,
    created_at            String,
    updated_at            String,
    chunk_index           UInt32,
    row_version           UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, capability_capsule_id, chunk_index);

CREATE TABLE IF NOT EXISTS conversation_message_embeddings
(
    message_block_id  String,
    tenant            String,
    embedding_model   LowCardinality(String),
    embedding_dim     Int64,
    embedding         Array(Float32),
    content_hash      String,
    source_updated_at String,
    created_at        String,
    updated_at        String,
    chunk_index       UInt32,
    row_version       UInt64
)
ENGINE = ReplacingMergeTree(row_version)
ORDER BY (tenant, message_block_id, chunk_index);
