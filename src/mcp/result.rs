use rmcp::model::{CallToolResult, Content};
use serde_json::Value;

pub fn ok_json(value: &Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    CallToolResult::success(vec![Content::text(text)])
}

pub fn ok_json_with_content(notice: &str, content: &str, value: &Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let message = format!("{}: {}\n\n{}", notice, content, text);
    CallToolResult::success(vec![Content::text(message)])
}

pub fn err_text(message: impl Into<String>) -> CallToolResult {
    let mut result = CallToolResult::success(vec![Content::text(message.into())]);
    result.is_error = Some(true);
    result
}
