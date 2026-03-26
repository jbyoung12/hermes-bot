use crate::config::AgentKind;
use crate::session::{SessionInfo, SessionStatus};
use crate::slack::AppState;
use chrono::Utc;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use tracing::{debug, error, info, warn};

/// Encode a repo path the same way Claude Code does for project directories.
fn encode_project_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

/// A single conversation turn extracted from a .jsonl file.
struct ConversationTurn {
    prompt: String,
    response: Option<String>,
}

/// Result of parsing a .jsonl transcript file.
struct ConversationData {
    turns: Vec<ConversationTurn>,
}

/// Check if a "user" entry is a real human message (string content)
/// vs. a tool_result (array content).
fn is_human_message(val: &serde_json::Value) -> Option<String> {
    val.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

/// Read conversation turns from a session .jsonl transcript.
/// A "turn" = one human message + all assistant text responses until the next human message.
fn read_conversation(jsonl_path: &Path) -> ConversationData {
    let mut turns: Vec<ConversationTurn> = Vec::new();
    let mut current_prompt: Option<String> = None;
    let mut current_response_parts: Vec<String> = Vec::new();
    // Track the last plan per turn (plans can be rewritten multiple times).
    let mut current_plan: Option<String> = None;

    let file = match std::fs::File::open(jsonl_path) {
        Ok(f) => f,
        Err(e) => {
            debug!("Cannot open session file {}: {}", jsonl_path.display(), e);
            return ConversationData { turns };
        }
    };

    for line in std::io::BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match val.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                // Only real human messages (string content) start a new turn.
                // Tool results (array content) are intermediate and ignored.
                if let Some(prompt) = is_human_message(&val) {
                    // Save previous turn if any.
                    if let Some(prev_prompt) = current_prompt.take() {
                        // Append the last plan (if any) before finalizing.
                        if let Some(plan) = current_plan.take() {
                            current_response_parts.push(format!("*Plan*\n\n{}", plan));
                        }
                        let response = if current_response_parts.is_empty() {
                            None
                        } else {
                            Some(current_response_parts.join("\n\n"))
                        };
                        turns.push(ConversationTurn {
                            prompt: prev_prompt,
                            response,
                        });
                        current_response_parts.clear();
                    }
                    current_prompt = Some(prompt);
                }
            }
            Some("assistant") if current_prompt.is_some() => {
                // Collect all text blocks from this assistant message.
                if let Some(text) = extract_assistant_text(&val) {
                    current_response_parts.push(text);
                }
                // Track the last plan Write per turn (overwrites previous drafts).
                if let Some(plan) = extract_plan_content(&val) {
                    current_plan = Some(plan);
                }
            }
            _ => {}
        }
    }

    // Save last turn.
    if let Some(prompt) = current_prompt {
        if let Some(plan) = current_plan.take() {
            current_response_parts.push(format!("*Plan*\n\n{}", plan));
        }
        let response = if current_response_parts.is_empty() {
            None
        } else {
            Some(current_response_parts.join("\n\n"))
        };
        turns.push(ConversationTurn { prompt, response });
    }

    ConversationData { turns }
}

/// Check if a .jsonl session file is a sidechain (subagent) session.
/// Reads up to the first 20 lines looking for the `isSidechain` field.
fn is_sidechain_session(jsonl_path: &Path) -> bool {
    let file = match std::fs::File::open(jsonl_path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    for line in std::io::BufReader::new(file).lines().take(20) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if val.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            return true;
        }
    }
    false
}

/// Extract plan content from Write tool_use blocks targeting `.claude/plans/`.
/// Returns the content of the last matching Write block (plans can be rewritten).
fn extract_plan_content(val: &serde_json::Value) -> Option<String> {
    let content = val
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;

    let mut last_plan: Option<String> = None;

    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        if block.get("name").and_then(|n| n.as_str()) != Some("Write") {
            continue;
        }
        let input = match block.get("input") {
            Some(i) => i,
            None => continue,
        };
        let file_path = match input.get("file_path").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => continue,
        };
        if !file_path.contains(".claude/plans/") {
            continue;
        }
        if let Some(plan_text) = input.get("content").and_then(|c| c.as_str()) {
            last_plan = Some(plan_text.to_string());
        }
    }

    last_plan
}

