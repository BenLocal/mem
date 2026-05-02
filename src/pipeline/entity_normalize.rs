//! Pure normalize_alias function shared by EntityRegistry and pipeline.
//!
//! Rules (per spec Q3 = C):
//! - Lowercase
//! - Trim leading/trailing whitespace
//! - Collapse internal runs of whitespace to a single space
//! - Preserve punctuation verbatim (C++/C#/.NET/F# keep their identity)
//! - Preserve Unicode verbatim (no NFKC; YAGNI for v1)

/// Normalize an alias string to its canonical lookup form.
///
/// `split_whitespace` is the all-in-one whitespace handler: it splits on
/// any Unicode whitespace, drops empties (collapsing runs), and yields
/// no items if the input is whitespace-only or empty (so the join produces
/// the empty string).
pub fn normalize_alias(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_case_and_whitespace() {
        assert_eq!(normalize_alias("Rust"), "rust");
        assert_eq!(normalize_alias("RUST"), "rust");
        assert_eq!(normalize_alias("  Rust  "), "rust");
        assert_eq!(normalize_alias("Rust  language"), "rust language");
        assert_eq!(normalize_alias("\t Rust\nLanguage \t"), "rust language");
    }

    #[test]
    fn preserves_ascii_punctuation() {
        assert_eq!(normalize_alias("C++"), "c++");
        assert_eq!(normalize_alias("C#"), "c#");
        assert_eq!(normalize_alias(".NET"), ".net");
        assert_eq!(normalize_alias("F#"), "f#");
        assert_eq!(normalize_alias("Node.js"), "node.js");
    }

    #[test]
    fn preserves_unicode_no_nfkc() {
        assert_eq!(normalize_alias("中文"), "中文");
        assert_eq!(normalize_alias("Naïve"), "naïve");
        // No NFKC: full-width chars stay full-width
        assert_eq!(normalize_alias("Ｒｕｓｔ"), "ｒｕｓｔ");
    }

    #[test]
    fn empty_and_whitespace_only() {
        assert_eq!(normalize_alias(""), "");
        assert_eq!(normalize_alias("   "), "");
        assert_eq!(normalize_alias("\t\n  \n\t"), "");
    }

    #[test]
    fn idempotent_on_already_normalized() {
        let inputs = ["rust", "rust language", "c++", "node.js"];
        for input in inputs {
            assert_eq!(
                normalize_alias(&normalize_alias(input)),
                normalize_alias(input)
            );
        }
    }
}
