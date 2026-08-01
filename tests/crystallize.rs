//! H4 skill crystallization — `mem crystallize`.
//!
//! Spec: `docs/superpowers/specs/2026-08-01-h4-skill-crystallization-design.md` §5.
//!
//! The CLI is a pure HTTP client (`mem serve` is the single Lance writer), so
//! the orchestration is generic over two seams — `ReviewClient` and
//! `WorkflowSynthesizer`. These tests drive the real `crystallize()` through
//! fakes: no network, no server, no gateway, fully deterministic.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use mem::cli::crystallize::{
    crystallize, CrystallizeOptions, ReviewClient, WorkflowSynthesizer, WORKFLOW_TAG,
};
use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, EditPendingRequest,
    Scope, Visibility,
};
use mem::pipeline::compress::compress;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn capsule(
    id: &str,
    kind: CapabilityCapsuleType,
    status: CapabilityCapsuleStatus,
) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "local".into(),
        capability_capsule_type: kind,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary of {id}"),
        content: format!("verbatim content of {id}"),
        evidence: vec![],
        code_refs: vec![],
        project: Some("xmbox-rs".into()),
        repo: None,
        module: None,
        task_type: None,
        tags: vec![],
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

/// An H4 placeholder exactly as `execute_generalize(workflow=true)` mints it.
fn placeholder(id: &str, members: &[&str]) -> CapabilityCapsuleRecord {
    let mut c = capsule(
        id,
        CapabilityCapsuleType::Workflow,
        CapabilityCapsuleStatus::PendingConfirmation,
    );
    c.tags = vec![WORKFLOW_TAG.into()];
    c.topics = vec!["aibox-nvr".into(), "java-to-rust".into()];
    c.evidence = members.iter().map(|s| s.to_string()).collect();
    c.summary = format!(
        "[evolution:workflow] procedure proposal over {} sibling executions",
        members.len()
    );
    c.content = "EVOLUTION PROPOSAL — workflow generalize (sibling executions → procedure)\n\
                 Review task: the source capsules below are N executions of the SAME recurring procedure."
        .into();
    c
}

fn source(id: &str, code_ref: &str) -> CapabilityCapsuleRecord {
    let mut c = capsule(
        id,
        CapabilityCapsuleType::Experience,
        CapabilityCapsuleStatus::Active,
    );
    c.code_refs = vec![code_ref.into()];
    c
}

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct FakeClient {
    pending: Vec<CapabilityCapsuleRecord>,
    sources: HashMap<String, CapabilityCapsuleRecord>,
    active_workflows: Vec<CapabilityCapsuleRecord>,
    /// Every write the run attempted — the assertion surface for "wrote nothing".
    edits: Mutex<Vec<EditPendingRequest>>,
    /// `(new_id, old_id)` for each `supersedes` lineage edge written.
    links: Mutex<Vec<(String, String)>>,
}

impl FakeClient {
    fn new(pending: Vec<CapabilityCapsuleRecord>, sources: Vec<CapabilityCapsuleRecord>) -> Self {
        Self {
            pending,
            sources: sources
                .into_iter()
                .map(|c| (c.capability_capsule_id.clone(), c))
                .collect(),
            active_workflows: Vec::new(),
            edits: Mutex::new(Vec::new()),
            links: Mutex::new(Vec::new()),
        }
    }

    fn with_active_workflows(mut self, w: Vec<CapabilityCapsuleRecord>) -> Self {
        self.active_workflows = w;
        self
    }

    fn edits(&self) -> Vec<EditPendingRequest> {
        self.edits.lock().expect("lock").clone()
    }

