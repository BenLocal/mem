//! Tantivy-backed BM25 indexes for the memories + conversation_messages
//! tables.
//!
//! Replaces the DuckDB-native FTS extension which is non-incremental
//! (every write requires a full `pragma drop_fts_index` +
//! `create_fts_index` cycle, plus the in-memory dependency-tracker bug
//! that surfaced as `subject "stopwords" has been deleted` on long-lived
//! connections — see commit history). Tantivy is incremental at the
//! `add_document` level: ingest writes a single segment, segments merge
//! in the background, search reads all live segments.
//!
//! Layout:
//!   <db>.duckdb.fts.memories/        — Tantivy index dir for memories
//!   <db>.duckdb.fts.transcripts/     — Tantivy index dir for transcripts
//!
//! Both indexes are owned by `DuckDbRepository` (one per repo). They live
//! beside the existing HNSW sidecars (`.usearch`, `.usearch.meta.json`).
//!
//! Concurrency: a single `IndexWriter` is shared via `Arc<Mutex<…>>`. All
//! mem-side writes happen under the connection mutex anyway, so two
//! writers contending isn't a real concern; the lock is for tantivy's
//! own thread-safety contract. Reads use `IndexReader::searcher()` which
//! is lock-free (snapshot semantics).
//!
//! The writer is created lazily on first write. Tantivy takes a
//! per-directory lockfile when `Index::writer()` is called; deferring
//! that means a repo opened only for read paths (e.g. `mem repair
//! --rebuild-graph`, which doesn't touch FTS) won't fight a concurrent
//! `mem serve` for the lock. The first write — be it `upsert`, `delete`,
//! or `bootstrap_batch` — initializes it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, FAST, STORED, STRING, TEXT};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FtsError {
    #[error("tantivy error: {0}")]
    Tantivy(String),
    #[error("io error: {0}")]
    Io(String),
}

impl From<tantivy::TantivyError> for FtsError {
    fn from(e: tantivy::TantivyError) -> Self {
        FtsError::Tantivy(e.to_string())
    }
}

/// 50 MB writer memory budget — large enough for typical batch ingest
/// (tantivy recommends ≥15 MB per writer thread), small enough to leave
/// room for the 1.2 GB Qwen3 model + DuckDB pages on a 4 GB box.
const WRITER_MEMORY_BUDGET: usize = 50_000_000;

// ─── memories ──────────────────────────────────────────────────────

#[derive(Clone)]
pub struct MemoryFts {
    index: Index,
    writer: Arc<Mutex<Option<IndexWriter>>>,
    reader: IndexReader,
    fields: MemoryFields,
}

#[derive(Debug, Clone, Copy)]
struct MemoryFields {
    memory_id: Field,
    tenant: Field,
    summary: Field,
    content: Field,
}

impl MemoryFts {
    pub fn open(path: &Path) -> Result<Self, FtsError> {
        std::fs::create_dir_all(path).map_err(|e| FtsError::Io(e.to_string()))?;
        let mut sb = Schema::builder();
        // STRING (raw) for ID/tenant — exact-match Term filters.
        // STORED so we can read memory_id off hits without a DuckDB join.
        // FAST gives us doc-value style fast-field access for tenant filter.
        let memory_id = sb.add_text_field("memory_id", STRING | STORED | FAST);
        let tenant = sb.add_text_field("tenant", STRING | STORED | FAST);
        // TEXT = tokenize + index for BM25. Not stored — we hydrate via
        // DuckDB after the search returns IDs.
        let summary = sb.add_text_field("summary", TEXT);
        let content = sb.add_text_field("content", TEXT);
        let schema = sb.build();

        let dir = MmapDirectory::open(path)
            .map_err(|e| FtsError::Tantivy(format!("open mmap dir: {e}")))?;
        let index = Index::open_or_create(dir, schema.clone())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let fields = MemoryFields {
            memory_id,
            tenant,
            summary,
            content,
        };

        Ok(Self {
            index,
            // Lazy: writer (and the lockfile it holds) is created on the
            // first write, not at open. See module docs.
            writer: Arc::new(Mutex::new(None)),
            reader,
            fields,
        })
    }