/// Extract text content from an assistant message entry.
fn extract_assistant_text(val: &serde_json::Value) -> Option<String> {
    val.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|block| {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        block.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        })
}

/// Extract a session ID from a .jsonl file path.
fn session_id_from_path(path: &Path) -> Option<String> {
    if path.extension().is_none_or(|e| e != "jsonl") {
        return None;
    }
    path.file_stem().map(|s| s.to_string_lossy().to_string())
}

/// Per-session sync state: what we've already posted to Slack.
struct SyncState {
    thread_ts: String,
    completed_turns: usize,
    /// Byte length of the last completed turn's response when we posted it.
    /// Used to detect when a turn's response grows (e.g. plan → execution).
    last_response_len: usize,
}

struct RepoSyncInfo {
    repo_name: String,
    project_dir: PathBuf,
    channel_id: String,
    repo_path: PathBuf,
    agent_kind: AgentKind,
}

/// Background task that watches for local Claude Code session activity and syncs to Slack.
pub async fn sync_sessions(state: AppState) {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => {
            error!("Cannot determine HOME directory, session sync disabled");
            return;
        }
    };

    let claude_projects_dir = home.join(".claude").join("projects");

    // Build per-repo sync info.
    let repo_channels = state.slack.repo_channels.read().await;
    let mut repos: Vec<RepoSyncInfo> = Vec::new();

    for (repo_name, repo_config) in &state.config.repos {
        if !repo_config.sync_enabled(&state.config.defaults) {
            info!("Sync disabled for repo '{}', skipping", repo_name);
            continue;
        }

        let encoded = encode_project_path(&repo_config.path);
        let project_dir = claude_projects_dir.join(&encoded);

        let channel_id = match repo_channels.get(repo_name) {
            Some(id) => id.clone(),
            None => {
                warn!("No channel for repo '{}', skipping sync", repo_name);
                continue;
            }
        };

        repos.push(RepoSyncInfo {
            repo_name: repo_name.clone(),
            project_dir,
            channel_id,
            repo_path: repo_config.path.clone(),
            agent_kind: repo_config.agent.clone(),
        });
    }
    drop(repo_channels);

    // Clear stale sessions whose channel_id no longer matches the current repo channel.
    {
        let repo_channel_map: HashMap<String, String> = repos
            .iter()
            .map(|r| (r.repo_name.clone(), r.channel_id.clone()))
            .collect();
        state.sessions.prune_stale_channels(&repo_channel_map).await;
        state
            .sessions
            .prune_expired(state.config.tuning.session_ttl_days)
            .await;
    }

    // Build a lookup from project_dir → repo index for fast matching on file events.
    let dir_to_repo: HashMap<PathBuf, usize> = repos
        .iter()
        .enumerate()
        .map(|(i, r)| (r.project_dir.clone(), i))
        .collect();

    // Track sync state per session so we can detect new turns and response growth.
    let mut synced_turns: HashMap<String, SyncState> = HashMap::new();

    // Set up filesystem watcher.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(100);
    let watcher = notify::recommended_watcher(move |res| {
        let _ = tx.blocking_send(res);
    });

    let mut watcher = match watcher {
        Ok(w) => {
            info!("File watcher initialized for session sync");
            Some(w)
        }
        Err(e) => {
            warn!(
                "Failed to create file watcher, falling back to polling only: {}",
                e
            );
            None
        }
    };

    // Watch project directories.
    if let Some(ref mut w) = watcher {
        for repo in &repos {
            if repo.project_dir.exists() {
                match w.watch(&repo.project_dir, RecursiveMode::NonRecursive) {
                    Ok(_) => info!("Watching {} for local sessions", repo.project_dir.display()),
                    Err(e) => warn!("Failed to watch {}: {}", repo.project_dir.display(), e),
                }
            }
        }
    }

    // Initial scan: pick up recent sessions that were created/modified before the watcher started.
    // Only sync files modified within the last 24 hours to avoid flooding Slack with old history.
    let recency_cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);

    for repo in &repos {
        if !repo.project_dir.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(&repo.project_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(
                    "Failed to scan {} for existing sessions: {}",
                    repo.project_dir.display(),
                    e
                );
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let session_id = match session_id_from_path(&path) {
                Some(id) => id,
                None => continue,
            };

            // Only sync recently modified sessions.
            let is_recent = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .is_some_and(|mtime| mtime >= recency_cutoff);
            if !is_recent {
                continue;
            }

            // Skip sidechain (subagent) sessions.
            if is_sidechain_session(&path) {
                debug!("Skipping sidechain session '{}'", session_id);
                continue;
            }

            // Skip sessions already persisted from a prior run.
            if state.sessions.has_session_id(&session_id).await {
                debug!(
                    "Initial scan: session '{}' already tracked, skipping",
                    session_id
                );
                continue;
            }

            if !state
                .try_claim_session_for_sync(&repo.repo_name, &session_id)
                .await
            {
                continue;
            }

            sync_session(&state, repo, &session_id, &mut synced_turns).await;
            state.sync.release(&session_id).await;
        }
    }

    let initial_count = synced_turns.len();
    if initial_count > 0 {
        info!(
            "Initial scan complete: synced {} existing session(s)",
            initial_count
        );
    }

    info!("Session sync started");

    loop {
        let event = match rx.recv().await {
            Some(Ok(event)) => event,
            Some(Err(_)) => continue,
            None => break,
        };

        if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
            continue;
        }

        for path in &event.paths {
            let session_id = match session_id_from_path(path) {
                Some(id) => id,
                None => continue,
            };

            let parent = match path.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };

            let repo = match dir_to_repo.get(&parent).map(|&i| &repos[i]) {
                Some(r) => r,
                None => continue,
            };

            // Already syncing this session — check for new/updated turns.
            if let Some(prev_state) = synced_turns.get(&session_id) {
                let thread_ts = prev_state.thread_ts.clone();
                let prev_turns = prev_state.completed_turns;
                let prev_resp_len = prev_state.last_response_len;
                sync_new_turns(
                    &state,
                    repo,
                    &session_id,
                    &thread_ts,
                    prev_turns,
                    prev_resp_len,
                    &mut synced_turns,
                )
                .await;
                continue;
            }

            // Skip sidechain (subagent) sessions.
            let jsonl_path = repo.project_dir.join(format!("{}.jsonl", &session_id));
            if is_sidechain_session(&jsonl_path) {
                debug!("Skipping sidechain session '{}'", session_id);
                continue;
            }

            // Atomically claim this session for sync if not already owned.
            // This prevents race conditions between sync and handle_new_message.
            if !state
                .try_claim_session_for_sync(&repo.repo_name, &session_id)
                .await
            {
                debug!(
                    "Skipping sync for session '{}' — already owned or repo pending",
                    session_id
                );
                continue;
            }

            sync_session(&state, repo, &session_id, &mut synced_turns).await;

            // Release the claim after sync completes (session is now persisted).
            state.sync.release(&session_id).await;
        }
    }
}