    fn links(&self) -> Vec<(String, String)> {
        self.links.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ReviewClient for FakeClient {
    async fn list_pending(&self, _tenant: &str) -> anyhow::Result<Vec<CapabilityCapsuleRecord>> {
        Ok(self.pending.clone())
    }
    async fn get_capsule(
        &self,
        _tenant: &str,
        id: &str,
    ) -> anyhow::Result<CapabilityCapsuleRecord> {
        self.sources
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such capsule {id}"))
    }
    async fn list_active_workflows(
        &self,
        _tenant: &str,
    ) -> anyhow::Result<Vec<CapabilityCapsuleRecord>> {
        Ok(self.active_workflows.clone())
    }
    async fn edit_accept(&self, _tenant: &str, req: EditPendingRequest) -> anyhow::Result<String> {
        let successor = format!("mem_successor_of_{}", req.capability_capsule_id);
        self.edits.lock().expect("lock").push(req);
        Ok(successor)
    }
    async fn link_supersedes(
        &self,
        _tenant: &str,
        new_id: &str,
        old_id: &str,
    ) -> anyhow::Result<()> {
        self.links
            .lock()
            .expect("lock")
            .push((new_id.into(), old_id.into()));
        Ok(())
    }
}

/// Deterministic stand-in for the gateway (the `MEM_RERANK_PROVIDER=fake`
/// precedent — a scripted provider so the test never needs a model).
struct FakeSynth(Result<String, String>);

#[async_trait]
impl WorkflowSynthesizer for FakeSynth {
    async fn synthesize(&self, _prompt: &str) -> Result<String, String> {
        self.0.clone()
    }
}

fn good_reply() -> FakeSynth {
    FakeSynth(Ok(
        r#"{"title":"NVR-APP Java→Rust: port one controller batch",
        "steps":["locate the Java controller and list this batch's endpoints",
                 "port read-only endpoints first",
                 "handle write endpoints, adding db methods as needed",
                 "record a checkpoint capsule citing the commit"]}"#
            .into(),
    ))
}

fn opts(accept: bool) -> CrystallizeOptions {
    CrystallizeOptions {
        tenant: "local".into(),
        candidate: None,
        accept,
    }
}

// ---------------------------------------------------------------------------
// §5 #3 — dry run writes nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dry_run_generates_but_writes_nothing() {
    let client = FakeClient::new(
        vec![placeholder("mem_ph", &["mem_s1", "mem_s2"])],
        vec![source("mem_s1", "src/a.rs"), source("mem_s2", "src/b.rs")],
    );
    let report = crystallize(&client, &good_reply(), &opts(false))
        .await
        .expect("run");

    assert_eq!(report.scanned, 1);
    assert_eq!(report.crystallized.len(), 1);
    // Generated, but nothing persisted.
    assert!(
        client.edits().is_empty(),
        "dry run must not call edit_accept"
    );
    assert!(report.crystallized[0].successor_id.is_none());
}

// ---------------------------------------------------------------------------
// §5 #4 — the accept path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accept_writes_workflow_with_steps_evidence_and_tag() {
    let client = FakeClient::new(
        vec![placeholder("mem_ph", &["mem_s1", "mem_s2"])],
        vec![source("mem_s1", "src/a.rs"), source("mem_s2", "src/b.rs")],
    );
    let report = crystallize(&client, &good_reply(), &opts(true))
        .await
        .expect("run");

    assert_eq!(report.crystallized.len(), 1);
    assert_eq!(
        report.crystallized[0].successor_id.as_deref(),
        Some("mem_successor_of_mem_ph")
    );

    let edits = client.edits();
    assert_eq!(edits.len(), 1);
    let e = &edits[0];
    assert_eq!(e.capability_capsule_id, "mem_ph");

    // Content is one step per line — `compress::split_steps` splits by line,
    // so this is what makes the slot render as a clean 4-step workflow.
    assert_eq!(e.content.lines().count(), 4, "content = {}", e.content);
    assert!(e.content.starts_with("1. locate the Java controller"));

    // The reviewer instruction boilerplate is gone.
    assert!(!e.content.contains("EVOLUTION PROPOSAL"));
    assert!(!e.content.contains("Review task"));

    // summary != content (ingest enforces this) and carries the title.
    assert_ne!(e.summary, e.content);
    assert!(e.summary.contains("port one controller batch"));

    // Provenance survives as evidence; code_refs are unioned from sources.
    assert_eq!(e.evidence, vec!["mem_s1".to_string(), "mem_s2".to_string()]);
    assert_eq!(
        e.code_refs,
        vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
    );

    // Tag retained so a later run recognises this as already crystallized.
    assert_eq!(e.tags, vec![WORKFLOW_TAG.to_string()]);
}

