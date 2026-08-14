use mem::cli::mine::{block_to_payload, parse_transcript_full, ArchivedBlock};
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::pipeline::completed_tool_round::project_completed_tool_rounds;

macro_rules! block {
    (
        $line_number:expr,
        $block_index:expr,
        $message_uuid:expr,
        $role:expr,
        $block_type:expr,
        $content:expr,
        $tool_name:expr,
        $tool_use_id:expr $(,)?
    ) => {{
        ConversationMessage {
            message_block_id: format!("mb-{}-{}", $line_number, $block_index),
            session_id: Some("session-1".to_string()),
            tenant: "local".to_string(),
            caller_agent: "claude-code".to_string(),
            transcript_path: "/tmp/claude.jsonl".to_string(),
            line_number: $line_number,
            block_index: $block_index,
            message_uuid: $message_uuid.map(str::to_string),
            role: $role,
            block_type: $block_type,
            content: $content.to_string(),
            tool_name: $tool_name.map(str::to_string),
            tool_use_id: $tool_use_id.map(str::to_string),
            embed_eligible: $block_type.embed_eligible_default(),
            created_at: format!("000000000000000000{:02}", $line_number),
            meta_json: None,
        }
    }};
}

fn codex_block(
    line_number: u64,
    role: MessageRole,
    block_type: BlockType,
    content: &str,
    tool_name: Option<&str>,
    tool_use_id: Option<&str>,
) -> ConversationMessage {
    ConversationMessage {
        message_block_id: format!("codex-{line_number}"),
        session_id: Some("codex-session".to_string()),
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
        embed_eligible: block_type.embed_eligible_default(),
        created_at: format!("000000000000000000{line_number:02}"),
        meta_json: None,
    }
}

fn normalized_messages(
    blocks: &[ArchivedBlock],
    caller_agent: &str,
    transcript_path: &str,
) -> Vec<ConversationMessage> {
    blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let mut payload = block_to_payload(block, transcript_path, "local", caller_agent);
            payload["message_block_id"] = format!("normalized-{index}").into();
            serde_json::from_value(payload).unwrap()
        })
        .collect()
}

