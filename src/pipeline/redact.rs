//! O5 — secret redaction at the index / output layer (closes oss-memory-diff O5).
//!
//! **Storage stays verbatim** — `memories.content` / transcript `content` are
//! NEVER rewritten on disk. We only mask high-confidence secret patterns in
//! DERIVED output so a leaked key captured in a transcript doesn't ride into the
//! vector index or an agent's prompt:
//!   - the compressed search answer + recall banner (via `compress::compress_text`,
//!     the single choke point all output prose flows through), and
//!   - the pre-embedding text (`embedding_worker::embed_input_chunks`).
//!
//! The explicit verbatim-fetch path (`capability_capsule_get`) is intentionally
//! NOT redacted — it is the access-controlled "give me the exact bytes" tool the
//! index-style banner points agents to. Default ON (security-by-default; pure
//! output-layer, doesn't touch storage); opt out with `MEM_REDACT_SECRETS_DISABLED=1`.
//!
//! Patterns are high-confidence only (the O5 white-list boundary) — the token
//! patterns carry a leading `\b` so an `sk-` inside `ask-...` / an `AKIA` inside a
//! word doesn't trigger a false redaction. Tune here; this is the one place.

use once_cell::sync::Lazy;
use regex::Regex;
use std::borrow::Cow;

struct Pat {
    re: Regex,
    repl: &'static str,
}

static PATTERNS: Lazy<Vec<Pat>> = Lazy::new(|| {
    vec![
        // Broad multiline blocks first so token patterns don't half-mask them.
        Pat {
            re: Regex::new(
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            )
            .unwrap(),
            repl: "[redacted:private-key]",
        },
        Pat {
            re: Regex::new(r"(?s)<private>.*?</private>").unwrap(),
            repl: "[redacted:private]",
        },
        // OpenAI / generic `sk-…` API keys.
        Pat {
            re: Regex::new(r"\bsk-[A-Za-z0-9_-]{16,}").unwrap(),
            repl: "[redacted:sk]",
        },
        // AWS access key id.
        Pat {
            re: Regex::new(r"\bAKIA[0-9A-Z]{16}").unwrap(),
            repl: "[redacted:aws]",
        },
        // GitHub tokens (ghp_/gho_/ghu_/ghs_/ghr_).
        Pat {
            re: Regex::new(r"\bgh[posru]_[A-Za-z0-9]{20,}").unwrap(),
            repl: "[redacted:github]",
        },
        // JWT (three base64url segments).
        Pat {
            re: Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}")
                .unwrap(),
            repl: "[redacted:jwt]",
        },
        // Bearer tokens.
        Pat {
            re: Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{12,}").unwrap(),
            repl: "[redacted:bearer]",
        },
    ]
});

/// Whether redaction is active. Default ON; `MEM_REDACT_SECRETS_DISABLED` truthy
/// opts out.
pub fn enabled() -> bool {
    !std::env::var("MEM_REDACT_SECRETS_DISABLED")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Mask high-confidence secrets in `text`. Respects the `enabled()` switch.
/// Returns `Cow::Borrowed` (no allocation) when nothing is redacted — the common
/// case on the hot `compress_text` path.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    if !enabled() {
        return Cow::Borrowed(text);
    }
    let out = redact_all(text);
    // `redact_all` returns `Owned` iff at least one pattern matched — the exact
    // "a secret was masked" signal for the observability counter. Counted here
    // (the env-gated entry), not in the pure `redact_all`, so unit tests of the
    // pattern logic stay side-effect-free.
    if matches!(out, Cow::Owned(_)) {
        crate::metrics::metrics().inc_redaction_hit();
    }
    out
}

/// Pure pattern application, independent of the env switch — unit-testable.
/// Fast-paths to `Borrowed` when no pattern matches (no allocation).
pub fn redact_all(text: &str) -> Cow<'_, str> {
    if !PATTERNS.iter().any(|p| p.re.is_match(text)) {
        return Cow::Borrowed(text);
    }
    let mut s = text.to_string();
    for p in PATTERNS.iter() {
        s = p.re.replace_all(&s, p.repl).into_owned();
    }
    Cow::Owned(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red(s: &str) -> String {
        redact_all(s).into_owned()
    }

    #[test]
    fn clean_text_is_borrowed_no_alloc() {
        assert!(matches!(
            redact_all("a perfectly normal sentence about Lance and vacuum"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn masks_openai_key_but_not_ask_prefix() {
        let out = red("the key is sk-abcdEFGH1234ijklMNOP and that's it");
        assert!(out.contains("[redacted:sk]"), "got {out}");
        assert!(!out.contains("sk-abcdEFGH"), "key leaked: {out}");
        // `ask-` must NOT trip the sk- pattern (\b guard).
        assert!(matches!(
            redact_all("please ask-questions-about-the-deployment-now"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn masks_aws_github_jwt_bearer() {
        assert!(red("id AKIAIOSFODNN7EXAMPLE here").contains("[redacted:aws]"));
        assert!(red("token ghp_abcdefghijklmnopqrstuvwxyz0123").contains("[redacted:github]"));
        assert!(
            red("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY.SflKxwRJSMeKKF2QT4")
                .contains("[redacted:jwt]")
        );
        assert!(red("Authorization: Bearer abcdef1234567890XYZ").contains("[redacted:bearer]"));
    }

    #[test]
    fn masks_private_key_block_and_marker() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----\nafter";
        let out = red(pem);
        assert!(out.contains("[redacted:private-key]"), "got {out}");
        assert!(!out.contains("MIIEowIB"), "key body leaked: {out}");
        assert!(out.contains("before") && out.contains("after"));

        let marker = red("note <private>my-secret-passphrase</private> done");
        assert!(marker.contains("[redacted:private]"));
        assert!(!marker.contains("passphrase"));
    }

    #[test]
    fn masks_multiple_secrets_in_one_text() {
        let out = red("sk-ABCDEFGH12345678ijkl and AKIAIOSFODNN7EXAMPLE both");
        assert!(
            out.contains("[redacted:sk]") && out.contains("[redacted:aws]"),
            "got {out}"
        );
    }
}
