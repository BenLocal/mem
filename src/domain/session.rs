use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub session_id: String,
    pub tenant: String,
    pub caller_agent: String,
    pub started_at: String,
    pub last_seen_at: String,
    pub ended_at: Option<String>,
    pub goal: Option<String>,
    pub memory_count: u32,
}
