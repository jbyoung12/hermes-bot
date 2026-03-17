use crate::agent::protocol;
use crate::agent::{AgentEvent, AgentHandle};
use crate::session::{SessionInfo, SessionStatus};
use chrono::Utc;
use slack_morphism::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use super::api::*;
use super::formatting::{self, AgentResponse, format_questions, split_for_slack, truncate_text};
use super::{AppState, FALLBACK_TITLE_MAX_LEN, SLACK_MAX_MESSAGE_CHARS, SLACK_UPDATE_MAX_CHARS};

// All magic numbers are now configurable via config.tuning.
// Defaults are defined in TuningConfig in config.rs.

/// Flush accumulated text to Slack — either update the existing editable message,
/// post a new editable message, or post as a permanent message if over the limit.
/// Returns the (possibly updated) `status_ts`.
async fn flush_to_slack(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    accumulated_text: &mut String,
    status_ts: Option<String>,
) -> Option<String> {
    let display = truncate_text(accumulated_text, SLACK_UPDATE_MAX_CHARS);

    if accumulated_text.len() > SLACK_UPDATE_MAX_CHARS {
        // Outgrown the edit limit — post as permanent and start fresh.
        post_thread_reply(state, channel_id, thread_ts, &display).await;
        accumulated_text.clear();
        None
    } else if let Some(ref ts) = status_ts {
        update_message(state, channel_id, ts, &display).await;
        status_ts
    } else {
        post_thread_reply_with_ts(state, channel_id, thread_ts, &display).await
    }
}

// ── Guards ─────────────────────────────────────────────────────────────

/// RAII guard that removes a repo name from `pending_repos` on drop.
/// Ensures cleanup even if the owning task panics.
///
/// NOTE: The drop implementation uses try_lock, which can silently fail if the lock
/// is held or poisoned. To handle this edge case, a background cleanup task runs
/// every 60 seconds to remove repos that have been pending for >5 minutes.
struct PendingRepoGuard {
    pending_repos: Arc<Mutex<HashMap<String, Instant>>>,
    repo_name: String,
}

// ── Utility functions ──────────────────────────────────────────────────

/// Accumulate text with size bounds to prevent unbounded memory growth.
/// Returns true if the accumulated text exceeds max_size and should be flushed.
fn should_flush_accumulated(accumulated: &str, new_text: &str, max_size: usize) -> bool {
    accumulated.len() + new_text.len() > max_size
}

/// Validate that plan content doesn't exceed reasonable size limits.
fn validate_plan_size(content: &str, max_size: usize) -> Result<(), String> {
    if content.len() > max_size {
        Err(format!(
            "Plan content too large: {} bytes (max: {})",
            content.len(),
            max_size
        ))
    } else {
        Ok(())
    }
}

/// Posts a user prompt to a thread, splitting into chunks if needed.
///
/// For the first chunk, prefixes with the user mention. Subsequent chunks
/// are posted without the mention.
async fn post_prompt_to_thread(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    user_id: Option<&str>,
    text: &str,
) {
    let prompt_chunks = split_for_slack(text, SLACK_MAX_MESSAGE_CHARS);
    for (i, chunk) in prompt_chunks.iter().enumerate() {
        let msg = if i == 0 {
            if let Some(uid) = user_id {
                format!("<@{}>: {}", uid, chunk)
            } else {
                format!("*>* {}", chunk)
            }
        } else {
            chunk.clone()
        };
        post_thread_reply(state, channel_id, thread_ts, &msg).await;
    }
}

impl Drop for PendingRepoGuard {
    fn drop(&mut self) {
        // Use try_lock since drop is sync — if the lock is poisoned/held,
        // the heartbeat or next startup will eventually clean up.
        if let Ok(mut guard) = self.pending_repos.try_lock() {
            guard.remove(&self.repo_name);
        }
    }
}

// ── Shared helpers ─────────────────────────────────────────────────────

/// Build an `AgentResponse` from `EventResult::TurnComplete` fields and format it for Slack.
fn format_turn_result(
    result_text: Option<String>,
    is_error: bool,
    duration_ms: u64,
    num_turns: u32,
    subtype: String,
    tool_summary: Option<String>,
) -> Vec<String> {
    let resp = AgentResponse {
        result: result_text.unwrap_or_default(),
        is_error,
        duration_ms,
        num_turns,
        subtype: Some(subtype),
        tool_summary,
    };
    formatting::format_agent_response(&resp)
}

fn format_exit_code(code: Option<i32>) -> String {
    code.map_or("unknown".to_string(), |c| c.to_string())
}

fn agent_exit_message(code: Option<i32>, recoverable: bool) -> String {
    let suffix = if recoverable {
        " Reply again to auto-recover."
    } else {
        ""
    };
    format!(
        "Agent process exited unexpectedly (code: {}).{}",
        format_exit_code(code),
        suffix
    )
}

fn agent_disconnected_message(recoverable: bool) -> &'static str {
    if recoverable {
        "Agent connection lost. Reply again to auto-recover."
    } else {
        "Agent connection lost."
    }
}

/// How to persist the session after a successful turn.
pub(crate) enum SessionAction {
    /// Create a new session (used by handle_new_message).
    /// Fields that depend on the event result (session_id, total_turns) are
    /// filled in by `handle_event_result`.
    Insert {
        repo: String,
        repo_path: std::path::PathBuf,
        agent_kind: crate::config::AgentKind,
        model: Option<String>,
    },
    /// Update an existing session. If `reset_status` is true, also set
    /// status back to Active (used by handle_execute_plan).
    Update { reset_status: bool },
}

/// Shared configuration for post-`process_events` result handling.
pub(crate) struct EventResultConfig {
    pub(crate) title: Option<String>,
    pub(crate) recoverable: bool,
    pub(crate) retry_first_chunk: bool,
    pub(crate) hourglass_ts: String,
    pub(crate) status_ts: Option<String>,
    pub(crate) repo_name: String,
}

