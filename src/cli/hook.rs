//! `mem hook <event>` — typed entry points for the Claude Code hook
//! scripts. Each subcommand reads the hook's JSON payload on stdin, runs
//! the recall/nudge logic in typed Rust, and prints the hook-output JSON
//! envelope (`{"hookSpecificOutput":{...}}`) — or `{}` to inject nothing —
//! on stdout. It ALWAYS exits 0: a hook must never block the user's work.
//!
//! This replaces the logic that used to live inline in the bash hooks
//! (`claude_code_post_tool_use_error.sh`, `claude_code_user_prompt_submit.sh`,
//! `claude_code_post_tool_use.sh`). Those scripts now `exec mem hook …`.
//! The motivation: the bash versions parsed each hook payload with `jq`
//! against ASSUMED field paths, and Claude Code's payload shape differs
//! per event — `PostToolUse` carries `tool_response.{stdout,stderr,…}`,
//! while `PostToolUseFailure` carries a top-level `.error`
//! (`"Exit code N\n<output>"`) and NO `tool_response`. Guessing those
//! shapes in jq silently mis-fired. Parsing them in typed Rust (with
//! unit tests over the exact payloads) does not.
//!
//! Event → subcommand map:
//!   - `PostToolUseFailure`(Bash) → `recall-error`  (incident recall after a failed command)
//!   - `UserPromptSubmit`         → `recall-prompt` (capsule + transcript recall for the prompt)
//!   - `PostToolUse`(Bash)        → `commit-nudge`  (propose-experience nudge after a real commit)

use clap::Subcommand;
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::OnceLock;
use std::time::Duration;

use super::common::RemoteArgs;

#[derive(Debug, Subcommand)]
pub enum HookCommand {
    /// PostToolUseFailure(Bash): recall related incidents after a command fails.
    RecallError(RemoteArgs),
    /// UserPromptSubmit: recall relevant capsules + past conversations for the prompt.
    RecallPrompt(RemoteArgs),
    /// PostToolUse(Bash): nudge to propose an experience capsule after a substantive commit.
    CommitNudge,
}

impl HookCommand {
    /// (label, log-file) for the event — used for the breadcrumb log that
    /// makes "did it fire / what did it decide" debuggable, same files the
    /// old bash hooks wrote.
    fn log_target(&self) -> (&'static str, &'static str) {
        match self {
            HookCommand::RecallError(_) => ("recall-error", "/tmp/mem-error-recall-hook.log"),
            HookCommand::RecallPrompt(_) => ("recall-prompt", "/tmp/mem-userprompt-hook.log"),
            HookCommand::CommitNudge => ("commit-nudge", "/tmp/mem-posttooluse-hook.log"),
        }
    }
}

/// Dispatch entry. Reads the hook payload from stdin, runs the handler,
/// prints the resulting JSON (compact, newline-terminated) and returns 0.
/// Never returns non-zero: hook failures must be invisible to the user.
pub async fn run(command: HookCommand) -> i32 {
    let payload: Value = serde_json::from_str(&read_stdin()).unwrap_or_else(|_| json!({}));
    let (label, logfile) = command.log_target();
    let out = match command {
        HookCommand::RecallError(remote) => recall_error(&payload, &remote).await,
        HookCommand::RecallPrompt(remote) => recall_prompt(&payload, &remote).await,
        HookCommand::CommitNudge => commit_nudge(&payload),
    };
    let decision = if out.get("hookSpecificOutput").is_some() {
        "inject"
    } else {
        "skip"
    };
    hook_log(logfile, &format!("{label} -> {decision}"));
    // Ignore write errors (e.g. a closed pipe): a hook must exit 0 regardless.
    let _ = writeln!(std::io::stdout(), "{out}");
    0
}

/// Best-effort breadcrumb log with size-capped rotation (keeps the last 200
/// lines once the file passes 256 KiB). All IO failures are ignored.
fn hook_log(path: &str, msg: &str) {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 262_144 {
        if let Ok(content) = std::fs::read_to_string(path) {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(200);
            let _ = std::fs::write(path, lines[start..].join("\n"));
        }
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{ts} {msg}");
    }
}

// ---------------------------------------------------------------------------
// recall-error  (PostToolUseFailure on Bash)
// ---------------------------------------------------------------------------

async fn recall_error(p: &Value, remote: &RemoteArgs) -> Value {
    if env_flag("MEM_RECALL_DISABLED") || env_flag("MEM_ERROR_RECALL_DISABLED") {
        return skip();
    }
    // Claude Code's shell tool is `Bash`; Codex's is `exec_command`.
    if !matches!(p["tool_name"].as_str(), Some("Bash" | "exec_command")) {
        return skip();
    }
    if p["is_interrupt"].as_bool() == Some(true) {
        return skip();
    }
    let Some(sig) = error_signature(failure_error_text(p)) else {
        return skip();
    };
    // Per-session dedup: agents retry the same failing command repeatedly.
    if dedup_seen(p["session_id"].as_str().unwrap_or(""), &sig) {
        return skip();
    }
    let resp = search_capsules(
        remote,
        &sig,
        "resolve an error / find related incident",
        1000,
        30,
        // No scope filter: an incident/fix is often cross-repo, so error
        // recall stays global on purpose.
        &[],
    )
    .await;
    format_error_recall(&resp)
}

