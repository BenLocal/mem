-- Full-text search extension. Bundled with DuckDB; INSTALL is idempotent.
-- LOAD must run on every fresh connection. We have one long-lived connection
-- (Arc<Mutex<Connection>>), so this runs once per process at bootstrap.
--
-- The actual FTS index (`fts_main_memories`) is built lazily by
-- `bm25_candidates` on first query / after writes — it's non-incremental
-- in DuckDB 1.x, so a build-on-demand strategy beats a startup rebuild that
-- would have to re-run on every bootstrap.
install fts;
load fts;