fn one_tool_round(user_text: &str) -> Vec<ConversationMessage> {
    vec![
        block!(
            1,
            0,
            Some("user-task"),
            MessageRole::User,
            BlockType::Text,
            user_text,
            None,
            None,
        ),
        block!(
            2,
            0,
            Some("assistant-tool"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            r#"{"cmd":"true"}"#,
            Some("exec_command"),
            Some("call-1"),
        ),
        block!(
            3,
            0,
            Some("tool-result"),
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-1"),
        ),
        block!(
            4,
            0,
            Some("assistant-final"),
            MessageRole::Assistant,
            BlockType::Text,
            "done",
            None,
            None,
        ),
    ]
}

#[test]
fn task_fingerprint_ignores_environment_specific_literals() {
    let first = project_completed_tool_rounds(&one_tool_round(
        "Deploy /root/workspace/acme build 123 for 550e8400-e29b-41d4-a716-446655440000",
    ));
    let second = project_completed_tool_rounds(&one_tool_round(
        "Deploy /srv/workspace/acme build 999 for 123e4567-e89b-12d3-a456-426614174000",
    ));

    assert!(first.rounds[0].task_fingerprint.is_some());
    assert_eq!(
        first.rounds[0].task_fingerprint,
        second.rounds[0].task_fingerprint
    );
}

#[test]
fn task_fingerprint_normalizes_uuid_v7_agent_locators() {
    let first = project_completed_tool_rounds(&one_tool_round(
        "Resume session 019ffa87-6e3f-7492-ac83-806c715e8ed4 safely",
    ));
    let second = project_completed_tool_rounds(&one_tool_round(
        "Resume session 019ffb99-7a4e-7c03-bd91-917d826f9fe1 safely",
    ));

    assert_eq!(
        first.rounds[0].task_fingerprint,
        second.rounds[0].task_fingerprint
    );
}

#[test]
fn task_fingerprint_normalizes_numbers_adjacent_to_cjk_text() {
    let first = project_completed_tool_rounds(&one_tool_round("处理任务123并安全部署"));
    let second = project_completed_tool_rounds(&one_tool_round("处理任务456并安全部署"));

    assert_eq!(
        first.rounds[0].task_fingerprint,
        second.rounds[0].task_fingerprint
    );
}

#[test]
fn oversized_human_task_is_not_fingerprinted() {
    let task = "a".repeat(64 * 1024 + 1);
    let projection = project_completed_tool_rounds(&one_tool_round(&task));

    assert_eq!(projection.rounds.len(), 1);
    assert!(projection.rounds[0].task_fingerprint.is_none());
}

#[test]
fn task_fingerprint_keeps_distinct_intents_separate() {
    let deploy = project_completed_tool_rounds(&one_tool_round("Deploy the service safely"));
    let diagnose = project_completed_tool_rounds(&one_tool_round("Diagnose the service latency"));

    assert_ne!(
        deploy.rounds[0].task_fingerprint,
        diagnose.rounds[0].task_fingerprint
    );
}

#[test]
fn unknown_tool_names_are_case_normalized_in_the_pattern() {
    let upper = one_tool_round("Run the custom validation");
    let mut lower = upper.clone();
    lower[1].tool_name = Some("mycustomtool".into());
    let mut upper = upper;
    upper[1].tool_name = Some("MyCustomTool".into());

    assert_eq!(
        project_completed_tool_rounds(&upper).rounds[0].tool_pattern_fingerprint,
        project_completed_tool_rounds(&lower).rounds[0].tool_pattern_fingerprint
    );
}

#[test]
fn rfc3339_final_timestamp_is_normalized_for_repeat_windows() {
    let mut messages = one_tool_round("Run the release validation");
    messages[3].created_at = "2026-08-14T00:00:04Z".into();

    let projection = project_completed_tool_rounds(&messages);

    assert_eq!(
        projection.rounds[0].completed_at.as_deref(),
        Some("00000001786665604000")
    );
}

#[test]
fn corrected_source_timestamp_changes_the_projection_fingerprint() {
    let original = one_tool_round("Run the release validation");
    let mut corrected = original.clone();
    corrected[3].created_at = "2026-08-14T00:00:04Z".into();

    let first = project_completed_tool_rounds(&original);
    let second = project_completed_tool_rounds(&corrected);

    assert_ne!(first.source_fingerprint, second.source_fingerprint);
    assert_ne!(
        first.rounds[0].source_fingerprint,
        second.rounds[0].source_fingerprint
    );
}

#[test]
fn pi_real_tool_call_shape_is_normalized_for_round_projection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pi-session.jsonl");
    let fixture = concat!(
        r#"{"type":"session","version":3,"id":"pi-s1","timestamp":"2026-08-14T00:00:00Z","cwd":"/repo"}"#,
        "\n",
        r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-14T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"run check"}],"timestamp":1}}"#,
        "\n",
        r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-14T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"I will inspect."},{"type":"toolCall","id":"call-1","name":"bash","arguments":{"command":"true"}}],"stopReason":"toolUse","timestamp":2}}"#,
        "\n",
        r#"{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-08-14T00:00:03Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false,"timestamp":3}}"#,
        "\n",
        r#"{"type":"message","id":"a2","parentId":"r1","timestamp":"2026-08-14T00:00:04Z","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop","timestamp":4}}"#,
        "\n",
    );
    std::fs::write(&path, fixture).unwrap();

    let (_, blocks) = parse_transcript_full(&path, false).unwrap();

    let kinds: Vec<(&str, &str)> = blocks
        .iter()
        .map(|block| (block.role.as_str(), block.block_type.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("user", "text"),
            ("assistant", "text"),
            ("assistant", "tool_use"),
            ("user", "tool_result"),
            ("assistant", "text"),
        ]
    );
    assert_eq!(blocks[1].message_uuid.as_deref(), Some("a1"));
    assert_eq!(blocks[2].message_uuid.as_deref(), Some("a1"));
    assert_eq!(blocks[2].tool_name.as_deref(), Some("bash"));
    assert_eq!(blocks[2].tool_use_id.as_deref(), Some("call-1"));
    assert_eq!(blocks[2].content, r#"{"command":"true"}"#);
    assert_eq!(blocks[3].role, "user");
    assert_eq!(blocks[3].message_uuid.as_deref(), Some("r1"));
    assert_eq!(blocks[3].tool_name.as_deref(), Some("bash"));
    assert_eq!(blocks[3].tool_use_id.as_deref(), Some("call-1"));
    assert_eq!(
        blocks[3]
            .meta_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|meta| meta["is_error"].as_bool()),
        Some(false)
    );
    let messages = normalized_messages(&blocks, "pi", path.to_str().unwrap());
    let projection = project_completed_tool_rounds(&messages);
    assert_eq!(projection.rounds.len(), 1);
    assert!(projection.rounds[0].integrity.is_clean());
}

