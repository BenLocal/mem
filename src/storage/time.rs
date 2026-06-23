//! Time helpers shared across storage and service layers.
//!
//! All timestamps in this codebase are stored as zero-padded 20-digit
//! millisecond strings (e.g. `"00000001714521600000"`). Keeping the format
//! parser/formatter pair in one place avoids accidental drift between the
//! many call sites that mint or advance these stamps.

/// Returns the current wall-clock time as a 20-digit millisecond string.
pub fn current_timestamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
}

/// Adds `add_ms` milliseconds to `ts` (a 20-digit millisecond string) and
/// returns the result in the same encoding. Non-digit characters in `ts` are
/// stripped before parsing; an unparseable input is treated as `0`. Saturates
/// on overflow rather than panicking.
pub fn timestamp_add_ms(ts: &str, add_ms: u128) -> String {
    let digits: String = ts.chars().filter(|c| c.is_ascii_digit()).collect();
    let base: u128 = digits.parse().unwrap_or(0);
    format!("{:020}", base.saturating_add(add_ms))
}

/// Subtracts `sub_ms` milliseconds from `ts` (a 20-digit millisecond string)
/// and returns the result in the same encoding. Mirrors [`timestamp_add_ms`]:
/// non-digit characters are stripped before parsing, an unparseable input is
/// treated as `0`, and the result **saturates at 0** rather than underflowing.
/// Used to compute a lease/visibility cutoff (`now - lease`) for reclaiming
/// orphaned in-flight jobs.
pub fn timestamp_sub_ms(ts: &str, sub_ms: u128) -> String {
    let digits: String = ts.chars().filter(|c| c.is_ascii_digit()).collect();
    let base: u128 = digits.parse().unwrap_or(0);
    format!("{:020}", base.saturating_sub(sub_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_sub_ms_subtracts_and_zero_pads() {
        // 1_778_000_010_000 - 300_000 = 1_777_999_710_000.
        let out = timestamp_sub_ms("00000001778000010000", 300_000);
        assert_eq!(out, "00000001777999710000");
        assert_eq!(out.len(), 20);
    }

    #[test]
    fn timestamp_sub_ms_saturates_at_zero() {
        assert_eq!(
            timestamp_sub_ms("00000000000000000100", 500),
            "00000000000000000000"
        );
    }

    #[test]
    fn timestamp_sub_ms_strips_non_digits_like_add() {
        // Same lenient parsing contract as timestamp_add_ms.
        assert_eq!(
            timestamp_sub_ms("1_778_000_010_000", 10_000),
            "00000001778000000000"
        );
    }
}
