use std::collections::{BTreeMap, HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::domain::{
    BlockType, CompletedToolRound, CompletedToolRoundProjection, ConversationMessage, MessageRole,
    RoundIntegrity, RoundSealKind, SourceAdapter, COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
    COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
};

static URL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)https?://[^\s]+\b").expect("valid task URL regex"));
static UUID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?-u:\b)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[89ab][0-9a-f]{3}-[0-9a-f]{12}(?-u:\b)",
    )
    .expect("valid task UUID regex")
});
static PATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:[a-z]:\\[^\s]+|/(?:[^\s/]+/)+[^\s]+)").expect("valid task path regex")
});
static LONG_HEX_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?-u:\b)[0-9a-f]{12,}(?-u:\b)").expect("valid task long-hex regex")
});
static NUMBER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?-u:\b)\d+(?-u:\b)").expect("valid task number regex"));
const MAX_TASK_SIGNAL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StreamKey {
    tenant: String,
    caller_agent: String,
    session_id: Option<String>,
    transcript_path: String,
}

#[derive(Debug)]
struct MessageGroup<'a> {
    message_uuid: Option<&'a str>,
    line_number: u64,
    blocks: Vec<&'a ConversationMessage>,
}

impl MessageGroup<'_> {
    fn is_human_input(&self) -> bool {
        self.blocks.iter().any(|block| {
            block.role == MessageRole::User
                && block.block_type == BlockType::Text
                && !block.content.trim().is_empty()
        })
    }

    fn has_agent_activity(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| block.role == MessageRole::Assistant)
    }

    fn has_tool_use(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| block.block_type == BlockType::ToolUse)
    }

    fn final_text_block(&self) -> Option<&ConversationMessage> {
        if self.has_tool_use() {
            return None;
        }
        self.blocks.iter().rev().copied().find(|block| {
            block.role == MessageRole::Assistant
                && block.block_type == BlockType::Text
                && !block.content.trim().is_empty()
                && is_final_answer_phase(block)
        })
    }
}

fn is_final_answer_phase(block: &ConversationMessage) -> bool {
    let metadata = block
        .meta_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    if let Some(phase) = metadata.as_ref().and_then(|meta| meta["phase"].as_str()) {
        return phase == "final_answer";
    }
    match metadata
        .as_ref()
        .and_then(|meta| meta["stop_reason"].as_str())
    {
        Some("stop" | "end_turn") => true,
        Some(_) => false,
        None => true,
    }
}

/// Project normalized transcript blocks into deterministic, rebuildable tool
/// round locators. The input may contain multiple tenants, agents, sessions,
/// and transcript files; streams are isolated before projection.
pub fn project_completed_tool_rounds(
    messages: &[ConversationMessage],
) -> CompletedToolRoundProjection {
    let mut streams: BTreeMap<StreamKey, Vec<&ConversationMessage>> = BTreeMap::new();
    for message in messages {
        streams
            .entry(StreamKey {
                tenant: message.tenant.clone(),
                caller_agent: message.caller_agent.clone(),
                session_id: message.session_id.clone(),
                transcript_path: message.transcript_path.clone(),
            })
            .or_default()
            .push(message);
    }

    let mut projection = CompletedToolRoundProjection {
        source_block_count: messages.len() as u64,
        ..CompletedToolRoundProjection::default()
    };
    let mut source_hash = Sha256::new();
    for (stream, mut blocks) in streams {
        blocks.sort_by(|left, right| {
            (left.line_number, left.block_index, &left.message_block_id).cmp(&(
                right.line_number,
                right.block_index,
                &right.message_block_id,
            ))
        });
        hash_stream_source(&mut source_hash, &stream, &blocks);
        let groups = group_messages(&blocks);
        project_stream(&stream, &groups, &mut projection);
    }
    projection.source_fingerprint = format!("{:x}", source_hash.finalize());
    projection
}

fn hash_stream_source(hash: &mut Sha256, stream: &StreamKey, blocks: &[&ConversationMessage]) {
    hash.update(b"mem.completed_tool_round.stream.v3");
    for value in [
        stream.tenant.as_str(),
        stream.caller_agent.as_str(),
        stream.session_id.as_deref().unwrap_or(""),
        stream.transcript_path.as_str(),
    ] {
        update_length_prefixed(hash, value);
    }
    for block in blocks {
        hash_block(hash, block);
    }
}

