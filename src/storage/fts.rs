//! Route-B full-text (BM25) subsystem — a self-contained Tantivy
//! inverted index that replaces the DuckDB `lance_fts(...)` read path
//! for capsule AND transcript search (see
//! `docs/remove-duckdb-keep-lance.md` §4).
//!
//! The index is corpus-agnostic: it stores an opaque `id` (the capsule's
//! `capability_capsule_id` or a transcript block's `message_block_id`), a
//! `tenant` filter field, and tokenized `content`. `LanceStore` holds one
//! [`FtsIndex`] per bucket (capsule + transcript), each rebuilt from its
//! own Lance table.
//!
//! ## Why Tantivy
//!
//! The DuckDB / lance-native `lance_fts` path intermittently fails with
//! a ragged-batch `IO Error` when an FTS scan merges an indexed segment
//! with the unindexed tail (a lance-core `scanner.rs` bug, present in
//! both the DuckDB extension and lance's own Rust reader — see the
//! CLAUDE.md transcript-search note). Tantivy is a pure-Rust embedded
//! inverted index with no external service, so it fits the local-first
//! single-binary constraint while side-stepping the upstream bug
//! entirely.
//!
//! ## Tokenizer: jieba (precision mode)
//!
//! The corpus is Chinese-heavy. We register [`tantivy_jieba::JiebaTokenizer`]
//! (precision mode — `::new()`) on the `content` field. A §6 microbenchmark
//! settled this: jieba ties cang-jie on top-10 recall overlap (0.975) and
//! builds ~1.85× faster.
//!
//! ## Rebuild strategy: startup full-rebuild into a RamDirectory
//!
//! The index lives entirely in a [`tantivy::directory::RamDirectory`]
//! (`Index::create_in_ram`) and is rebuilt from scratch via
//! [`FtsIndex::rebuild`]. At real scale (~31k docs) a full
//! rebuild is <1s; at 10× (~314k) it's ~6s — well under the 30s gate
//! (§6). A RAM index is the simplest sane choice for `mem serve` startup:
//! no on-disk path to thread through `LanceStore`, no stale-index window,
//! and the source of truth (the Lance `capability_capsules` table) is
//! always re-scannable. The index is rebuilt at open time and again
//! whenever `MaintenanceStore::rebuild_query_indexes` runs.
//!
//! ## CJK query term-split (load-bearing)
//!
//! Tantivy's `QueryParser` treats an unspaced CJK run as a *phrase*
//! query, which returns 0 hits against a tokenized index. [`FtsIndex::bm25`]
//! therefore runs the query text through the same jieba tokenizer and
//! builds a `should`/OR [`BooleanQuery`] over the resulting terms instead
//! of handing the raw run to a parser. This is the only correct way to
//! query a jieba-tokenized CJK index.

