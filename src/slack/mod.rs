mod api;
mod formatting;
pub(crate) mod handlers;

use crate::agent::{Agent, AgentHandle};
use crate::config::{AgentKind, Config};
use slack_morphism::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use tokio::time::Instant;
use tracing::{info, warn};

// Re-export public API.
pub use api::{ensure_repo_channels, post_channel_message, post_thread_reply};
pub use formatting::split_for_slack;
pub use handlers::handle_slash_command;

/// Slack's approximate max message length (characters) for `chat.postMessage`.
/// Configurable via tuning.slack_max_message_chars, defaults to 39000.
pub(crate) const SLACK_MAX_MESSAGE_CHARS: usize = 39_000;
/// Slack's max text length for `chat.update` (the API enforces ~4 000 chars).
/// This is a Slack API limitation and cannot be configured.
pub(crate) const SLACK_UPDATE_MAX_CHARS: usize = 3_900;
/// Max length of the fallback title (truncated at word boundary).
const FALLBACK_TITLE_MAX_LEN: usize = 80;

/// Shared application state passed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub sessions: crate::session::SessionStore,
    pub agents: Arc<HashMap<AgentKind, Arc<dyn Agent>>>,
    pub bot_token: SlackApiToken,
    pub slack_client: Arc<SlackHyperClient>,
    /// Repo name → channel ID mapping (populated on startup).
    pub repo_channels: Arc<tokio::sync::RwLock<HashMap<String, String>>>,
    /// Bot's own user ID (to filter self-messages).
    pub bot_user_id: Arc<String>,
    /// Concurrency guard: set of thread_ts currently being processed.
    pub in_progress: Arc<Mutex<HashSet<String>>>,
    /// Repos with in-flight new message processing (prevents sync from duplicating threads).
    /// Tracks repo name → timestamp when marked pending for cleanup.
    pub pending_repos: Arc<Mutex<HashMap<String, Instant>>>,
    /// Session IDs currently being claimed (not yet persisted to SessionStore).
    /// Prevents race between sync and handle_new_message where both try to claim the same session.
    pub pending_session_ids: Arc<Mutex<HashSet<String>>>,
    /// Running agent processes: thread_ts → AgentHandle.
    pub agent_handles: Arc<Mutex<HashMap<String, AgentHandle>>>,
    /// Kill senders: thread_ts → oneshot kill signal.
    /// Stored separately so `!stop` can always reach the kill signal
    /// even when the handle is borrowed by a running task.
    pub kill_senders: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    /// Last detected plan content per thread: thread_ts → plan text.
    pub last_plan: Arc<Mutex<HashMap<String, String>>>,
    /// Pending question answers: thread_ts → oneshot sender for the user's reply.
    pub pending_answers: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    /// Pending tool approvals: thread_ts → oneshot sender for the user's allow/deny.
    pub pending_tool_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// Per-channel rate limiter: channel_id → last write time.
    pub rate_limiter: Arc<Mutex<HashMap<String, Instant>>>,
    /// Per-thread model overrides from `/model` command: thread_ts → model ID.
    pub thread_models: Arc<Mutex<HashMap<String, String>>>,
}

impl AppState {
    pub fn is_allowed_user(&self, user_id: &str) -> bool {
        self.config
            .slack
            .allowed_users
            .contains(&user_id.to_string())
    }

    /// Look up which repo a channel belongs to.
    pub async fn repo_for_channel(&self, channel_id: &str) -> Option<String> {
        let channels = self.repo_channels.read().await;
        channels
            .iter()
            .find(|(_, cid)| cid.as_str() == channel_id)
            .map(|(name, _)| name.clone())
    }

    /// Atomically claim a new session for sync if:
    /// 1. The session_id is not already owned by Hermes (in SessionStore or pending)
    /// 2. The repo doesn't have an in-flight new message
    ///
    /// Returns true if the claim succeeded, false if already claimed.
    /// On success, the session_id is added to pending_session_ids (caller must remove it later).
    pub async fn try_claim_session_for_sync(&self, repo_name: &str, session_id: &str) -> bool {
        // First check if session already exists (read-only, no lock held).
        if self.sessions.has_session_id(session_id).await {
            return false;
        }

        // Now atomically check and claim using both locks together to prevent TOCTOU.
        // We need both locks to ensure atomicity.
        let pending_repos = self.pending_repos.lock().await;
        let mut pending_sessions = self.pending_session_ids.lock().await;

        // Check if repo is pending or session is pending.
        if pending_repos.contains_key(repo_name) || pending_sessions.contains(session_id) {
            return false;
        }

        // Both checks passed — claim the session.
        pending_sessions.insert(session_id.to_string());
        true
    }