/// Create a new Slack thread for a synced session.
/// Returns the thread timestamp, or None if the thread could not be created.
async fn create_sync_thread(
    state: &AppState,
    repo: &RepoSyncInfo,
    session_id: &str,
    first_prompt: &str,
) -> Option<String> {
    let anchor_chunks =
        crate::slack::split_for_slack(first_prompt, crate::slack::SLACK_MAX_MESSAGE_CHARS);

    let ts = match crate::slack::post_channel_message(state, &repo.channel_id, &anchor_chunks[0])
        .await
    {
        Some(ts) => ts,
        None => {
            error!("Failed to post sync message for session {}", session_id);
            return None;
        }
    };

    // Post remaining prompt chunks as thread replies.
    for chunk in &anchor_chunks[1..] {
        crate::slack::post_thread_reply(state, &repo.channel_id, &ts, chunk).await;
    }

    // Store session so thread replies can --resume it.
    let session_info = SessionInfo {
        session_id: session_id.to_string(),
        repo: repo.repo_name.clone(),
        repo_path: repo.repo_path.clone(),
        agent_kind: repo.agent_kind.clone(),
        channel_id: repo.channel_id.clone(),
        thread_ts: ts.clone(),
        created_at: Utc::now(),
        last_active: Utc::now(),
        status: SessionStatus::Active,
        total_turns: 0,
        model: None,
    };

    if let Err(e) = state.sessions.insert(session_info).await {
        error!("Failed to store synced session: {}", e);
        return None;
    }

    info!(
        "New synced session '{}' for repo '{}'",
        session_id, repo.repo_name
    );

    Some(ts)
}

