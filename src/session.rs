//! Persistent session storage for tracking Claude Code sessions.
//!
//! The [`SessionStore`] maintains a mapping from Slack threads to Claude Code
//! sessions, persisted in a SQLite database. This allows sessions to survive
//! restarts and enables the bot to resume conversations.
//!
//! # Features
//!
//! - Thread-safe concurrent access (Mutex<Connection>)
//! - Per-row updates (no full-file rewrites)
//! - Indexed lookups on session_id
//! - WAL mode for concurrent readers
//! - Automatic migration from legacy sessions.json

use crate::config::AgentKind;
use crate::error::{HermesError, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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

impl SessionStatus {
    fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Active | SessionStatus::Stopped => "active",
            SessionStatus::Error => "error",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "error" => SessionStatus::Error,
            "stopped" => SessionStatus::Stopped,
            _ => SessionStatus::Active,
        }
    }
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

/// Thread-safe persistent session store backed by SQLite.
///
/// Manages active Claude Code sessions, tracking which Slack threads
/// correspond to which agent sessions. Uses WAL mode for concurrent
/// read access and per-row updates.
///
/// # Thread Safety
///
/// All methods are async and use `spawn_blocking` with a sync `Mutex`
/// to avoid holding locks across await points.
#[derive(Clone)]
pub struct SessionStore {
    conn: Arc<Mutex<Connection>>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    thread_ts    TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    repo         TEXT NOT NULL,
    repo_path    TEXT NOT NULL,
    agent_kind   TEXT NOT NULL DEFAULT 'claude',
    channel_id   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    last_active  TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'active',
    total_turns  INTEGER NOT NULL DEFAULT 0,
    model        TEXT
);
CREATE INDEX IF NOT EXISTS idx_sessions_session_id ON sessions(session_id);
";

impl SessionStore {
    /// Creates a new session store, opening or creating the SQLite database.
    ///
    /// If a legacy `sessions.json` file exists next to the database path,
    /// it will be migrated automatically.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the SQLite database file
    ///
    /// # Returns
    ///
    /// A new `SessionStore` instance.
    pub fn new(path: PathBuf) -> Self {
        let conn = Connection::open(&path).unwrap_or_else(|e| {
            panic!("Failed to open SQLite database '{}': {}", path.display(), e);
        });

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .unwrap_or_else(|e| {
                panic!("Failed to set SQLite pragmas: {}", e);
            });

        conn.execute_batch(SCHEMA).unwrap_or_else(|e| {
            panic!("Failed to create sessions schema: {}", e);
        });

        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };

        // Attempt JSON migration
        store.migrate_from_json(&path);

