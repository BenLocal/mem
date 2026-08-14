mod common;

use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::service::{CompletedToolRoundService, TranscriptService};
use mem::storage::CompletedToolRoundStore;
use std::sync::Arc;

fn message(
    line_number: u64,
    role: MessageRole,
    block_type: BlockType,
    content: &str,
    tool_name: Option<&str>,
    tool_use_id: Option<&str>,
) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("mb-{line_number}"),
        session_id: Some("session-1".to_string()),
        tenant: "local".to_string(),
        caller_agent: "codex".to_string(),
        transcript_path: "/tmp/codex.jsonl".to_string(),
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

#[tokio::test]
async fn rebuild_publishes_round_without_changing_verbatim_transcript() {
    let (_dir, store) = common::test_store().await;
    let transcript_service = TranscriptService::new(store.clone(), None);
    let round_service = CompletedToolRoundService::new(store.clone());
    let messages = vec![
        message(1, MessageRole::User, BlockType::Text, "check", None, None),
        message(
            2,
            MessageRole::Assistant,
            BlockType::ToolUse,
            r#"{"cmd":"status"}"#,
            Some("exec_command"),
            Some("call-1"),
        ),
        message(
            3,
            MessageRole::User,
            BlockType::ToolResult,
            "running",
            None,
            Some("call-1"),
        ),
        message(
            4,
            MessageRole::Assistant,
            BlockType::Text,
            "healthy",
            None,
            None,
        ),
    ];
    transcript_service.ingest_batch(messages).await.unwrap();
    let bounded_source = store
        .load_round_source_messages("local", "session-1", 100, usize::MAX)
        .await
        .unwrap();
    assert_eq!(bounded_source.len(), 4);
    let before = transcript_service
        .get_by_session("local", "session-1")
        .await
        .unwrap();

    let report = round_service
        .rebuild_session("local", "session-1", false)
        .await
        .unwrap();
    let latest = round_service.latest("local", "session-1").await.unwrap();
    let after = transcript_service
        .get_by_session("local", "session-1")
        .await
        .unwrap();

    assert_eq!(report.status, "published");
    assert_eq!(report.completed_rounds, 1);
    assert!(!report.degraded);
    assert_eq!(latest.rounds.len(), 1);
    assert!(!latest.degraded);
    assert_eq!(
        before, after,
        "round rebuild must not rewrite transcript rows"
    );

    // Keep the concrete store alive until both services have been dropped.
    drop(Arc::clone(&store));
}

#[tokio::test]
async fn unchanged_rebuild_reuses_the_completed_generation() {
    let (_dir, store) = common::test_store().await;
    let transcript_service = TranscriptService::new(store.clone(), None);
    let round_service = CompletedToolRoundService::new(store);
    transcript_service
        .ingest_batch(vec![
            message(1, MessageRole::User, BlockType::Text, "check", None, None),
            message(
                2,
                MessageRole::Assistant,
                BlockType::ToolUse,
                "{}",
                Some("health"),
                Some("call-1"),
            ),
            message(
                3,
                MessageRole::User,
                BlockType::ToolResult,
                "ok",
                None,
                Some("call-1"),
            ),
            message(
                4,
                MessageRole::Assistant,
                BlockType::Text,
                "healthy",
                None,
                None,
            ),
        ])
        .await
        .unwrap();

    let first = round_service
        .rebuild_session("local", "session-1", false)
        .await
        .unwrap();
    let second = round_service
        .rebuild_session("local", "session-1", false)
        .await
        .unwrap();
    let latest = round_service.latest("local", "session-1").await.unwrap();

    assert_eq!(first.status, "published");
    assert_eq!(second.status, "unchanged");
    assert_eq!(second.generation_id, first.generation_id);
    assert_eq!(latest.generation_id, first.generation_id);
    assert_eq!(latest.rounds.len(), 1);
}

#[tokio::test]
async fn appending_a_round_replaces_the_generation_and_keeps_existing_round_id() {
    let (_dir, store) = common::test_store().await;
    let transcript_service = TranscriptService::new(store.clone(), None);
    let round_service = CompletedToolRoundService::new(store);
    transcript_service
        .ingest_batch(vec![
            message(1, MessageRole::User, BlockType::Text, "first", None, None),
            message(
                2,
                MessageRole::Assistant,
                BlockType::ToolUse,
                "{}",
                Some("health"),
                Some("call-1"),
            ),
            message(
                3,
                MessageRole::User,
                BlockType::ToolResult,
                "ok",
                None,
                Some("call-1"),
            ),
            message(
                4,
                MessageRole::Assistant,
                BlockType::Text,
                "first done",
                None,
                None,
            ),
        ])
        .await
        .unwrap();
    let first = round_service
        .rebuild_session("local", "session-1", false)
        .await
        .unwrap();
    let first_round_id = round_service
        .latest("local", "session-1")
        .await
        .unwrap()
        .rounds[0]
        .round_id
        .clone();

    transcript_service
        .ingest_batch(vec![
            message(5, MessageRole::User, BlockType::Text, "second", None, None),
            message(
                6,
                MessageRole::Assistant,
                BlockType::ToolUse,
                "{}",
                Some("status"),
                Some("call-2"),
            ),
            message(
                7,
                MessageRole::User,
                BlockType::ToolResult,
                "ok",
                None,
                Some("call-2"),
            ),
            message(
                8,
                MessageRole::Assistant,
                BlockType::Text,
                "second done",
                None,
                None,
            ),
        ])
        .await
        .unwrap();
    let second = round_service
        .rebuild_session("local", "session-1", false)
        .await
        .unwrap();
    let latest = round_service.latest("local", "session-1").await.unwrap();

    assert_eq!(second.status, "published");
    assert_ne!(second.generation_id, first.generation_id);
    assert_eq!(latest.rounds.len(), 2);
    assert_eq!(latest.rounds[0].round_id, first_round_id);
}

#[tokio::test]
async fn rebuild_isolates_quoted_tenant_and_session_scope() {
    let (_dir, store) = common::test_store().await;
    let transcript_service = TranscriptService::new(store.clone(), None);
    let round_service = CompletedToolRoundService::new(store);
    let tenant = "team'o";
    let session_id = "session'quoted";
    let mut scoped = vec![
        message(1, MessageRole::User, BlockType::Text, "check", None, None),
        message(
            2,
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-1"),
        ),
        message(
            3,
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-1"),
        ),
        message(
            4,
            MessageRole::Assistant,
            BlockType::Text,
            "done",
            None,
            None,
        ),
    ];
    for block in &mut scoped {
        block.tenant = tenant.to_string();
        block.session_id = Some(session_id.to_string());
        block.message_block_id = format!("quoted-{}", block.message_block_id);
        block.transcript_path = "/tmp/quoted.jsonl".to_string();
    }
    let mut other = scoped.clone();
    for block in &mut other {
        block.tenant = "other".to_string();
        block.message_block_id = format!("other-{}", block.message_block_id);
        block.transcript_path = "/tmp/other.jsonl".to_string();
    }
    transcript_service.ingest_batch(scoped).await.unwrap();
    transcript_service.ingest_batch(other).await.unwrap();

    round_service
        .rebuild_session(tenant, session_id, false)
        .await
        .unwrap();
    let latest = round_service.latest(tenant, session_id).await.unwrap();

    assert_eq!(latest.rounds.len(), 1);
    assert_eq!(latest.rounds[0].tenant, tenant);
    assert_eq!(latest.rounds[0].session_id.as_deref(), Some(session_id));
    assert_eq!(latest.rounds[0].caller_agent, "codex");
}
