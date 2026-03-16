//! Slack API wrappers for posting messages, managing channels, and reactions.
//!
//! Provides higher-level functions built on top of `slack-morphism` for:
//! - Auto-creating channels for repositories
//! - Posting and updating messages with rate limiting
//! - Managing reactions for status indicators
//!
//! All functions handle errors gracefully, logging failures instead of panicking.

use crate::config::Config;
use crate::error::{HermesError, Result};
use slack_morphism::prelude::*;
use std::collections::HashMap;
use tracing::{error, info, warn};

use super::AppState;

// ── Channel auto-creation ──────────────────────────────────────────────

/// Ensures a Slack channel exists for each configured repository.
///
/// For each repo in the configuration:
/// - If the channel already exists, reuses it
/// - If not, creates a new channel and invites all allowed users
/// - Handles reserved channel names gracefully with helpful error messages
///
/// # Arguments
///
/// * `config` - The application configuration containing repo definitions
/// * `client` - Slack API client for making requests
/// * `token` - Bot token for authentication
///
/// # Returns
///
/// A map of repository name to Slack channel ID
///
/// # Errors
///
/// Returns an error if:
/// - Slack API calls fail (network, permissions, etc.)
/// - A channel name is reserved and no alternative is configured
#[must_use = "the returned channel mapping is required for routing messages to repos"]
pub async fn ensure_repo_channels(
    config: &Config,
    client: &SlackHyperClient,
    token: &SlackApiToken,
) -> Result<HashMap<String, String>> {
    let session = client.open_session(token);
    let mut mapping = HashMap::new();

    // List existing channels to avoid creating duplicates.
    let existing = list_all_channels(&session).await?;

    for (repo_name, repo_config) in &config.repos {
        let channel_name = repo_config
            .channel
            .clone()
            .unwrap_or_else(|| repo_name.replace('.', "-").to_lowercase());

        if let Some(existing_id) = existing.get(&channel_name) {
            info!(
                "Channel #{} already exists for repo '{}'",
                channel_name, repo_name
            );
            mapping.insert(repo_name.clone(), existing_id.clone());
        } else {
            info!(
                "Creating channel #{} for repo '{}'",
                channel_name, repo_name
            );
            match create_channel(&session, &channel_name).await {
                Ok(id) => {
                    // Invite allowed users to the new channel.
                    for user_id in &config.slack.allowed_users {
                        invite_to_channel(&session, &id, user_id).await;
                    }
                    mapping.insert(repo_name.clone(), id);
                }
                Err(HermesError::SlackApi(ref msg)) if msg.contains("name_taken") => {
                    error!(
                        "Channel name '{}' is reserved by Slack (likely a previously deleted channel). \
                         Set a custom channel name in config: [repos.{}] channel = \"some-other-name\"",
                        channel_name, repo_name
                    );
                    return Err(HermesError::SlackApi(format!(
                        "Channel name '{}' is reserved. Set `channel` in [repos.{}] config to use a different name.",
                        channel_name, repo_name
                    )));
                }
                Err(e) => {
                    error!("Failed to create channel #{}: {}", channel_name, e);
                    return Err(e);
                }
            }
        }
    }

    Ok(mapping)
}

async fn list_all_channels(
    session: &SlackClientSession<'_, impl SlackClientHttpConnector + Send>,
) -> Result<HashMap<String, String>> {
    const MAX_PAGES: usize = 100;
    let mut channels = HashMap::new();
    let mut cursor: Option<SlackCursorId> = None;

    for _ in 0..MAX_PAGES {
        let mut req = SlackApiConversationsListRequest::new()
            .with_exclude_archived(true)
            .with_limit(200);
        if let Some(c) = cursor {
            req = req.with_cursor(c);
        }

        let resp = session
            .conversations_list(&req)
            .await
            .map_err(|e| HermesError::SlackApi(format!("conversations.list: {}", e)))?;

        for ch in &resp.channels {
            if let Some(name) = &ch.name {
                channels.insert(name.to_string(), ch.id.to_string());
            }
        }

        match resp.response_metadata.and_then(|m| m.next_cursor) {
            Some(c) if !c.to_string().is_empty() => cursor = Some(c),
            _ => break,
        }
    }

    Ok(channels)
}

async fn create_channel(
    session: &SlackClientSession<'_, impl SlackClientHttpConnector + Send>,
    name: &str,
) -> Result<String> {
    let req = SlackApiConversationsCreateRequest::new(name.to_string());

    let resp = session
        .conversations_create(&req)
        .await
        .map_err(|e| HermesError::SlackApi(format!("conversations.create: {}", e)))?;

    Ok(resp.channel.id.to_string())
}

async fn invite_to_channel(
    session: &SlackClientSession<'_, impl SlackClientHttpConnector + Send>,
    channel_id: &str,
    user_id: &str,
) {
    let req = SlackApiConversationsInviteRequest::new(
        SlackChannelId::new(channel_id.to_string()),
        vec![SlackUserId::new(user_id.to_string())],
    );
    match session.conversations_invite(&req).await {
        Ok(_) => info!("Invited user {} to channel {}", user_id, channel_id),
        Err(e) => warn!(
            "Failed to invite user {} to channel {}: {}",
            user_id, channel_id, e
        ),
    }
}

// ── Message posting ────────────────────────────────────────────────────