/// Handle the result of `process_events`, posting responses, updating sessions,
/// and cleaning up reactions. This is the shared logic for all three entry points
/// (new message, thread reply, plan execution).
pub(crate) async fn handle_event_result(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    mut handle: AgentHandle,
    result: EventResult,
    config: EventResultConfig,
    session_action: SessionAction,
) {
    // Take session_action into an Option so we can consume it in TurnComplete
    // or fall through to persist it for early-exit cases.
    let mut session_action = Some(session_action);
    let handle_session_id = handle.session_id.clone();

    match result {
        EventResult::TurnComplete {
            result_text,
            subtype,
            num_turns,
            duration_ms,
            is_error,
            session_id,
            streamed,
            plan_posted,
            tool_summary,
        } => {
            // Update the thread root with the title (new sessions only).
            if let Some(ref title) = config.title {
                update_message(state, channel_id, thread_ts, &format!("*{}*", title)).await;
            }

            // Post the response unless it was already streamed live
            // or a plan was already posted (to avoid duplication).
            let mut posted = streamed || plan_posted;
            if !streamed && !plan_posted {
                let chunks = format_turn_result(
                    result_text,
                    is_error,
                    duration_ms,
                    num_turns,
                    subtype,
                    tool_summary,
                );

                // If the response is a single chunk that fits in the update
                // limit and we have a placeholder message, edit it in place.
                if chunks.len() == 1 && chunks[0].len() <= SLACK_UPDATE_MAX_CHARS {
                    if let Some(ref ts) = config.status_ts {
                        update_message(state, channel_id, ts, &chunks[0]).await;
                        posted = true;
                    }
                } else {
                    // Delete the placeholder — we'll post full messages instead.
                    if let Some(ref ts) = config.status_ts {
                        delete_message(state, channel_id, ts).await;
                    }

                    for (ci, chunk) in chunks.iter().enumerate() {
                        if ci == 0 && config.retry_first_chunk {
                            for attempt in 0..state.config.tuning.first_chunk_max_retries {
                                if post_thread_reply_result(state, channel_id, thread_ts, chunk)
                                    .await
                                {
                                    posted = true;
                                    break;
                                }
                                if attempt < state.config.tuning.first_chunk_max_retries - 1 {
                                    warn!(
                                        "Retrying thread post for repo '{}' (attempt {})",
                                        config.repo_name,
                                        attempt + 2
                                    );
                                    tokio::time::sleep(std::time::Duration::from_secs(
                                        1 << attempt,
                                    ))
                                    .await;
                                }
                            }
                        } else {
                            post_thread_reply(state, channel_id, thread_ts, chunk).await;
                            posted = true;
                        }
                    }
                }
            } else {
                // Response was streamed — clean up the placeholder if present.
                if let Some(ref ts) = config.status_ts {
                    delete_message(state, channel_id, ts).await;
                }

                // Post a stats footer. Tool calls were already posted live,
                // so we only need turns/duration here.
                let duration = if duration_ms >= 1000 {
                    format!("{:.1}s", duration_ms as f64 / 1000.0)
                } else {
                    format!("{}ms", duration_ms)
                };
                let hit_turn_limit = subtype == "max_turns_reached" || subtype == "error_max_turns";
                let footer = if hit_turn_limit {
                    format!(
                        "_:warning: Hit turn limit ({} turns, {})._ Reply in this thread to continue where Claude left off.",
                        num_turns, duration
                    )
                } else {
                    format!("_Turns: {} | Duration: {}_", num_turns, duration)
                };
                post_thread_reply(state, channel_id, thread_ts, &footer).await;
            }

            if !posted {
                error!(
                    "Failed to create thread for repo '{}' after retries — session {} will be orphaned",
                    config.repo_name, session_id
                );
            }

            // Store the handle for future thread replies.
            handle.session_id = Some(session_id.clone());
            state
                .agent_handles
                .lock()
                .await
                .insert(thread_ts.to_string(), handle);

            // Persist session.
            match session_action.take().unwrap() {
                SessionAction::Insert {
                    repo,
                    repo_path,
                    agent_kind,
                    model,
                } => {
                    let session = SessionInfo {
                        session_id,
                        repo,
                        repo_path,
                        agent_kind,
                        channel_id: channel_id.to_string(),
                        thread_ts: thread_ts.to_string(),
                        created_at: Utc::now(),
                        last_active: Utc::now(),
                        status: SessionStatus::Active,
                        total_turns: num_turns,
                        model,
                    };
                    if let Err(e) = state.sessions.insert(session).await {
                        error!("Failed to store session: {}", e);
                    }
                }
                SessionAction::Update { reset_status } => {
                    let new_session_id = session_id;
                    if let Err(e) = state
                        .sessions
                        .update(thread_ts, move |s| {
                            s.last_active = Utc::now();
                            s.total_turns = num_turns;
                            s.session_id = new_session_id.clone();
                            if reset_status {
                                s.status = SessionStatus::Active;
                            }
                        })
                        .await
                    {
                        error!("Failed to update session: {}", e);
                    }
                }
            }
        }
        EventResult::ProcessExited { code } => {
            state.kill_senders.lock().await.remove(thread_ts);
            if let Some(ref title) = config.title {
                update_message(state, channel_id, thread_ts, &format!("*{}*", title)).await;
            }
            if let Some(ref ts) = config.status_ts {
                delete_message(state, channel_id, ts).await;
            }
            post_thread_reply(
                state,
                channel_id,
                thread_ts,
                &agent_exit_message(code, config.recoverable),
            )
            .await;
        }
        EventResult::ChannelClosed => {
            // If the kill sender was already removed, this was an intentional
            // kill (!stop or interrupted by a new message) — skip the message.
            let was_unexpected = state.kill_senders.lock().await.remove(thread_ts).is_some();
            if was_unexpected {
                if let Some(ref title) = config.title {
                    update_message(state, channel_id, thread_ts, &format!("*{}*", title)).await;
                }
                if let Some(ref ts) = config.status_ts {
                    delete_message(state, channel_id, ts).await;
                }
                post_thread_reply(
                    state,
                    channel_id,
                    thread_ts,
                    agent_disconnected_message(config.recoverable),
                )
                .await;
            } else if let Some(ref ts) = config.status_ts {
                delete_message(state, channel_id, ts).await;
            }
        }
    }

    // For early exits (ProcessExited / ChannelClosed on a new session), the
    // session was never persisted. Insert it now so thread replies still work.
    if let Some(SessionAction::Insert {
        repo,
        repo_path,
        agent_kind,
        model,
    }) = session_action
    {
        let session_id = handle_session_id.unwrap_or_else(|| format!("pending-{}", thread_ts));
        let session = SessionInfo {
            session_id,
            repo,
            repo_path,
            agent_kind,
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            status: SessionStatus::Active,
            total_turns: 0,
            model,
        };
        if let Err(e) = state.sessions.insert(session).await {
            error!("Failed to store early session: {}", e);
        }
    }

    // Clean up hourglass reaction and concurrency guard.
    remove_reaction(
        state,
        channel_id,
        &config.hourglass_ts,
        "hourglass_flowing_sand",
    )
    .await;
    remove_in_progress(state, thread_ts).await;
}

