//! H4 — skill crystallization lane (`mem crystallize`).
//!
//! Turns an `evolution:workflow` review placeholder (minted by
//! `evolution_worker::execute_generalize` when `is_procedural_sibling_cluster`
//! fires) into a real, reusable Workflow capsule.
//!
//! Design: `docs/superpowers/specs/2026-08-01-h4-skill-crystallization-design.md`.
//!
//! **Why a one-shot CLI and not the worker.** `docs/evolution-worker.md` §5
//! principle 5 keeps the resident process LLM-free in every phase; §6.2's
//! `local`/`api` synthesis backends stay deliberately unimplemented
//! (`config.rs` rejects them at parse time). Crystallization needs generation,
//! so it lives here — outside the sweep, triggered only by an explicit
//! invocation.
//!
//! **Fail-safe by construction**, three independent guards (the O7(c)
//! `llm_extract` posture):
//!   1. Not running the subcommand = zero behavior. There is no timer, no
//!      worker, no default execution path.
//!   2. [`LlmExtractConfig::from_env`] returns `None` when the gateway is
//!      unconfigured (`LLM_API_BASE` / `LLM_MODEL` unset) → the run reports
//!      "unconfigured" and exits without touching anything.
//!   3. A synthesis error is swallowed per candidate — the placeholder stays
//!      exactly as it was, in the review queue. A dead gateway never corrupts
//!      state.
//!
//! **Never writes without `--accept`.** The default is dry-run: generate,
//! print, exit. `docs/evolution-worker.md` §6.2 requires generated content to
//! pass a human before it becomes authoritative; the explicit flag is that
//! gate (same posture as `POST /reviews/evolution {dry_run}` and friends).
//!
//! **Pure HTTP client.** `mem serve` is the single writer for the Lance
//! datasets; a CLI opening its own store would fight it (evolution E2/E3
//! lesson). Every read and write here goes through the existing HTTP surface —
//! this module adds no endpoints.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Args;
use serde::Deserialize;
use tracing::warn;

use super::common::RemoteArgs;
use super::llm_extract::LlmExtractConfig;
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, EditPendingRequest,
};

/// Tag the evolution worker stamps on H4 placeholders, and which the
/// crystallized successor inherits so later runs can find it.
pub const WORKFLOW_TAG: &str = "evolution:workflow";

/// Overlap floor for "this candidate continues an already-crystallized
/// workflow" (spec §4.3). Metric is `|A ∩ B| / min(|A|, |B|)` — the same
/// `overlap/min` shape `is_procedural_sibling_cluster` uses for its anchor
/// disjointness test, kept identical for cross-module consistency.
///
/// `min` rather than Jaccard because the growth shape is "old 5 ⊂ new 12":
/// Jaccard would read 5/12 ≈ 0.42 and wrongly mint a second parallel
/// workflow, while `min` reads 5/5 = 1.0 and correctly supersedes.
pub const SUPERSEDE_OVERLAP_FLOOR: f64 = 0.5;

/// Cap on how many source capsules feed one prompt — a runaway cluster
/// must not blow the gateway's context window.
const MAX_SOURCES: usize = 12;
/// Cap on per-source verbatim content fed to the model.
const MAX_SOURCE_CHARS: usize = 4000;
/// Per-call timeout. Crystallization reads several long capsules, so it gets
/// a longer budget than the O7(c) per-block extract.
const TIMEOUT_SECS: u64 = 180;

const SYS_PROMPT: &str = "You distill a reusable procedure from several records of the SAME \
recurring task being executed multiple times (each record cites different commits/files — they are \
siblings, not duplicates). Output ONLY a JSON object: \
{\"title\": \"<one line naming the procedure>\", \"steps\": [\"<step>\", \"<step>\", ...]}. \
Each step must be one imperative line a future session can follow — concrete, ordered, and free of \
run-specific details (no commit shas, no one-off file names, no dates). Merge what the executions \
share; drop what was incidental to a single run. Aim for 3-10 steps. Write the steps in the same \
language the source records are written in. No prose, no markdown, no code fences — just the JSON \
object.";

#[derive(Debug, Args)]
pub struct CrystallizeArgs {
    #[command(flatten)]
    pub remote: RemoteArgs,

    /// Crystallize only this placeholder capsule id. Default: every pending
    /// `evolution:workflow` placeholder for the tenant.
    #[arg(long)]
    pub candidate: Option<String>,