#[test]
fn pi_aborted_assistant_text_does_not_seal_the_round() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pi-aborted.jsonl");
    let fixture = concat!(
        r#"{"type":"session","version":3,"id":"pi-s1","timestamp":"2026-08-14T00:00:00Z","cwd":"/repo"}"#,
        "\n",
        r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-14T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"run check"}],"timestamp":1}}"#,
        "\n",
        r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-14T00:00:02Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"bash","arguments":{"command":"true"}}],"stopReason":"toolUse","timestamp":2}}"#,
        "\n",
        r#"{"type":"message","id":"r1","parentId":"a1","timestamp":"2026-08-14T00:00:03Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false,"timestamp":3}}"#,
        "\n",
        r#"{"type":"message","id":"a2","parentId":"r1","timestamp":"2026-08-14T00:00:04Z","message":{"role":"assistant","content":[{"type":"text","text":"interrupted while finishing"}],"stopReason":"aborted","timestamp":4}}"#,
        "\n",
    );
    std::fs::write(&path, fixture).unwrap();

    let (_, blocks) = parse_transcript_full(&path, false).unwrap();
    let messages = normalized_messages(&blocks, "pi", path.to_str().unwrap());
    let projection = project_completed_tool_rounds(&messages);

    assert_eq!(
        messages
            .last()
            .unwrap()
            .meta_json
            .as_deref()
            .and_then(|raw| {
                serde_json::from_str::<serde_json::Value>(raw)
                    .ok()
                    .and_then(|meta| meta["stop_reason"].as_str().map(str::to_string))
            }),
        Some("aborted".to_string())
    );
    assert!(projection.rounds.is_empty());
    assert_eq!(projection.incomplete_segments, 1);
}

