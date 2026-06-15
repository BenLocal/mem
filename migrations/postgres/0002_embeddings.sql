-- postgres-backend P3 — pgvector setup for the Postgres
-- `EmbeddingVectorStore`. This migration ONLY installs the pgvector
-- extension; it deliberately does NOT create the embedding tables.
--
-- Why no CREATE TABLE here: the `vector(dim)` column type needs a
-- fixed dimension, but the dim is provider-dependent (the embedding
-- provider's `dim()` — default 1024, but the fake provider used in
-- tests picks its own) and is unknown at migration time. So, exactly
-- like the Lance backend lazy-creates its embedding datasets on first
-- upsert, the Postgres backend lazy-creates
-- `capability_capsule_embeddings` / `conversation_message_embeddings`
-- inside the `upsert_*` methods, splicing the dim into the DDL there.
--
-- Dim drift (changing embedding provider after a table already exists
-- with a different dim) is NOT handled in P3 — `CREATE TABLE IF NOT
-- EXISTS` won't alter an existing column. Same limitation as Lance.

CREATE EXTENSION IF NOT EXISTS vector;