    /// Actually write the crystallized workflow back (mints the successor via
    /// `POST /reviews/pending/edit_accept`). Without this flag the run is a
    /// dry run: it generates and prints, but writes nothing.
    #[arg(long)]
    pub accept: bool,
}

/// One crystallized (or would-be-crystallized) workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crystallized {
    /// The placeholder this came from.
    pub placeholder_id: String,
    /// One-line procedure name → the successor capsule's `summary`.
    pub title: String,
    /// Ordered steps → the successor capsule's `content`, one per line.
    pub steps: Vec<String>,
    /// Source capsule ids, carried through as `evidence`.
    pub evidence: Vec<String>,
    /// Union of the sources' `code_refs`.
    pub code_refs: Vec<String>,
    /// Set when this continues an already-crystallized workflow (spec §4.3).
    pub supersedes: Option<String>,
    /// Successor capsule id — only present after a real (`--accept`) write.
    pub successor_id: Option<String>,
}

impl Crystallized {
    /// Capsule `content`: one step per line.
    ///
    /// Load-bearing format. `compress.rs::split_steps` splits a Workflow
    /// capsule's content **by line**, so one-step-per-line is what renders as
    /// a clean `suggested_workflow`. Provenance deliberately does NOT go here
    /// — it lives in `evidence`, which `workflow_success_signals` renders.
    /// (Putting "distilled from 5 runs: <shas>" in the content would surface
    /// as a bogus extra step — the exact defect the placeholder had.)
    pub fn content(&self) -> String {
        self.steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Default)]
pub struct CrystallizeReport {
    pub scanned: usize,
    pub crystallized: Vec<Crystallized>,
    /// `(placeholder_id, reason)` for candidates that produced nothing.
    pub skipped: Vec<(String, String)>,
    /// True when the gateway was unconfigured (guard #2) — the run did
    /// nothing at all.
    pub unconfigured: bool,
}

// ---------------------------------------------------------------------------
// Seams. Both are traits so tests can drive the whole orchestration with
// fakes — no network, no `mem serve`, no gateway.
// ---------------------------------------------------------------------------

/// The subset of the review HTTP surface crystallization needs.
#[async_trait]
pub trait ReviewClient: Send + Sync {
    async fn list_pending(&self, tenant: &str) -> Result<Vec<CapabilityCapsuleRecord>>;
    async fn get_capsule(&self, tenant: &str, id: &str) -> Result<CapabilityCapsuleRecord>;
    /// Already-crystallized workflows (Active, Workflow-typed) for the
    /// supersede check.
    async fn list_active_workflows(&self, tenant: &str) -> Result<Vec<CapabilityCapsuleRecord>>;
    /// Returns the successor capsule id.
    async fn edit_accept(&self, tenant: &str, req: EditPendingRequest) -> Result<String>;
    /// Record `new --supersedes--> old` in the graph (spec §4.3 deviation —
    /// see [`crystallize`]).
    async fn link_supersedes(&self, tenant: &str, new_id: &str, old_id: &str) -> Result<()>;
}

/// Generation seam. The real implementation talks to the `llm_entry` gateway.
#[async_trait]
pub trait WorkflowSynthesizer: Send + Sync {
    async fn synthesize(&self, prompt: &str) -> Result<String, String>;
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested below; no I/O).
// ---------------------------------------------------------------------------

/// Is this record an H4 review placeholder awaiting crystallization?
pub fn is_workflow_placeholder(c: &CapabilityCapsuleRecord) -> bool {
    matches!(c.status, CapabilityCapsuleStatus::PendingConfirmation)
        && matches!(c.capability_capsule_type, CapabilityCapsuleType::Workflow)
        && c.tags.iter().any(|t| t == WORKFLOW_TAG)
}

/// `|A ∩ B| / min(|A|, |B|)`. 0.0 when either side is empty.
pub fn overlap_ratio(a: &[String], b: &[String]) -> f64 {
    let sa: BTreeSet<&str> = a.iter().map(String::as_str).collect();
    let sb: BTreeSet<&str> = b.iter().map(String::as_str).collect();
    let floor = sa.len().min(sb.len());
    if floor == 0 {
        return 0.0;
    }
    sa.intersection(&sb).count() as f64 / floor as f64
}

