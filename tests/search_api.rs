use serde_json::json;
use mem::domain::{
    query::{DirectiveItem, FactItem, PatternItem, SearchMemoryRequest, SearchMemoryResponse},
    workflow::WorkflowOutline,
};

#[test]
fn search_request_missing_required_field_fails_deserialization() {
    let value = json!({
        "intent": "debugging",
        "scope_filters": ["repo:billing"],
        "token_budget": 500,
        "caller_agent": "codex-worker",
        "expand_graph": true
    });

    let result = serde_json::from_value::<SearchMemoryRequest>(value);

    assert!(result.is_err());
}

#[test]
fn search_request_serializes_expected_shape() {
    let request = SearchMemoryRequest {
        query: "how should I debug invoice retry failures".into(),
        intent: "debugging".into(),
        scope_filters: vec!["repo:billing".into()],
        token_budget: 500,
        caller_agent: "codex-worker".into(),
        expand_graph: true,
    };

    let value = serde_json::to_value(request).unwrap();

    assert_eq!(value["query"], "how should I debug invoice retry failures");
    assert_eq!(value["expand_graph"], true);
}

#[test]
fn search_response_serializes_compressed_shapes() {
    let response = SearchMemoryResponse {
        directives: vec![DirectiveItem {
            memory_id: "mem_1".into(),
            text: "Use cache busting on schema changes".into(),
            source_summary: "Known rule from prior implementation".into(),
        }],
        relevant_facts: vec![FactItem {
            memory_id: "mem_2".into(),
            text: "DuckDB stores canonical memory records".into(),
            code_refs: vec!["src/storage/duckdb.rs".into()],
            source_summary: "Architecture note".into(),
        }],
        reusable_patterns: vec![PatternItem {
            memory_id: "mem_3".into(),
            text: "Check invariants before writing migrations".into(),
            applicability: None,
            source_summary: "Repeated successful workflow".into(),
        }],
        suggested_workflow: Some(WorkflowOutline {
            memory_id: "mem_4".into(),
            goal: "ship a safe schema change".into(),
            steps: vec!["write tests".into(), "implement".into(), "verify".into()],
            success_signals: vec!["tests pass".into()],
        }),
    };

    let value = serde_json::to_value(response).unwrap();

    assert_eq!(value["directives"][0]["memory_id"], "mem_1");
    assert_eq!(value["relevant_facts"][0]["code_refs"][0], "src/storage/duckdb.rs");
    assert!(value["reusable_patterns"][0].get("applicability").is_none());
    assert_eq!(value["suggested_workflow"]["goal"], "ship a safe schema change");
}
