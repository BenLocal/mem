use rmcp::model::{CallToolResult, Content};
use serde_json::Value;

pub fn ok_json(value: &Value) -> CallToolResult {
    let text =
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    CallToolResult::success(vec![Content::text(text)])
}

pub fn err_text(message: impl Into<String>) -> CallToolResult {
    let mut result = CallToolResult::success(vec![Content::text(message.into())]);
    result.is_error = Some(true);
    result
}