/// The failure text from a PostToolUseFailure payload. Claude Code puts it
/// in a top-level `.error` string ("Exit code N\n<stderr+stdout>"); Codex
/// mirrors the Claude field names but may deliver it under `tool_response`
/// (stderr / output / error) or as a bare string. Probe all so error recall
/// fires on both runtimes.
fn failure_error_text(p: &Value) -> &str {
    p["error"]
        .as_str()
        .or_else(|| p["tool_response"]["stderr"].as_str())
        .or_else(|| p["tool_response"]["output"].as_str())
        .or_else(|| p["tool_response"]["error"].as_str())
        .or_else(|| p["tool_response"].as_str())
        .unwrap_or("")
}

/// Extract a searchable error signature from a `PostToolUseFailure`
/// `.error` field. Returns `None` for benign non-zero exits (`grep`
/// no-match, `diff` found-differences, `test` false) whose output carries
/// no structured error signature — those are not incidents worth recalling.
fn error_signature(error: &str) -> Option<String> {
    let re = strong_error_re();
    let salient: Vec<&str> = error
        .lines()
        // Drop the runtime's "Exit code N" envelope line.
        .filter(|l| !is_exit_code_line(l))
        .filter(|l| re.is_match(l))
        .take(3)
        .collect();
    if salient.is_empty() {
        return None;
    }
    let sig: String = salient
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let sig: String = sig.chars().take(400).collect();
    if sig.chars().count() < 8 {
        return None;
    }
    Some(sig)
}

fn is_exit_code_line(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 9 && t[..9].eq_ignore_ascii_case("exit code")
}

/// Strong, structured error patterns (anchored / multi-token), NOT bare
/// substrings like `error`/`fail` that match benign output. Case-insensitive.
fn strong_error_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)panic|traceback|exception|assertion|segfault|core dumped|fatal|error\[|could not compile|command not found|no such file|not found|permission denied|connection refused|cannot open|unresolved|undefined reference|[0-9]+ (failed|errors?)|E[0-9]{3,4}|[A-Za-z_.]+(Error|Exception)",
        )
        .expect("static strong-error regex")
    })
}

fn format_error_recall(resp: &Value) -> Value {
    let dir = section(resp, "directives");
    let facts = section(resp, "relevant_facts");
    let pat = section(resp, "reusable_patterns");
    if dir.is_empty() && facts.is_empty() && pat.is_empty() {
        return skip();
    }
    let mut lines = vec![
        "⚠️ The last Bash command failed — mem found related incidents/fixes. Check these BEFORE re-deriving; if one matches, `capability_capsule_get` it for the verbatim fix and send `mcp__mem__memory_feedback` `useful` after. Ignore if unrelated.".to_string(),
    ];
    push_section(
        &mut lines,
        "**Directives**",
        &dir,
        2,
        false,
        RecallStyle::Snippet,
    );
    push_section(
        &mut lines,
        "**Related incidents**",
        &facts,
        3,
        true,
        RecallStyle::Snippet,
    );
    push_section(
        &mut lines,
        "**Reusable fixes**",
        &pat,
        2,
        false,
        RecallStyle::Snippet,
    );
    let mut env = hook_envelope("PostToolUseFailure", &lines.join("\n"));
    // User-visible headline (the recall itself is model-only additionalContext).
    if env.get("hookSpecificOutput").is_some() {
        let n = dir.len() + facts.len() + pat.len();
        env["systemMessage"] = json!(format!("🧠 mem · {n} incident hit(s) for the last failure"));
    }
    env
}

// ---------------------------------------------------------------------------
// recall-prompt  (UserPromptSubmit)
// ---------------------------------------------------------------------------

async fn recall_prompt(p: &Value, remote: &RemoteArgs) -> Value {
    if env_flag("MEM_RECALL_DISABLED") {
        return skip();
    }
    let Some(query) = prompt_should_recall(p["prompt"].as_str().unwrap_or("")) else {
        return skip();
    };
    // Capsule search FIRST, to completion. It is the primary signal and is
    // fast (~0.6s). Doing it before the transcript search is deliberate:
    // capsule recall is the load-bearing signal, so running it to completion
    // first guarantees its hits survive even if the slower transcript search
    // (currently 5–11s) runs long and hits the timeout — otherwise
    // recall-prompt could return `{}` even though capsule hits existed.
    // Sequential capsule-first keeps the capsule hits regardless of how
    // slow the transcript search is.
    // Scope recall to the current repo/project (derived from the hook payload's
    // `cwd`). This keeps narrowly-scoped guidance from leaking across projects
    // — a `project:NVR-APP` preference no longer surfaces while working in
    // `mem` — and floats in-scope facts/patterns up. Falls back to no scope
    // (global, original behavior) when `cwd` is absent.
    let scope = scope_filters_from_cwd(p);
    let cap = search_capsules(remote, &query, "", 1200, 35, &scope).await;
    // Transcript windows are secondary and the transcript search is currently
    // slow; best-effort with a tight timeout so it never blocks the prompt for
    // long. It returns `{}` (no windows) on timeout — capsule hits still inject.
    let tr = search_transcripts(remote, &query).await;
    format_prompt_recall(&cap, &tr)
}