// ---------------------------------------------------------------------------
// §5 #2 — a broken gateway must not disturb state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn synthesis_failure_leaves_placeholder_untouched() {
    let client = FakeClient::new(
        vec![placeholder("mem_ph", &["mem_s1"])],
        vec![source("mem_s1", "src/a.rs")],
    );
    let dead = FakeSynth(Err("gateway 502: <empty body>".into()));
    // Even with --accept: a failed generation must write nothing.
    let report = crystallize(&client, &dead, &opts(true)).await.expect("run");

    assert_eq!(report.scanned, 1);
    assert!(report.crystallized.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].1.contains("synthesis failed"));
    assert!(client.edits().is_empty(), "a dead gateway must never write");
}

#[tokio::test]
async fn unparseable_reply_leaves_placeholder_untouched() {
    let client = FakeClient::new(
        vec![placeholder("mem_ph", &["mem_s1"])],
        vec![source("mem_s1", "src/a.rs")],
    );
    let prose = FakeSynth(Ok("I'm not sure I can distil a procedure here.".into()));
    let report = crystallize(&client, &prose, &opts(true))
        .await
        .expect("run");

    assert!(report.crystallized.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(client.edits().is_empty());
}

// ---------------------------------------------------------------------------
// §5 #5 — a growing cluster supersedes rather than forking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn growing_cluster_supersedes_prior_workflow() {
    // Prior crystallization covered 5 executions.
    let mut prior = capsule(
        "mem_wf_v1",
        CapabilityCapsuleType::Workflow,
        CapabilityCapsuleStatus::Active,
    );
    prior.tags = vec![WORKFLOW_TAG.into()];
    prior.evidence = ["mem_s1", "mem_s2", "mem_s3", "mem_s4", "mem_s5"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // The migration continued: the new cluster is a strict superset.
    let members = [
        "mem_s1", "mem_s2", "mem_s3", "mem_s4", "mem_s5", "mem_s6", "mem_s7",
    ];
    let client = FakeClient::new(
        vec![placeholder("mem_ph2", &members)],
        members.iter().map(|m| source(m, "src/x.rs")).collect(),
    )
    .with_active_workflows(vec![prior]);

    let report = crystallize(&client, &good_reply(), &opts(true))
        .await
        .expect("run");

    assert_eq!(report.crystallized.len(), 1);
    assert_eq!(
        report.crystallized[0].supersedes.as_deref(),
        Some("mem_wf_v1"),
        "old ⊂ new must chain, not fork (|A∩B|/min = 5/5 = 1.0)"
    );
    // §4.3 deviation: the version chain can't express this (the successor's
    // single-valued `supersedes_capability_capsule_id` is already spent on the
    // placeholder), so the continuation is recorded as a graph edge.
    assert_eq!(
        client.links(),
        vec![(
            "mem_successor_of_mem_ph2".to_string(),
            "mem_wf_v1".to_string()
        )]
    );
}

#[tokio::test]
async fn unrelated_cluster_does_not_supersede() {
    let mut prior = capsule(
        "mem_wf_other",
        CapabilityCapsuleType::Workflow,
        CapabilityCapsuleStatus::Active,
    );
    prior.tags = vec![WORKFLOW_TAG.into()];
    prior.evidence = ["mem_q1", "mem_q2"].iter().map(|s| s.to_string()).collect();

    let client = FakeClient::new(
        vec![placeholder("mem_ph", &["mem_s1", "mem_s2"])],
        vec![source("mem_s1", "src/a.rs"), source("mem_s2", "src/b.rs")],
    )
    .with_active_workflows(vec![prior]);

    let report = crystallize(&client, &good_reply(), &opts(true))
        .await
        .expect("run");
    assert!(report.crystallized[0].supersedes.is_none());
    assert!(
        client.links().is_empty(),
        "no continuation → no supersedes edge"
    );
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_evolution_workflow_placeholders_are_touched() {
    let mut other_pending = capsule(
        "mem_generalize_ph",
        CapabilityCapsuleType::Experience,
        CapabilityCapsuleStatus::PendingConfirmation,
    );
    other_pending.tags = vec!["evolution:generalize".into()];

    let client = FakeClient::new(
        vec![other_pending, placeholder("mem_ph", &["mem_s1"])],
        vec![source("mem_s1", "src/a.rs")],
    );
    let report = crystallize(&client, &good_reply(), &opts(true))
        .await
        .expect("run");

    assert_eq!(
        report.scanned, 1,
        "the ② generalize placeholder is not ours"
    );
    assert_eq!(client.edits().len(), 1);
    assert_eq!(client.edits()[0].capability_capsule_id, "mem_ph");
}

#[tokio::test]
async fn candidate_filter_narrows_to_one() {
    let client = FakeClient::new(
        vec![
            placeholder("mem_ph_a", &["mem_s1"]),
            placeholder("mem_ph_b", &["mem_s1"]),
        ],
        vec![source("mem_s1", "src/a.rs")],
    );
    let o = CrystallizeOptions {
        tenant: "local".into(),
        candidate: Some("mem_ph_b".into()),
        accept: true,
    };
    let report = crystallize(&client, &good_reply(), &o).await.expect("run");
    assert_eq!(report.scanned, 1);
    assert_eq!(client.edits()[0].capability_capsule_id, "mem_ph_b");
}

#[tokio::test]
async fn empty_queue_is_a_no_op() {
    let client = FakeClient::new(vec![], vec![]);
    let report = crystallize(&client, &good_reply(), &opts(true))
        .await
        .expect("run");
    assert_eq!(report.scanned, 0);
    assert!(client.edits().is_empty());
}

// ---------------------------------------------------------------------------
// §5 #6 — the pollution fix (`compress.rs`)
// ---------------------------------------------------------------------------

#[test]
fn pending_workflow_placeholder_is_not_served_as_suggested_workflow() {
    // Reproduces the live defect: a Workflow-typed PendingConfirmation
    // placeholder whose body is reviewer instructions was filling the
    // `suggested_workflow` slot, so agents were handed the review form.
    let ph = placeholder("mem_ph", &["mem_s1", "mem_s2"]);
    let response = compress(&[ph], 800);
    assert!(
        response.suggested_workflow.is_none(),
        "a pending proposal must not be served as an authoritative workflow"
    );
}

#[test]
fn active_workflow_still_fills_the_slot() {
    // The fix must not blank the slot for real workflows.
    let mut wf = capsule(
        "mem_wf",
        CapabilityCapsuleType::Workflow,
        CapabilityCapsuleStatus::Active,
    );
    wf.summary = "port one controller batch".into();
    wf.content = "1. locate the controller\n2. port read-only endpoints".into();
    wf.evidence = vec!["mem_s1".into()];

    let response = compress(&[wf], 800);
    let outline = response
        .suggested_workflow
        .expect("an Active workflow must still be served");
    assert_eq!(outline.capability_capsule_id, "mem_wf");
    assert_eq!(outline.steps.len(), 2);
}

#[test]
fn active_workflow_wins_the_slot_over_a_pending_one() {
    // Ordering-independent: the pending placeholder is skipped even when it
    // is ranked first, so the real workflow behind it still gets the slot.
    let ph = placeholder("mem_ph", &["mem_s1"]);
    let mut wf = capsule(
        "mem_wf",
        CapabilityCapsuleType::Workflow,
        CapabilityCapsuleStatus::Active,
    );
    wf.content = "1. do the thing".into();

    let response = compress(&[ph, wf], 800);
    assert_eq!(
        response
            .suggested_workflow
            .expect("real workflow should fill the slot")
            .capability_capsule_id,
        "mem_wf"
    );
}
