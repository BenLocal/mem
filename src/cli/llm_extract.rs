//! O7(c) — opt-in generative-LLM extraction lane.
//!
//! The only lane that uses a generative LLM. It is **DEFAULT OFF** and
//! **fail-safe by construction** — three independent guards mean mem never
//! hard-depends on an LLM (the §6 "no inline LLM extraction" philosophy stays
//! the default):
//!   1. `enabled()` — gated by `MEM_MINE_LLM_EXTRACT` (default off).
//!   2. `LlmExtractConfig::from_env()` — returns `None` when the gateway isn't
//!      configured (`LLM_API_BASE` / `LLM_MODEL` unset) → lane inactive.
//!   3. `llm_candidates()` — catches EVERY error (network / non-2xx / parse) and
//!      returns an empty Vec; the miner then silently falls back to the
//!      zero-LLM lanes (O7 a/b) / `<mem-save>` tags. Never panics, never
//!      propagates, never blocks the mine on a dead gateway.
//!
//! Everything it surfaces is still ingested as `PendingConfirmation`
//! (review-gated), exactly like O7(b) — the LLM proposes, a human/agent
//! confirms.
//!
//! Gateway: an OpenAI-compatible `POST {base}/chat/completions`. The internal
//! `llm_entry` gateway authenticates at the network edge, so `LLM_API_KEY` is
//! empty there (no `Authorization` header sent). The reqwest client is built
//! with `.no_proxy()` — the Rust equivalent of httpx `trust_env=False`: without
//! it, an ambient `HTTP(S)_PROXY` (set for git/cargo) would route the
//! internal-IP gateway call through the public proxy and 502.

use serde::Deserialize;
use tracing::warn;

/// System prompt: extract atomic, durable, reusable facts/decisions verbatim-ish,
/// as a JSON array of strings. Empty array when nothing is worth persisting.
const SYS_PROMPT: &str = "You extract durable, reusable memories from a snippet of a developer's \
conversation. Output ONLY a JSON array of short strings — each one atomic, \
self-contained, and worth recalling in a future session (a decision, a fix, a \
gotcha, a configuration fact). Keep the original wording; do not summarize away \
specifics. If nothing is worth saving, output []. No prose, no markdown, no code \
fences — just the JSON array.";

/// Per-block candidate cap and length window (mirrors the O7 b filter).
const MAX_PER_BLOCK: usize = 5;
const MIN_LEN: usize = 12;
const MAX_LEN: usize = 400;
const MIN_SUBSTANTIVE: usize = 4;
/// Per-call timeout — a dead/slow gateway must not stall the mine indefinitely.
const TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct LlmExtractConfig {
    pub base: String,
    pub model: String,
    pub api_key: String,
}

impl LlmExtractConfig {
    /// Read gateway config from a getter (env in prod, a closure in tests).
    /// `None` when `LLM_API_BASE` or `LLM_MODEL` is missing/blank — the lane is
    /// then inactive (silent fallback). An empty `LLM_API_KEY` is fine: the
    /// internal gateway authenticates at the edge.
    pub fn from_get(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let base = get("LLM_API_BASE").filter(|s| !s.trim().is_empty())?;
        let model = get("LLM_MODEL").filter(|s| !s.trim().is_empty())?;
        let api_key = get("LLM_API_KEY").unwrap_or_default();
        Some(Self {
            base: base.trim().trim_end_matches('/').to_string(),
            model: model.trim().to_string(),
            api_key: api_key.trim().to_string(),
        })
    }

    pub fn from_env() -> Option<Self> {
        Self::from_get(|k| std::env::var(k).ok())
    }
}

/// Whether the O7(c) lane is switched on. Default OFF.
pub fn enabled_from(get: impl Fn(&str) -> Option<String>) -> bool {
    get("MEM_MINE_LLM_EXTRACT")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

pub fn enabled() -> bool {
    enabled_from(|k| std::env::var(k).ok())
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: ChatMsg,
}
#[derive(Deserialize)]
struct ChatMsg {
    content: Option<String>,
}

/// Extract candidates from one text block via the gateway. **Fail-safe**: any
/// error (config, network, non-2xx, malformed body) is logged and collapsed to
/// an empty Vec — the caller continues with the zero-LLM lanes.
pub async fn llm_candidates(cfg: &LlmExtractConfig, text: &str) -> Vec<String> {
    match call_gateway(cfg, text).await {
        Ok(content) => parse_candidates(&content),
        Err(e) => {
            warn!(error = %e, "O7(c): LLM extract failed — falling back (no candidates)");
            Vec::new()
        }
    }
}

async fn call_gateway(cfg: &LlmExtractConfig, text: &str) -> Result<String, String> {
    // `.no_proxy()` is load-bearing: don't route the internal-IP gateway call
    // through an ambient HTTP(S)_PROXY (would 502). See module docs.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| e.to_string())?;

    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {"role": "system", "content": SYS_PROMPT},
            {"role": "user", "content": text},
        ],
        "temperature": 0.2,
        // gateway call-stats attribution (extra_body equivalent).
        "caller": "mem-mine-o7c",
    });

    let url = format!("{}/chat/completions", cfg.base);
    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body);
    // Internal gateway: empty key → no Authorization header. Cloud-proxied
    // models behind the gateway would set a real key.
    if !cfg.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "gateway {status}: {}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    let parsed: ChatResp = serde_json::from_slice(&bytes).map_err(|e| format!("chat JSON: {e}"))?;
    Ok(parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default())
}