fn group_messages<'a>(blocks: &[&'a ConversationMessage]) -> Vec<MessageGroup<'a>> {
    let mut groups: Vec<MessageGroup<'a>> = Vec::new();
    for block in blocks {
        let same_as_last = groups.last().is_some_and(|group| {
            match (group.message_uuid, block.message_uuid.as_deref()) {
                (Some(left), Some(right)) => left == right,
                (None, None) => group.line_number == block.line_number,
                _ => false,
            }
        });
        if same_as_last {
            groups
                .last_mut()
                .expect("last group exists")
                .blocks
                .push(block);
        } else {
            groups.push(MessageGroup {
                message_uuid: block.message_uuid.as_deref(),
                line_number: block.line_number,
                blocks: vec![block],
            });
        }
    }
    groups
}

fn project_stream(
    stream: &StreamKey,
    groups: &[MessageGroup<'_>],
    projection: &mut CompletedToolRoundProjection,
) {
    let mut segment: Vec<&MessageGroup<'_>> = Vec::new();
    let mut has_agent_activity = false;

    for group in groups {
        if group.is_human_input() {
            if segment.is_empty() || !has_agent_activity {
                segment.push(group);
                continue;
            }
            finish_segment(stream, &segment, RoundSealKind::NextHuman, projection);
            segment.clear();
            segment.push(group);
            has_agent_activity = false;
            continue;
        }
        if !segment.is_empty() {
            has_agent_activity |= group.has_agent_activity();
            segment.push(group);
        }
    }

    if !segment.is_empty() {
        finish_segment(stream, &segment, RoundSealKind::StreamEof, projection);
    }
}

fn finish_segment(
    stream: &StreamKey,
    segment: &[&MessageGroup<'_>],
    seal_kind: RoundSealKind,
    projection: &mut CompletedToolRoundProjection,
) {
    let tool_call_count = segment
        .iter()
        .flat_map(|group| &group.blocks)
        .filter(|block| block.block_type == BlockType::ToolUse)
        .count() as u32;
    if tool_call_count == 0 {
        projection.skipped_without_tools += 1;
        return;
    }

    let Some((final_group_index, final_block)) = segment
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, group)| group.final_text_block().map(|block| (index, block)))
    else {
        projection.incomplete_segments += 1;
        return;
    };

    let has_tool_activity_after_final = segment[final_group_index + 1..]
        .iter()
        .flat_map(|group| &group.blocks)
        .any(|block| matches!(block.block_type, BlockType::ToolUse | BlockType::ToolResult));
    if has_tool_activity_after_final {
        projection.incomplete_segments += 1;
        return;
    }

    let Some(start_block) = segment
        .iter()
        .flat_map(|group| &group.blocks)
        .find(|block| {
            block.role == MessageRole::User
                && block.block_type == BlockType::Text
                && !block.content.trim().is_empty()
        })
    else {
        projection.incomplete_segments += 1;
        return;
    };

    let stats = tool_stats(segment);
    projection.auxiliary_tool_result_count += stats.auxiliary_tool_result_count as u64;
    let integrity = if stats.missing_result_count == 0 && stats.orphan_result_count == 0 {
        RoundIntegrity::Clean
    } else {
        RoundIntegrity::Gapped
    };
    let round_id = stable_round_id(stream, start_block);
    let source_fingerprint = source_fingerprint(segment);
    let task_fingerprint = task_fingerprint(segment);
    let tool_pattern_fingerprint = tool_pattern_fingerprint(segment);

    projection.rounds.push(CompletedToolRound {
        round_id,
        tenant: stream.tenant.clone(),
        caller_agent: stream.caller_agent.clone(),
        source_adapter: SourceAdapter::from_caller_agent(&stream.caller_agent),
        session_id: stream.session_id.clone(),
        transcript_path: stream.transcript_path.clone(),
        start_line_number: start_block.line_number,
        start_block_index: start_block.block_index,
        end_line_number: final_block.line_number,
        end_block_index: final_block.block_index,
        start_message_uuid: start_block.message_uuid.clone(),
        final_message_uuid: final_block.message_uuid.clone(),
        tool_call_ids: stats.tool_call_ids,
        tool_names: stats.tool_names,
        tool_call_count,
        matched_result_count: stats.matched_result_count,
        missing_result_count: stats.missing_result_count,
        orphan_result_count: stats.orphan_result_count,
        error_result_count: stats.error_result_count,
        unknown_result_status_count: stats.unknown_result_status_count,
        completed_at: normalize_event_timestamp(&final_block.created_at),
        seal_kind,
        integrity,
        source_fingerprint,
        task_fingerprint,
        task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        tool_pattern_fingerprint,
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
    });
}

