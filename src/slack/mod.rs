mod api;
mod formatting;
pub(crate) mod handlers;

use crate::agent::{Agent, AgentHandle};
use crate::config::{AgentKind, Config};
use slack_morphism::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
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

// ── Sub-structs ───────────────────────────────────────────────────────

/// Slack API connectivity, identity, channel mapping, rate limiting, and event dedup.
pub struct SlackContext {
    pub token: SlackApiToken,
    pub client: Arc<SlackHyperClient>,
    pub bot_user_id: String,
    pub repo_channels: tokio::sync::RwLock<HashMap<String, String>>,
    pub(crate) rate_limiter: Mutex<HashMap<String, Instant>>,
    pub(crate) seen_messages: Mutex<HashMap<String, Instant>>,
}

impl SlackContext {
    /// Look up which repo a channel belongs to.
    pub async fn repo_for_channel(&self, channel_id: &str) -> Option<String> {
        let channels = self.repo_channels.read().await;
        channels
            .iter()
            .find(|(_, cid)| cid.as_str() == channel_id)
            .map(|(name, _)| name.clone())
    }

    /// Deduplicate a message event. Returns `true` if this is a duplicate.
    pub async fn is_duplicate(&self, message_ts: &str) -> bool {
        let mut seen = self.seen_messages.lock().await;
        if seen.contains_key(message_ts) {
            return true;
        }
        seen.insert(message_ts.to_string(), Instant::now());
        // Prune entries older than 5 minutes to prevent unbounded growth.
        if seen.len() > 100 {
            let cutoff = std::time::Duration::from_secs(5 * 60);
            seen.retain(|_, ts| ts.elapsed() < cutoff);
        }
        false
    }

    /// Wait if needed to respect Slack's per-channel rate limit before posting.
    pub async fn rate_limit(&self, channel_id: &str, interval_ms: u64) {
        let min_interval = std::time::Duration::from_millis(interval_ms);
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

/// A message queued while the agent is busy processing a turn.
pub struct QueuedMessage {
    pub text: String,
    pub message_ts: String,
}

/// Per-thread runtime state (all keyed by `thread_ts`).
pub struct ThreadState {
    pub handles: Mutex<HashMap<String, AgentHandle>>,
    pub kill_senders: Mutex<HashMap<String, oneshot::Sender<()>>>,
    pub plans: Mutex<HashMap<String, String>>,
    pub pending_answers: Mutex<HashMap<String, oneshot::Sender<String>>>,
    pub pending_approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    pub models: Mutex<HashMap<String, String>>,
    pub in_progress: Mutex<HashSet<String>>,
    pub queued_messages: Mutex<HashMap<String, Vec<QueuedMessage>>>,
}

impl ThreadState {
    /// Remove all state for a thread (handles, kill_senders, plans, models,
    /// pending_answers, pending_approvals, queued_messages). Does NOT touch `in_progress`.
    pub async fn cleanup(&self, thread_ts: &str) {
        self.handles.lock().await.remove(thread_ts);
        self.kill_senders.lock().await.remove(thread_ts);
        self.plans.lock().await.remove(thread_ts);
        self.models.lock().await.remove(thread_ts);
        self.queued_messages.lock().await.remove(thread_ts);
        // Dropping the senders unblocks process_events → ChannelClosed.
        self.pending_answers.lock().await.remove(thread_ts);
        self.pending_approvals.lock().await.remove(thread_ts);
    }

    /// Drain kill_senders (sending kill signals), clear handles/plans/pending_answers.
    pub async fn shutdown(&self) {
        let mut kill_senders = self.kill_senders.lock().await;
        let count = kill_senders.len();
        for (thread_ts, kill_tx) in kill_senders.drain() {
            let _ = kill_tx.send(());
            info!("Killed agent for thread {}", thread_ts);
        }
        if count > 0 {
            info!("Cleaned up {} agent process(es)", count);
        }
        self.handles.lock().await.clear();
        self.plans.lock().await.clear();
        self.pending_answers.lock().await.clear();
        self.queued_messages.lock().await.clear();
    }

    /// Returns the number of active agent handles.
    pub async fn active_count(&self) -> usize {
        self.handles.lock().await.len()
    }
}

/// Sync coordination guards for local CLI session sync.
pub struct SyncGuard {
    pub pending_repos: Mutex<HashMap<String, Instant>>,
    pub pending_session_ids: Mutex<HashSet<String>>,
}

impl SyncGuard {
    /// Atomically check both maps; inserts session_id if available.
    /// Returns `true` if the claim succeeded.
    pub async fn try_claim(&self, repo_name: &str, session_id: &str) -> bool {
        let pending_repos = self.pending_repos.lock().await;
        let mut pending_sessions = self.pending_session_ids.lock().await;

        if pending_repos.contains_key(repo_name) || pending_sessions.contains(session_id) {
            return false;
        }

        pending_sessions.insert(session_id.to_string());
        true
    }

    /// Remove a session ID from pending_session_ids.
    pub async fn release(&self, session_id: &str) {
        self.pending_session_ids.lock().await.remove(session_id);
    }

    /// Remove repos that have been pending for >5 minutes.
    pub async fn cleanup_stale(&self) {
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
}

// ── AppState ──────────────────────────────────────────────────────────

/// Shared application state passed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub agents: Arc<HashMap<AgentKind, Arc<dyn Agent>>>,
    pub sessions: crate::session::SessionStore,
    pub slack: Arc<SlackContext>,
    pub threads: Arc<ThreadState>,
    pub sync: Arc<SyncGuard>,
}

impl AppState {
    pub fn is_allowed_user(&self, user_id: &str) -> bool {
        self.config
            .slack
            .allowed_users
            .contains(&user_id.to_string())
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

        // Delegate to SyncGuard for the atomic check.
        self.sync.try_claim(repo_name, session_id).await
    }

    /// Whether we're in "live" streaming mode (edit messages in real-time).
    pub fn is_live_mode(&self) -> bool {
        self.config.defaults.streaming_mode == crate::config::StreamingMode::Live
    }

    /// Resolve the model for a thread: thread override > repo config > global default.
    pub async fn resolved_model(&self, repo_name: &str, thread_ts: Option<&str>) -> String {
        if let Some(ts) = thread_ts
            && let Some(m) = self.threads.models.lock().await.get(ts)
        {
            return m.clone();
        }
        match self.config.repos.get(repo_name) {
            Some(repo) => repo.resolved_model(&self.config.defaults),
            None => crate::config::DEFAULT_MODEL.to_string(),
        }
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
    if user_id == state.slack.bot_user_id {
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

    // Deduplicate: Socket Mode can redeliver the same event if ack is slow.
    if state.slack.is_duplicate(&message_ts).await {
        info!("Ignoring duplicate event for message {}", message_ts);
        return;
    }

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