/// Detect a new local session, create the Slack thread, and post initial response(s).
/// Records the session in `synced_turns` so the file watcher picks up future turns.
async fn sync_session(
    state: &AppState,
    repo: &RepoSyncInfo,
    session_id: &str,
    synced_turns: &mut HashMap<String, SyncState>,
) {
    let jsonl_path = repo.project_dir.join(format!("{}.jsonl", session_id));
    let conversation = match tokio::task::spawn_blocking({
        let path = jsonl_path.clone();
        move || read_conversation(&path)
    })
    .await
    {
        Ok(c) => c,
        Err(e) => {
            error!(
                "Failed to read conversation for session '{}': {}",
                session_id, e
            );
            return;
        }
    };
    let turns = &conversation.turns;

    // Need at least one complete turn (prompt + response) before we start syncing.
    let has_complete_turn = turns.iter().any(|t| t.response.is_some());
    if !has_complete_turn {
        debug!(
            "sync_session '{}': no complete turn yet, skipping",
            session_id
        );
        return;
    }

    // Create the Slack thread with the first prompt.
    let thread_ts = match create_sync_thread(state, repo, session_id, &turns[0].prompt).await {
        Some(ts) => ts,
        None => return,
    };

    // Post all completed responses from the initial file read.
    let mut completed_count = 0;
    for (i, turn) in turns.iter().enumerate() {
        let response = match &turn.response {
            Some(r) => r,
            None => break,
        };

        completed_count += 1;

        // For turns after the first, post the follow-up prompt.
        if i > 0 {
            let prompt_chunks =
                crate::slack::split_for_slack(&turn.prompt, crate::slack::SLACK_MAX_MESSAGE_CHARS);
            for (pi, chunk) in prompt_chunks.iter().enumerate() {
                let msg = if pi == 0 {
                    format!("*>* {}", chunk)
                } else {
                    chunk.clone()
                };
                crate::slack::post_thread_reply(state, &repo.channel_id, &thread_ts, &msg).await;
            }
        }

        let response_chunks =
            crate::slack::split_for_slack(response, crate::slack::SLACK_MAX_MESSAGE_CHARS);
        for chunk in &response_chunks {
            crate::slack::post_thread_reply(state, &repo.channel_id, &thread_ts, chunk).await;
        }
    }

    // Record sync state so the file watcher picks up future changes.
    let last_response_len = turns
        .iter()
        .rev()
        .find_map(|t| t.response.as_ref().map(|r| r.len()))
        .unwrap_or(0);
    synced_turns.insert(
        session_id.to_string(),
        SyncState {
            thread_ts,
            completed_turns: completed_count,
            last_response_len,
        },
    );
}