fn task_fingerprint(segment: &[&MessageGroup<'_>]) -> Option<String> {
    let mut human_text = String::new();
    for content in segment
        .iter()
        .flat_map(|group| &group.blocks)
        .filter(|block| block.role == MessageRole::User && block.block_type == BlockType::Text)
        .map(|block| block.content.trim())
        .filter(|content| !content.is_empty())
    {
        let separator_bytes = usize::from(!human_text.is_empty());
        if human_text
            .len()
            .saturating_add(separator_bytes)
            .saturating_add(content.len())
            > MAX_TASK_SIGNAL_BYTES
        {
            return None;
        }
        if !human_text.is_empty() {
            human_text.push('\n');
        }
        human_text.push_str(content);
    }
    let normalized = normalize_task_text(&human_text)?;
    Some(versioned_fingerprint(
        COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        &normalized,
    ))
}

fn normalize_task_text(input: &str) -> Option<String> {
    let redacted = crate::pipeline::redact::redact_all(input);
    let lowered = redacted.nfkc().collect::<String>().to_lowercase();
    let normalized = URL_RE.replace_all(&lowered, " <url> ");
    let normalized = UUID_RE.replace_all(&normalized, " <uuid> ");
    let normalized = PATH_RE.replace_all(&normalized, " <path> ");
    let normalized = LONG_HEX_RE.replace_all(&normalized, " <hex> ");
    let normalized = NUMBER_RE.replace_all(&normalized, " <num> ");
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let has_meaningful_text = normalized
        .chars()
        .any(|character| character.is_alphabetic());
    (has_meaningful_text && normalized.len() >= 8).then_some(normalized)
}

fn normalize_event_timestamp(input: &str) -> Option<String> {
    let value = input.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse::<u128>()
            .ok()
            .map(|millis| format!("{millis:020}"));
    }
    let millis = chrono::DateTime::parse_from_rfc3339(value)
        .ok()?
        .timestamp_millis();
    (millis >= 0).then(|| format!("{:020}", millis as u64))
}

fn tool_pattern_fingerprint(segment: &[&MessageGroup<'_>]) -> String {
    let pattern = segment
        .iter()
        .flat_map(|group| &group.blocks)
        .filter(|block| block.block_type == BlockType::ToolUse)
        .map(|block| canonical_tool_family(block.tool_name.as_deref().unwrap_or("unknown")))
        .collect::<Vec<_>>()
        .join("\0");
    versioned_fingerprint(COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION, &pattern)
}

fn canonical_tool_family(name: &str) -> String {
    let normalized = name.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "bash" | "shell" | "exec" | "exec_command" | "run_command" => "shell".into(),
        "read" | "read_file" | "view_file" => "read_file".into(),
        "grep" | "rg" | "search" | "search_files" => "search_files".into(),
        "write" | "write_file" | "edit" | "apply_patch" => "edit_file".into(),
        _ => "other".into(),
    }
}

fn versioned_fingerprint(version: u32, value: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(version.to_le_bytes());
    hash.update([0]);
    hash.update(value.as_bytes());
    format!("{:x}", hash.finalize())
}

#[derive(Default)]
struct ToolStats {
    tool_call_ids: Vec<String>,
    tool_names: Vec<String>,
    matched_result_count: u32,
    missing_result_count: u32,
    orphan_result_count: u32,
    error_result_count: u32,
    unknown_result_status_count: u32,
    auxiliary_tool_result_count: u32,
}

