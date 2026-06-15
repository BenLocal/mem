-- postgres-backend P4 — lexical (BM25-equivalent) search support for the
-- Postgres `CapsuleSearchStore`. This migration adds a generated
-- `tsvector` column over `capability_capsules.content` plus a GIN index,
-- giving the Postgres backend a full-text search path that mirrors the
-- Lance backend's `lance_fts(...)` BM25 候选 path.
--
-- Why 'simple' (not 'english'):
--   The 'english' text-search config runs a Porter stemmer + an English
--   stopword list. For a memory corpus full of code identifiers
--   (`fn_name`, `EMBEDDING_BATCH_SIZE`, `DuckDbQuery`) and mixed-language
--   content, stemming/stopwords would mangle or drop tokens that callers
--   search for verbatim. 'simple' just lowercases + splits on
--   whitespace/punctuation with no stemming and no stopwords, so an
--   identifier query matches the identifier in the content. This keeps the
--   lexical channel a literal-token match, closest in spirit to BM25 over
--   un-stemmed terms.
--
-- KNOWN LIMITATION — Chinese (and other unspaced CJK) text:
--   The 'simple' config tokenizes purely on whitespace/punctuation. It has
--   no CJK word segmenter, so a run of Han characters with no spaces is
--   treated as ONE token. A query for a sub-phrase of that run will NOT
--   match lexically. This means Chinese lexical recall through this column
--   is weak-to-nonexistent — that is an accepted, documented trade-off:
--   the pgvector semantic channel (`ann_candidate_ids`) carries Chinese
--   recall, and the RRF fusion lets the semantic hit surface even when the
--   lexical side is silent. We do NOT attempt CJK parity here (would need a
--   `zhparser` / `pg_jieba` extension that isn't installed). See the
--   matching note in tests/postgres_backend.rs.
--
-- Idempotent: ADD COLUMN IF NOT EXISTS + CREATE INDEX IF NOT EXISTS make
-- re-running this on an already-migrated database a no-op, so `connect`
-- can run it on every `mem serve` boot (after 0001/0002).

ALTER TABLE capability_capsules
    ADD COLUMN IF NOT EXISTS content_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED;

CREATE INDEX IF NOT EXISTS idx_capsules_content_tsv
    ON capability_capsules USING GIN (content_tsv);
