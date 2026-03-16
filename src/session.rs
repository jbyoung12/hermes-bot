//! Persistent session storage for tracking Claude Code sessions.
//!
//! The [`SessionStore`] maintains a mapping from Slack threads to Claude Code
//! sessions, persisted to disk as JSON. This allows sessions to survive restarts
//! and enables the bot to resume conversations.
//!
//! # Features
//!
//! - Thread-safe concurrent access (RwLock)
//! - Atomic writes (temp file + rename)
//! - Automatic pruning of stale/expired sessions
//! - Session recovery after crashes

use crate::config::AgentKind;
use crate::error::{HermesError, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::error;

fn serialize_as_active<S: serde::Serializer>(s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str("active")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Error,
    /// Legacy: old sessions may have "stopped" on disk. Deserializes from
    /// "stopped" but serializes back as "active" so it can round-trip.
    #[serde(
        rename(deserialize = "stopped"),
        serialize_with = "serialize_as_active"
    )]
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The agent's session ID (used for --resume).
    pub session_id: String,
    /// Repo name from config.
    pub repo: String,
    /// Absolute path to the repo.
    pub repo_path: PathBuf,
    /// Which agent backend.
    pub agent_kind: AgentKind,
    /// Slack channel ID.
    pub channel_id: String,
    /// Thread timestamp (Slack's thread_ts) — identifies the thread.
    pub thread_ts: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub status: SessionStatus,
    pub total_turns: u32,
    /// Model used for this session (for display; CLI remembers on --resume).
    #[serde(default)]
    pub model: Option<String>,
}

/// Thread-safe persistent session store backed by a JSON file.
///
/// Manages active Claude Code sessions, tracking which Slack threads
/// correspond to which agent sessions. Persists to disk atomically
/// to survive restarts.
///
/// # Thread Safety
///
/// All methods are async and internally use RwLock for concurrent access.
/// Multiple readers can access simultaneously; writes are exclusive.
#[derive(Clone)]
pub struct SessionStore {
    /// Key: thread_ts (Slack thread identifier)
    sessions: Arc<RwLock<HashMap<String, SessionInfo>>>,
    path: PathBuf,
}

impl SessionStore {
    /// Creates a new session store, loading existing sessions from disk if present.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the JSON file for persisting sessions
    ///
    /// # Returns
    ///
    /// A new `SessionStore` instance. If the file doesn't exist or is invalid,
    /// starts with an empty session map (does not error).
    /// Creates a new session store using blocking I/O for initial load.
    pub fn new(path: PathBuf) -> Self {
        let sessions = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => match serde_json::from_str(&contents) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse session file '{}', starting fresh: {}",
                            path.display(),
                            e
                        );
                        HashMap::new()
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "Failed to read session file '{}', starting fresh: {}",
                        path.display(),
                        e
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