    fn ensure_writer<'a>(
        &'a self,
        guard: &'a mut std::sync::MutexGuard<'_, Option<IndexWriter>>,
    ) -> Result<&'a mut IndexWriter, FtsError> {
        if guard.is_none() {
            **guard = Some(self.index.writer(WRITER_MEMORY_BUDGET)?);
        }
        Ok(guard.as_mut().expect("writer just initialized"))
    }

    /// Add or replace one memory in the index. Caller passes the same
    /// memory_id on update — we delete the existing term, then add the
    /// new doc. Tantivy treats `delete_term` + `add_document` in the
    /// same uncommitted batch as an atomic update.
    pub fn upsert(
        &self,
        memory_id: &str,
        tenant: &str,
        summary: &str,
        content: &str,
    ) -> Result<(), FtsError> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|e| FtsError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        let w = self.ensure_writer(&mut guard)?;
        let term = Term::from_field_text(self.fields.memory_id, memory_id);
        w.delete_term(term);
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.memory_id, memory_id);
        doc.add_text(self.fields.tenant, tenant);
        doc.add_text(self.fields.summary, summary);
        doc.add_text(self.fields.content, content);
        w.add_document(doc)?;
        w.commit()?;
        // ReloadPolicy::OnCommitWithDelay batches reader reloads (~50 ms);
        // explicitly reload so a search firing right after upsert sees the
        // new doc. Cost is negligible (≤1 ms on a small index).
        self.reader.reload()?;
        Ok(())
    }

    /// Permanently remove one memory by id. Idempotent — deleting a
    /// missing id is a no-op from tantivy's perspective.
    pub fn delete(&self, memory_id: &str) -> Result<(), FtsError> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|e| FtsError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        let w = self.ensure_writer(&mut guard)?;
        let term = Term::from_field_text(self.fields.memory_id, memory_id);
        w.delete_term(term);
        w.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// BM25 search scoped to one tenant. Returns up to `k` memory_ids
    /// in descending score order. Caller hydrates with `fetch_memories_by_ids`.
    pub fn search(&self, tenant: &str, query: &str, k: usize) -> Result<Vec<String>, FtsError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        // QueryParser defaults to OR-of-terms (any term matches) — exactly
        // what we want for ranked retrieval; calling
        // `set_conjunction_by_default()` would force AND.
        let parser =
            QueryParser::for_index(&self.index, vec![self.fields.summary, self.fields.content]);

        let user_q = parser.parse_query(query).map_err(|e| {
            FtsError::Tantivy(format!(
                "query parse: {e} (input={:?})",
                query.chars().take(60).collect::<String>()
            ))
        })?;
        let tenant_q: Box<dyn Query> = Box::new(TermQuery::new(
            Term::from_field_text(self.fields.tenant, tenant),
            IndexRecordOption::Basic,
        ));
        let combined = BooleanQuery::new(vec![(Occur::Must, user_q), (Occur::Must, tenant_q)]);

        let top = searcher.search(&combined, &TopDocs::with_limit(k))?;
        let mut out = Vec::with_capacity(top.len());
        for (_score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(v) = doc.get_first(self.fields.memory_id) {
                if let Some(s) = v.as_str() {
                    out.push(s.to_string());
                }
            }
        }
        Ok(out)
    }

    /// Total documents currently indexed (across all tenants). Used by
    /// the bootstrap path to decide whether to populate from DuckDB.
    pub fn doc_count(&self) -> Result<u64, FtsError> {
        Ok(self.reader.searcher().num_docs())
    }
}

// ─── transcripts ───────────────────────────────────────────────────

#[derive(Clone)]
pub struct TranscriptFts {
    index: Index,
    writer: Arc<Mutex<Option<IndexWriter>>>,
    reader: IndexReader,
    fields: TranscriptFields,
}

#[derive(Debug, Clone, Copy)]
struct TranscriptFields {
    block_id: Field,
    tenant: Field,
    content: Field,
}

impl TranscriptFts {
    pub fn open(path: &Path) -> Result<Self, FtsError> {
        std::fs::create_dir_all(path).map_err(|e| FtsError::Io(e.to_string()))?;
        let mut sb = Schema::builder();
        let block_id = sb.add_text_field("message_block_id", STRING | STORED | FAST);
        let tenant = sb.add_text_field("tenant", STRING | STORED | FAST);
        let content = sb.add_text_field("content", TEXT);
        let schema = sb.build();

        let dir = MmapDirectory::open(path)
            .map_err(|e| FtsError::Tantivy(format!("open mmap dir: {e}")))?;
        let index = Index::open_or_create(dir, schema.clone())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let fields = TranscriptFields {
            block_id,
            tenant,
            content,
        };

        Ok(Self {
            index,
            writer: Arc::new(Mutex::new(None)),
            reader,
            fields,
        })
    }