        store
    }

    /// Migrate sessions from a legacy JSON file if one exists.
    fn migrate_from_json(&self, db_path: &std::path::Path) {
        // Look for sessions.json in the same directory as the DB file
        let json_path = db_path.with_extension("json");
        // Also check if the original path was .json (shouldn't happen post-migration,
        // but handle the edge case of a path like "sessions.json" being passed)
        let candidates = [json_path];

        for candidate in &candidates {
            if !candidate.exists() {
                continue;
            }

            let contents = match std::fs::read_to_string(candidate) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "Found legacy session file '{}' but failed to read it: {}",
                        candidate.display(),
                        e
                    );
                    continue;
                }
            };

            let sessions: HashMap<String, SessionInfo> = match serde_json::from_str(&contents) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        "Found legacy session file '{}' but failed to parse it: {}",
                        candidate.display(),
                        e
                    );
                    continue;
                }
            };

            if sessions.is_empty() {
                // Remove empty JSON file
                let backup = candidate.with_extension("json.bak");
                if let Err(e) = std::fs::rename(candidate, &backup) {
                    tracing::warn!("Failed to rename empty legacy file: {}", e);
                }
                continue;
            }

            let conn = self.conn.lock().unwrap();
            let result = (|| -> std::result::Result<usize, rusqlite::Error> {
                let tx = conn.unchecked_transaction()?;
                let mut count = 0;
                for (thread_ts, session) in &sessions {
                    tx.execute(
                        "INSERT OR IGNORE INTO sessions (thread_ts, session_id, repo, repo_path, agent_kind, channel_id, created_at, last_active, status, total_turns, model) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            thread_ts,
                            session.session_id,
                            session.repo,
                            session.repo_path.to_string_lossy().to_string(),
                            "claude",
                            session.channel_id,
                            session.created_at.to_rfc3339(),
                            session.last_active.to_rfc3339(),
                            session.status.as_str(),
                            session.total_turns,
                            session.model,
                        ],
                    )?;
                    count += 1;
                }
                tx.commit()?;
                Ok(count)
            })();

            drop(conn);

            match result {
                Ok(count) => {
                    tracing::info!(
                        "Migrated {} session(s) from '{}' to SQLite",
                        count,
                        candidate.display()
                    );
                    let backup = candidate.with_extension("json.bak");
                    if let Err(e) = std::fs::rename(candidate, &backup) {
                        tracing::warn!(
                            "Failed to rename '{}' to '{}': {}",
                            candidate.display(),
                            backup.display(),
                            e
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to migrate sessions from '{}': {} (continuing without migration)",
                        candidate.display(),
                        e
                    );
                }
            }
        }
    }

    /// Inserts a new session into the database.
    #[must_use = "session insert errors mean the session won't persist across restarts"]
    pub async fn insert(&self, session: SessionInfo) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO sessions (thread_ts, session_id, repo, repo_path, agent_kind, channel_id, created_at, last_active, status, total_turns, model) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    session.thread_ts,
                    session.session_id,
                    session.repo,
                    session.repo_path.to_string_lossy().to_string(),
                    "claude",
                    session.channel_id,
                    session.created_at.to_rfc3339(),
                    session.last_active.to_rfc3339(),
                    session.status.as_str(),
                    session.total_turns,
                    session.model,
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap()
    }

    /// Retrieves a session by its Slack thread timestamp.
    pub async fn get_by_thread(&self, thread_ts: &str) -> Option<SessionInfo> {
        let conn = self.conn.clone();
        let thread_ts = thread_ts.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            row_to_session(&conn, &thread_ts)
        })
        .await
        .unwrap()
    }

    /// Updates a session in-place within a transaction.
    ///
    /// # Arguments
    ///
    /// * `thread_ts` - Thread timestamp of the session to update
    /// * `f` - Closure that modifies the session
    ///
    /// # Errors
    ///
    /// Returns `SessionNotFound` if the thread doesn't exist.
    #[must_use = "session update errors mean changes won't persist to disk"]
    pub async fn update<F>(&self, thread_ts: &str, f: F) -> Result<()>
    where
        F: FnOnce(&mut SessionInfo) + Send + 'static,
    {
        let conn = self.conn.clone();
        let thread_ts = thread_ts.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut session = row_to_session(&conn, &thread_ts)
                .ok_or_else(|| HermesError::SessionNotFound(thread_ts.clone()))?;
            f(&mut session);
            conn.execute(
                "UPDATE sessions SET session_id=?1, repo=?2, repo_path=?3, agent_kind=?4, channel_id=?5, created_at=?6, last_active=?7, status=?8, total_turns=?9, model=?10 WHERE thread_ts=?11",
                params![
                    session.session_id,
                    session.repo,
                    session.repo_path.to_string_lossy().to_string(),
                    "claude",
                    session.channel_id,
                    session.created_at.to_rfc3339(),
                    session.last_active.to_rfc3339(),
                    session.status.as_str(),
                    session.total_turns,
                    session.model,
                    thread_ts,
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap()
    }

    pub async fn active_sessions(&self) -> Vec<SessionInfo> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT thread_ts, session_id, repo, repo_path, agent_kind, channel_id, created_at, last_active, status, total_turns, model FROM sessions WHERE status != 'error'")
                .unwrap();
            stmt.query_map([], row_mapper)
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        })
        .await
        .unwrap()
    }

    /// Checks if any session has the given agent session ID (indexed lookup).
    pub async fn has_session_id(&self, session_id: &str) -> bool {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1 LIMIT 1)",
                    params![session_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            exists
        })
        .await
        .unwrap()
    }

    /// Remove sessions whose channel_id doesn't match the current channel for their repo.
    pub async fn prune_stale_channels(&self, repo_channels: &HashMap<String, String>) {
        let conn = self.conn.clone();
        let repo_channels = repo_channels.clone();
        let result = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            // Read all sessions, determine which to delete
            let mut stmt = conn
                .prepare("SELECT thread_ts, repo, channel_id FROM sessions")
                .unwrap();
            let stale: Vec<String> = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap()
                .filter_map(|r| r.ok())
                .filter(|(_, repo, channel_id)| match repo_channels.get(repo) {
                    Some(current_channel) => channel_id != current_channel,
                    None => true, // Repo no longer configured
                })
                .map(|(thread_ts, _, _)| thread_ts)
                .collect();

            if stale.is_empty() {
                return 0usize;
            }

            let count = stale.len();
            for thread_ts in &stale {
                if let Err(e) = conn.execute(
                    "DELETE FROM sessions WHERE thread_ts = ?1",
                    params![thread_ts],
                ) {
                    error!("Failed to delete stale session '{}': {}", thread_ts, e);
                }
            }
            count
        })
        .await
        .unwrap();

        if result > 0 {
            tracing::info!("Pruned {} stale session(s) from previous run", result);
        }
    }

    /// Remove sessions whose last_active is older than the TTL.
    pub async fn prune_expired(&self, ttl_days: i64) {
        let conn = self.conn.clone();
        let result = tokio::task::spawn_blocking(move || {
            let cutoff = Utc::now() - Duration::days(ttl_days);
            let cutoff_str = cutoff.to_rfc3339();
            let conn = conn.lock().unwrap();
            conn.execute(
                "DELETE FROM sessions WHERE last_active < ?1",
                params![cutoff_str],
            )
        })
        .await
        .unwrap();

        match result {
            Ok(count) if count > 0 => {
                tracing::info!(
                    "Pruned {} expired session(s) (older than {} days)",
                    count,
                    ttl_days
                );
            }
            Err(e) => {
                error!("Failed to prune expired sessions: {}", e);
            }
            _ => {}
        }
    }
}