/// Pure: parse the model's reply into candidate strings. Lenient — strips code
/// fences and pulls the first `[ ... ]` JSON array, parses it as a string list,
/// then applies the same length/quality filter + cap + dedup as O7(b). Returns
/// empty on any parse failure (the model returned prose, not JSON). Unit-tested
/// without a network.
pub fn parse_candidates(content: &str) -> Vec<String> {
    let trimmed = content
        .trim()
        .trim_start_matches("```json")
        .trim_matches('`')
        .trim();
    // Slice the first array literal so leading/trailing prose can't break parse.
    let arr = match (trimmed.find('['), trimmed.rfind(']')) {
        (Some(a), Some(b)) if b > a => &trimmed[a..=b],
        _ => return Vec::new(),
    };
    let raw: Vec<String> = match serde_json::from_str(arr) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for s in raw {
        let s = s.trim().to_string();
        if !is_candidate_len(&s) {
            continue;
        }
        if seen.insert(s.clone()) {
            out.push(s);
            if out.len() >= MAX_PER_BLOCK {
                break;
            }
        }
    }
    out
}

fn is_candidate_len(s: &str) -> bool {
    let n = s.chars().count();
    if !(MIN_LEN..=MAX_LEN).contains(&n) {
        return false;
    }
    s.chars().filter(|c| c.is_alphanumeric()).count() >= MIN_SUBSTANTIVE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn config_none_when_unconfigured() {
        assert!(LlmExtractConfig::from_get(getter(&[])).is_none());
        // base without model → still None.
        assert!(
            LlmExtractConfig::from_get(getter(&[("LLM_API_BASE", "http://gw:16777")])).is_none()
        );
        // blank base → None.
        assert!(LlmExtractConfig::from_get(getter(&[
            ("LLM_API_BASE", "  "),
            ("LLM_MODEL", "qwen3-max")
        ]))
        .is_none());
    }

    #[test]
    fn config_some_with_empty_key_and_trims_base() {
        let cfg = LlmExtractConfig::from_get(getter(&[
            ("LLM_API_BASE", "http://gw:16777/"),
            ("LLM_MODEL", "qwen3-max"),
        ]))
        .expect("configured");
        assert_eq!(cfg.base, "http://gw:16777"); // trailing slash trimmed
        assert_eq!(cfg.model, "qwen3-max");
        assert_eq!(cfg.api_key, ""); // empty key OK for internal gateway
    }

    #[test]
    fn enabled_defaults_off() {
        assert!(!enabled_from(getter(&[])));
        assert!(!enabled_from(getter(&[("MEM_MINE_LLM_EXTRACT", "0")])));
        assert!(enabled_from(getter(&[("MEM_MINE_LLM_EXTRACT", "1")])));
        assert!(enabled_from(getter(&[("MEM_MINE_LLM_EXTRACT", "true")])));
    }

    #[test]
    fn parse_plain_json_array() {
        let c = parse_candidates(
            r#"["decided to use Lance for local storage", "vacuum stays off by default to avoid manifest deletion"]"#,
        );
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn parse_strips_fences_and_prose() {
        let c = parse_candidates(
            "Here are the memories:\n```json\n[\"we chose rustls over native-tls for the TLS stack\"]\n```\nhope that helps",
        );
        assert_eq!(c.len(), 1, "got {c:?}");
        assert!(c[0].contains("rustls"));
    }

    #[test]
    fn parse_empty_and_garbage() {
        assert!(parse_candidates("[]").is_empty());
        assert!(parse_candidates("I could not find anything to save.").is_empty());
        assert!(parse_candidates("").is_empty());
        // too-short entries filtered out.
        assert!(parse_candidates(r#"["ok", "no"]"#).is_empty());
    }

    #[test]
    fn parse_caps_and_dedups() {
        let many = r#"["alpha decision number one here","alpha decision number one here","beta decision number two here","gamma decision number three","delta decision number four here","epsilon decision number five","zeta decision number six here"]"#;
        let c = parse_candidates(many);
        assert!(
            c.len() <= MAX_PER_BLOCK,
            "cap at {MAX_PER_BLOCK}, got {}",
            c.len()
        );
        // dedup: the repeated "alpha…" appears once.
        assert_eq!(c.iter().filter(|s| s.starts_with("alpha")).count(), 1);
    }
}