/// Common cleanup when a new session fails to start: set fallback title
/// and remove hourglass reaction. The pending_repos guard is handled by
/// the PendingRepoGuard drop.
async fn cleanup_failed_new_session(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    prompt: &str,
    error_msg: &str,
) {
    post_thread_reply(state, channel_id, thread_ts, error_msg).await;
    update_message(
        state,
        channel_id,
        thread_ts,
        &format!("*{}*", fallback_title(prompt)),
    )
    .await;
    remove_reaction(state, channel_id, thread_ts, "hourglass_flowing_sand").await;
}

/// Common core for all handler entry points: store kill sender, send prompt,
/// post status message, process events, and handle the result.
///
/// Returns `false` if the prompt could not be sent (caller handles cleanup).
#[allow(clippy::too_many_arguments)]
async fn run_agent_turn(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    mut handle: AgentHandle,
    prompt: &str,
    status_msg: &str,
    mut config: EventResultConfig,
    session_action: SessionAction,
) -> bool {
    // Store kill sender so !stop can reach it.
    if let Some(kill_tx) = handle.kill_tx.take() {
        state
            .kill_senders
            .lock()
            .await
            .insert(thread_ts.to_string(), kill_tx);
    }

    // Send the prompt.
    if let Err(e) = handle.sender.send(prompt.to_string()).await {
        error!("Failed to send prompt to agent: {}", e);
        return false;
    }

    // Post a status placeholder that can be edited in-place with the response.
    config.status_ts = post_thread_reply_with_ts(state, channel_id, thread_ts, status_msg).await;

    // Process events until TurnComplete or ProcessExited.
    let result = process_events(state, channel_id, thread_ts, &mut handle).await;

    // Post response, persist session, clean up reactions.
    handle_event_result(
        state,
        channel_id,
        thread_ts,
        handle,
        result,
        config,
        session_action,
    )
    .await;
    true
}

// ── Message handlers ──────────────────────────────────────────────────

/// A new top-level message in a repo channel → start a new agent session.
pub(crate) async fn handle_new_message(
    state: AppState,
    channel_id: String,
    message_ts: String,
    user_id: String,
    text: String,
) {
    let repo_name = match state.repo_for_channel(&channel_id).await {
        Some(r) => r,
        None => {
            info!("Message in non-repo channel {}, ignoring", channel_id);
            return;
        }
    };

    let repo_config = match state.config.repos.get(&repo_name) {
        Some(r) => r,
        None => return,
    };

    let agent = match state.agents.get(&repo_config.agent) {
        Some(a) => a.clone(),
        None => {
            error!("No agent for kind {:?}", repo_config.agent);
            return;
        }
    };

    let merged_tools = repo_config.merged_tools(&state.config.defaults);
    let system_prompt = state.config.defaults.append_system_prompt.clone();
    let model = repo_config.resolved_model(&state.config.defaults);

    info!(
        "Starting {:?} session for repo '{}' (model: {})",
        repo_config.agent, repo_name, model
    );

    // Mark repo as having an in-flight new session (prevents sync from duplicating).
    // The guard ensures cleanup even if the spawned task panics.
    state
        .pending_repos
        .lock()
        .await
        .insert(repo_name.clone(), Instant::now());
    let pending_guard = PendingRepoGuard {
        pending_repos: state.pending_repos.clone(),
        repo_name: repo_name.clone(),
    };

    // Acknowledge the user's original message with a reaction.
    add_reaction(&state, &channel_id, &message_ts, "eyes").await;

    // Post a placeholder message that the bot owns — this becomes the thread root.
    let thread_ts = match post_channel_message(&state, &channel_id, "_Processing..._").await {
        Some(ts) => ts,
        None => {
            error!(
                "Failed to post placeholder message for repo '{}'",
                repo_name
            );
            // pending_guard dropped here → removes from pending_repos
            return;
        }
    };

    // Add hourglass reaction to the bot's thread root.
    add_reaction(&state, &channel_id, &thread_ts, "hourglass_flowing_sand").await;

    // Post the user's full prompt as thread replies.
    post_prompt_to_thread(&state, &channel_id, &thread_ts, Some(&user_id), &text).await;

    let repo_path = repo_config.path.clone();
    let agent_kind = repo_config.agent.clone();
    let state_clone = state.clone();
    let thread_ts_clone = thread_ts.clone();

    tokio::spawn(async move {
        let _pending_guard = pending_guard;

        let handle = match agent
            .spawn(
                &repo_path,
                &merged_tools,
                system_prompt.as_deref(),
                None,
                Some(&model),
            )
            .await
        {
            Ok(h) => h,
            Err(e) => {
                error!("Failed to spawn agent for repo '{}': {}", repo_name, e);
                cleanup_failed_new_session(
                    &state_clone,
                    &channel_id,
                    &thread_ts_clone,
                    &text,
                    &format!("Failed to start session: {}", e),
                )
                .await;
                return;
            }
        };

        let title = fallback_title(&text);

        if !run_agent_turn(
            &state_clone,
            &channel_id,
            &thread_ts_clone,
            handle,
            &text,
            "_Thinking..._",
            EventResultConfig {
                title: Some(title),
                recoverable: false,
                retry_first_chunk: true,
                hourglass_ts: thread_ts_clone.clone(),
                status_ts: None,
                repo_name: repo_name.clone(),
            },
            SessionAction::Insert {
                repo: repo_name,
                repo_path,
                agent_kind,
                model: Some(model),
            },
        )
        .await
        {
            cleanup_failed_new_session(
                &state_clone,
                &channel_id,
                &thread_ts_clone,
                &text,
                "Failed to send prompt to agent.",
            )
            .await;
        }
    });
}

