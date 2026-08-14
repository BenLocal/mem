mod common;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use mem::{
    domain::{BlockType, ConversationMessage, MessageRole},
    http,
    service::{CapabilityCapsuleService, TranscriptService},
};
use serde_json::{json, Value};
use tower::ServiceExt;

fn message(
    line_number: u64,
    role: MessageRole,
    block_type: BlockType,
    content: &str,
    tool_name: Option<&str>,
    tool_use_id: Option<&str>,
) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("api-{line_number}"),
        session_id: Some("session-api".to_string()),
        tenant: "local".to_string(),
        caller_agent: "codex".to_string(),
        transcript_path: "/tmp/api.jsonl".to_string(),
        line_number,
        block_index: 0,
        message_uuid: None,
        role,
        block_type,
        content: content.to_string(),
        tool_name: tool_name.map(str::to_string),
        tool_use_id: tool_use_id.map(str::to_string),
        embed_eligible: false,
        created_at: format!("000000000000000000{line_number:02}"),
        meta_json: None,
    }
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn completed_round_admin_routes_reject_missing_bearer_token() {
    let (_dir, store) = common::test_store().await;
    let state = common::test_app_state(store.clone(), CapabilityCapsuleService::new(store));
    let router = http::router().with_state(state);

    let response = router
        .oneshot(
            Request::get("/admin/transcript-rounds?tenant=local&session_id=session-api")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_auth_rejects_before_parsing_a_malformed_rebuild_body() {
    let (_dir, store) = common::test_store().await;
    let state = common::test_app_state(store.clone(), CapabilityCapsuleService::new(store));
    let router = http::router().with_state(state);

    let response = router
        .oneshot(
            Request::post("/admin/transcript-rounds/rebuild")
                .header("content-type", "application/json")
                .body(Body::from("not-json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rebuild_and_list_rounds_over_http_without_exposing_transcript_content() {
    let (_dir, store) = common::test_store().await;
    TranscriptService::new(store.clone(), None)
        .ingest_batch(vec![
            message(
                1,
                MessageRole::User,
                BlockType::Text,
                "secret prompt",
                None,
                None,
            ),
            message(
                2,
                MessageRole::Assistant,
                BlockType::ToolUse,
                r#"{"token":"secret"}"#,
                Some("health"),
                Some("call-1"),
            ),
            message(
                3,
                MessageRole::User,
                BlockType::ToolResult,
                "secret result",
                None,
                Some("call-1"),
            ),
            message(
                4,
                MessageRole::Assistant,
                BlockType::Text,
                "secret final",
                None,
                None,
            ),
        ])
        .await
        .unwrap();
    let state = common::test_app_state(store.clone(), CapabilityCapsuleService::new(store));
    let router = http::router().with_state(state);

    let rebuild = router
        .clone()
        .oneshot(
            Request::post("/admin/transcript-rounds/rebuild")
                .header("content-type", "application/json")
                .header(
                    "authorization",
                    format!("Bearer {}", common::TEST_ADMIN_TOKEN),
                )
                .body(Body::from(
                    json!({
                        "tenant": "local",
                        "session_id": "session-api",
                        "dry_run": false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rebuild.status(), StatusCode::OK);
    assert_eq!(response_json(rebuild).await["status"], "published");

    let listed = router
        .oneshot(
            Request::get("/admin/transcript-rounds?tenant=local&session_id=session-api")
                .header(
                    "authorization",
                    format!("Bearer {}", common::TEST_ADMIN_TOKEN),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let body = response_json(listed).await;
    assert_eq!(body["degraded"], false);
    assert_eq!(body["total_rounds"], 1);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["rounds"].as_array().unwrap().len(), 1);
    let round = &body["rounds"][0];
    for internal_key in [
        "transcript_path",
        "start_message_uuid",
        "final_message_uuid",
        "tool_call_ids",
        "source_fingerprint",
    ] {
        assert!(
            round.get(internal_key).is_none(),
            "public DTO exposed internal field: {internal_key}"
        );
    }
    let encoded = body.to_string();
    for forbidden in [
        "secret prompt",
        "secret result",
        "secret final",
        r#"\"token\":\"secret\""#,
    ] {
        assert!(
            !encoded.contains(forbidden),
            "leaked derived content: {forbidden}"
        );
    }
}