/// Pick the already-crystallized workflow this candidate continues, if any
/// (spec §4.3). Highest overlap wins; ties break on the later `created_at`
/// so we chain onto the most recent version rather than an ancestor.
pub fn find_supersede_target<'a>(
    existing: &'a [CapabilityCapsuleRecord],
    members: &[String],
) -> Option<&'a CapabilityCapsuleRecord> {
    existing
        .iter()
        .filter(|c| c.tags.iter().any(|t| t == WORKFLOW_TAG))
        .filter_map(|c| {
            let r = overlap_ratio(&c.evidence, members);
            (r >= SUPERSEDE_OVERLAP_FLOOR).then_some((c, r))
        })
        .max_by(|(ca, ra), (cb, rb)| {
            ra.partial_cmp(rb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| ca.created_at.cmp(&cb.created_at))
        })
        .map(|(c, _)| c)
}

/// Assemble the gateway prompt from the sources' **verbatim content**.
///
/// The placeholder itself only carries `id — summary` (it honours the verbatim
/// rule by not copying source bodies). Summaries are 80-char index hints —
/// far too thin to distil a procedure from — so crystallization fetches the
/// full text here. That is a *read*: the verbatim rule constrains storage, and
/// nothing fetched here is ever written back.
pub fn build_prompt(sources: &[CapabilityCapsuleRecord], shared_topics: &[String]) -> String {
    let mut p = String::new();
    if !shared_topics.is_empty() {
        p.push_str(&format!("Shared topics: {}\n\n", shared_topics.join(", ")));
    }
    p.push_str(&format!(
        "Below are {} executions of the same procedure.\n\n",
        sources.len().min(MAX_SOURCES)
    ));
    for (i, s) in sources.iter().take(MAX_SOURCES).enumerate() {
        let body: String = s.content.chars().take(MAX_SOURCE_CHARS).collect();
        p.push_str(&format!("--- execution {} ---\n{}\n\n", i + 1, body));
    }
    p
}

#[derive(Deserialize)]
struct SynthReply {
    title: String,
    steps: Vec<String>,
}

/// Parse the model's reply into a title + steps. Lenient like
/// `llm_extract::parse_candidates`: strips fences and slices the first
/// `{ ... }` object so surrounding prose can't break it. `None` on any
/// failure — the caller then leaves the placeholder untouched.
pub fn parse_reply(content: &str) -> Option<(String, Vec<String>)> {
    let trimmed = content
        .trim()
        .trim_start_matches("```json")
        .trim_matches('`')
        .trim();
    let (a, b) = (trimmed.find('{')?, trimmed.rfind('}')?);
    if b <= a {
        return None;
    }
    let parsed: SynthReply = serde_json::from_str(&trimmed[a..=b]).ok()?;
    let title = parsed.title.trim().to_string();
    let steps: Vec<String> = parsed
        .steps
        .iter()
        .map(|s| s.trim().trim_start_matches('-').trim().to_string())
        .filter(|s| !s.is_empty())
        // A step must not carry embedded newlines: `split_steps` would
        // explode it into several steps downstream.
        .map(|s| s.replace(['\n', '\r'], " "))
        .collect();
    if title.is_empty() || steps.is_empty() {
        return None;
    }
    Some((title, steps))
}