/// A thread reply in a repo channel → resume the agent session or handle commands.
pub(crate) async fn handle_thread_reply(
    state: AppState,
    channel_id: String,
    thread_ts: String,
    message_ts: String,
    text: String,
) {
    // Check for thread commands.
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("!stop") {
        handle_stop(&state, &channel_id, &thread_ts).await;
        return;
    }
    if trimmed.eq_ignore_ascii_case("!status") {
        handle_status(&state, &channel_id, &thread_ts).await;
        return;
    }
    if trimmed.eq_ignore_ascii_case("!execute") {
        handle_execute_plan(state, channel_id, thread_ts, message_ts).await;
        return;
    }
    if let Some(rest) = trimmed
        .strip_prefix("!model")
        .filter(|r| r.is_empty() || r.starts_with(' '))
    {
        handle_model_command(&state, &channel_id, &thread_ts, rest.trim()).await;
        return;
    }

    // Check if this thread is waiting for a question answer.
    if let Some(answer_tx) = state.pending_answers.lock().await.remove(&thread_ts) {
        let _ = answer_tx.send(text);
        return;
    }

    // Check if this thread is waiting for a tool approval.
    if let Some(approval_tx) = state.pending_tool_approvals.lock().await.remove(&thread_ts) {
        let trimmed = text.trim().to_lowercase();
        let approved = matches!(trimmed.as_str(), "y" | "yes" | "allow" | "!allow");
        let _ = approval_tx.send(approved);
        return;
    }

    // Look up session for this thread.
    let session = match state.sessions.get_by_thread(&thread_ts).await {
        Some(s) => s,
        None => {
            warn!(
                "No session found for thread {} in channel {}",
                thread_ts, channel_id
            );
            post_thread_reply(
                &state,
                &channel_id,
                &thread_ts,
                "No session found for this thread. It may have been cleared or never created.",
            )
            .await;
            return;
        }
    };

    if session.status == SessionStatus::Error {
        // Clean up any stale handle for this thread.
        state.agent_handles.lock().await.remove(&thread_ts);
        state.last_plan.lock().await.remove(&thread_ts);
        post_thread_reply(
            &state,
            &channel_id,
            &thread_ts,
            "This session is in an error state. Start a new conversation in the channel.",
        )
        .await;
        return;
    }

    // Concurrency guard — interrupt running agent if busy.
    {
        let mut guard = state.in_progress.lock().await;
        if guard.contains(&thread_ts) {
            // Kill the running agent so the new message takes over.
            if let Some(kill_tx) = state.kill_senders.lock().await.remove(&thread_ts) {
                let _ = kill_tx.send(());
            }
            state.agent_handles.lock().await.remove(&thread_ts);
            info!("Interrupted running agent for thread {}", thread_ts);
        }
        guard.insert(thread_ts.clone());
    }

    add_reaction(&state, &channel_id, &message_ts, "hourglass_flowing_sand").await;

    let repo_config = match state.config.repos.get(&session.repo) {
        Some(r) => r,
        None => {
            remove_in_progress(&state, &thread_ts).await;
            return;
        }
    };

    let agent = match state.agents.get(&repo_config.agent) {
        Some(a) => a.clone(),
        None => {
            remove_in_progress(&state, &thread_ts).await;
            return;
        }
    };

    let merged_tools = repo_config.merged_tools(&state.config.defaults);
    let system_prompt = state.config.defaults.append_system_prompt.clone();
    let session_id = session.session_id.clone();
    let repo_path = session.repo_path.clone();
    let repo_name = session.repo.clone();
    let state_clone = state.clone();
    let thread_ts_clone = thread_ts.clone();

    tokio::spawn(async move {
        // Get existing handle or respawn with --resume.
        let mut handle_opt = state_clone
            .agent_handles
            .lock()
            .await
            .remove(&thread_ts_clone);

        if handle_opt.is_none() {
            info!(
                "No agent handle for thread {}, respawning with --resume {}",
                thread_ts_clone, session_id
            );
            match agent
                .spawn(
                    &repo_path,
                    &merged_tools,
                    system_prompt.as_deref(),
                    Some(&session_id),
                    None,
                )
                .await
            {
                Ok(h) => handle_opt = Some(h),
                Err(e) => {
                    error!(
                        "Failed to respawn agent for thread {}: {}",
                        thread_ts_clone, e
                    );
                    post_thread_reply(
                        &state_clone,
                        &channel_id,
                        &thread_ts_clone,
                        &format!("Agent error: {}", e),
                    )
                    .await;
                    if let Err(e) = state_clone
                        .sessions
                        .update(&thread_ts_clone, move |s| {
                            s.status = SessionStatus::Error;
                        })
                        .await
                    {
                        error!("Failed to mark session as error: {}", e);
                    }
                    remove_reaction(
                        &state_clone,
                        &channel_id,
                        &message_ts,
                        "hourglass_flowing_sand",
                    )
                    .await;
                    remove_in_progress(&state_clone, &thread_ts_clone).await;
                    return;
                }
            }
        }

        let handle = handle_opt.unwrap();

        if !run_agent_turn(
            &state_clone,
            &channel_id,
            &thread_ts_clone,
            handle,
            &text,
            "_Thinking..._",
            EventResultConfig {
                title: None,
                recoverable: true,
                retry_first_chunk: false,
                hourglass_ts: message_ts.clone(),
                status_ts: None,
                repo_name,
            },
            SessionAction::Update {
                reset_status: false,
            },
        )
        .await
        {
            post_thread_reply(
                &state_clone,
                &channel_id,
                &thread_ts_clone,
                "Failed to send message to agent.",
            )
            .await;
            remove_reaction(
                &state_clone,
                &channel_id,
                &message_ts,
                "hourglass_flowing_sand",
            )
            .await;
            remove_in_progress(&state_clone, &thread_ts_clone).await;
        }
    });
}

// ── Event processing ──────────────────────────────────────────────────

pub(crate) enum EventResult {
    TurnComplete {
        result_text: Option<String>,
        subtype: String,
        num_turns: u32,
        duration_ms: u64,
        is_error: bool,
        session_id: String,
        /// True when the response was already streamed to Slack (live mode).
        /// Callers should skip re-posting the response text.
        streamed: bool,
        /// True when a plan file was already posted to Slack during this turn.
        /// Callers should suppress the duplicate result text.
        plan_posted: bool,
        /// Summary of interesting tool usage during this turn (e.g. Edit, Write, WebSearch).
        tool_summary: Option<String>,
    },
    ProcessExited {
        code: Option<i32>,
    },
    ChannelClosed,
}

/// Accumulates interesting tool usage during a turn for a summary footer.
struct ToolUseSummary {
    tools: Vec<(String, Vec<String>)>,
}