/// Read a single session row by thread_ts.
fn row_to_session(conn: &Connection, thread_ts: &str) -> Option<SessionInfo> {
    conn.query_row(
        "SELECT thread_ts, session_id, repo, repo_path, agent_kind, channel_id, created_at, last_active, status, total_turns, model FROM sessions WHERE thread_ts = ?1",
        params![thread_ts],
        row_mapper,
    )
    .ok()
}

/// Map a row to SessionInfo.
fn row_mapper(row: &rusqlite::Row) -> rusqlite::Result<SessionInfo> {
    let thread_ts: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let repo: String = row.get(2)?;
    let repo_path: String = row.get(3)?;
    let _agent_kind: String = row.get(4)?;
    let channel_id: String = row.get(5)?;
    let created_at: String = row.get(6)?;
    let last_active: String = row.get(7)?;
    let status: String = row.get(8)?;
    let total_turns: u32 = row.get(9)?;
    let model: Option<String> = row.get(10)?;

    Ok(SessionInfo {
        session_id,
        repo,
        repo_path: PathBuf::from(repo_path),
        agent_kind: AgentKind::Claude,
        channel_id,
        thread_ts,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        last_active: DateTime::parse_from_rfc3339(&last_active)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        status: SessionStatus::from_str(&status),
        total_turns,
        model,
    })
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
        let path = std::env::temp_dir().join(format!("hermes_test_{}.db", unique_id()));
        let store = SessionStore::new(path.clone());
        (store, path)
    }

    fn unique_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    fn cleanup_db(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
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

        cleanup_db(&path);
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

        cleanup_db(&path);
    }

    #[tokio::test]
    async fn test_update_nonexistent_returns_error() {
        let (store, path) = temp_store();
        let result = store.update("nonexistent", |_| {}).await;
        assert!(result.is_err());

        cleanup_db(&path);
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

        cleanup_db(&path);
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

        cleanup_db(&path);
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

        cleanup_db(&path);
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

        cleanup_db(&path);
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

        cleanup_db(&path);
    }

    #[tokio::test]
    async fn test_new_with_nonexistent_file() {
        let path = std::env::temp_dir().join("hermes_test_nonexistent_12345.db");
        cleanup_db(&path);
        let store = SessionStore::new(path.clone());

        assert!(store.active_sessions().await.is_empty());

        cleanup_db(&path);
    }

    #[tokio::test]
    async fn test_json_migration() {
        let db_path = std::env::temp_dir().join(format!("hermes_test_migrate_{}.db", unique_id()));
        let json_path = db_path.with_extension("json");

        // Create a legacy JSON file
        let mut sessions = HashMap::new();
        sessions.insert("t1".to_string(), make_session("s1", "t1", "repo1"));
        sessions.insert("t2".to_string(), make_session("s2", "t2", "repo2"));
        let json = serde_json::to_string_pretty(&sessions).unwrap();
        std::fs::write(&json_path, &json).unwrap();

        // Open the store — should migrate
        let store = SessionStore::new(db_path.clone());

        // Verify sessions were migrated
        assert!(store.get_by_thread("t1").await.is_some());
        assert!(store.get_by_thread("t2").await.is_some());

        // Verify JSON file was renamed to .bak
        assert!(!json_path.exists());
        assert!(db_path.with_extension("json.bak").exists());

        cleanup_db(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("json.bak"));
    }
}