/// Derive `scope_filters` from a hook payload's `cwd` (the session's working
/// directory). Returns `["project:<base>", "repo:<base>"]` where `<base>` is
/// the cwd's last path segment (the repo dir name), matching how capsules carry
/// `project` / `repo`. Empty when `cwd` is absent/blank → no scoping.
fn scope_filters_from_cwd(p: &Value) -> Vec<String> {
    let base = p["cwd"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if base.is_empty() {
        Vec::new()
    } else {
        vec![format!("project:{base}"), format!("repo:{base}")]
    }
}

/// Gate a user prompt: returns the search query (the prompt, capped) when
/// it is substantive enough to warrant a recall round-trip, else `None`.
/// Skips slash/bang commands, sub-4-char prompts, and bare continuations.
fn prompt_should_recall(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('!') {
        return None;
    }
    if trimmed.chars().count() < 4 {
        return None;
    }
    if is_continuation(&trimmed.to_lowercase()) {
        return None;
    }
    Some(prompt.chars().take(1000).collect())
}

fn is_continuation(lc: &str) -> bool {
    matches!(
        lc,
        "继续"
            | "继续吧"
            | "嗯"
            | "好"
            | "好的"
            | "行"
            | "可以"
            | "go"
            | "ok"
            | "okay"
            | "yes"
            | "y"
            | "yep"
            | "yeah"
            | "sure"
            | "proceed"
            | "continue"
            | "do it"
            | "next"
            | "go on"
    )
}

/// Banner rendering style — progressive disclosure (refs the
/// claude-mem comparison in `docs/oss-memory-diff.md` follow-ups).
///
/// `Index` (default): one headline line per hit (`source_summary`,
/// else the content head) + the `[mem_…]` id; the agent fetches the
/// verbatim body via `capability_capsule_get` only when it actually
/// needs it. Cuts the per-prompt injection cost ~3-4× and makes the
/// feedback "consumed" signal sharper (a deliberate get beats a fuzzy
/// n-gram match). `Snippet` is the legacy 240-char-body shape, kept
/// as a one-env rollback (`MEM_RECALL_STYLE=snippet`).
///
/// COUPLING NOTE: whatever this renders is parsed back by
/// `cli::feedback::scan_transcript`. The round-trip tests in
/// `cli/feedback.rs` feed this renderer's verbatim output into that
/// parser — change the format only together with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecallStyle {
    Index,
    Snippet,
}

/// Parse `MEM_RECALL_STYLE`. Unknown values fall back to the default
/// (`Index`) — a typo in an env var must never kill the hook.
pub(crate) fn parse_recall_style(raw: Option<&str>) -> RecallStyle {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("snippet") => RecallStyle::Snippet,
        _ => RecallStyle::Index,
    }
}

/// Relevance floor for injected transcript windows. Reads
/// `MEM_RECALL_TRANSCRIPT_MIN_SCORE` (default 20); invalid/negative → default.
/// `0` admits everything (disables the floor).
fn transcript_min_score() -> i64 {
    std::env::var("MEM_RECALL_TRANSCRIPT_MIN_SCORE")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&v| v >= 0)
        .unwrap_or(20)
}

fn format_prompt_recall(cap: &Value, tr: &Value) -> Value {
    let style = parse_recall_style(std::env::var("MEM_RECALL_STYLE").ok().as_deref());
    format_prompt_recall_styled(cap, tr, style)
}

pub(crate) fn format_prompt_recall_styled(cap: &Value, tr: &Value, style: RecallStyle) -> Value {
    let dir = section(cap, "directives");
    let facts = section(cap, "relevant_facts");
    let pat = section(cap, "reusable_patterns");
    // Drop low-relevance transcript windows: the transcript search returns its
    // top-N by RRF score, but the tail is often a loose semantic match (noise).
    // A floor keeps only windows worth injecting. Tunable via
    // MEM_RECALL_TRANSCRIPT_MIN_SCORE (default 20); `0` disables.
    let tr_floor = transcript_min_score();
    let windows: Vec<Value> = section(tr, "windows")
        .into_iter()
        .filter(|w| w["score"].as_i64().unwrap_or(0) >= tr_floor)
        .collect();
    if dir.is_empty() && facts.is_empty() && pat.is_empty() && windows.is_empty() {
        return skip();
    }
    let preamble = match style {
        RecallStyle::Index => {
            "🧠 mem auto-recall (index) — hits relevant to this prompt, headlines only. To USE one, `capability_capsule_get` its id FIRST for the verbatim content, then send `mcp__mem__capability_capsule_feedback` for it — silence freezes ranking. Ignore if irrelevant."
        }
        RecallStyle::Snippet => {
            "🧠 mem auto-recall — memories & past conversations relevant to this prompt (auto-retrieved). Read before answering. If a hit is load-bearing, `capability_capsule_get` it for the verbatim content and then send `mcp__mem__memory_feedback` `useful` for that id — silence freezes ranking. Ignore if irrelevant."
        }
    };
    let mut lines = vec![preamble.to_string()];
    // Directives are few, load-bearing instructions — full text in BOTH
    // styles (hiding a directive behind a get defeats its purpose).
    push_section(
        &mut lines,
        "**Directives**",
        &dir,
        3,
        false,
        RecallStyle::Snippet,
    );
    push_section(&mut lines, "**Relevant facts**", &facts, 3, true, style);
    push_section(&mut lines, "**Reusable patterns**", &pat, 2, false, style);
    if !windows.is_empty() {
        lines.push(String::new());
        lines.push("**Past conversations** (`transcripts_search` for full threads)".to_string());
        let window_cap = match style {
            RecallStyle::Index => 120,
            RecallStyle::Snippet => 240,
        };
        for w in windows.iter().take(2) {
            lines.push(format_window(w, window_cap));
        }
    }
    let mut env = hook_envelope("UserPromptSubmit", &lines.join("\n"));
    // Surface a one-line, user-VISIBLE headline. The recall banner above is
    // emitted as `additionalContext` — Claude Code injects it into the model's
    // prompt but never renders it to the user, so without this the user never
    // sees that recall fired. `systemMessage` IS rendered to the user. The
    // `additionalContext` payload is unchanged (its format is parsed back by
    // `cli/feedback.rs::scan_transcript` — do not touch it).
    if env.get("hookSpecificOutput").is_some() {
        env["systemMessage"] = json!(recall_headline(
            dir.len(),
            facts.len(),
            pat.len(),
            windows.len()
        ));
    }
    env
}

