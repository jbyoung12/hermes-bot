//! Integration tests for Hermes core functionality.
//!
//! These tests verify session management and core utility functions.

mod common;

use common::make_test_session;
use hermes_bot::session::SessionStore;

#[tokio::test]
async fn test_session_persistence() {
    // Test that sessions can be inserted and retrieved
    let path = std::env::temp_dir().join(format!("test_session_{}.json", rand::random::<u64>()));
    let store = SessionStore::new(path.clone());

    let session = make_test_session("s1", "t1", "test-repo");
    store.insert(session.clone()).await.unwrap();

    let retrieved = store.get_by_thread("t1").await.unwrap();
    assert_eq!(retrieved.session_id, "s1");
    assert_eq!(retrieved.repo, "test-repo");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_session_update() {
    // Test that sessions can be updated
    let path = std::env::temp_dir().join(format!("test_session_{}.json", rand::random::<u64>()));
    let store = SessionStore::new(path.clone());

    store
        .insert(make_test_session("s1", "t1", "test-repo"))
        .await
        .unwrap();

    store
        .update("t1", |s| {
            s.total_turns = 5;
        })
        .await
        .unwrap();

    let retrieved = store.get_by_thread("t1").await.unwrap();
    assert_eq!(retrieved.total_turns, 5);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_active_sessions_filter() {
    // Test that active_sessions filters out errored sessions
    let path = std::env::temp_dir().join(format!("test_session_{}.json", rand::random::<u64>()));
    let store = SessionStore::new(path.clone());

    let session1 = make_test_session("s1", "t1", "test-repo");
    let mut session2 = make_test_session("s2", "t2", "test-repo");
    session2.status = hermes_bot::session::SessionStatus::Error;

    store.insert(session1).await.unwrap();
    store.insert(session2).await.unwrap();

    let active = store.active_sessions().await;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].session_id, "s1");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_session_ttl_pruning() {
    // Test that old sessions are pruned
    let path = std::env::temp_dir().join(format!("test_session_{}.json", rand::random::<u64>()));
    let store = SessionStore::new(path.clone());

    let mut old_session = make_test_session("s1", "t1", "test-repo");
    old_session.last_active = chrono::Utc::now() - chrono::Duration::days(10);

    let new_session = make_test_session("s2", "t2", "test-repo");

    store.insert(old_session).await.unwrap();
    store.insert(new_session).await.unwrap();

    // Prune sessions older than 7 days
    store.prune_expired(7).await;

    assert!(store.get_by_thread("t1").await.is_none());
    assert!(store.get_by_thread("t2").await.is_some());

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_session_has_session_id() {
    // Test session ID lookup
    let path = std::env::temp_dir().join(format!("test_session_{}.json", rand::random::<u64>()));
    let store = SessionStore::new(path.clone());

    store
        .insert(make_test_session("s1", "t1", "test-repo"))
        .await
        .unwrap();

    assert!(store.has_session_id("s1").await);
    assert!(!store.has_session_id("nonexistent").await);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_session_persistence_across_reload() {
    // Test that sessions survive a reload
    let path = std::env::temp_dir().join(format!("test_session_{}.json", rand::random::<u64>()));

    {
        let store = SessionStore::new(path.clone());
        store
            .insert(make_test_session("s1", "t1", "test-repo"))
            .await
            .unwrap();
    }

    // Create a new store from the same file
    let store2 = SessionStore::new(path.clone());
    let retrieved = store2.get_by_thread("t1").await.unwrap();
    assert_eq!(retrieved.session_id, "s1");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_markdown_to_slack_formatting() {
    // Test markdown conversion
    use hermes_bot::slack::split_for_slack;

    let text = "**bold** and _italic_";
    let chunks = split_for_slack(text, 100);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0], text);
}

#[tokio::test]
async fn test_split_for_slack_long_text() {
    // Test message splitting for long text
    use hermes_bot::slack::split_for_slack;

    let text = "a".repeat(100);
    let chunks = split_for_slack(&text, 50);
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|c| c.len() <= 50));
}