/// Union of the sources' `code_refs`, deduped and ordered.
fn union_code_refs(sources: &[CapabilityCapsuleRecord]) -> Vec<String> {
    let set: BTreeSet<String> = sources.iter().flat_map(|s| s.code_refs.clone()).collect();
    set.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Orchestration (generic over the two seams → fully testable).
// ---------------------------------------------------------------------------

pub struct CrystallizeOptions {
    pub tenant: String,
    pub candidate: Option<String>,
    pub accept: bool,
}

pub async fn crystallize(
    client: &dyn ReviewClient,
    synth: &dyn WorkflowSynthesizer,
    opts: &CrystallizeOptions,
) -> Result<CrystallizeReport> {
    let mut report = CrystallizeReport::default();

    let pending = client.list_pending(&opts.tenant).await?;
    let placeholders: Vec<&CapabilityCapsuleRecord> = pending
        .iter()
        .filter(|c| is_workflow_placeholder(c))
        .filter(|c| match &opts.candidate {
            Some(id) => &c.capability_capsule_id == id,
            None => true,
        })
        .collect();
    report.scanned = placeholders.len();
    if placeholders.is_empty() {
        return Ok(report);
    }

    // Only consulted when a candidate actually produces a workflow.
    let mut existing_workflows: Option<Vec<CapabilityCapsuleRecord>> = None;

    for ph in placeholders {
        let id = ph.capability_capsule_id.clone();

        // Sources: the placeholder's `evidence` is the member id list
        // (`execute_generalize` puts them there precisely so the row is
        // auditable without the graph).
        let mut sources = Vec::new();
        for sid in ph.evidence.iter().take(MAX_SOURCES) {
            match client.get_capsule(&opts.tenant, sid).await {
                Ok(c) => sources.push(c),
                // A missing/archived source is not fatal — distil from the rest.
                Err(e) => warn!(source_id = %sid, error = %e, "crystallize: source fetch failed"),
            }
        }
        if sources.is_empty() {
            report
                .skipped
                .push((id, "no source capsules could be fetched".into()));
            continue;
        }

        let prompt = build_prompt(&sources, &ph.topics);
        // Guard #3: a synthesis failure is per-candidate and non-destructive.
        let raw = match synth.synthesize(&prompt).await {
            Ok(r) => r,
            Err(e) => {
                warn!(placeholder = %id, error = %e, "crystallize: synthesis failed — placeholder untouched");
                report.skipped.push((id, format!("synthesis failed: {e}")));
                continue;
            }
        };
        let Some((title, steps)) = parse_reply(&raw) else {
            report
                .skipped
                .push((id, "model reply was not parseable JSON".into()));
            continue;
        };

        if existing_workflows.is_none() {
            existing_workflows = Some(
                client
                    .list_active_workflows(&opts.tenant)
                    .await
                    .unwrap_or_else(|e| {
                        // Non-fatal: worst case we mint a parallel workflow
                        // instead of superseding.
                        warn!(error = %e, "crystallize: active-workflow list failed; skipping supersede check");
                        Vec::new()
                    }),
            );
        }
        let supersedes = existing_workflows
            .as_deref()
            .and_then(|ex| find_supersede_target(ex, &ph.evidence))
            .map(|c| c.capability_capsule_id.clone());

        let mut item = Crystallized {
            placeholder_id: id.clone(),
            title,
            steps,
            evidence: ph.evidence.clone(),
            code_refs: union_code_refs(&sources),
            supersedes,
            successor_id: None,
        };

        if opts.accept {
            let req = EditPendingRequest {
                capability_capsule_id: id.clone(),
                summary: item.title.clone(),
                content: item.content(),
                evidence: item.evidence.clone(),
                code_refs: item.code_refs.clone(),
                // Keep the tag: it is how a later run recognises this row as
                // an already-crystallized workflow for the supersede check.
                tags: vec![WORKFLOW_TAG.to_string()],
            };
            match client.edit_accept(&opts.tenant, req).await {
                Ok(successor) => {
                    // §4.3 DEVIATION (discovered in implementation; code is
                    // authoritative). The spec said to chain the new workflow
                    // onto the prior one via the version chain. That is not
                    // reachable: `supersedes_capability_capsule_id` is
                    // single-valued and `edit_and_accept_pending` already
                    // spends it linking the successor to its *placeholder*
                    // (`service::superseding_active_version`). So the
                    // continuation is recorded as a `supersedes` graph edge
                    // instead — auditable and using an existing predicate,
                    // but it does NOT make retrieval hide the prior workflow.
                    // Retiring that row stays an explicit operator action,
                    // which `render_report` calls out.
                    if let Some(old) = &item.supersedes {
                        if let Err(e) = client.link_supersedes(&opts.tenant, &successor, old).await
                        {
                            warn!(error = %e, "crystallize: supersedes edge write failed");
                        }
                    }
                    item.successor_id = Some(successor);
                }
                Err(e) => {
                    report
                        .skipped
                        .push((id, format!("edit_accept failed: {e}")));
                    continue;
                }
            }
        }
        report.crystallized.push(item);
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Real implementations.
// ---------------------------------------------------------------------------

pub struct HttpReviewClient {
    base_url: String,
    client: reqwest::Client,
}

impl HttpReviewClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ReviewClient for HttpReviewClient {
    async fn list_pending(&self, tenant: &str) -> Result<Vec<CapabilityCapsuleRecord>> {
        let url = format!("{}/reviews/pending?tenant={}", self.base_url, tenant);
        self.client
            .get(&url)
            .send()
            .await
            .context("GET /reviews/pending")?
            .error_for_status()
            .context("GET /reviews/pending")?
            .json()
            .await
            .context("decode /reviews/pending")
    }

    async fn get_capsule(&self, tenant: &str, id: &str) -> Result<CapabilityCapsuleRecord> {
        let url = format!(
            "{}/capability_capsules/{}?tenant={}",
            self.base_url, id, tenant
        );
        let detail: crate::domain::capability_capsule::CapabilityCapsuleDetailResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("GET /capability_capsules/{id}")?
            .error_for_status()
            .context("GET /capability_capsules/{id}")?
            .json()
            .await
            .context("decode capsule detail")?;
        Ok(detail.capability_capsule)
    }

    async fn list_active_workflows(&self, tenant: &str) -> Result<Vec<CapabilityCapsuleRecord>> {
        #[derive(Deserialize)]
        struct ListResp {
            #[serde(default)]
            items: Vec<CapabilityCapsuleRecord>,
        }
        let url = format!("{}/capability_capsules/list", self.base_url);
        let resp: ListResp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "tenant": tenant,
                "capability_capsule_type": "workflow",
                "status": "active",
            }))
            .send()
            .await
            .context("POST /capability_capsules/list")?
            .error_for_status()
            .context("POST /capability_capsules/list")?
            .json()
            .await
            .context("decode capsule list")?;
        Ok(resp.items)
    }

    async fn edit_accept(&self, tenant: &str, req: EditPendingRequest) -> Result<String> {
        let url = format!("{}/reviews/pending/edit_accept", self.base_url);
        let body = serde_json::json!({
            "tenant": tenant,
            "capability_capsule_id": req.capability_capsule_id,
            "summary": req.summary,
            "content": req.content,
            "evidence": req.evidence,
            "code_refs": req.code_refs,
            "tags": req.tags,
        });
        let resp: crate::domain::capability_capsule::EditPendingResponse = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("POST /reviews/pending/edit_accept")?
            .error_for_status()
            .context("POST /reviews/pending/edit_accept")?
            .json()
            .await
            .context("decode edit_accept response")?;
        Ok(resp.capability_capsule.capability_capsule_id)
    }

    async fn link_supersedes(&self, _tenant: &str, new_id: &str, old_id: &str) -> Result<()> {
        let url = format!("{}/graph/edges", self.base_url);
        self.client
            .post(&url)
            .json(&serde_json::json!({
                "from_node_id": format!("capability_capsule:{new_id}"),
                "to_node_id": format!("capability_capsule:{old_id}"),
                "relation": "supersedes",
            }))
            .send()
            .await
            .context("POST /graph/edges")?
            .error_for_status()
            .context("POST /graph/edges")?;
        Ok(())
    }
}