    /// Release a claimed session ID (called after session is persisted to SessionStore).
    pub async fn release_claimed_session(&self, session_id: &str) {
        self.pending_session_ids.lock().await.remove(session_id);
    }

    /// Clean up repos that have been pending for too long (>5 minutes).
    /// This handles cases where PendingRepoGuard drop fails silently.
    pub async fn cleanup_stale_pending_repos(&self) {
        const MAX_PENDING_DURATION: std::time::Duration = std::time::Duration::from_secs(5 * 60);

        let mut pending = self.pending_repos.lock().await;
        let before = pending.len();
        pending.retain(|repo, timestamp| {
            let elapsed = timestamp.elapsed();
            if elapsed > MAX_PENDING_DURATION {
                warn!(
                    "Cleaning up stale pending repo '{}' (pending for {:?})",
                    repo, elapsed
                );
                false
            } else {
                true
            }
        });
        let cleaned = before - pending.len();
        if cleaned > 0 {
            info!("Cleaned up {} stale pending repo(s)", cleaned);
        }
    }

    /// Whether we're in "live" streaming mode (edit messages in real-time).
    pub fn is_live_mode(&self) -> bool {
        self.config.defaults.streaming_mode == crate::config::StreamingMode::Live
    }

    /// Resolve the model for a thread: thread override > repo config > global default.
    pub async fn resolved_model(&self, repo_name: &str, thread_ts: Option<&str>) -> String {
        if let Some(ts) = thread_ts {
            if let Some(m) = self.thread_models.lock().await.get(ts) {
                return m.clone();
            }
        }
        match self.config.repos.get(repo_name) {
            Some(repo) => repo.resolved_model(&self.config.defaults),
            None => crate::config::DEFAULT_MODEL.to_string(),
        }
    }

    /// Wait if needed to respect Slack's per-channel rate limit before posting.
    pub(crate) async fn rate_limit(&self, channel_id: &str) {
        let min_interval =
            std::time::Duration::from_millis(self.config.tuning.rate_limit_interval_ms);
        let mut limiter = self.rate_limiter.lock().await;
        if let Some(last) = limiter.get(channel_id) {
            let elapsed = last.elapsed();
            if elapsed < min_interval {
                let wait = min_interval - elapsed;
                drop(limiter); // Release lock while sleeping.
                tokio::time::sleep(wait).await;
                limiter = self.rate_limiter.lock().await;
            }
        }
        limiter.insert(channel_id.to_string(), Instant::now());
    }
}

/// Handle an incoming Slack message event.
pub async fn handle_message(state: AppState, event: SlackMessageEvent) {
    let channel_id = match &event.origin.channel {
        Some(c) => c.to_string(),
        None => return,
    };

    let user_id = match event.sender.user.as_ref() {
        Some(u) => u.to_string(),
        None => return,
    };

    // Ignore bot's own messages.
    if user_id == state.bot_user_id.as_str() {
        return;
    }

    // Only process regular user messages (ignore subtypes like channel_join, message_changed, etc.).
    if let Some(ref subtype) = event.subtype {
        info!(
            "Ignoring message with subtype {:?} in channel {}",
            subtype, channel_id
        );
        return;
    }

    // Check authorization.
    if !state.is_allowed_user(&user_id) {
        tracing::warn!(
            "Unauthorized message from user {} in channel {}",
            user_id,
            channel_id
        );
        return;
    }

    let text = match event.content.as_ref().and_then(|c| c.text.as_ref()) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            info!(
                "Ignoring message without text content from user {} in channel {}",
                user_id, channel_id
            );
            return;
        }
    };

    let message_ts = event.origin.ts.to_string();

    // Determine if this is a thread reply or a new top-level message.
    if let Some(thread_ts) = event.origin.thread_ts.as_ref() {
        let thread_ts = thread_ts.to_string();
        info!(
            "Thread reply from {} in channel {} (thread {}): {}",
            user_id,
            channel_id,
            thread_ts,
            &text[..crate::util::floor_char_boundary(
                &text,
                state.config.tuning.log_preview_max_len
            )]
        );
        handlers::handle_thread_reply(state, channel_id, thread_ts, message_ts, text).await;
    } else {
        info!(
            "New message from {} in channel {}: {}",
            user_id,
            channel_id,
            &text[..crate::util::floor_char_boundary(
                &text,
                state.config.tuning.log_preview_max_len
            )]
        );
        handlers::handle_new_message(state, channel_id, message_ts, user_id, text).await;
    }
}