#[test]
fn claude_text_and_tool_use_in_one_message_do_not_close_the_round_early() {
    let messages = vec![
        block!(
            1,
            0,
            Some("user-1"),
            MessageRole::User,
            BlockType::Text,
            "inspect the service",
            None,
            None,
        ),
        block!(
            2,
            0,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::Text,
            "I will inspect it.",
            None,
            None,
        ),
        block!(
            2,
            1,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            r#"{"cmd":"status"}"#,
            Some("exec_command"),
            Some("call-1"),
        ),
        block!(
            3,
            0,
            Some("tool-result"),
            MessageRole::User,
            BlockType::ToolResult,
            "running",
            None,
            Some("call-1"),
        ),
        block!(
            4,
            0,
            Some("assistant-final"),
            MessageRole::Assistant,
            BlockType::Text,
            "The service is running.",
            None,
            None,
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);

    assert_eq!(projection.rounds.len(), 1);
    let round = &projection.rounds[0];
    assert_eq!(round.start_message_uuid.as_deref(), Some("user-1"));
    assert_eq!(round.final_message_uuid.as_deref(), Some("assistant-final"));
    assert_eq!(round.end_line_number, 4);
    assert_eq!(round.tool_call_count, 1);
    assert_eq!(round.matched_result_count, 1);
    assert_eq!(round.tool_names, vec!["shell"]);
    assert!(round.integrity.is_clean());
}

#[test]
fn duplicate_tool_call_ids_do_not_hide_a_missing_result() {
    let messages = vec![
        block!(
            1,
            0,
            Some("user-1"),
            MessageRole::User,
            BlockType::Text,
            "run twice",
            None,
            None,
        ),
        block!(
            2,
            0,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("exec_command"),
            Some("duplicate-id"),
        ),
        block!(
            2,
            1,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("exec_command"),
            Some("duplicate-id"),
        ),
        block!(
            3,
            0,
            Some("tool-result"),
            MessageRole::User,
            BlockType::ToolResult,
            "one result",
            None,
            Some("duplicate-id"),
        ),
        block!(
            4,
            0,
            Some("assistant-final"),
            MessageRole::Assistant,
            BlockType::Text,
            "finished",
            None,
            None,
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);

    assert_eq!(projection.rounds.len(), 1);
    let round = &projection.rounds[0];
    assert_eq!(round.tool_call_count, 2);
    assert_eq!(round.matched_result_count, 1);
    assert_eq!(round.missing_result_count, 1);
    assert!(!round.integrity.is_clean());
}

#[test]
fn codex_commentary_before_tool_call_is_not_a_final_answer() {
    let messages = vec![
        codex_block(1, MessageRole::User, BlockType::Text, "check", None, None),
        codex_block(
            2,
            MessageRole::Assistant,
            BlockType::Text,
            "I am checking.",
            None,
            None,
        ),
        codex_block(
            3,
            MessageRole::Assistant,
            BlockType::ToolUse,
            r#"{"cmd":"status"}"#,
            Some("exec_command"),
            Some("call-1"),
        ),
        codex_block(
            4,
            MessageRole::User,
            BlockType::ToolResult,
            "running",
            None,
            Some("call-1"),
        ),
        codex_block(
            5,
            MessageRole::Assistant,
            BlockType::Text,
            "It is running.",
            None,
            None,
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);

    assert_eq!(projection.rounds.len(), 1);
    let round = &projection.rounds[0];
    assert_eq!(round.end_line_number, 5);
    assert_eq!(round.tool_call_count, 1);
    assert_eq!(round.matched_result_count, 1);
    assert!(round.integrity.is_clean());
}

#[test]
fn codex_commentary_after_tool_result_does_not_seal_the_round() {
    let mut trailing_commentary = codex_block(
        5,
        MessageRole::Assistant,
        BlockType::Text,
        "I am checking one more thing.",
        None,
        None,
    );
    trailing_commentary.meta_json = Some(r#"{"phase":"commentary"}"#.to_string());
    let messages = vec![
        codex_block(1, MessageRole::User, BlockType::Text, "check", None, None),
        codex_block(
            2,
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-1"),
        ),
        codex_block(
            3,
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-1"),
        ),
        trailing_commentary,
    ];

    let projection = project_completed_tool_rounds(&messages);

    assert!(projection.rounds.is_empty());
    assert_eq!(projection.incomplete_segments, 1);
}

#[test]
fn codex_parser_preserves_assistant_message_phase() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let fixture = concat!(
        r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"codex-s1"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-14T00:00:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"still checking"}],"phase":"commentary"}}"#,
        "\n",
    );
    std::fs::write(&path, fixture).unwrap();

    let (_, blocks) = parse_transcript_full(&path, false).unwrap();

    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0]
            .meta_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|meta| meta["phase"].as_str().map(str::to_string))
            .as_deref(),
        Some("commentary")
    );
}

