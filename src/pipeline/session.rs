use crate::domain::session::Session;
use crate::storage::duckdb::DuckDbRepository;
use crate::storage::StorageError;

/// Read `MEM_SESSION_IDLE_MINUTES` from the environment.  Returns 30 if unset or invalid.
pub fn idle_minutes_from_env() -> u64 {
    std::env::var("MEM_SESSION_IDLE_MINUTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(30)
}

/// Return the number of whole minutes between two zero-padded 20-digit millisecond timestamps
/// (the format produced by `current_timestamp()` in `memory_service`).
/// Returns `None` if either string cannot be parsed as `u64`.
pub fn minutes_since(from: &str, to: &str) -> Option<i64> {
    let from_ms: u64 = from.trim_start_matches('0').parse().ok().or_else(|| {
        if from.chars().all(|c| c == '0') {
            Some(0)
        } else {
            None
        }
    })?;
    let to_ms: u64 = to.trim_start_matches('0').parse().ok().or_else(|| {
        if to.chars().all(|c| c == '0') {
            Some(0)
        } else {
            None
        }
    })?;
    let diff_ms = to_ms as i64 - from_ms as i64;
    Some(diff_ms / 60_000)
}

pub enum SessionDecision {
    Continue(String),
    OpenNew { previous: Option<String> },
}

/// Pure decision: given the latest session (if any) and the current timestamp,
/// decide whether to continue the existing session or open a new one.
pub fn decide_session(
    latest: Option<(&str, &str)>,
    now: &str,
    idle_minutes: u64,
) -> SessionDecision {
    match latest {
        Some((id, last_seen)) => {
            let elapsed = minutes_since(last_seen, now).unwrap_or(i64::MAX);
            if elapsed >= 0 && (elapsed as u64) < idle_minutes {
                SessionDecision::Continue(id.to_string())
            } else {
                SessionDecision::OpenNew {
                    previous: Some(id.to_string()),
                }
            }
        }
        None => SessionDecision::OpenNew { previous: None },
    }
}

/// Resolve the session for the given `(tenant, caller_agent)` pair.
/// Opens a new session if none is active or the most recent one has been idle
/// for at least `idle_minutes`.  Closes the stale session first when opening new.
pub async fn resolve_session(
    repo: &DuckDbRepository,
    tenant: &str,
    caller_agent: &str,
    now: &str,
    idle_minutes: u64,
) -> Result<String, StorageError> {
    let latest: Option<Session> = repo.latest_active_session(tenant, caller_agent).await?;

    let decision = decide_session(
        latest
            .as_ref()
            .map(|s| (s.session_id.as_str(), s.last_seen_at.as_str())),
        now,
        idle_minutes,
    );

    match decision {
        SessionDecision::Continue(id) => Ok(id),
        SessionDecision::OpenNew { previous } => {
            if let Some(prev) = previous {
                repo.close_session(&prev, now).await?;
            }
            let new_id = uuid::Uuid::now_v7().to_string();
            repo.open_session(&new_id, tenant, caller_agent, now)
                .await?;
            Ok(new_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Zero-padded 20-digit millisecond timestamps as produced by current_timestamp().
    // T_BASE  = 2026-04-29T08:00:00Z in milliseconds
    // T_PLUS_5  = T_BASE + 5 minutes
    // T_PLUS_60 = T_BASE + 60 minutes
    const T_BASE: &str = "00000001777449600000";
    const T_PLUS_5: &str = "00000001777449900000";
    const T_PLUS_60: &str = "00000001777453200000";

    #[test]
    fn decide_session_continue_when_within_idle() {
        let result = decide_session(Some(("sid_a", T_BASE)), T_PLUS_5, 30);
        match result {
            SessionDecision::Continue(id) => assert_eq!(id, "sid_a"),
            _ => panic!("expected Continue, got OpenNew"),
        }
    }

    #[test]
    fn decide_session_open_new_when_idle_exceeded() {
        let result = decide_session(Some(("sid_a", T_BASE)), T_PLUS_60, 30);
        match result {
            SessionDecision::OpenNew { previous } => {
                assert_eq!(previous.as_deref(), Some("sid_a"))
            }
            _ => panic!("expected OpenNew, got Continue"),
        }
    }

    #[test]
    fn decide_session_open_new_when_no_existing() {
        let result = decide_session(None, T_BASE, 30);
        match result {
            SessionDecision::OpenNew { previous } => assert!(previous.is_none()),
            _ => panic!("expected OpenNew with previous=None"),
        }
    }

    #[test]
    fn minutes_since_basic_math() {
        assert_eq!(minutes_since(T_BASE, T_PLUS_5), Some(5));
        assert_eq!(minutes_since(T_BASE, T_PLUS_60), Some(60));
    }

    #[test]
    fn minutes_since_invalid_returns_none() {
        assert_eq!(minutes_since("not-a-timestamp", T_BASE), None);
        assert_eq!(minutes_since(T_BASE, "also-not"), None);
    }

    #[test]
    fn idle_minutes_default_when_unset() {
        // SAFETY: env var mutation; tests using env vars share process state.
        // Same pattern used in retrieve.rs's MEM_RANKER kill switch test.
        unsafe {
            std::env::remove_var("MEM_SESSION_IDLE_MINUTES");
        }
        assert_eq!(idle_minutes_from_env(), 30);
    }
}