impl ToolUseSummary {
    fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Record a tool use. Only tracks interesting tools; skips noisy ones
    /// like Read, Grep, Glob, etc.
    fn record(&mut self, name: &str, input: &serde_json::Value) {
        match name {
            "Edit" | "Write" => {
                let detail = input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .and_then(|p| {
                        // Skip plan file writes/edits from the summary
                        if (name == "Write" || name == "Edit") && p.contains(".claude/plans/") {
                            return None;
                        }
                        // Extract basename
                        Some(p.rsplit('/').next().unwrap_or(p).to_string())
                    });

                if let Some(basename) = detail {
                    // Find existing entry for this tool or create one
                    if let Some(entry) = self.tools.iter_mut().find(|(t, _)| t == name) {
                        if !entry.1.contains(&basename) {
                            entry.1.push(basename);
                        }
                    } else {
                        self.tools.push((name.to_string(), vec![basename]));
                    }
                }
            }
            "Task" => {
                let detail = input
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("subagent")
                    .to_string();
                let truncated = if detail.len() > 60 {
                    format!("{}...", &detail[..57])
                } else {
                    detail
                };
                if let Some(entry) = self.tools.iter_mut().find(|(t, _)| t == name) {
                    entry.1.push(truncated);
                } else {
                    self.tools.push((name.to_string(), vec![truncated]));
                }
            }
            "WebSearch" => {
                let query = input
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("search")
                    .to_string();
                if let Some(entry) = self.tools.iter_mut().find(|(t, _)| t == name) {
                    entry.1.push(format!("\"{}\"", query));
                } else {
                    self.tools
                        .push((name.to_string(), vec![format!("\"{}\"", query)]));
                }
            }
            "WebFetch" => {
                let url = input
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("url")
                    .to_string();
                if let Some(entry) = self.tools.iter_mut().find(|(t, _)| t == name) {
                    entry.1.push(url);
                } else {
                    self.tools.push((name.to_string(), vec![url]));
                }
            }
            "Bash" => {
                let cmd = input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("command")
                    .to_string();
                let truncated = if cmd.len() > 60 {
                    format!("`{}...`", &cmd[..57])
                } else {
                    format!("`{}`", cmd)
                };
                if let Some(entry) = self.tools.iter_mut().find(|(t, _)| t == name) {
                    entry.1.push(truncated);
                } else {
                    self.tools.push((name.to_string(), vec![truncated]));
                }
            }
            _ => {} // Skip noisy tools: Read, Grep, Glob, etc.
        }
    }

    /// Format the summary for display, e.g.:
    /// `Edit(config.rs, main.rs) | Write(new_file.rs) | WebSearch("rust async patterns")`
    fn format(&self) -> Option<String> {
        if self.tools.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .tools
            .iter()
            .map(|(name, details)| format!("{}({})", name, details.join(", ")))
            .collect();
        Some(parts.join(" | "))
    }
}

