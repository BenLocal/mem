//! Content chunking for embedding (③ long-content recall).
//!
//! mem currently embeds `summary + content` whole. Content longer than
//! the embedder's context window (Qwen3-Embedding ~32k tokens) is
//! silently truncated, so a long capsule's tail becomes unrecallable by
//! semantic search (BM25 still covers it lexically). This module splits
//! content into overlapping token windows so every part is embedded.
//!
//! Token counting reuses the `o200k_base` tokenizer (same as
//! `compress.rs`) as a conservative proxy for the embedder's limit — the
//! exact token boundaries differ from Qwen3's tokenizer, but chunks are
//! sized well under any embedder limit, so the proxy is safe.
//!
//! Short content (`<= max_tokens`) returns a single chunk equal to the
//! whole text, so typical capsules are unchanged (1 chunk = today's
//! behaviour). Only genuinely long content splits.
//!
//! This is the pure chunking primitive (③ phase 1). Wiring it into the
//! embedding worker (one embedding row per chunk) + search aggregation
//! is a later phase that changes the embeddings schema.

use tiktoken_rs::o200k_base_singleton;

/// Default per-chunk token budget. Conservative — well under the
/// embedder's context window so a chunk never truncates, while staying
/// large enough to keep each chunk coherent.
pub const DEFAULT_CHUNK_TOKENS: usize = 2000;

/// Default overlap (in tokens) between consecutive chunks, so a fact
/// straddling a window boundary still lands wholly inside at least one
/// chunk.
pub const DEFAULT_CHUNK_OVERLAP: usize = 200;

/// Split `text` into overlapping token windows of at most `max_tokens`,
/// each advancing `max_tokens - overlap` tokens from the previous start.
///
/// - Text that fits in one window → a single chunk equal to `text`
///   (verbatim, no tokenizer round-trip), so short capsules are
///   unchanged.
/// - `overlap` is clamped to `max_tokens - 1` to guarantee forward
///   progress; `max_tokens` of 0 is treated as 1.
/// - Window pieces are produced by decoding the token slice; for valid
///   UTF-8 this round-trips faithfully (a multi-byte char split across a
///   token boundary is preserved because BPE tokens are whole-char).
pub fn chunk_text(text: &str, max_tokens: usize, overlap: usize) -> Vec<String> {
    let max_tokens = max_tokens.max(1);
    let overlap = overlap.min(max_tokens - 1);
    let bpe = o200k_base_singleton();
    let tokens = bpe.encode_with_special_tokens(text);
    if tokens.len() <= max_tokens {
        return vec![text.to_string()];
    }
    let step = max_tokens - overlap;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        let end = (start + max_tokens).min(tokens.len());
        // A window boundary can fall inside a multi-byte char (emoji / uncommon
        // CJK are split into byte-fragment tokens), where tiktoken's strict
        // `decode` errors and the old `.unwrap_or_default()` blanked the entire
        // chunk — silently dropping that span from the embedding index. Decode
        // the raw bytes and recover lossily, trimming a dangling partial char at
        // either boundary (U+FFFD) instead of losing the whole window.
        let bytes = bpe.decode_bytes(&tokens[start..end]).unwrap_or_default();
        chunks.push(
            String::from_utf8_lossy(&bytes)
                .trim_matches('\u{FFFD}')
                .to_string(),
        );
        if end == tokens.len() {
            break;
        }
        start += step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_single_verbatim_chunk() {
        let t = "short content about rust and lance ANN ranking";
        let chunks = chunk_text(t, DEFAULT_CHUNK_TOKENS, DEFAULT_CHUNK_OVERLAP);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], t, "short content must pass through verbatim");
    }

    #[test]
    fn long_text_splits_and_preserves_head_and_tail() {
        // ~1000 tokens at a 100-token window → several chunks.
        let text = "alpha beta gamma delta epsilon ".repeat(200);
        let chunks = chunk_text(&text, 100, 20);
        assert!(
            chunks.len() > 1,
            "long text must split, got {}",
            chunks.len()
        );
        // The recall bug this fixes: the TAIL must not be dropped.
        assert!(chunks[0].contains("alpha"), "head present in first chunk");
        assert!(
            chunks.last().unwrap().contains("epsilon"),
            "tail present in last chunk (not truncated)"
        );
    }

    #[test]
    fn overlap_ge_window_is_clamped_and_terminates() {
        // overlap >= max_tokens would stall the slide; it must be clamped.
        // Reaching the assertion proves termination.
        let text = "x ".repeat(500);
        let chunks = chunk_text(&text, 50, 999);
        assert!(chunks.len() > 1);
    }

    #[test]
    fn consecutive_chunks_overlap() {
        let text = "alpha beta gamma delta epsilon ".repeat(200);
        let chunks = chunk_text(&text, 100, 30);
        // With overlap, the start of chunk[1] re-includes the end of
        // chunk[0]'s window — so the two share content.
        assert!(chunks.len() >= 2, "expected multiple chunks");
        assert_ne!(chunks[0], chunks[1], "consecutive windows must differ");
    }

    #[test]
    fn split_char_chunks_are_never_empty() {
        // o200k_base splits emoji / uncommon CJK into byte-fragment tokens, so a
        // window boundary (`tokens[start..end]`) can cut mid-char at EITHER end.
        // The old `decode(...).unwrap_or_default()` blanked the WHOLE chunk on
        // such a cut, silently dropping that span from the embedding index. Every
        // chunk of this emoji+CJK text must carry content, and that content must
        // be a contiguous substring of the source (nothing fabricated).
        let text: String = "任务✅完成了🚀部署到生产🔥环境🎉成功".repeat(20);
        let chunks = chunk_text(&text, 16, 4);
        assert!(chunks.len() > 1, "fixture must split into multiple chunks");
        for (i, c) in chunks.iter().enumerate() {
            assert!(!c.trim().is_empty(), "chunk {i} is empty");
            assert!(
                text.contains(c.as_str()),
                "chunk {i} is not a substring of the source: {c:?}"
            );
        }
    }
}
