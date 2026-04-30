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