/// Process agent events until TurnComplete or ProcessExited.
/// Posts tool activity and questions to the Slack thread.
pub(crate) async fn process_events(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    handle: &mut AgentHandle,
) -> EventResult {
    let live_mode = state.is_live_mode();
    let mut accumulated_text = String::new();
    let mut status_ts: Option<String> = None;
    let mut last_update = std::time::Instant::now();
    let mut streamed = false;
    let mut plan_posted = false;
    let mut tool_summary = ToolUseSummary::new();
    // Deferred plan: capture the latest plan content instead of posting inline.
    // Posted once at TurnComplete so repeated writes only show the final version.
    let mut latest_plan: Option<String> = None;
    // Track plan file path when an Edit (not Write) targets a plan file,
    // so we can read the full content from disk at TurnComplete.
    let mut plan_edit_path: Option<String> = None;

    loop {
        let event = match handle.receiver.recv().await {
            Some(e) => e,
            None => return EventResult::ChannelClosed,
        };

        match event {
            AgentEvent::SessionInit { session_id, model } => {
                debug!("Session init: id={}, model={}", session_id, model);
                handle.session_id = Some(session_id);
            }
            AgentEvent::Text(text) => {
                // Check bounds before accumulating to prevent unbounded growth.
                if should_flush_accumulated(
                    &accumulated_text,
                    &text,
                    state.config.tuning.max_accumulated_text_bytes,
                ) {
                    // Flush current accumulated text before adding more.
                    if !accumulated_text.is_empty() {
                        let display = truncate_text(&accumulated_text, SLACK_UPDATE_MAX_CHARS);
                        post_thread_reply(state, channel_id, thread_ts, &display).await;
                        accumulated_text.clear();
                        status_ts = None;
                        streamed = true;
                    }
                }

                accumulated_text.push_str(&text);

                if live_mode
                    && last_update.elapsed()
                        > std::time::Duration::from_secs(
                            state.config.tuning.live_update_interval_secs,
                        )
                {
                    status_ts = flush_to_slack(
                        state,
                        channel_id,
                        thread_ts,
                        &mut accumulated_text,
                        status_ts,
                    )
                    .await;
                    streamed = true;
                    last_update = std::time::Instant::now();
                }
            }
            AgentEvent::ToolUse {
                ref name,
                ref input,
            } => {
                // Record tool usage for the summary footer.
                tool_summary.record(name, input);

                // Capture plan files written to ~/.claude/plans/ for deferred posting.
                if name == "Write"
                    && let Some(path) = input.get("file_path").and_then(|v| v.as_str())
                    && path.contains(".claude/plans/")
                    && let Some(content) = input.get("content").and_then(|v| v.as_str())
                {
                    // Validate plan size before processing.
                    let max_size = state.config.tuning.max_accumulated_text_bytes;
                    if let Err(e) = validate_plan_size(content, max_size) {
                        warn!("Plan validation failed: {}", e);
                        post_thread_reply(state, channel_id, thread_ts, &format!("⚠️ {}", e)).await;
                    } else {
                        // Defer posting — store the latest version to post at TurnComplete.
                        latest_plan = Some(content.to_string());

                        // Always update last_plan for !execute command.
                        state
                            .last_plan
                            .lock()
                            .await
                            .insert(thread_ts.to_string(), content.to_string());
                    }
                }

                // Detect Edit tool calls targeting plan files so we can read
                // the full updated content from disk at TurnComplete.
                if name == "Edit"
                    && let Some(path) = input.get("file_path").and_then(|v| v.as_str())
                    && path.contains(".claude/plans/")
                {
                    plan_edit_path = Some(path.to_string());
                }

                if live_mode {
                    // Append tool call notifications to accumulated text,
                    // using the same edit-message pattern as regular text.
                    if let Some(msg) = formatting::format_tool_use(name, input) {
                        if !accumulated_text.is_empty() {
                            accumulated_text.push('\n');
                        }
                        accumulated_text.push_str(&msg);

                        status_ts = flush_to_slack(
                            state,
                            channel_id,
                            thread_ts,
                            &mut accumulated_text,
                            status_ts,
                        )
                        .await;
                        streamed = true;
                        last_update = std::time::Instant::now();
                    }
                }
            }
            AgentEvent::QuestionPending {
                request_id,
                questions,
            } => {
                // Post the question to Slack.
                let formatted = format_questions(&questions);
                post_thread_reply(state, channel_id, thread_ts, &formatted).await;

                // Create a oneshot channel to receive the user's reply.
                let (answer_tx, answer_rx) = oneshot::channel::<String>();
                state
                    .pending_answers
                    .lock()
                    .await
                    .insert(thread_ts.to_string(), answer_tx);

                // Remove from in_progress so the user's thread reply is accepted
                // (instead of being treated as a new prompt that interrupts the agent).
                remove_in_progress(state, thread_ts).await;

                // Wait for the user's reply or cancellation (!stop drops the sender).
                match answer_rx.await {
                    Ok(answer) => {
                        // Re-add to in_progress now that we're resuming.
                        state.in_progress.lock().await.insert(thread_ts.to_string());

                        // Send the control response to approve and inject the answer.
                        let resp = protocol::answer_question(&request_id, &questions, &answer);
                        if let Err(e) = handle.stdin_tx.send(resp).await {
                            warn!("Failed to send answer control response: {}", e);
                            return EventResult::ChannelClosed;
                        }
                        // Continue the event loop — Claude will proceed with the answer.
                    }
                    Err(_) => {
                        // Sender was dropped (e.g. by !stop) — abort.
                        return EventResult::ChannelClosed;
                    }
                }
            }
            AgentEvent::ToolApprovalPending {
                request_id,
                tool_name,
                tool_input,
            } => {
                // Post the tool approval request to Slack.
                let msg = formatting::format_tool_approval(&tool_name, &tool_input);
                post_thread_reply(state, channel_id, thread_ts, &msg).await;

                // Create a oneshot channel to receive the user's approval/denial.
                let (approval_tx, approval_rx) = oneshot::channel::<bool>();
                state
                    .pending_tool_approvals
                    .lock()
                    .await
                    .insert(thread_ts.to_string(), approval_tx);

                // Remove from in_progress so the user's thread reply is accepted
                // (instead of being treated as a new prompt that interrupts the agent).
                remove_in_progress(state, thread_ts).await;

                // Wait for the user's reply or cancellation (/stop drops the sender).
                match approval_rx.await {
                    Ok(approved) => {
                        // Re-add to in_progress now that we're resuming.
                        state.in_progress.lock().await.insert(thread_ts.to_string());

                        let resp = if approved {
                            protocol::approve_tool(&request_id)
                        } else {
                            protocol::deny_tool(&request_id)
                        };
                        if let Err(e) = handle.stdin_tx.send(resp).await {
                            warn!("Failed to send tool approval response: {}", e);
                            return EventResult::ChannelClosed;
                        }
                        // Continue the event loop — Claude will proceed.
                    }
                    Err(_) => {
                        // Sender was dropped (e.g. by /stop) — abort.
                        return EventResult::ChannelClosed;
                    }
                }
            }
            AgentEvent::TurnComplete {
                result,
                subtype,
                num_turns,
                duration_ms,
                is_error,
                session_id,
            } => {
                // In live mode, finalize any remaining text so the streamed
                // messages ARE the response (no delete-and-repost).
                if live_mode && !accumulated_text.is_empty() {
                    let display = truncate_text(&accumulated_text, SLACK_UPDATE_MAX_CHARS);
                    if let Some(ref ts) = status_ts {
                        update_message(state, channel_id, ts, &display).await;
                    } else {
                        post_thread_reply(state, channel_id, thread_ts, &display).await;
                    }
                    streamed = true;
                } else if let Some(ref ts) = status_ts {
                    // No remaining text but a status message exists — clean it up.
                    delete_message(state, channel_id, ts).await;
                }

                // If a plan file was edited (not written fresh), read the
                // full updated content from disk so we can post it.
                if latest_plan.is_none()
                    && let Some(ref path) = plan_edit_path
                {
                    match tokio::fs::read_to_string(path).await {
                        Ok(content) => {
                            let max_size = state.config.tuning.max_accumulated_text_bytes;
                            if let Err(e) = validate_plan_size(&content, max_size) {
                                warn!("Edited plan validation failed: {}", e);
                                post_thread_reply(
                                    state,
                                    channel_id,
                                    thread_ts,
                                    &format!("⚠️ {}", e),
                                )
                                .await;
                            } else {
                                state
                                    .last_plan
                                    .lock()
                                    .await
                                    .insert(thread_ts.to_string(), content.clone());
                                latest_plan = Some(content);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to read edited plan file {}: {}", path, e);
                        }
                    }
                }

                // Post deferred plan if one was captured during this turn.
                if let Some(plan_content) = latest_plan {
                    let formatted_content =
                        crate::slack::formatting::markdown_to_slack(&plan_content);
                    let plan_msg = format!("*Plan*\n\n{}", formatted_content);
                    post_thread_reply(state, channel_id, thread_ts, &plan_msg).await;
                    post_thread_reply(
                        state,
                        channel_id,
                        thread_ts,
                        "_Reply `!execute` to run this plan with a fresh context window._",
                    )
                    .await;
                    plan_posted = true;
                }

                return EventResult::TurnComplete {
                    result_text: result,
                    subtype,
                    num_turns,
                    duration_ms,
                    is_error,
                    session_id,
                    streamed,
                    plan_posted,
                    tool_summary: tool_summary.format(),
                };
            }
            AgentEvent::ToolProgress { tool_name } => {
                debug!("Tool progress: {}", tool_name);
            }
            AgentEvent::ProcessExited { code } => {
                if let Some(ref ts) = status_ts {
                    delete_message(state, channel_id, ts).await;
                }
                return EventResult::ProcessExited { code };
            }
        }
    }
}

// ── !stop / !status / !model ───────────────────────────────────────────

async fn handle_stop(state: &AppState, channel_id: &str, thread_ts: &str) {
    // Kill the agent process if running.
    let killed = if let Some(kill_tx) = state.kill_senders.lock().await.remove(thread_ts) {
        let _ = kill_tx.send(());
        true
    } else {
        false
    };

    // Clean up handle, stored plan, pending answers/approvals, and thread model override.
    state.agent_handles.lock().await.remove(thread_ts);
    state.last_plan.lock().await.remove(thread_ts);
    state.thread_models.lock().await.remove(thread_ts);
    // Dropping the senders unblocks process_events → ChannelClosed.
    state.pending_answers.lock().await.remove(thread_ts);
    state.pending_tool_approvals.lock().await.remove(thread_ts);

    if killed {
        post_thread_reply(state, channel_id, thread_ts, "Agent stopped.").await;
    } else if state.sessions.get_by_thread(thread_ts).await.is_some() {
        post_thread_reply(
            state,
            channel_id,
            thread_ts,
            "No agent running in this thread.",
        )
        .await;
    } else {
        post_thread_reply(
            state,
            channel_id,
            thread_ts,
            "No active session in this thread.",
        )
        .await;
    }
}

async fn handle_status(state: &AppState, channel_id: &str, thread_ts: &str) {
    let has_handle = state.agent_handles.lock().await.contains_key(thread_ts);

    match state.sessions.get_by_thread(thread_ts).await {
        Some(session) => {
            let process_status = if has_handle {
                "running"
            } else {
                "not running (will auto-recover on next reply)"
            };
            let model = state.resolved_model(&session.repo, Some(thread_ts)).await;
            let status_text = format!(
                "*Session Status*\nRepo: `{}`\nAgent: {:?}\nModel: `{}`\nStatus: {:?}\nProcess: {}\nTotal turns: {}\nCreated: {}\nLast active: {}",
                session.repo,
                session.agent_kind,
                model,
                session.status,
                process_status,
                session.total_turns,
                session.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
                session.last_active.format("%Y-%m-%d %H:%M:%S UTC"),
            );
            post_thread_reply(state, channel_id, thread_ts, &status_text).await;
        }
        None => {
            post_thread_reply(state, channel_id, thread_ts, "No session in this thread.").await;
        }
    }
}

/// Resolve a model alias to a full model ID.
fn resolve_model_alias(input: &str) -> Option<String> {
    match input.to_lowercase().as_str() {
        "opus" => Some("claude-opus-4-6".to_string()),
        "sonnet" => Some("claude-sonnet-4-5-20250929".to_string()),
        "haiku" => Some("claude-haiku-4-5-20251001".to_string()),
        _ if input.starts_with("claude-") => Some(input.to_string()),
        _ => None,
    }
}

async fn handle_model_command(state: &AppState, channel_id: &str, thread_ts: &str, arg: &str) {
    if arg.is_empty() {
        // Show current model.
        let model = match state.sessions.get_by_thread(thread_ts).await {
            Some(session) => state.resolved_model(&session.repo, Some(thread_ts)).await,
            None => crate::config::DEFAULT_MODEL.to_string(),
        };
        post_thread_reply(
            state,
            channel_id,
            thread_ts,
            &format!(
                "Current model: `{}`\nUse `!model <name>` to change. Aliases: `opus`, `sonnet`, `haiku`.",
                model
            ),
        )
        .await;
        return;
    }

    match resolve_model_alias(arg) {
        Some(model_id) => {
            state
                .thread_models
                .lock()
                .await
                .insert(thread_ts.to_string(), model_id.clone());
            post_thread_reply(
                state,
                channel_id,
                thread_ts,
                &format!(
                    "Model set to `{}` for this thread. New sessions will use this model.",
                    model_id
                ),
            )
            .await;
        }
        None => {
            post_thread_reply(
                state,
                channel_id,
                thread_ts,
                &format!(
                    "Unknown model `{}`. Use an alias (`opus`, `sonnet`, `haiku`) or a full model ID (e.g. `claude-sonnet-4-5-20250929`).",
                    arg
                ),
            )
            .await;
        }
    }
}

/// Execute the last detected plan with a fresh context window (no --resume).
async fn handle_execute_plan(
    state: AppState,
    channel_id: String,
    thread_ts: String,
    message_ts: String,
) {
    // Retrieve stored plan content.
    let plan_content = match state.last_plan.lock().await.remove(&thread_ts) {
        Some(plan) => plan,
        None => {
            post_thread_reply(
                &state,
                &channel_id,
                &thread_ts,
                "No plan found for this thread. Ask Claude to create a plan first.",
            )
            .await;
            return;
        }
    };

    // Look up session for repo info.
    let session = match state.sessions.get_by_thread(&thread_ts).await {
        Some(s) => s,
        None => {
            post_thread_reply(
                &state,
                &channel_id,
                &thread_ts,
                "No session found for this thread.",
            )
            .await;
            return;
        }
    };

    let repo_config = match state.config.repos.get(&session.repo) {
        Some(r) => r,
        None => return,
    };

    let agent = match state.agents.get(&repo_config.agent) {
        Some(a) => a.clone(),
        None => return,
    };

    // Kill existing agent if running.
    if let Some(mut handle) = state.agent_handles.lock().await.remove(&thread_ts)
        && let Some(kill_tx) = handle.kill_tx.take()
    {
        let _ = kill_tx.send(());
    }

    // Concurrency guard.
    state.in_progress.lock().await.insert(thread_ts.clone());
    add_reaction(&state, &channel_id, &message_ts, "hourglass_flowing_sand").await;

    // Mark repo as pending so the session sync doesn't pick up the fresh
    // session file and duplicate it as a new channel message.
    let repo_name = session.repo.clone();
    state
        .pending_repos
        .lock()
        .await
        .insert(repo_name.clone(), Instant::now());
    let pending_guard = PendingRepoGuard {
        pending_repos: state.pending_repos.clone(),
        repo_name: repo_name.clone(),
    };

    let merged_tools = repo_config.merged_tools(&state.config.defaults);
    let system_prompt = state.config.defaults.append_system_prompt.clone();
    let repo_path = session.repo_path.clone();
    let model = state.resolved_model(&session.repo, Some(&thread_ts)).await;
    let state_clone = state.clone();
    let thread_ts_clone = thread_ts.clone();

    tokio::spawn(async move {
        let _pending_guard = pending_guard;

        // Spawn a fresh session (no --resume) for clean context.
        let handle = match agent
            .spawn(
                &repo_path,
                &merged_tools,
                system_prompt.as_deref(),
                None,
                Some(&model),
            )
            .await
        {
            Ok(h) => h,
            Err(e) => {
                error!("Failed to spawn agent for plan execution: {}", e);
                post_thread_reply(
                    &state_clone,
                    &channel_id,
                    &thread_ts_clone,
                    &format!("Failed to start session: {}", e),
                )
                .await;
                remove_reaction(
                    &state_clone,
                    &channel_id,
                    &message_ts,
                    "hourglass_flowing_sand",
                )
                .await;
                remove_in_progress(&state_clone, &thread_ts_clone).await;
                return;
            }
        };

        let prompt = format!("Execute the following plan:\n\n{}", plan_content);

        if !run_agent_turn(
            &state_clone,
            &channel_id,
            &thread_ts_clone,
            handle,
            &prompt,
            "_Executing plan with fresh context..._",
            EventResultConfig {
                title: None,
                recoverable: true,
                retry_first_chunk: false,
                hourglass_ts: message_ts.clone(),
                status_ts: None,
                repo_name,
            },
            SessionAction::Update { reset_status: true },
        )
        .await
        {
            post_thread_reply(
                &state_clone,
                &channel_id,
                &thread_ts_clone,
                "Failed to send plan to agent.",
            )
            .await;
            remove_reaction(
                &state_clone,
                &channel_id,
                &message_ts,
                "hourglass_flowing_sand",
            )
            .await;
            remove_in_progress(&state_clone, &thread_ts_clone).await;
        }
    });
}

pub(crate) async fn remove_in_progress(state: &AppState, thread_ts: &str) {
    let mut guard = state.in_progress.lock().await;
    guard.remove(thread_ts);
}

// ── Slash command handler ──────────────────────────────────────────────

/// Handle /claude slash commands (admin only: schedule, schedules, unschedule, help, sessions).
pub async fn handle_slash_command(
    state: AppState,
    event: SlackCommandEvent,
) -> SlackCommandEventResponse {
    let user_id = event.user_id.to_string();
    if !state.is_allowed_user(&user_id) {
        return SlackCommandEventResponse::new(
            SlackMessageContent::new().with_text("Unauthorized.".to_string()),
        );
    }

    let text = event.text.as_deref().unwrap_or("").trim().to_string();
    let parts: Vec<&str> = text.splitn(4, ' ').collect();

    match parts.first().copied().unwrap_or("help") {
        "sessions" => {
            let sessions = state.sessions.active_sessions().await;
            if sessions.is_empty() {
                return SlackCommandEventResponse::new(
                    SlackMessageContent::new().with_text("No active sessions.".to_string()),
                );
            }
            let mut lines = vec!["*Active Sessions*".to_string()];
            for s in &sessions {
                let has_handle = state.agent_handles.lock().await.contains_key(&s.thread_ts);
                let process = if has_handle { "running" } else { "idle" };
                lines.push(format!(
                    "- `{}` ({:?}) — {} turns, process: {}, last active {}",
                    s.repo,
                    s.agent_kind,
                    s.total_turns,
                    process,
                    s.last_active.format("%H:%M:%S UTC"),
                ));
            }
            SlackCommandEventResponse::new(SlackMessageContent::new().with_text(lines.join("\n")))
        }
        _ => {
            let help = concat!(
                "*Hermes — Claude Code via Slack*\n\n",
                "Just type in a repo channel to start a session. Reply in the thread to continue.\n\n",
                "*Thread commands:*\n",
                "`!status` — show session info\n",
                "`!stop` — stop the session\n",
                "`!model` — show current model\n",
                "`!model <name>` — set model (`opus`, `sonnet`, `haiku`, or full ID)\n",
                "`!execute` — run the last plan with a fresh context\n\n",
                "*Slash commands:*\n",
                "`/claude sessions` — list active sessions\n",
                "`/claude help` — this message",
            );
            SlackCommandEventResponse::new(SlackMessageContent::new().with_text(help.to_string()))
        }
    }
}

// ── Title ─────────────────────────────────────────────────────────────

/// Title: first ~80 chars of the prompt, trimmed at word boundary.
fn fallback_title(prompt: &str) -> String {
    if prompt.len() <= FALLBACK_TITLE_MAX_LEN {
        return prompt.to_string();
    }
    let end = crate::util::floor_char_boundary(prompt, FALLBACK_TITLE_MAX_LEN);
    match prompt[..end].rfind(' ') {
        Some(pos) => format!("{}...", &prompt[..pos]),
        None => format!("{}...", &prompt[..end]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_exit_code_some() {
        assert_eq!(format_exit_code(Some(0)), "0");
        assert_eq!(format_exit_code(Some(1)), "1");
        assert_eq!(format_exit_code(Some(-1)), "-1");
    }

    #[test]
    fn test_format_exit_code_none() {
        assert_eq!(format_exit_code(None), "unknown");
    }

    #[test]
    fn test_agent_exit_message_recoverable() {
        let msg = agent_exit_message(Some(1), true);
        assert!(msg.contains("1"));
        assert!(msg.contains("auto-recover"));
    }

    #[test]
    fn test_agent_exit_message_not_recoverable() {
        let msg = agent_exit_message(Some(1), false);
        assert!(msg.contains("1"));
        assert!(!msg.contains("auto-recover"));
    }

    #[test]
    fn test_fallback_title_short() {
        let prompt = "Fix the login bug";
        assert_eq!(fallback_title(prompt), "Fix the login bug");
    }

    #[test]
    fn test_fallback_title_long() {
        let prompt = "a ".repeat(100); // 200 chars
        let title = fallback_title(&prompt);
        assert!(title.ends_with("..."));
        assert!(title.len() <= FALLBACK_TITLE_MAX_LEN + 3); // +3 for "..."
    }

    #[test]
    fn test_agent_disconnected_message() {
        assert!(agent_disconnected_message(true).contains("auto-recover"));
        assert!(!agent_disconnected_message(false).contains("auto-recover"));
    }

    #[test]
    fn test_resolve_model_alias_opus() {
        assert_eq!(
            resolve_model_alias("opus"),
            Some("claude-opus-4-6".to_string())
        );
    }

    #[test]
    fn test_resolve_model_alias_sonnet() {
        assert_eq!(
            resolve_model_alias("sonnet"),
            Some("claude-sonnet-4-5-20250929".to_string())
        );
    }

    #[test]
    fn test_resolve_model_alias_haiku() {
        assert_eq!(
            resolve_model_alias("haiku"),
            Some("claude-haiku-4-5-20251001".to_string())
        );
    }

    #[test]
    fn test_resolve_model_alias_full_id() {
        assert_eq!(
            resolve_model_alias("claude-sonnet-4-20250514"),
            Some("claude-sonnet-4-20250514".to_string())
        );
    }

    #[test]
    fn test_resolve_model_alias_unknown() {
        assert_eq!(resolve_model_alias("gpt-4"), None);
    }

    #[test]
    fn test_resolve_model_alias_case_insensitive() {
        assert_eq!(
            resolve_model_alias("Opus"),
            Some("claude-opus-4-6".to_string())
        );
        assert_eq!(
            resolve_model_alias("SONNET"),
            Some("claude-sonnet-4-5-20250929".to_string())
        );
    }
}