/// Re-read a synced session's .jsonl file and post any new completed turns
/// (or response growth within the last turn) to Slack.
async fn sync_new_turns(
    state: &AppState,
    repo: &RepoSyncInfo,
    session_id: &str,
    thread_ts: &str,
    prev_count: usize,
    prev_resp_len: usize,
    synced_turns: &mut HashMap<String, SyncState>,
) {
    let jsonl_path = repo.project_dir.join(format!("{}.jsonl", session_id));
    let conversation = match tokio::task::spawn_blocking({
        let path = jsonl_path.clone();
        move || read_conversation(&path)
    })
    .await
    {
        Ok(c) => c,
        Err(e) => {
            error!(
                "Failed to read conversation for session '{}': {}",
                session_id, e
            );
            return;
        }
    };

    let completed_count = conversation
        .turns
        .iter()
        .filter(|t| t.response.is_some())
        .count();

    // Check if the last completed turn's response has grown (e.g. plan → execution
    // within the same turn, where no new human message separates them).
    let last_resp_len = conversation
        .turns
        .iter()
        .rev()
        .find_map(|t| t.response.as_ref().map(|r| r.len()))
        .unwrap_or(0);
    let response_grew = completed_count == prev_count && last_resp_len > prev_resp_len;

    if completed_count <= prev_count && !response_grew {
        return;
    }

    // If a Slack-driven turn is active on this thread, just update the counter
    // to stay in sync (the Slack handler will post the response).
    {
        let in_prog = state.threads.in_progress.lock().await;
        if in_prog.contains(thread_ts) {
            debug!(
                "sync_new_turns '{}': thread {} is in_progress, updating counter only",
                session_id, thread_ts
            );
            synced_turns.insert(
                session_id.to_string(),
                SyncState {
                    thread_ts: thread_ts.to_string(),
                    completed_turns: completed_count,
                    last_response_len: last_resp_len,
                },
            );
            return;
        }
    }

    let mut posted = 0;

    if response_grew {
        // The last completed turn's response grew — post the new content.
        // Find the last completed turn and extract the delta.
        if let Some(turn) = conversation
            .turns
            .iter()
            .rev()
            .find(|t| t.response.is_some())
        {
            let response = turn.response.as_ref().unwrap();
            // Post only the new portion (text added since last sync).
            let delta = &response[prev_resp_len..];
            let trimmed = delta.trim_start_matches("\n\n").trim();
            if !trimmed.is_empty() {
                let chunks =
                    crate::slack::split_for_slack(trimmed, crate::slack::SLACK_MAX_MESSAGE_CHARS);
                for chunk in &chunks {
                    crate::slack::post_thread_reply(state, &repo.channel_id, thread_ts, chunk)
                        .await;
                }
                posted += 1;
                debug!(
                    "sync_new_turns '{}': posted response delta ({} → {} bytes)",
                    session_id, prev_resp_len, last_resp_len
                );
            }
        }
    }

    // Post new completed turns that haven't been synced yet.
    if completed_count > prev_count {
        let mut completed_seen = 0;
        for turn in &conversation.turns {
            if turn.response.is_none() {
                continue;
            }
            completed_seen += 1;
            if completed_seen <= prev_count {
                continue;
            }

            // Post the follow-up prompt.
            let prompt_chunks =
                crate::slack::split_for_slack(&turn.prompt, crate::slack::SLACK_MAX_MESSAGE_CHARS);
            for (pi, chunk) in prompt_chunks.iter().enumerate() {
                let msg = if pi == 0 {
                    format!("*>* {}", chunk)
                } else {
                    chunk.clone()
                };
                crate::slack::post_thread_reply(state, &repo.channel_id, thread_ts, &msg).await;
            }

            let response = turn.response.as_ref().unwrap();
            let response_chunks =
                crate::slack::split_for_slack(response, crate::slack::SLACK_MAX_MESSAGE_CHARS);
            for chunk in &response_chunks {
                crate::slack::post_thread_reply(state, &repo.channel_id, thread_ts, chunk).await;
            }

            posted += 1;
        }
    }

    if posted > 0 {
        info!(
            "Synced {} update(s) for session '{}' (turns: {})",
            posted, session_id, completed_count
        );
    }

    synced_turns.insert(
        session_id.to_string(),
        SyncState {
            thread_ts: thread_ts.to_string(),
            completed_turns: completed_count,
            last_response_len: last_resp_len,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_encode_project_path() {
        let path = Path::new("/Users/josh/code/hermes");
        assert_eq!(encode_project_path(path), "-Users-josh-code-hermes");
    }

    #[test]
    fn test_session_id_from_path_valid() {
        let path = Path::new("/tmp/abc-123.jsonl");
        assert_eq!(session_id_from_path(path), Some("abc-123".to_string()));
    }

    #[test]
    fn test_session_id_from_path_wrong_extension() {
        assert_eq!(session_id_from_path(Path::new("/tmp/abc.json")), None);
        assert_eq!(session_id_from_path(Path::new("/tmp/abc.txt")), None);
    }

    #[test]
    fn test_session_id_from_path_no_extension() {
        assert_eq!(session_id_from_path(Path::new("/tmp/abc")), None);
    }

    #[test]
    fn test_is_human_message_string_content() {
        let val = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "hello" }
        });
        assert_eq!(is_human_message(&val), Some("hello".to_string()));
    }

    #[test]
    fn test_is_human_message_array_content() {
        let val = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{"type": "tool_result"}] }
        });
        assert_eq!(is_human_message(&val), None);
    }

    #[test]
    fn test_extract_assistant_text() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "Hello" },
                    { "type": "tool_use", "id": "1", "name": "Read", "input": {} },
                    { "type": "text", "text": "World" }
                ]
            }
        });
        assert_eq!(
            extract_assistant_text(&val),
            Some("Hello\nWorld".to_string())
        );
    }

    #[test]
    fn test_extract_assistant_text_no_text_blocks() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "tool_use", "id": "1", "name": "Read", "input": {} }
                ]
            }
        });
        assert_eq!(extract_assistant_text(&val), None);
    }

    #[test]
    fn test_read_conversation_single_turn() {
        let dir = std::env::temp_dir().join("hermes_test_read_conv");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_single.jsonl");

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"fix the bug"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Done"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"result","subtype":"success","session_id":"s1"}}"#
        )
        .unwrap();

        let conv = read_conversation(&path);
        assert_eq!(conv.turns.len(), 1);
        assert_eq!(conv.turns[0].prompt, "fix the bug");
        assert_eq!(conv.turns[0].response.as_deref(), Some("Done"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_conversation_multi_turn() {
        let dir = std::env::temp_dir().join("hermes_test_read_conv");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_multi.jsonl");

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"first prompt"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"resp1"}}]}}}}"#
        )
        .unwrap();
        // tool_result (array content) — should not start a new turn
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"second prompt"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"resp2"}}]}}}}"#
        )
        .unwrap();

        let conv = read_conversation(&path);
        assert_eq!(conv.turns.len(), 2);
        assert_eq!(conv.turns[0].prompt, "first prompt");
        assert_eq!(conv.turns[0].response.as_deref(), Some("resp1"));
        assert_eq!(conv.turns[1].prompt, "second prompt");
        assert_eq!(conv.turns[1].response.as_deref(), Some("resp2"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_conversation_no_response_yet() {
        let dir = std::env::temp_dir().join("hermes_test_read_conv");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_no_resp.jsonl");

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"waiting"}}}}"#
        )
        .unwrap();

        let conv = read_conversation(&path);
        assert_eq!(conv.turns.len(), 1);
        assert_eq!(conv.turns[0].prompt, "waiting");
        assert!(conv.turns[0].response.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_read_conversation_nonexistent_file() {
        let conv = read_conversation(Path::new("/tmp/hermes_nonexistent_file.jsonl"));
        assert!(conv.turns.is_empty());
    }

    #[test]
    fn test_is_sidechain_session_false() {
        let dir = std::env::temp_dir().join("hermes_test_sidechain");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("normal_session.jsonl");

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","isSidechain":false,"message":{{"role":"user","content":"hello"}}}}"#
        )
        .unwrap();

        assert!(!is_sidechain_session(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_is_sidechain_session_true() {
        let dir = std::env::temp_dir().join("hermes_test_sidechain");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sidechain_session.jsonl");

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","isSidechain":true,"message":{{"role":"user","content":"subagent task"}}}}"#
        )
        .unwrap();

        assert!(is_sidechain_session(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_extract_plan_content() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "Let me write a plan." },
                    {
                        "type": "tool_use",
                        "id": "1",
                        "name": "Write",
                        "input": {
                            "file_path": "/home/user/.claude/plans/my-plan.md",
                            "content": "# Plan\n\n1. Do the thing\n2. Test it"
                        }
                    }
                ]
            }
        });
        assert_eq!(
            extract_plan_content(&val),
            Some("# Plan\n\n1. Do the thing\n2. Test it".to_string())
        );
    }

    #[test]
    fn test_extract_plan_content_no_plan() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "1",
                        "name": "Write",
                        "input": {
                            "file_path": "/home/user/project/src/main.rs",
                            "content": "fn main() {}"
                        }
                    }
                ]
            }
        });
        assert_eq!(extract_plan_content(&val), None);
    }

    #[test]
    fn test_read_conversation_with_plan() {
        let dir = std::env::temp_dir().join("hermes_test_read_conv");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_plan.jsonl");

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"make a plan"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"1","name":"Write","input":{{"file_path":"/home/user/.claude/plans/plan.md","content":"Step 1\nStep 2"}}}}]}}}}"#
        )
        .unwrap();

        let conv = read_conversation(&path);
        assert_eq!(conv.turns.len(), 1);
        let response = conv.turns[0].response.as_deref().unwrap();
        assert!(response.contains("*Plan*"));
        assert!(response.contains("Step 1\nStep 2"));

        let _ = std::fs::remove_file(&path);
    }
}