/// The `llm_entry` gateway synthesizer (same wire shape as O7(c)).
pub struct GatewaySynthesizer {
    cfg: LlmExtractConfig,
}

impl GatewaySynthesizer {
    pub fn new(cfg: LlmExtractConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl WorkflowSynthesizer for GatewaySynthesizer {
    async fn synthesize(&self, prompt: &str) -> Result<String, String> {
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

        // `.no_proxy()` is load-bearing — an ambient HTTP(S)_PROXY would route
        // the internal-IP gateway call through the public proxy and 502.
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()
            .map_err(|e| e.to_string())?;

        let body = serde_json::json!({
            "model": self.cfg.model,
            "messages": [
                {"role": "system", "content": SYS_PROMPT},
                {"role": "user", "content": prompt},
            ],
            "temperature": 0.2,
            "caller": "mem-crystallize-h4",
        });
        let mut req = client
            .post(format!("{}/chat/completions", self.cfg.base))
            .header("Content-Type", "application/json")
            .json(&body);
        if !self.cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.cfg.api_key));
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
        let parsed: ChatResp =
            serde_json::from_slice(&bytes).map_err(|e| format!("chat JSON: {e}"))?;
        Ok(parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Render a report for the terminal.
pub fn render_report(report: &CrystallizeReport, accepted: bool) -> String {
    let mut out = String::new();
    if report.unconfigured {
        out.push_str(
            "crystallize: LLM gateway not configured (set LLM_API_BASE + LLM_MODEL).\n\
             Nothing was read or written.\n",
        );
        return out;
    }
    if report.scanned == 0 {
        out.push_str("crystallize: no pending `evolution:workflow` placeholders.\n");
        return out;
    }
    out.push_str(&format!(
        "crystallize: {} placeholder(s) scanned, {} crystallized, {} skipped{}\n\n",
        report.scanned,
        report.crystallized.len(),
        report.skipped.len(),
        if accepted {
            ""
        } else {
            "  [DRY RUN — nothing written]"
        },
    ));
    for c in &report.crystallized {
        out.push_str(&format!("── {} ──\n", c.placeholder_id));
        out.push_str(&format!("summary: {}\n", c.title));
        out.push_str(&format!("{}\n", c.content()));
        if let Some(s) = &c.supersedes {
            out.push_str(&format!(
                "continues: {s}  (a `supersedes` graph edge is written; retrieval still\n  \
                 surfaces that older workflow — archive it explicitly if you want it gone)\n"
            ));
        }
        match &c.successor_id {
            Some(id) => out.push_str(&format!("→ written as {id}\n")),
            None => out.push_str("→ not written (re-run with --accept)\n"),
        }
        out.push('\n');
    }
    for (id, why) in &report.skipped {
        out.push_str(&format!("skipped {id}: {why}\n"));
    }
    out
}

pub async fn run(args: CrystallizeArgs) -> i32 {
    // Guard #2: unconfigured gateway → do nothing at all.
    let Some(cfg) = LlmExtractConfig::from_env() else {
        print!(
            "{}",
            render_report(
                &CrystallizeReport {
                    unconfigured: true,
                    ..Default::default()
                },
                args.accept,
            )
        );
        return 0;
    };

    let client = HttpReviewClient::new(args.remote.base_url.clone());
    let synth = GatewaySynthesizer::new(cfg);
    let opts = CrystallizeOptions {
        tenant: args.remote.tenant.clone(),
        candidate: args.candidate.clone(),
        accept: args.accept,
    };
    match crystallize(&client, &synth, &opts).await {
        Ok(report) => {
            print!("{}", render_report(&report, args.accept));
            0
        }
        Err(e) => {
            eprintln!("crystallize failed: {e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{Scope, Visibility};

    fn rec(id: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: "local".into(),
            capability_capsule_type: CapabilityCapsuleType::Workflow,
            status: CapabilityCapsuleStatus::PendingConfirmation,
            scope: Scope::Repo,
            visibility: Visibility::Private,
            version: 1,
            summary: "s".into(),
            content: "c".into(),
            evidence: vec![],
            code_refs: vec![],
            project: None,
            repo: None,
            module: None,
            task_type: None,
            tags: vec![WORKFLOW_TAG.into()],
            topics: vec![],
            confidence: 0.6,
            decay_score: 0.0,
            content_hash: String::new(),
            idempotency_key: None,
            session_id: None,
            supersedes_capability_capsule_id: None,
            source_agent: "evolution_worker".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
            last_validated_at: None,
            last_used_at: None,
            last_recalled_at: None,
            expires_at: None,
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn placeholder_detection_needs_all_three_marks() {
        let good = rec("a");
        assert!(is_workflow_placeholder(&good));

        let mut wrong_status = rec("b");
        wrong_status.status = CapabilityCapsuleStatus::Active;
        assert!(!is_workflow_placeholder(&wrong_status));

        let mut wrong_type = rec("c");
        wrong_type.capability_capsule_type = CapabilityCapsuleType::Experience;
        assert!(!is_workflow_placeholder(&wrong_type));

        let mut wrong_tag = rec("d");
        wrong_tag.tags = vec!["evolution:generalize".into()];
        assert!(!is_workflow_placeholder(&wrong_tag));
    }

    #[test]
    fn overlap_uses_min_not_union() {
        let old = ids(&["a", "b", "c", "d", "e"]);
        // The growth shape: old ⊂ new, new is much larger.
        let new = ids(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]);
        assert_eq!(overlap_ratio(&old, &new), 1.0);
        // Jaccard would have been 5/12 ≈ 0.42 — below the floor, i.e. wrong.
        assert!(overlap_ratio(&old, &new) >= SUPERSEDE_OVERLAP_FLOOR);

        // Genuinely unrelated clusters stay below the floor.
        let other = ids(&["x", "y", "z", "w"]);
        assert_eq!(overlap_ratio(&old, &other), 0.0);
        // Half-overlap sits exactly at the floor and counts as a continuation.
        assert_eq!(overlap_ratio(&ids(&["a", "b"]), &ids(&["a", "q"])), 0.5);
        // Empty either side → no match.
        assert_eq!(overlap_ratio(&[], &new), 0.0);
    }

    #[test]
    fn supersede_target_picks_best_overlap_and_ignores_untagged() {
        let mut chained = rec("wf_old");
        chained.status = CapabilityCapsuleStatus::Active;
        chained.evidence = ids(&["a", "b", "c"]);

        let mut unrelated = rec("wf_other");
        unrelated.status = CapabilityCapsuleStatus::Active;
        unrelated.evidence = ids(&["x", "y", "z"]);

        // A hand-written Workflow capsule with no evolution tag must never be
        // superseded by the crystallizer.
        let mut handwritten = rec("wf_manual");
        handwritten.status = CapabilityCapsuleStatus::Active;
        handwritten.tags = vec![];
        handwritten.evidence = ids(&["a", "b", "c"]);

        let existing = vec![chained.clone(), unrelated, handwritten];
        let members = ids(&["a", "b", "c", "d"]);
        let got = find_supersede_target(&existing, &members).expect("should chain");
        assert_eq!(got.capability_capsule_id, "wf_old");

        // No overlap anywhere → mint fresh, don't supersede.
        assert!(find_supersede_target(&existing, &ids(&["q", "r"])).is_none());
    }

    #[test]
    fn content_is_one_step_per_line() {
        // Load-bearing: `compress::split_steps` splits by line, so N steps
        // must render as exactly N lines.
        let c = Crystallized {
            placeholder_id: "p".into(),
            title: "t".into(),
            steps: ids(&["locate the controller", "fill read-only endpoints first"]),
            evidence: vec![],
            code_refs: vec![],
            supersedes: None,
            successor_id: None,
        };
        let content = c.content();
        assert_eq!(content.lines().count(), 2);
        assert_eq!(
            content,
            "1. locate the controller\n2. fill read-only endpoints first"
        );
        // Provenance must NOT be in the content (it would read as a step).
        assert!(!content.contains("distilled"));
    }

    #[test]
    fn parse_reply_plain_and_fenced() {
        let (t, s) = parse_reply(
            r#"{"title":"migrate one controller batch","steps":["locate","port","test"]}"#,
        )
        .expect("plain");
        assert_eq!(t, "migrate one controller batch");
        assert_eq!(s.len(), 3);

        let (t2, s2) = parse_reply(
            "Sure!\n```json\n{\"title\":\"do the thing\",\"steps\":[\"- step one\",\"step two\"]}\n```\nhope that helps",
        )
        .expect("fenced");
        assert_eq!(t2, "do the thing");
        assert_eq!(s2[0], "step one", "leading dash trimmed");
    }

    #[test]
    fn parse_reply_rejects_garbage() {
        assert!(parse_reply("I couldn't find a procedure.").is_none());
        assert!(parse_reply("").is_none());
        assert!(parse_reply(r#"{"title":"","steps":["a"]}"#).is_none());
        assert!(parse_reply(r#"{"title":"t","steps":[]}"#).is_none());
        // Wrong schema.
        assert!(parse_reply(r#"{"foo":1}"#).is_none());
    }

    #[test]
    fn parse_reply_flattens_embedded_newlines() {
        // A step containing a newline would explode into two steps in
        // `split_steps` downstream.
        let (_, s) =
            parse_reply("{\"title\":\"t\",\"steps\":[\"line one\\nline two\"]}").expect("parsed");
        assert_eq!(s.len(), 1);
        assert!(!s[0].contains('\n'));
    }

    #[test]
    fn prompt_carries_verbatim_content_not_summaries() {
        let mut a = rec("mem_a");
        a.content = "verbatim body of A".into();
        a.summary = "hint A".into();
        let mut b = rec("mem_b");
        b.content = "verbatim body of B".into();
        let p = build_prompt(&[a, b], &ids(&["migration"]));
        assert!(p.contains("verbatim body of A"));
        assert!(p.contains("verbatim body of B"));
        assert!(p.contains("migration"));
        assert!(p.contains("2 executions"));
    }

    #[test]
    fn union_code_refs_dedups() {
        let mut a = rec("a");
        a.code_refs = ids(&["src/x.rs", "src/y.rs"]);
        let mut b = rec("b");
        b.code_refs = ids(&["src/y.rs", "src/z.rs"]);
        assert_eq!(
            union_code_refs(&[a, b]),
            ids(&["src/x.rs", "src/y.rs", "src/z.rs"])
        );
    }
}