/// Posts a message as a reply in a Slack thread and returns the message timestamp.
///
/// Respects per-channel rate limits before posting. The returned timestamp
/// can be used to update or delete the message later.
///
/// # Arguments
///
/// * `state` - Application state containing Slack client and rate limiter
/// * `channel_id` - Slack channel ID where the thread exists
/// * `thread_ts` - Thread timestamp (identifies the parent message)
/// * `text` - Message text to post (Slack mrkdwn format)
///
/// # Returns
///
/// The message timestamp on success, or `None` if the post failed
#[must_use = "ignoring the message timestamp means you can't update or track this message"]
pub async fn post_thread_reply_with_ts(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    text: &str,
) -> Option<String> {
    state.rate_limit(channel_id).await;
    let session = state.slack_client.open_session(&state.bot_token);
    let req = SlackApiChatPostMessageRequest::new(
        SlackChannelId::new(channel_id.to_string()),
        SlackMessageContent::new().with_text(text.to_string()),
    )
    .with_thread_ts(SlackTs::new(thread_ts.to_string()));

    match session.chat_post_message(&req).await {
        Ok(resp) => Some(resp.ts.to_string()),
        Err(e) => {
            error!("Failed to post message: {}", e);
            None
        }
    }
}

/// Posts a message as a reply in a Slack thread (fire-and-forget).
///
/// Like [`post_thread_reply_with_ts`] but discards the timestamp.
/// Use this when you don't need to update or track the message later.
///
/// # Arguments
///
/// * `state` - Application state containing Slack client and rate limiter
/// * `channel_id` - Slack channel ID where the thread exists
/// * `thread_ts` - Thread timestamp (identifies the parent message)
/// * `text` - Message text to post (Slack mrkdwn format)
pub async fn post_thread_reply(state: &AppState, channel_id: &str, thread_ts: &str, text: &str) {
    let _ = post_thread_reply_with_ts(state, channel_id, thread_ts, text).await;
}

/// Like `post_thread_reply` but returns true on success, false on failure.
///
/// Useful for retry loops where you need to know if the post succeeded.
pub(crate) async fn post_thread_reply_result(
    state: &AppState,
    channel_id: &str,
    thread_ts: &str,
    text: &str,
) -> bool {
    post_thread_reply_with_ts(state, channel_id, thread_ts, text)
        .await
        .is_some()
}

/// Updates an existing Slack message in-place.
///
/// Uses the Slack `chat.update` API to edit a message's text.
/// Note: Slack has stricter length limits for updates (~4000 chars) than new messages.
///
/// # Arguments
///
/// * `state` - Application state containing Slack client
/// * `channel_id` - Slack channel ID where the message exists
/// * `ts` - Message timestamp (identifies the message to update)
/// * `text` - New message text (Slack mrkdwn format)
pub async fn update_message(state: &AppState, channel_id: &str, ts: &str, text: &str) {
    let session = state.slack_client.open_session(&state.bot_token);
    let req = SlackApiChatUpdateRequest::new(
        SlackChannelId::new(channel_id.to_string()),
        SlackMessageContent::new().with_text(text.to_string()),
        SlackTs::new(ts.to_string()),
    );
    if let Err(e) = session.chat_update(&req).await {
        warn!("Failed to update message: {}", e);
    }
}

/// Delete a Slack message.
pub(crate) async fn delete_message(state: &AppState, channel_id: &str, ts: &str) {
    let session = state.slack_client.open_session(&state.bot_token);
    let req = SlackApiChatDeleteRequest::new(
        SlackChannelId::new(channel_id.to_string()),
        SlackTs::new(ts.to_string()),
    );
    if let Err(e) = session.chat_delete(&req).await {
        warn!("Failed to delete message: {}", e);
    }
}

/// Posts a new top-level message to a Slack channel and returns the timestamp.
///
/// Respects per-channel rate limits before posting. The returned timestamp
/// becomes the thread identifier if users reply to this message.
///
/// # Arguments
///
/// * `state` - Application state containing Slack client and rate limiter
/// * `channel_id` - Slack channel ID where to post the message
/// * `text` - Message text to post (Slack mrkdwn format)
///
/// # Returns
///
/// The message timestamp on success, or `None` if the post failed
#[must_use = "ignoring the thread timestamp means you won't be able to reply to this message"]
pub async fn post_channel_message(
    state: &AppState,
    channel_id: &str,
    text: &str,
) -> Option<String> {
    state.rate_limit(channel_id).await;
    let session = state.slack_client.open_session(&state.bot_token);
    let req = SlackApiChatPostMessageRequest::new(
        SlackChannelId::new(channel_id.to_string()),
        SlackMessageContent::new().with_text(text.to_string()),
    );

    match session.chat_post_message(&req).await {
        Ok(resp) => Some(resp.ts.to_string()),
        Err(e) => {
            error!("Failed to post channel message: {}", e);
            None
        }
    }
}

// ── Reactions ──────────────────────────────────────────────────────────

pub(crate) async fn add_reaction(state: &AppState, channel_id: &str, ts: &str, emoji: &str) {
    let session = state.slack_client.open_session(&state.bot_token);
    let req = SlackApiReactionsAddRequest::new(
        SlackChannelId::new(channel_id.to_string()),
        SlackReactionName::new(emoji.to_string()),
        SlackTs::new(ts.to_string()),
    );
    if let Err(e) = session.reactions_add(&req).await {
        warn!("Failed to add reaction '{}': {}", emoji, e);
    }
}

pub(crate) async fn remove_reaction(state: &AppState, channel_id: &str, ts: &str, emoji: &str) {
    let session = state.slack_client.open_session(&state.bot_token);
    let req = SlackApiReactionsRemoveRequest::new(SlackReactionName::new(emoji.to_string()))
        .with_channel(SlackChannelId::new(channel_id.to_string()))
        .with_timestamp(SlackTs::new(ts.to_string()));
    if let Err(e) = session.reactions_remove(&req).await {
        warn!("Failed to remove reaction '{}': {}", emoji, e);
    }
}