/// One-line, user-visible recall headline (emitted as `systemMessage`).
/// Summarizes how many hits auto-recall surfaced this turn, by section.
fn recall_headline(directives: usize, facts: usize, patterns: usize, windows: usize) -> String {
    let total = directives + facts + patterns + windows;
    // Spell the section counts out in full words (regular +s plural, singular
    // when n == 1) instead of the d/f/p/w abbreviations — the headline is the
    // only user-visible signal, so it should be self-explanatory. `windows` are
    // the "Past conversations" transcript hits.
    fn seg(n: usize, label: &str) -> String {
        if n == 1 {
            format!("{n} {label}")
        } else {
            format!("{n} {label}s")
        }
    }
    format!(
        "🧠 mem · recalled {total} ({}, {}, {}, {})",
        seg(directives, "directive"),
        seg(facts, "fact"),
        seg(patterns, "pattern"),
        seg(windows, "conversation"),
    )
}

/// One transcript window → one bullet: `- [sid8] yyyy-mm-dd: <primary block text>`.
fn format_window(w: &Value, content_cap: usize) -> String {
    let sid: String = w["session_id"]
        .as_str()
        .unwrap_or("?")
        .chars()
        .take(8)
        .collect();
    let blocks = w["blocks"].as_array().cloned().unwrap_or_default();
    let primary = blocks
        .iter()
        .find(|b| b["is_primary"].as_bool() == Some(true))
        .or_else(|| blocks.first())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let date: String = primary["created_at"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(10)
        .collect();
    let content = clean_to(primary["content"].as_str().unwrap_or(""), content_cap);
    format!("- [{sid}] {date}: {content}")
}

// ---------------------------------------------------------------------------
// commit-nudge  (PostToolUse on Bash)
// ---------------------------------------------------------------------------

fn commit_nudge(p: &Value) -> Value {
    // Claude Code's shell tool is `Bash`; Codex's is `exec_command`. Both
    // send Claude-compatible PostToolUse field names (tool_name / tool_input
    // / tool_response), so the only per-runtime differences are the tool
    // name and the command key (Codex nests it under `cmd`, Claude `command`).
    if !matches!(p["tool_name"].as_str(), Some("Bash" | "exec_command")) {
        return skip();
    }
    let command = p["tool_input"]["command"]
        .as_str()
        .or_else(|| p["tool_input"]["cmd"].as_str())
        .unwrap_or("");
    if !contains_git_commit(command) || command.contains("--amend") {
        return skip();
    }
    // Neither runtime populates a boolean success flag for shell tools, so
    // use git's `[branch sha] subject` stdout envelope as the "commit
    // landed" signal. Codex may deliver the exec output under a different
    // key (or as a bare string), so probe stdout / output / a plain string.
    let stdout = p["tool_response"]["stdout"]
        .as_str()
        .or_else(|| p["tool_response"]["output"].as_str())
        .or_else(|| p["tool_response"].as_str())
        .unwrap_or("");
    let Some(subject) = commit_subject(stdout) else {
        return skip();
    };
    if is_routine_commit(&subject) {
        return skip();
    }
    let mut env = hook_envelope("PostToolUse", &commit_nudge_text(&subject));
    // User-visible headline (the nudge itself is model-only additionalContext).
    if env.get("hookSpecificOutput").is_some() {
        env["systemMessage"] = json!(format!(
            "💡 mem · committed `{}` — consider propose_experience",
            clean_to(&subject, 50)
        ));
    }
    env
}

fn contains_git_commit(cmd: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(^|[\s&;|`(])git\s+commit($|\s)").expect("git-commit regex"))
        .is_match(cmd)
}

/// Pull the subject off git's `[branch sha] subject` success line. `None`
/// if stdout's first line isn't that envelope (i.e. the commit didn't land).
fn commit_subject(stdout: &str) -> Option<String> {
    let first = stdout.lines().next().unwrap_or("");
    if !first.starts_with('[') {
        return None;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let subject = RE
        .get_or_init(|| Regex::new(r"^\[[^\]]+\]\s*").expect("commit-envelope regex"))
        .replace(first, "")
        .to_string();
    if subject.trim().is_empty() {
        None
    } else {
        Some(subject)
    }
}

fn is_routine_commit(subject: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(chore\(deps\)|chore\(makefile\)|chore\(logging\)|docs(\(|:)|test(\(|:)|style(\(|:))")
            .expect("routine-commit regex")
    })
    .is_match(subject)
}

fn commit_nudge_text(subject: &str) -> String {
    format!(
        "Commit just landed: `{subject}`. **Default action: call `mcp__mem__capability_capsule_ingest` with `capability_capsule_type=\"experience\"` and `write_mode=\"propose\"` now.** This writes a capsule row with status=PendingConfirmation — it sits in the review queue (visible via `capability_capsule_list_pending_review`), NOT the active pool, so a human or future agent must run `review_accept` (or `review_edit_accept` for edits) to promote, and over-proposing is harmless (one `review_reject` click discards a noise row). The threshold is low: any commit that touches business logic, non-trivial config, a bug fix, an architectural decision, or a learned API gotcha is worth proposing. Required args: capability_capsule_type=\"experience\", content (full cause/symptom/fix verbatim — never refine), scope (e.g. \"repo\" or \"project\"), write_mode=\"propose\". Optional but useful: summary (≤80 char headline), project (repo basename), source_agent (\"claude-code\"). Skip only for: typo-only commits, dependency bumps, pure formatting / rename-only refactors, or commits whose entire content was already captured by an earlier capsule in this session. When in doubt → propose."
    )
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn read_stdin() -> String {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| v == "1").unwrap_or(false)
}

fn skip() -> Value {
    json!({})
}

/// Build a hook-output envelope, or `{}` when `ctx` is effectively empty.
fn hook_envelope(event: &str, ctx: &str) -> Value {
    if ctx.trim().is_empty() {
        return skip();
    }
    json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": ctx } })
}

/// Whitespace-collapse + truncate to 240 chars with an ellipsis.
fn clean(s: &str) -> String {
    clean_to(s, 240)
}

/// Whitespace-collapse + char-cap with ellipsis. `max` is the style-
/// dependent budget (240 snippet body / 80 index headline / 120 index
/// transcript window).
fn clean_to(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        let head: String = collapsed.chars().take(max).collect();
        format!("{head}…")
    } else {
        collapsed
    }
}

fn section(v: &Value, key: &str) -> Vec<Value> {
    v[key].as_array().cloned().unwrap_or_default()
}

/// Append `**Header**` + up to `take` bullet items to `lines`. Snippet
/// style renders `- <text 240>[ (code_refs)]  \`[id]\``; index style
/// renders `- <source_summary|text head 80>  \`[id]\`` (no refs — the
/// get brings them). The trailing `` `[id]` `` token is the contract
/// `cli::feedback::extract_injected_ids` parses; keep it in BOTH arms.
fn push_section(
    lines: &mut Vec<String>,
    header: &str,
    items: &[Value],
    take: usize,
    with_refs: bool,
    style: RecallStyle,
) {
    if items.is_empty() {
        return;
    }
    lines.push(String::new());
    lines.push(header.to_string());
    for it in items.iter().take(take) {
        let id = it["capability_capsule_id"].as_str().unwrap_or("");
        match style {
            RecallStyle::Index => {
                let headline = it["source_summary"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| it["text"].as_str().unwrap_or(""));
                let headline = clean_to(headline, 80);
                lines.push(format!("- {headline}  `[{id}]`"));
            }
            RecallStyle::Snippet => {
                let text = clean(it["text"].as_str().unwrap_or(""));
                let refs = if with_refs {
                    let joined = it["code_refs"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    if joined.is_empty() {
                        String::new()
                    } else {
                        format!(" ({joined})")
                    }
                } else {
                    String::new()
                };
                lines.push(format!("- {text}{refs}  `[{id}]`"));
            }
        }
    }
}

/// Best-effort per-session dedup against the last error signature. Returns
/// true when this signature equals the previous one for the session (so the
/// caller should skip). State lives in `/tmp`; all IO failures are ignored.
fn dedup_seen(session_id: &str, sig: &str) -> bool {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sig.hash(&mut h);
    let hash = h.finish().to_string();
    let path = if session_id.is_empty() {
        "/tmp/mem-error-recall-last".to_string()
    } else {
        format!("/tmp/mem-error-recall-last_{session_id}")
    };
    if std::fs::read_to_string(&path).unwrap_or_default() == hash {
        return true;
    }
    let _ = std::fs::write(&path, &hash);
    false
}

async fn post_json(url: String, body: Value, timeout_ms: u64) -> Value {
    match Client::new()
        .post(&url)
        .json(&body)
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json::<Value>().await.unwrap_or_else(|_| json!({})),
        _ => json!({}),
    }
}

async fn search_capsules(
    remote: &RemoteArgs,
    query: &str,
    intent: &str,
    token_budget: u32,
    min_score: i64,
    scope_filters: &[String],
) -> Value {
    let body = json!({
        "query": query,
        "intent": intent,
        "scope_filters": scope_filters,
        "token_budget": token_budget,
        "caller_agent": "claude-code",
        "expand_graph": false,
        "tenant": remote.tenant,
        "min_score": min_score,
    });
    post_json(
        format!("{}/capability_capsules/search", remote.base_url),
        body,
        3000,
    )
    .await
}

async fn search_transcripts(remote: &RemoteArgs, query: &str) -> Value {
    let q: String = query.chars().take(1000).collect();
    let body = json!({ "query": q, "tenant": remote.tenant, "limit": 3, "context_window": 1 });
    // Tight timeout: the transcript search is currently slow (5–11s) and its
    // windows are a secondary signal — never block the prompt waiting on it.
    post_json(
        format!("{}/transcripts/search", remote.base_url),
        body,
        1500,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- error_signature -------------------------------------------------

    #[test]
    fn error_signature_fires_on_real_failures() {
        assert!(error_signature("Exit code 1\ncat: /x: No such file or directory").is_some());
        assert!(error_signature(
            "Exit code 1\nerror[E0277]: the trait bound is not satisfied\nerror: could not compile"
        )
        .is_some());
        assert!(
            error_signature("Exit code 1\nE   AssertionError: assert 1 == 2\n1 failed").is_some()
        );
        assert!(error_signature("Exit code 13\npermission denied while connecting").is_some());
    }

    #[test]
    fn error_signature_skips_benign_non_zero_exits() {
        // grep no-match: only the exit-code envelope, no output.
        assert_eq!(error_signature("Exit code 1"), None);
        assert_eq!(error_signature("Exit code 1\n"), None);
        // diff found-differences: output present but not error-shaped.
        assert_eq!(error_signature("Exit code 1\n3c3\n< foo\n---\n> bar"), None);
        // test/[ ] false: no output.
        assert_eq!(error_signature(""), None);
        // too-short residual.
        assert_eq!(error_signature("Exit code 1\nno"), None);
    }

    #[test]
    fn error_signature_strips_exit_code_envelope() {
        let sig = error_signature("Exit code 4\ncat: /x: No such file or directory").unwrap();
        assert!(!sig.to_lowercase().contains("exit code"));
        assert!(sig.contains("No such file"));
    }

    // ---- prompt_should_recall -------------------------------------------

    #[test]
    fn prompt_gate_skips_noise() {
        assert_eq!(prompt_should_recall(""), None);
        assert_eq!(prompt_should_recall("   "), None);
        assert_eq!(prompt_should_recall("/status"), None);
        assert_eq!(prompt_should_recall("!ls -la"), None);
        assert_eq!(prompt_should_recall("abc"), None); // < 4 chars
        assert_eq!(prompt_should_recall("ok"), None);
        assert_eq!(prompt_should_recall("继续"), None);
        assert_eq!(prompt_should_recall("继续吧"), None);
        assert_eq!(prompt_should_recall("Proceed"), None); // case-insensitive
    }

    #[test]
    fn prompt_gate_accepts_substantive_and_caps_query() {
        assert_eq!(
            prompt_should_recall("how does the embedding worker batch jobs?").as_deref(),
            Some("how does the embedding worker batch jobs?")
        );
        let long = "x".repeat(5000);
        let q = prompt_should_recall(&long).unwrap();
        assert_eq!(q.chars().count(), 1000);
    }

    // ---- commit gating ---------------------------------------------------

    #[test]
    fn commit_subject_extracts_from_envelope() {
        assert_eq!(
            commit_subject("[master a1b2c3d] fix(storage): tolerate UInt64\n 1 file changed")
                .as_deref(),
            Some("fix(storage): tolerate UInt64")
        );
        assert_eq!(commit_subject("nothing landed here"), None);
        assert_eq!(commit_subject(""), None);
    }

    #[test]
    fn routine_commits_are_skipped() {
        assert!(is_routine_commit("docs: update readme"));
        assert!(is_routine_commit("docs(hooks): note binding"));
        assert!(is_routine_commit("chore(deps): bump serde"));
        assert!(is_routine_commit("test(api): add case"));
        assert!(is_routine_commit("style: rustfmt"));
        assert!(!is_routine_commit("fix(storage): real bug"));
        assert!(!is_routine_commit("feat(hook): new thing"));
    }

    #[test]
    fn contains_git_commit_matches_real_invocations() {
        assert!(contains_git_commit("git commit -m 'x'"));
        assert!(contains_git_commit("cd /x && git commit -am y"));
        assert!(!contains_git_commit("git status"));
        assert!(!contains_git_commit("git committer-config"));
    }

    #[test]
    fn commit_nudge_fires_on_claude_bash_commit() {
        // Regression: the Claude Code PostToolUse(Bash) path is unchanged.
        let p = json!({
            "tool_name": "Bash",
            "tool_input": {"command": "git commit -m \"add feature\""},
            "tool_response": {"stdout": "[master abc1234] add feature\n 1 file changed"},
        });
        let out = commit_nudge(&p);
        assert!(
            out.get("systemMessage").is_some(),
            "claude bash commit should nudge, got {out}"
        );
    }

    #[test]
    fn commit_nudge_fires_on_codex_exec_command_commit() {
        // Codex's shell tool is `exec_command`; the command lives in
        // tool_input.cmd (not .command). PostToolUse field names are
        // otherwise Claude-compatible.
        let p = json!({
            "tool_name": "exec_command",
            "tool_input": {"cmd": "git commit -m \"add feature\"", "workdir": "/repo"},
            "tool_response": {"output": "[master abc1234] add feature\n 1 file changed"},
        });
        let out = commit_nudge(&p);
        assert!(
            out.get("systemMessage").is_some(),
            "codex exec_command commit should nudge, got {out}"
        );
    }

    #[test]
    fn commit_nudge_skips_non_shell_tool() {
        let p = json!({"tool_name": "Read", "tool_input": {"file_path": "x"}});
        assert_eq!(commit_nudge(&p), json!({}));
    }

    #[test]
    fn failure_error_text_probes_both_runtimes() {
        // Claude Code: top-level `.error`.
        assert_eq!(
            failure_error_text(&json!({"error": "Exit code 1\nboom"})),
            "Exit code 1\nboom"
        );
        // Codex: mirrored field names, error under tool_response.
        assert_eq!(
            failure_error_text(&json!({"tool_response": {"stderr": "seg fault"}})),
            "seg fault"
        );
        assert_eq!(
            failure_error_text(&json!({"tool_response": {"output": "oops"}})),
            "oops"
        );
        assert_eq!(
            failure_error_text(&json!({"tool_response": "bare error string"})),
            "bare error string"
        );
        // Nothing usable → empty (→ error_signature returns None → skip).
        assert_eq!(failure_error_text(&json!({})), "");
    }

    // ---- envelopes -------------------------------------------------------

    #[test]
    fn hook_envelope_empty_is_skip() {
        assert_eq!(hook_envelope("PostToolUse", ""), json!({}));
        assert_eq!(hook_envelope("PostToolUse", "   \n "), json!({}));
    }

    #[test]
    fn hook_envelope_wraps_event_and_context() {
        let v = hook_envelope("PostToolUseFailure", "hello");
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("PostToolUseFailure")
        );
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"].as_str(),
            Some("hello")
        );
    }

    #[test]
    fn format_error_recall_empty_resp_is_skip() {
        assert_eq!(format_error_recall(&json!({})), json!({}));
        assert_eq!(
            format_error_recall(
                &json!({"directives":[],"relevant_facts":[],"reusable_patterns":[]})
            ),
            json!({})
        );
    }

    #[test]
    fn format_error_recall_renders_hits() {
        let resp = json!({
            "relevant_facts": [
                {"text": "the fix is X", "capability_capsule_id": "mem_1", "code_refs": ["src/a.rs"]}
            ]
        });
        let v = format_error_recall(&resp);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("Related incidents"));
        assert!(ctx.contains("the fix is X"));
        assert!(ctx.contains("(src/a.rs)"));
        assert!(ctx.contains("`[mem_1]`"));
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("PostToolUseFailure")
        );
        // User-visible headline present.
        let sys = v["systemMessage"].as_str().unwrap();
        assert!(
            sys.starts_with("🧠 mem ·") && sys.contains("incident"),
            "got {sys}"
        );
    }

    #[test]
    fn format_prompt_recall_injects_capsule_facts_without_transcript() {
        // The regression this guards: a slow/empty transcript search must NOT
        // suppress capsule recall. cap has facts, tr is empty → still inject.
        let cap = json!({
            "relevant_facts": [
                {"text": "EMBEDDING_BATCH_SIZE default flipped to 8", "capability_capsule_id": "mem_x"}
            ]
        });
        let tr = json!({});
        let v = format_prompt_recall(&cap, &tr);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("Relevant facts"));
        assert!(ctx.contains("EMBEDDING_BATCH_SIZE"));
        assert!(!ctx.contains("Past conversations"));
    }

    #[test]
    fn format_prompt_recall_includes_transcript_window() {
        let cap = json!({});
        let tr = json!({
            "windows": [
                {"session_id": "abcdef12-3456", "score": 30, "blocks": [
                    {"is_primary": true, "created_at": "2026-06-04T01:00:00Z", "content": "we fixed the hook"}
                ]}
            ]
        });
        let v = format_prompt_recall(&cap, &tr);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("Past conversations"));
        assert!(ctx.contains("[abcdef12]"));
        assert!(ctx.contains("2026-06-04"));
        assert!(ctx.contains("we fixed the hook"));
    }

    #[test]
    fn transcript_floor_drops_low_score_windows() {
        // Below the default floor (20) → dropped; above → kept. Drops the loose
        // semantic-match noise that the user observed in the recall banner.
        let tr = json!({
            "windows": [
                {"session_id": "good1234-5678", "score": 30, "blocks": [
                    {"is_primary": true, "created_at": "2026-06-04T01:00:00Z", "content": "relevant hit"}]},
                {"session_id": "noise999-0000", "score": 8, "blocks": [
                    {"is_primary": true, "created_at": "2026-05-31T01:00:00Z", "content": "loose semantic noise"}]}
            ]
        });
        let v = format_prompt_recall(&json!({}), &tr);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("[good1234]"), "high-score window kept");
        assert!(!ctx.contains("[noise999]"), "low-score window dropped");
        // Headline counts only the surviving window.
        assert!(
            v["systemMessage"]
                .as_str()
                .unwrap()
                .contains("1 conversation"),
            "got {}",
            v["systemMessage"]
        );
    }

    #[test]
    fn recall_headline_counts_by_section() {
        assert_eq!(
            recall_headline(1, 3, 2, 2),
            "🧠 mem · recalled 8 (1 directive, 3 facts, 2 patterns, 2 conversations)"
        );
        assert_eq!(
            recall_headline(0, 1, 0, 0),
            "🧠 mem · recalled 1 (0 directives, 1 fact, 0 patterns, 0 conversations)"
        );
    }

    #[test]
    fn format_prompt_recall_emits_visible_system_message() {
        // Recall must surface a user-visible `systemMessage` headline (the
        // banner itself is model-only `additionalContext`).
        let cap = json!({
            "relevant_facts": [
                {"text": "fact one", "capability_capsule_id": "mem_a"},
                {"text": "fact two", "capability_capsule_id": "mem_b"}
            ]
        });
        let v = format_prompt_recall(&cap, &json!({}));
        let sys = v["systemMessage"].as_str().unwrap();
        assert!(sys.starts_with("🧠 mem · recalled"), "got {sys}");
        assert!(sys.contains("2 facts"), "two facts → 2 facts; got {sys}");
        // additionalContext is still present + unchanged in shape.
        assert!(v["hookSpecificOutput"]["additionalContext"].is_string());
    }

    #[test]
    fn format_prompt_recall_skip_has_no_system_message() {
        // Nothing recalled → `{}` skip envelope, no headline.
        let v = format_prompt_recall(&json!({}), &json!({}));
        assert!(v.get("systemMessage").is_none());
        assert!(v.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn clean_collapses_and_truncates() {
        assert_eq!(clean("a\n  b   c"), "a b c");
        let long = "x ".repeat(300);
        let c = clean(&long);
        assert_eq!(c.chars().count(), 241); // 240 + ellipsis
        assert!(c.ends_with('…'));
    }

    // ---- progressive disclosure (index style) -----------------------------

    fn fact_with_long_body() -> Value {
        json!({
            "relevant_facts": [{
                "text": format!("DEEPBODY{} tail-marker-deep-in-body", "正文很长".repeat(60)),
                "source_summary": "E1.5 泛化共享信号改 topics∪tags 的一行摘要",
                "capability_capsule_id": "mem_01900000-0000-7000-8000-000000000abc",
                "code_refs": ["src/worker/evolution_worker.rs"]
            }]
        })
    }

    #[test]
    fn index_style_renders_summary_headline_not_body() {
        let v = format_prompt_recall_styled(&fact_with_long_body(), &json!({}), RecallStyle::Index);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            ctx.contains("E1.5 泛化共享信号改"),
            "headline from source_summary"
        );
        assert!(ctx.contains("`[mem_01900000-0000-7000-8000-000000000abc]`"));
        assert!(
            !ctx.contains("tail-marker-deep-in-body"),
            "index mode must not inject the capsule body"
        );
        assert!(
            !ctx.contains("(src/worker/evolution_worker.rs)"),
            "index mode drops code_refs — get brings them"
        );
        // The instruction line tells the agent to get-before-use.
        assert!(ctx.contains("capability_capsule_get"));
    }

    #[test]
    fn index_style_falls_back_to_text_head_when_no_summary() {
        let cap = json!({
            "relevant_facts": [{
                "text": format!("HEADMARK 前八十字内的内容{} tail-marker-deep-in-body", "填充".repeat(80)),
                "capability_capsule_id": "mem_01900000-0000-7000-8000-000000000abc"
            }]
        });
        let v = format_prompt_recall_styled(&cap, &json!({}), RecallStyle::Index);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("HEADMARK"), "falls back to the text head");
        assert!(!ctx.contains("tail-marker-deep-in-body"));
    }

    #[test]
    fn index_style_keeps_directives_full() {
        let directive_text = format!(
            "MUST 指令全文必须完整保留{}END_OF_DIRECTIVE",
            "规则".repeat(50)
        );
        let cap = json!({
            "directives": [{"text": directive_text, "capability_capsule_id": "mem_01900000-0000-7000-8000-000000000abc"}]
        });
        let v = format_prompt_recall_styled(&cap, &json!({}), RecallStyle::Index);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            ctx.contains("END_OF_DIRECTIVE"),
            "directives are few and load-bearing — never index-truncated"
        );
    }

    #[test]
    fn index_style_caps_transcript_windows() {
        let tr = json!({
            "windows": [{"session_id": "abcdef12-3456", "score": 30, "blocks": [
                {"is_primary": true, "created_at": "2026-06-04T01:00:00Z",
                 "content": format!("WINHEAD {} WINTAIL", "对话".repeat(120))}
            ]}]
        });
        let v = format_prompt_recall_styled(&json!({}), &tr, RecallStyle::Index);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("WINHEAD"));
        assert!(
            !ctx.contains("WINTAIL"),
            "window content capped in index mode"
        );
    }

    #[test]
    fn snippet_style_preserves_legacy_shape() {
        let v =
            format_prompt_recall_styled(&fact_with_long_body(), &json!({}), RecallStyle::Snippet);
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            ctx.contains("DEEPBODY"),
            "snippet mode keeps the 240-char body"
        );
        assert!(
            ctx.contains("(src/worker/evolution_worker.rs)"),
            "snippet mode keeps code_refs"
        );
    }

    #[test]
    fn recall_style_parses_with_index_default() {
        assert_eq!(parse_recall_style(None), RecallStyle::Index);
        assert_eq!(parse_recall_style(Some("index")), RecallStyle::Index);
        assert_eq!(parse_recall_style(Some("snippet")), RecallStyle::Snippet);
        assert_eq!(parse_recall_style(Some("SNIPPET")), RecallStyle::Snippet);
        // Unknown values fall back to the default, never crash the hook.
        assert_eq!(parse_recall_style(Some("garbage")), RecallStyle::Index);
    }
}
