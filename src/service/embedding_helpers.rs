//! Helpers shared between embedding workers (memories + transcripts).
//!
//! The two workers (`embedding_worker` and `transcript_embedding_worker`) are
//! structural mirrors of each other. The functions here are the pieces that
//! are byte-for-byte identical between them — hashing content, truncating
//! long error strings, and computing retry backoffs. Keeping them in a
//! sibling module lets each worker `use super::embedding_helpers::*;`
//! rather than duplicating the bodies.
//!
//! Timestamp helpers (`current_timestamp`, `timestamp_add_ms`) live in
//! `crate::storage::time` instead, since the storage layer also needs them.
//!
//! Vector wire format (`encode_f32_blob` / `decode_f32_blob`) lives in
//! `crate::embedding::wire` — moved out of this module in QW-3 so the
//! dependency arrow stays `application → embedding ← storage` rather than
//! `storage → service`. See `docs/backend-coupling.md` §4.3.

/// Retry backoff schedule for a failed embedding job, in milliseconds.
///
/// `attempt_after_fail` is the number of failures recorded so far on the
/// row. Schedule: 1 min → 5 min → 30 min for subsequent retries.
pub fn failure_backoff_ms(attempt_after_fail: i64) -> u128 {
    match attempt_after_fail {
        1 => 60_000,
        2 => 300_000,
        _ => 1_800_000,
    }
}

/// Truncates a worker error message so it fits in the `embedding_jobs.error`
/// column (or its transcript counterpart) without unbounded growth.
pub fn truncate_error(message: &str) -> String {
    const MAX: usize = 2000;
    if message.len() <= MAX {
        message.to_string()
    } else {
        message.chars().take(MAX).collect()
    }
}

/// Hex-encoded SHA-256 of `text`. Used for content-hash drift detection on
/// embedding jobs.
pub fn sha2_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
