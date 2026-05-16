//! Wire format for embedding vectors crossing the application ↔ storage
//! boundary. The codec is **native-endian f32 bytes**, packed in dim
//! order — chosen historically because the legacy DuckDB-as-storage
//! backend stored embeddings as `BLOB` columns, and both providers
//! (`embed_anything`, `openai`) hand out `Vec<f32>` natively.
//!
//! ## Why this lives in `crate::embedding` and not `crate::storage`
//!
//! Per `docs/backend-coupling.md` §4.3 (QW-3): the codec is the
//! contract between the *embedding source* (provider) and the
//! *embedding sink* (storage). Historically the encode side lived in
//! `service::embedding_helpers` (because the workers there were the
//! only encoders) and the decode side lived inline in
//! `storage::lance_store::mod` (because the lance reader was the only
//! decoder). That layering had storage → service as an
//! implicit dependency, which is backwards (storage is the lower
//! layer).
//!
//! Centralising the codec here lets the dependency arrow always point
//! `application → embedding ← storage`, and exposes the wire format
//! as a single edit point if a future backend chooses something
//! other than native-endian f32 bytes (e.g. pgvector's native
//! `vector(N)` type that takes `Vec<f32>` directly, no blob layer).

/// Encode a slice of `f32` values as a native-endian byte blob.
/// Used by every embedding worker right before handing the embedding
/// to the storage layer's `upsert_*_embedding(blob, dim, ...)`
/// methods.
pub fn encode_f32_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_ne_bytes());
    }
    out
}

/// Decode a native-endian `[f32]` blob back into `Vec<f32>`. Used by
/// every storage backend right after reading an embedding row.
///
/// Returns `Err(&'static str)` on length mismatch — callers wrap into
/// their preferred error type (e.g. storage uses
/// `StorageError::InvalidData`, services / HTTP map to a 500). The
/// `'static` lifetime keeps the codec dependency-free.
pub fn decode_f32_blob(blob: &[u8], dim: usize) -> Result<Vec<f32>, &'static str> {
    if blob.len() != dim * 4 {
        return Err("embedding blob length mismatch (expected dim * 4 bytes)");
    }
    let mut out = Vec::with_capacity(dim);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_values() {
        // Mix of common float shapes — zero, integer-valued, fractional,
        // negative, +infinity. `3.5` rather than 3.14 sidesteps
        // `clippy::approx_constant` (which thinks 3.14 is an attempt at PI).
        let vec = vec![0.0_f32, 1.0, -1.5, 3.5, f32::INFINITY];
        let blob = encode_f32_blob(&vec);
        assert_eq!(blob.len(), vec.len() * 4);
        let decoded = decode_f32_blob(&blob, vec.len()).unwrap();
        assert_eq!(decoded.len(), vec.len());
        for (a, b) in vec.iter().zip(decoded.iter()) {
            // INFINITY is bit-equivalent; NaN we don't test (NaN != NaN).
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn empty_vec_round_trips() {
        let blob = encode_f32_blob(&[]);
        assert!(blob.is_empty());
        let decoded = decode_f32_blob(&blob, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_rejects_length_mismatch() {
        // 13 bytes is not a multiple of 4.
        let err = decode_f32_blob(&[0u8; 13], 3).unwrap_err();
        assert!(err.contains("length mismatch"));
        // 8 bytes is 2 f32s, but caller asked for 3.
        let err = decode_f32_blob(&[0u8; 8], 3).unwrap_err();
        assert!(err.contains("length mismatch"));
    }
}