use std::sync::RwLock;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{doc, Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::storage::types::StorageError;

/// The jieba tokenizer name registered on the `content` field.
const JIEBA: &str = "jieba";

/// Map any tantivy error into the crate's generic [`StorageError`]. We
/// drop the rich variant info — upper layers only branch on coarse
/// error classes — but keep the message for logs.
fn fts_err(e: impl std::fmt::Display) -> StorageError {
    StorageError::InvalidInput(format!("tantivy fts: {e}"))
}

/// A corpus row as the FTS index needs it: the opaque `id` to return
/// (a `capability_capsule_id` or a `message_block_id`), the tenant to
/// filter on, and the verbatim content to tokenize + index.
#[derive(Clone, Debug)]
pub struct FtsDoc {
    pub id: String,
    pub tenant: String,
    pub content: String,
}

/// The schema field handles, cached so query/write paths don't re-look
/// them up by name on every call.
struct Fields {
    content: Field,
    id: Field,
    tenant: Field,
}

/// One live Tantivy index + its reader, replaced wholesale on rebuild.
struct Live {
    index: Index,
    reader: IndexReader,
}

/// A Tantivy-backed BM25 full-text index over capsule `content`,
/// tenant-filtered. Rebuilt from scratch (startup full-rebuild) — see
/// the module docs. Thread-safe: the live index is behind an `RwLock`,
/// swapped atomically on rebuild so concurrent `bm25` reads always see a
/// consistent index.
pub struct FtsIndex {
    schema: Schema,
    fields: Fields,
    live: RwLock<Live>,
}

impl FtsIndex {
    /// Build the shared schema: `content` (TEXT, jieba tokenizer,
    /// WithFreqsAndPositions), `id` (STRING, STORED — the opaque id we
    /// return), `tenant` (STRING, INDEXED — for tenant filtering as a
    /// `Must` term in the BM25 query).
    fn build_schema() -> (Schema, Fields) {
        let mut builder = Schema::builder();
        // `TEXT` is tokenized + WithFreqsAndPositions by default; we
        // override the tokenizer name to jieba below. We don't STORE
        // content — the index only needs to return ids.
        let content_options = tantivy::schema::TextOptions::default().set_indexing_options(
            tantivy::schema::TextFieldIndexing::default()
                .set_tokenizer(JIEBA)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let content = builder.add_text_field("content", content_options);
        // STRING = raw (un-tokenized) single-token field; STORED so we
        // can read the id back off a hit.
        let id = builder.add_text_field("id", STRING | STORED);
        // tenant is a raw STRING term used as a `Must` filter. `STRING`
        // is already indexed + untokenized (single-token), exactly what a
        // tenant exact-match filter needs; not stored (we already know
        // which tenant we queried).
        let tenant = builder.add_text_field("tenant", STRING);
        let schema = builder.build();
        (
            schema,
            Fields {
                content,
                id,
                tenant,
            },
        )
    }

    /// Register the jieba tokenizer on a freshly-created index so the
    /// `content` field tokenizes the same way at write and query time.
    fn register_jieba(index: &Index) {
        index
            .tokenizers()
            .register(JIEBA, tantivy_jieba::JiebaTokenizer::new());
    }

    /// Create an empty in-RAM index with the jieba tokenizer registered.
    fn fresh_index(schema: &Schema) -> Result<Live, StorageError> {
        let index = Index::create_in_ram(schema.clone());
        Self::register_jieba(&index);
        let reader = index.reader().map_err(fts_err)?;
        Ok(Live { index, reader })
    }

    /// Create an empty FtsIndex. Call [`Self::rebuild`] to populate it
    /// from a corpus (capsules or transcript blocks).
    pub fn new() -> Result<Self, StorageError> {
        let (schema, fields) = Self::build_schema();
        let live = Self::fresh_index(&schema)?;
        Ok(Self {
            schema,
            fields,
            live: RwLock::new(live),
        })
    }

    /// Full rebuild: drop the old index and index every supplied doc into
    /// a fresh in-RAM index (single writer, single `commit` +
    /// `wait_merging_threads` so segments are merged before we publish).
    /// The new index is swapped in under the write lock — concurrent
    /// `bm25` readers see either the old or the new index, never a
    /// half-built one.
    pub fn rebuild(&self, docs: &[FtsDoc]) -> Result<(), StorageError> {
        let next = Self::fresh_index(&self.schema)?;
        {
            // 50 MB writer heap — ample for the route-B corpus (tens of
            // thousands of short docs); Tantivy requires ≥3 MB.
            let mut writer: IndexWriter = next.index.writer(50_000_000).map_err(fts_err)?;
            for d in docs {
                writer
                    .add_document(doc!(
                        self.fields.content => d.content.as_str(),
                        self.fields.id => d.id.as_str(),
                        self.fields.tenant => d.tenant.as_str(),
                    ))
                    .map_err(fts_err)?;
            }
            writer.commit().map_err(fts_err)?;
            // Block on segment merges so the published reader reflects a
            // fully-merged index (the §6 "real value" — merge included).
            writer.wait_merging_threads().map_err(fts_err)?;
        }
        next.reader.reload().map_err(fts_err)?;
        *self.live.write().expect("fts live lock poisoned") = next;
        Ok(())
    }

    /// Term-split `query_text` with the jieba tokenizer and collect the
    /// distinct, lexically-meaningful terms (in first-seen order). This is
    /// the CJK fix: an unspaced CJK run becomes several terms instead of
    /// one phrase.
    ///
    /// jieba emits a standalone whitespace token (`" "`) between words.
    /// That token is indexed into nearly every multi-word document, so
    /// querying for it would make the `should`/OR query match the whole
    /// corpus. We drop whitespace-only tokens here so the query carries
    /// only real terms. (The index still contains the space token, which
    /// is harmless as long as no query ever searches for it.)
    fn split_terms(analyzer: &TextAnalyzer, query_text: &str) -> Vec<String> {
        let mut analyzer = analyzer.clone();
        let mut stream = analyzer.token_stream(query_text);
        let mut terms: Vec<String> = Vec::new();
        while let Some(tok) = stream.next() {
            if tok.text.trim().is_empty() {
                continue;
            }
            if !terms.iter().any(|t| t == &tok.text) {
                terms.push(tok.text.clone());
            }
        }
        terms
    }

    /// BM25 search: term-split `query_text` (see [`Self::split_terms`]),
    /// build a `should`/OR boolean query over the content terms AND a
    /// `must` tenant filter, score by BM25, and return up to `k`
    /// `(id, rank)` pairs. Ranks are 1-based, ordered by score desc with
    /// a deterministic tie-break on id asc.
    ///
    /// Empty / whitespace `query_text` or `k == 0` → `Ok(vec![])`. A
    /// query that tokenizes to no terms (e.g. all punctuation) likewise
    /// returns empty.
    pub fn bm25(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_text.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let live = self.live.read().expect("fts live lock poisoned");
        let analyzer = live
            .index
            .tokenizers()
            .get(JIEBA)
            .ok_or_else(|| StorageError::InvalidInput("fts: jieba tokenizer missing".into()))?;
        let terms = Self::split_terms(&analyzer, query_text);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        // should/OR over the content terms — NOT a phrase. This is the
        // load-bearing CJK fix: handing the raw run to a QueryParser would
        // make it a phrase query and return 0 hits.
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len() + 1);
        let should: Vec<(Occur, Box<dyn Query>)> = terms
            .iter()
            .map(|t| {
                let term = Term::from_field_text(self.fields.content, t);
                let q: Box<dyn Query> =
                    Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                (Occur::Should, q)
            })
            .collect();
        // Wrap the OR-of-terms in its own BooleanQuery so the tenant
        // filter is a top-level `Must` over the whole disjunction.
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(should))));
        let tenant_term = Term::from_field_text(self.fields.tenant, tenant);
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(tenant_term, IndexRecordOption::Basic)),
        ));
        let query = BooleanQuery::new(clauses);

        let searcher = live.reader.searcher();
        // Oversample the collector a little so the id tie-break sort
        // below is stable across score ties, then truncate to k.
        let limit = k.saturating_mul(4).max(k);
        let hits = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(fts_err)?;

        // (id, score) for each hit; sort by (score DESC, id ASC) for
        // determinism, then assign 1-based ranks and truncate to k.
        let mut scored: Vec<(String, f32)> = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            let stored: TantivyDocument = searcher.doc(addr).map_err(fts_err)?;
            let id = stored
                .get_first(self.fields.id)
                .and_then(|v| v.as_str())
                .ok_or_else(|| StorageError::InvalidData("fts: hit missing id"))?
                .to_string();
            scored.push((id, score));
        }
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(k);
        Ok(scored
            .into_iter()
            .enumerate()
            .map(|(idx, (id, _))| (id, idx as i64 + 1))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, tenant: &str, content: &str) -> FtsDoc {
        FtsDoc {
            id: id.into(),
            tenant: tenant.into(),
            content: content.into(),
        }
    }

    fn ids(pairs: &[(String, i64)]) -> Vec<String> {
        pairs.iter().map(|(id, _)| id.clone()).collect()
    }

    #[test]
    fn empty_query_and_zero_k_are_empty() {
        let fts = FtsIndex::new().unwrap();
        fts.rebuild(&[d("a", "t1", "hello world")]).unwrap();
        assert!(fts.bm25("t1", "", 5).unwrap().is_empty());
        assert!(fts.bm25("t1", "   ", 5).unwrap().is_empty());
        assert!(fts.bm25("t1", "hello", 0).unwrap().is_empty());
    }

    #[test]
    fn english_bm25_matches_and_ranks() {
        let fts = FtsIndex::new().unwrap();
        fts.rebuild(&[
            d("a", "t1", "old decay formula uses updated_at only"),
            d("b", "t1", "new decay formula anchors on last_used_at"),
            d("c", "t1", "always run fmt and clippy before every commit"),
        ])
        .unwrap();
        let hits = fts.bm25("t1", "decay formula", 5).unwrap();
        let got = ids(&hits);
        // Both decay docs match; the unrelated commit doc does not.
        assert!(got.contains(&"a".to_string()), "got {got:?}");
        assert!(got.contains(&"b".to_string()), "got {got:?}");
        assert!(!got.contains(&"c".to_string()), "got {got:?}");
        // Ranks are 1-based and contiguous.
        assert_eq!(hits[0].1, 1);
    }

    #[test]
    fn tenant_filter_isolates() {
        let fts = FtsIndex::new().unwrap();
        fts.rebuild(&[
            d("a", "t1", "decay formula here"),
            d("x", "t2", "decay formula leaks?"),
        ])
        .unwrap();
        let hits = fts.bm25("t1", "decay formula", 5).unwrap();
        assert_eq!(ids(&hits), vec!["a".to_string()]);
    }

    #[test]
    fn cjk_unspaced_run_is_term_split_not_phrase() {
        let fts = FtsIndex::new().unwrap();
        fts.rebuild(&[
            d("zh1", "t1", "向量检索与全文检索的混合排序"),
            d("zh2", "t1", "完全无关的内容"),
        ])
        .unwrap();
        // An unspaced CJK query run must term-split (jieba) and match via
        // should/OR — a phrase query would return 0 hits here.
        let hits = fts.bm25("t1", "全文检索", 5).unwrap();
        assert_eq!(ids(&hits), vec!["zh1".to_string()], "got {hits:?}");
    }

    #[test]
    fn rebuild_replaces_corpus() {
        let fts = FtsIndex::new().unwrap();
        fts.rebuild(&[d("a", "t1", "decay formula")]).unwrap();
        assert_eq!(fts.bm25("t1", "decay", 5).unwrap().len(), 1);
        // Rebuild with a disjoint corpus — the old doc is gone.
        fts.rebuild(&[d("b", "t1", "unrelated text")]).unwrap();
        assert!(fts.bm25("t1", "decay", 5).unwrap().is_empty());
        assert_eq!(fts.bm25("t1", "unrelated", 5).unwrap().len(), 1);
    }
}
