//! Test utilities for integration tests.

use hermes_bot::{
    config::AgentKind,
    session::{SessionInfo, SessionStatus},
};
use std::path::PathBuf;

/// Create a test session
pub fn make_test_session(session_id: &str, thread_ts: &str, repo: &str) -> SessionInfo {
    SessionInfo {
        session_id: session_id.to_string(),
        repo: repo.to_string(),
        repo_path: PathBuf::from("/tmp"),
        agent_kind: AgentKind::Claude,
        channel_id: "C123".to_string(),
        thread_ts: thread_ts.to_string(),
        created_at: chrono::Utc::now(),
        last_active: chrono::Utc::now(),
        status: SessionStatus::Active,
        total_turns: 1,
        model: None,
    }
}