#[test]
fn hook_marker_content_cannot_hide_an_orphan_result() {
    let hook_result = block!(
        1,
        1,
        Some("user-1"),
        MessageRole::User,
        BlockType::ToolResult,
        "mem auto-recall — context injected by hook",
        None,
        Some("hook-context"),
    );
    let messages = vec![
        block!(
            1,
            0,
            Some("user-1"),
            MessageRole::User,
            BlockType::Text,
            "check service",
            None,
            None,
        ),
        hook_result,
        block!(
            2,
            0,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-1"),
        ),
        block!(
            3,
            0,
            Some("tool-result"),
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-1"),
        ),
        block!(
            4,
            0,
            Some("assistant-final"),
            MessageRole::Assistant,
            BlockType::Text,
            "healthy",
            None,
            None,
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);

    assert_eq!(projection.rounds.len(), 1);
    assert_eq!(projection.auxiliary_tool_result_count, 0);
    assert_eq!(projection.rounds[0].orphan_result_count, 1);
    assert!(!projection.rounds[0].integrity.is_clean());
}

#[test]
fn parallel_tool_results_match_by_id_even_when_reordered() {
    let messages = vec![
        block!(
            1,
            0,
            Some("user-1"),
            MessageRole::User,
            BlockType::Text,
            "inspect both",
            None,
            None,
        ),
        block!(
            2,
            0,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("first"),
            Some("call-1"),
        ),
        block!(
            2,
            1,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("second"),
            Some("call-2"),
        ),
        block!(
            3,
            0,
            Some("results"),
            MessageRole::User,
            BlockType::ToolResult,
            "two",
            None,
            Some("call-2"),
        ),
        block!(
            3,
            1,
            Some("results"),
            MessageRole::User,
            BlockType::ToolResult,
            "one",
            None,
            Some("call-1"),
        ),
        block!(
            4,
            0,
            Some("assistant-final"),
            MessageRole::Assistant,
            BlockType::Text,
            "done",
            None,
            None,
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);
    let round = &projection.rounds[0];

    assert_eq!(round.tool_call_count, 2);
    assert_eq!(round.matched_result_count, 2);
    assert_eq!(round.missing_result_count, 0);
    assert_eq!(round.orphan_result_count, 0);
    assert_eq!(round.tool_names, vec!["other"]);
    assert!(round.integrity.is_clean());
}

#[test]
fn missing_result_is_retained_as_a_gapped_round() {
    let messages = vec![
        block!(
            1,
            0,
            Some("user-1"),
            MessageRole::User,
            BlockType::Text,
            "inspect",
            None,
            None,
        ),
        block!(
            2,
            0,
            Some("assistant-tools"),
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-1"),
        ),
        block!(
            3,
            0,
            Some("assistant-final"),
            MessageRole::Assistant,
            BlockType::Text,
            "best effort answer",
            None,
            None,
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);
    let round = &projection.rounds[0];

    assert_eq!(round.matched_result_count, 0);
    assert_eq!(round.missing_result_count, 1);
    assert!(!round.integrity.is_clean());
}

#[test]
fn eof_while_waiting_for_final_answer_emits_no_round() {
    let messages = vec![
        codex_block(1, MessageRole::User, BlockType::Text, "check", None, None),
        codex_block(
            2,
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-1"),
        ),
        codex_block(
            3,
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-1"),
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);

    assert!(projection.rounds.is_empty());
    assert_eq!(projection.incomplete_segments, 1);
}

#[test]
fn reminted_storage_ids_do_not_change_round_identity_or_fingerprint() {
    let original = vec![
        codex_block(1, MessageRole::User, BlockType::Text, "check", None, None),
        codex_block(
            2,
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-1"),
        ),
        codex_block(
            3,
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-1"),
        ),
        codex_block(
            4,
            MessageRole::Assistant,
            BlockType::Text,
            "healthy",
            None,
            None,
        ),
    ];
    let reminted: Vec<_> = original
        .iter()
        .enumerate()
        .map(|(index, message)| ConversationMessage {
            message_block_id: format!("new-{index}"),
            ..message.clone()
        })
        .collect();

    let first = project_completed_tool_rounds(&original);
    let second = project_completed_tool_rounds(&reminted);

    assert_eq!(first.rounds[0].round_id, second.rounds[0].round_id);
    assert_eq!(
        first.rounds[0].source_fingerprint,
        second.rounds[0].source_fingerprint
    );
    assert_eq!(first.source_fingerprint, second.source_fingerprint);
}

#[test]
fn next_human_seals_the_previous_round_and_starts_the_next_one() {
    let messages = vec![
        codex_block(1, MessageRole::User, BlockType::Text, "first", None, None),
        codex_block(
            2,
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-1"),
        ),
        codex_block(
            3,
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-1"),
        ),
        codex_block(
            4,
            MessageRole::Assistant,
            BlockType::Text,
            "first done",
            None,
            None,
        ),
        codex_block(5, MessageRole::User, BlockType::Text, "second", None, None),
        codex_block(
            6,
            MessageRole::Assistant,
            BlockType::ToolUse,
            "{}",
            Some("health"),
            Some("call-2"),
        ),
        codex_block(
            7,
            MessageRole::User,
            BlockType::ToolResult,
            "ok",
            None,
            Some("call-2"),
        ),
        codex_block(
            8,
            MessageRole::Assistant,
            BlockType::Text,
            "second done",
            None,
            None,
        ),
    ];

    let projection = project_completed_tool_rounds(&messages);

    assert_eq!(projection.rounds.len(), 2);
    assert_eq!(projection.rounds[0].start_line_number, 1);
    assert_eq!(projection.rounds[0].seal_kind.as_db_str(), "next_human");
    assert_eq!(projection.rounds[1].start_line_number, 5);
    assert_eq!(projection.rounds[1].seal_kind.as_db_str(), "stream_eof");
}
