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
        // Stripe live secret / restricted keys (`sk_live_…` / `rk_live_…`).
        // The `sk-` pattern above needs a hyphen, so Stripe's underscore form
        // slips past it — a distinct, high-confidence prefix of its own.
        Pat {
            re: Regex::new(r"\b[rs]k_live_[A-Za-z0-9]{16,}").unwrap(),
            repl: "[redacted:stripe]",
        },
        // GitHub fine-grained PAT (`github_pat_…`) — the newer format the
        // classic `gh[posru]_` pattern doesn't cover.
        Pat {
            re: Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{22,}").unwrap(),
            repl: "[redacted:github]",
        },
        // Slack tokens (`xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-`).
        Pat {
            re: Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}").unwrap(),
            repl: "[redacted:slack]",
        },
        // Google API key (fixed `AIza` prefix + 35 chars).
        Pat {
            re: Regex::new(r"\bAIza[0-9A-Za-z_-]{35}").unwrap(),
            repl: "[redacted:google]",
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
    fn masks_stripe_github_pat_slack_google() {
        // Each token is assembled at RUNTIME from a bare prefix + a low-entropy
        // body, so no complete realistic-looking literal ever sits in the source
        // file. That keeps GitHub push-protection / secret-scanning from
        // flagging the test fixtures (a contiguous `sk_live_…` / `AIza…` literal
        // would be rejected on push) while still exercising each regex.
        let stripe = red(&format!("STRIPE_KEY=sk_live_{} done", "0".repeat(24)));
        assert!(stripe.contains("[redacted:stripe]"), "got {stripe}");
        assert!(!stripe.contains("sk_live_0"), "stripe key leaked: {stripe}");
        assert!(red(&format!("rk_live_{}", "0".repeat(24))).contains("[redacted:stripe]"));

        // GitHub fine-grained PAT (distinct from the classic gh*_ format).
        let ghpat = red(&format!("token github_pat_{} ok", "0".repeat(30)));
        assert!(ghpat.contains("[redacted:github]"), "got {ghpat}");
        assert!(!ghpat.contains("github_pat_0"), "PAT leaked: {ghpat}");

        // Slack tokens.
        assert!(red(&format!("xoxb-{}", "0".repeat(16))).contains("[redacted:slack]"));
        assert!(red(&format!("xoxp-{}", "0".repeat(16))).contains("[redacted:slack]"));

        // Google API key (AIza + 35).
        let g = red(&format!("key AIza{} here", "0".repeat(35)));
        assert!(g.contains("[redacted:google]"), "got {g}");
        assert!(!g.contains("AIza0"), "google key leaked: {g}");
    }

    #[test]
    fn new_prefixes_dont_false_trigger_on_prose() {
        // Bare prefixes / lookalikes without a real key body must NOT match
        // (\b guard + length floors), so prose stays a zero-alloc Borrowed.
        for clean in [
            "we set sk_live_ mode off",        // prefix, no key body
            "the github_pat process ran",      // no underscore+body
            "xox is a placeholder token name", // not xox[baprs]-
            "AIza is a short fragment",        // < 35 trailing chars
        ] {
            assert!(
                matches!(redact_all(clean), Cow::Borrowed(_)),
                "false positive on: {clean}"
            );
        }
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