fn tool_stats(segment: &[&MessageGroup<'_>]) -> ToolStats {
    let mut stats = ToolStats::default();
    let mut pending: HashMap<&str, (u32, u32)> = HashMap::new();
    let mut unnamed_calls = 0u32;
    let mut seen_names = HashSet::new();

    for block in segment.iter().flat_map(|group| &group.blocks) {
        match block.block_type {
            BlockType::ToolUse => {
                if let Some(id) = block.tool_use_id.as_deref() {
                    stats.tool_call_ids.push(id.to_string());
                    pending
                        .entry(id)
                        .and_modify(|(call_count, _)| *call_count += 1)
                        .or_insert((1, 0));
                } else {
                    unnamed_calls += 1;
                }
                if let Some(name) = block.tool_name.as_deref() {
                    let family = canonical_tool_family(name);
                    if seen_names.insert(family.clone()) {
                        stats.tool_names.push(family);
                    }
                }
            }
            BlockType::ToolResult => {
                let matched = block
                    .tool_use_id
                    .as_deref()
                    .and_then(|id| pending.get_mut(id))
                    .is_some_and(|(call_count, matched_count)| {
                        if *matched_count >= *call_count {
                            false
                        } else {
                            *matched_count += 1;
                            true
                        }
                    });
                if matched {
                    stats.matched_result_count += 1;
                    match tool_result_is_error(block) {
                        Some(true) => stats.error_result_count += 1,
                        Some(false) => {}
                        None => stats.unknown_result_status_count += 1,
                    }
                } else {
                    stats.orphan_result_count += 1;
                }
            }
            _ => {}
        }
    }

    stats.missing_result_count = unnamed_calls
        + pending
            .values()
            .map(|(call_count, matched_count)| call_count - matched_count)
            .sum::<u32>();
    stats
}

fn tool_result_is_error(block: &ConversationMessage) -> Option<bool> {
    block
        .meta_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.get("is_error").and_then(|flag| flag.as_bool()))
}

fn stable_round_id(stream: &StreamKey, start: &ConversationMessage) -> String {
    let mut name = b"mem.completed_tool_round.id.v3".to_vec();
    for value in [
        stream.tenant.as_str(),
        stream.caller_agent.as_str(),
        stream.session_id.as_deref().unwrap_or(""),
        stream.transcript_path.as_str(),
    ] {
        append_length_prefixed(&mut name, value);
    }
    if let Some(message_uuid) = start.message_uuid.as_deref() {
        name.push(1);
        append_length_prefixed(&mut name, message_uuid);
    } else {
        name.push(0);
        name.extend_from_slice(&start.line_number.to_le_bytes());
        name.extend_from_slice(&start.block_index.to_le_bytes());
    }
    format!("round_{}", Uuid::new_v5(&Uuid::NAMESPACE_OID, &name))
}

fn source_fingerprint(segment: &[&MessageGroup<'_>]) -> String {
    let mut hash = Sha256::new();
    for block in segment.iter().flat_map(|group| &group.blocks) {
        hash_block(&mut hash, block);
    }
    format!("{:x}", hash.finalize())
}

fn hash_block(hash: &mut Sha256, block: &ConversationMessage) {
    hash.update(b"mem.completed_tool_round.block.v3");
    for value in [
        block.message_uuid.as_deref().unwrap_or(""),
        block.role.as_db_str(),
        block.block_type.as_db_str(),
        block.tool_name.as_deref().unwrap_or(""),
        block.tool_use_id.as_deref().unwrap_or(""),
        &block.content,
        block.meta_json.as_deref().unwrap_or(""),
        &block.created_at,
    ] {
        update_length_prefixed(hash, value);
    }
    hash.update(block.line_number.to_le_bytes());
    hash.update(block.block_index.to_le_bytes());
}

fn update_length_prefixed(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value.as_bytes());
}

fn append_length_prefixed(output: &mut Vec<u8>, value: &str) {
    output.extend_from_slice(&(value.len() as u64).to_le_bytes());
    output.extend_from_slice(value.as_bytes());
}