        Self {
            sessions: Arc::new(RwLock::new(sessions)),
            path,
        }
    }

    /// Inserts a new session and persists to disk atomically.
    ///
    /// # Arguments
    ///
    /// * `session` - Session information to store
    ///
    /// # Errors
    ///
    /// Returns an error if the write to disk fails (filesystem error, permissions, etc.)
    #[must_use = "session insert errors mean the session won't persist across restarts"]
    pub async fn insert(&self, session: SessionInfo) -> Result<()> {
        let key = session.thread_ts.clone();
        let json = {
            let mut sessions = self.sessions.write().await;
            sessions.insert(key, session);
            serde_json::to_string_pretty(&*sessions)?
        };
        self.write_to_disk(&json).await
    }

    /// Retrieves a session by its Slack thread timestamp.
    ///
    /// # Arguments
    ///
    /// * `thread_ts` - Slack thread timestamp (unique identifier for the thread)
    ///
    /// # Returns
    ///
    /// The session if found, or `None` if no session exists for this thread
    pub async fn get_by_thread(&self, thread_ts: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(thread_ts).cloned()
    }

    /// Updates a session in-place and persists to disk atomically.
    ///
    /// # Arguments
    ///
    /// * `thread_ts` - Thread timestamp of the session to update
    /// * `f` - Closure that modifies the session
    ///
    /// # Errors
    ///
    /// Returns `SessionNotFound` if the thread doesn't exist, or a write error
    /// if persistence fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// store.update("1234.5678", |session| {
    ///     session.total_turns += 1;
    ///     session.last_active = Utc::now();
    /// }).await?;
    /// ```
    #[must_use = "session update errors mean changes won't persist to disk"]
    pub async fn update<F>(&self, thread_ts: &str, f: F) -> Result<()>
    where
        F: FnOnce(&mut SessionInfo),
    {
        let json = {
            let mut sessions = self.sessions.write().await;
            let session = sessions
                .get_mut(thread_ts)
                .ok_or_else(|| HermesError::SessionNotFound(thread_ts.to_string()))?;
            f(session);
            serde_json::to_string_pretty(&*sessions)?
        };
        self.write_to_disk(&json).await
    }

    pub async fn active_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .filter(|s| s.status != SessionStatus::Error)
            .cloned()
            .collect()
    }

    /// Checks if any session has the given agent session ID.
    ///
    /// Used to prevent duplicate sessions and detect races in session sync.
    ///
    /// # Arguments
    ///
    /// * `session_id` - Agent session ID to search for
    ///
    /// # Returns
    ///
    /// `true` if a session with this ID exists, `false` otherwise
    pub async fn has_session_id(&self, session_id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions.values().any(|s| s.session_id == session_id)
    }

    /// Remove sessions whose channel_id doesn't match the current channel for their repo.
    pub async fn prune_stale_channels(&self, repo_channels: &HashMap<String, String>) {
        let json = {
            let mut sessions = self.sessions.write().await;
            let before = sessions.len();
            sessions.retain(|_, s| match repo_channels.get(&s.repo) {
                Some(current_channel) => s.channel_id == *current_channel,
                None => false, // Repo no longer configured.
            });
            let pruned = before - sessions.len();
            if pruned == 0 {
                return;
            }
            tracing::info!("Pruned {} stale session(s) from previous run", pruned);
            match serde_json::to_string_pretty(&*sessions) {
                Ok(j) => j,
                Err(e) => {
                    error!("Failed to serialize sessions after pruning: {}", e);
                    return;
                }
            }
        };
        if let Err(e) = self.write_to_disk(&json).await {
            error!("Failed to persist after pruning: {}", e);
        }
    }

    /// Remove sessions whose last_active is older than the TTL.
    pub async fn prune_expired(&self, ttl_days: i64) {
        let cutoff = Utc::now() - Duration::days(ttl_days);
        let json = {
            let mut sessions = self.sessions.write().await;
            let before = sessions.len();
            sessions.retain(|_, s| s.last_active > cutoff);
            let pruned = before - sessions.len();
            if pruned == 0 {
                return;
            }
            tracing::info!(
                "Pruned {} expired session(s) (older than {} days)",
                pruned,
                ttl_days
            );
            match serde_json::to_string_pretty(&*sessions) {
                Ok(j) => j,
                Err(e) => {
                    error!("Failed to serialize sessions after pruning: {}", e);
                    return;
                }
            }
        };
        if let Err(e) = self.write_to_disk(&json).await {
            error!("Failed to persist after pruning expired sessions: {}", e);
        }
    }

    /// Write pre-serialized JSON to disk asynchronously.
    /// Uses atomic write (temp file + rename) to avoid corruption.
    async fn write_to_disk(&self, json: &str) -> Result<()> {
        let tmp_path = self.path.with_extension("json.tmp");
        if let Err(e) = tokio::fs::write(&tmp_path, json).await {
            error!("Failed to write temp session file: {}", e);
            // Clean up partial temp file.
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }
        if let Err(e) = tokio::fs::rename(&tmp_path, &self.path).await {
            error!("Failed to rename temp session file: {}", e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(session_id: &str, thread_ts: &str, repo: &str) -> SessionInfo {
        SessionInfo {
            session_id: session_id.to_string(),
            repo: repo.to_string(),
            repo_path: PathBuf::from("/tmp"),
            agent_kind: AgentKind::Claude,
            channel_id: "C123".to_string(),
            thread_ts: thread_ts.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            status: SessionStatus::Active,
            total_turns: 0,
            model: None,
        }
    }

    fn temp_store() -> (SessionStore, PathBuf) {
        let path = std::env::temp_dir().join(format!("hermes_test_{}.json", unique_id()));
        let store = SessionStore::new(path.clone());
        (store, path)
    }

    fn unique_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    #[tokio::test]
    async fn test_insert_and_get() {
        let (store, path) = temp_store();
        let session = make_session("s1", "t1", "repo1");
        store.insert(session.clone()).await.unwrap();

        let retrieved = store.get_by_thread("t1").await.unwrap();
        assert_eq!(retrieved.session_id, "s1");
        assert_eq!(retrieved.repo, "repo1");

        assert!(store.get_by_thread("nonexistent").await.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_update() {
        let (store, path) = temp_store();
        store
            .insert(make_session("s1", "t1", "repo1"))
            .await
            .unwrap();

        store
            .update("t1", |s| {
                s.total_turns = 5;
                s.status = SessionStatus::Error;
            })
            .await
            .unwrap();

        let retrieved = store.get_by_thread("t1").await.unwrap();
        assert_eq!(retrieved.total_turns, 5);
        assert_eq!(retrieved.status, SessionStatus::Error);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_update_nonexistent_returns_error() {
        let (store, path) = temp_store();
        let result = store.update("nonexistent", |_| {}).await;
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_active_sessions() {
        let (store, path) = temp_store();
        store
            .insert(make_session("s1", "t1", "repo1"))
            .await
            .unwrap();

        let mut errored = make_session("s2", "t2", "repo1");
        errored.status = SessionStatus::Error;
        store.insert(errored).await.unwrap();

        let active = store.active_sessions().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "s1");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_has_session_id() {
        let (store, path) = temp_store();
        store
            .insert(make_session("s1", "t1", "repo1"))
            .await
            .unwrap();

        assert!(store.has_session_id("s1").await);
        assert!(!store.has_session_id("s999").await);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_persistence_survives_reload() {
        let (store, path) = temp_store();
        store
            .insert(make_session("s1", "t1", "repo1"))
            .await
            .unwrap();

        // Create a new store from the same file.
        let store2 = SessionStore::new(path.clone());
        let retrieved = store2.get_by_thread("t1").await.unwrap();
        assert_eq!(retrieved.session_id, "s1");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_prune_stale_channels() {
        let (store, path) = temp_store();
        store
            .insert(make_session("s1", "t1", "repo1"))
            .await
            .unwrap();

        let mut s2 = make_session("s2", "t2", "repo2");
        s2.channel_id = "C999".to_string();
        store.insert(s2).await.unwrap();

        // Only repo1 with C123 is current.
        let mut repo_channels = HashMap::new();
        repo_channels.insert("repo1".to_string(), "C123".to_string());

        store.prune_stale_channels(&repo_channels).await;

        assert!(store.get_by_thread("t1").await.is_some());
        assert!(store.get_by_thread("t2").await.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_prune_expired() {
        let (store, path) = temp_store();

        // Recent session — should survive.
        store
            .insert(make_session("s1", "t1", "repo1"))
            .await
            .unwrap();

        // Old session — should be pruned.
        let mut old = make_session("s2", "t2", "repo1");
        old.last_active = Utc::now() - Duration::days(10);
        store.insert(old).await.unwrap();

        store.prune_expired(7).await;

        assert!(store.get_by_thread("t1").await.is_some());
        assert!(store.get_by_thread("t2").await.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_new_with_nonexistent_file() {
        let path = std::env::temp_dir().join("hermes_test_nonexistent_12345.json");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::new(path.clone());

        assert!(store.active_sessions().await.is_empty());
    }
}