    fn ensure_writer<'a>(
        &'a self,
        guard: &'a mut std::sync::MutexGuard<'_, Option<IndexWriter>>,
    ) -> Result<&'a mut IndexWriter, FtsError> {
        if guard.is_none() {
            **guard = Some(self.index.writer(WRITER_MEMORY_BUDGET)?);
        }
        Ok(guard.as_mut().expect("writer just initialized"))
    }

    pub fn upsert(&self, block_id: &str, tenant: &str, content: &str) -> Result<(), FtsError> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|e| FtsError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        let w = self.ensure_writer(&mut guard)?;
        let term = Term::from_field_text(self.fields.block_id, block_id);
        w.delete_term(term);
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.block_id, block_id);
        doc.add_text(self.fields.tenant, tenant);
        doc.add_text(self.fields.content, content);
        w.add_document(doc)?;
        w.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    pub fn delete(&self, block_id: &str) -> Result<(), FtsError> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|e| FtsError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        let w = self.ensure_writer(&mut guard)?;
        let term = Term::from_field_text(self.fields.block_id, block_id);
        w.delete_term(term);
        w.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    pub fn search(&self, tenant: &str, query: &str, k: usize) -> Result<Vec<String>, FtsError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.fields.content]);

        let user_q = parser.parse_query(query).map_err(|e| {
            FtsError::Tantivy(format!(
                "query parse: {e} (input={:?})",
                query.chars().take(60).collect::<String>()
            ))
        })?;
        let tenant_q: Box<dyn Query> = Box::new(TermQuery::new(
            Term::from_field_text(self.fields.tenant, tenant),
            IndexRecordOption::Basic,
        ));
        let combined = BooleanQuery::new(vec![(Occur::Must, user_q), (Occur::Must, tenant_q)]);

        let top = searcher.search(&combined, &TopDocs::with_limit(k))?;
        let mut out = Vec::with_capacity(top.len());
        for (_score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if let Some(v) = doc.get_first(self.fields.block_id) {
                if let Some(s) = v.as_str() {
                    out.push(s.to_string());
                }
            }
        }
        Ok(out)
    }

    pub fn doc_count(&self) -> Result<u64, FtsError> {
        Ok(self.reader.searcher().num_docs())
    }
}

// Tantivy types (Index, IndexReader, IndexWriter) don't implement Debug.
// Provide a hollow impl on each FTS wrapper so `DuckDbRepository`'s
// `#[derive(Debug)]` keeps working without surfacing tantivy internals.
impl std::fmt::Debug for MemoryFts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryFts").finish_non_exhaustive()
    }
}
impl std::fmt::Debug for TranscriptFts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptFts").finish_non_exhaustive()
    }
}

impl MemoryFts {
    /// Bulk-upsert during repo open when the tantivy index is empty but
    /// DuckDB has rows (e.g. first run after the FTS replacement, or a
    /// deleted sidecar). One commit at the end — much cheaper than a
    /// commit per row.
    pub fn bootstrap_batch<I>(&self, rows: I) -> Result<usize, FtsError>
    where
        I: IntoIterator<Item = (String, String, String, String)>,
    {
        let mut iter = rows.into_iter().peekable();
        if iter.peek().is_none() {
            // Don't acquire the writer lockfile when there's nothing to index.
            return Ok(0);
        }
        let mut guard = self
            .writer
            .lock()
            .map_err(|e| FtsError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        let w = self.ensure_writer(&mut guard)?;
        let mut count = 0usize;
        for (id, tenant, summary, content) in iter {
            let term = Term::from_field_text(self.fields.memory_id, &id);
            w.delete_term(term);
            let mut doc = TantivyDocument::default();
            doc.add_text(self.fields.memory_id, &id);
            doc.add_text(self.fields.tenant, &tenant);
            doc.add_text(self.fields.summary, &summary);
            doc.add_text(self.fields.content, &content);
            w.add_document(doc)?;
            count += 1;
        }
        w.commit()?;
        self.reader.reload()?;
        Ok(count)
    }
}

impl TranscriptFts {
    pub fn bootstrap_batch<I>(&self, rows: I) -> Result<usize, FtsError>
    where
        I: IntoIterator<Item = (String, String, String)>,
    {
        let mut iter = rows.into_iter().peekable();
        if iter.peek().is_none() {
            return Ok(0);
        }
        let mut guard = self
            .writer
            .lock()
            .map_err(|e| FtsError::Tantivy(format!("writer mutex poisoned: {e}")))?;
        let w = self.ensure_writer(&mut guard)?;
        let mut count = 0usize;
        for (id, tenant, content) in iter {
            let term = Term::from_field_text(self.fields.block_id, &id);
            w.delete_term(term);
            let mut doc = TantivyDocument::default();
            doc.add_text(self.fields.block_id, &id);
            doc.add_text(self.fields.tenant, &tenant);
            doc.add_text(self.fields.content, &content);
            w.add_document(doc)?;
            count += 1;
        }
        w.commit()?;
        self.reader.reload()?;
        Ok(count)
    }
}

/// Sidecar paths colocated with the DuckDB file (next to the HNSW
/// `.usearch` files).
pub fn memory_fts_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".fts.memories");
    PathBuf::from(s)
}

pub fn transcript_fts_path(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".fts.transcripts");
    PathBuf::from(s)
}
