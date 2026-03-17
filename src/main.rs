mod agent;
mod config;
mod error;
mod session;
mod slack;
mod sync;
mod util;

use crate::agent::Agent;
use crate::agent::claude::ClaudeAgent;
use crate::config::AgentKind;
use crate::session::SessionStore;
use crate::slack::AppState;
use slack_morphism::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    util::install_panic_hook();

    info!("Loading configuration...");
    let config = config::Config::load()?;
    info!(
        "Loaded {} repo(s): {:?}",
        config.repos.len(),
        config.repos.keys().collect::<Vec<_>>()
    );

    // Create session store.
    let sessions = SessionStore::new(config.sessions_file.clone());

    // Create agents.
    let mut agents: HashMap<AgentKind, Arc<dyn Agent>> = HashMap::new();
    agents.insert(AgentKind::Claude, Arc::new(ClaudeAgent));

    // Create Slack client.
    let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
    let bot_token = SlackApiToken::new(config.slack.bot_token.clone().into());
    let app_token = SlackApiToken::new(config.slack.app_token.clone().into());

    // Get bot user ID to filter self-messages.
    let bot_user_id = {
        let session = client.open_session(&bot_token);
        let resp = session.auth_test().await?;
        resp.user_id.to_string()
    };
    info!("Bot user ID: {}", bot_user_id);

    // Auto-create channels for repos.
    info!("Ensuring repo channels exist...");
    let repo_channels = slack::ensure_repo_channels(&config, &client, &bot_token).await?;
    for (repo, channel_id) in &repo_channels {
        info!("  {} → #{}", repo, channel_id);
    }

    info!("Streaming mode: {}", config.defaults.streaming_mode);

    let config = Arc::new(config);
    let agents = Arc::new(agents);

    // Build shared state.
    let state = AppState {
        config: config.clone(),
        sessions,
        agents: agents.clone(),
        bot_token: bot_token.clone(),
        slack_client: client.clone(),
        repo_channels: Arc::new(tokio::sync::RwLock::new(repo_channels)),
        bot_user_id: Arc::new(bot_user_id),
        in_progress: Arc::new(Mutex::new(HashSet::new())),
        pending_repos: Arc::new(Mutex::new(HashMap::new())),
        pending_session_ids: Arc::new(Mutex::new(HashSet::new())),
        agent_handles: Arc::new(Mutex::new(HashMap::new())),
        kill_senders: Arc::new(Mutex::new(HashMap::new())),
        last_plan: Arc::new(Mutex::new(HashMap::new())),
        pending_answers: Arc::new(Mutex::new(HashMap::new())),
        pending_tool_approvals: Arc::new(Mutex::new(HashMap::new())),
        rate_limiter: Arc::new(Mutex::new(HashMap::new())),
        thread_models: Arc::new(Mutex::new(HashMap::new())),
    };

    // Spawn session sync (watch for local Claude Code sessions).
    // Skip entirely if no repos have sync enabled.
    let any_sync_enabled = config
        .repos
        .values()
        .any(|r| r.sync_enabled(&config.defaults));
    let sync_handle = if any_sync_enabled {
        let sync_state = state.clone();
        Some(tokio::spawn(async move {
            sync::sync_sessions(sync_state).await;
        }))
    } else {
        info!("Local session sync disabled for all repos, skipping");
        None
    };

    // Set up Socket Mode listener.
    let shutdown_state = state.clone();
    let socket_mode_callbacks = SlackSocketModeListenerCallbacks::new()
        .with_command_events(handle_command_event)
        .with_push_events(handle_push_event);

    let heartbeat_state = state.clone();

    let listener_environment =
        Arc::new(SlackClientEventsListenerEnvironment::new(client.clone()).with_user_state(state));

    let socket_listener = SlackClientSocketModeListener::new(
        &SlackClientSocketModeConfig::new(),
        listener_environment,
        socket_mode_callbacks,
    );

    info!("Connecting to Slack via Socket Mode...");
    socket_listener.listen_for(&app_token).await?;

    info!("Hermes is running. Ctrl+C to stop.");

    tokio::select! {
        _ = socket_listener.serve() => {
            error!("Socket listener exited unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal (Ctrl+C)");
        }
        _ = async {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.tick().await; // Skip the immediate first tick.
            let mut tick_count: u32 = 0;
            loop {
                interval.tick().await;
                tick_count += 1;

                // Run cleanup every 60 seconds.
                heartbeat_state.cleanup_stale_pending_repos().await;

                // Prune expired sessions every 5 minutes (every 5 ticks).
                if tick_count.is_multiple_of(5) {
                    heartbeat_state
                        .sessions
                        .prune_expired(heartbeat_state.config.tuning.session_ttl_days)
                        .await;
                }

                let active = heartbeat_state.sessions.active_sessions().await.len();
                let handles = heartbeat_state.agent_handles.lock().await.len();
                let pending = heartbeat_state.pending_repos.lock().await.len();
                info!(
                    "Heartbeat: {} active session(s), {} running agent(s), {} pending repo(s)",
                    active, handles, pending
                );
            }
        } => {}
    }

    // Kill all running agent processes.
    let mut kill_senders = shutdown_state.kill_senders.lock().await;
    let count = kill_senders.len();
    for (thread_ts, kill_tx) in kill_senders.drain() {
        let _ = kill_tx.send(());
        info!("Killed agent for thread {}", thread_ts);
    }
    if count > 0 {
        info!("Cleaned up {} agent process(es)", count);
    }
    shutdown_state.agent_handles.lock().await.clear();

    // Clear cached plans and pending answers.
    shutdown_state.last_plan.lock().await.clear();
    shutdown_state.pending_answers.lock().await.clear();

    // Abort the sync task gracefully (it's an infinite loop, so we abort it).
    if let Some(sync_handle) = sync_handle {
        sync_handle.abort();
        match sync_handle.await {
            Ok(_) => info!("Session sync task stopped cleanly"),
            Err(e) if e.is_cancelled() => info!("Session sync task cancelled"),
            Err(e) => error!("Session sync task failed: {}", e),
        }
    }

    info!("Shutdown complete");
    Ok(())
}

async fn handle_command_event(
    event: SlackCommandEvent,
    _client: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> UserCallbackResult<SlackCommandEventResponse> {
    let app_state = {
        let guard = states.read().await;
        match guard.get_user_state::<AppState>() {
            Some(s) => s.clone(),
            None => {
                error!("AppState not found in user state (command handler)");
                return Err("AppState not found".into());
            }
        }
    };
    Ok(slack::handle_slash_command(app_state, event).await)
}

async fn handle_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let app_state = {
        let guard = states.read().await;
        match guard.get_user_state::<AppState>() {
            Some(s) => s.clone(),
            None => {
                error!("AppState not found in user state (push handler)");
                return Err("AppState not found".into());
            }
        }
    };

    if let SlackEventCallbackBody::Message(msg_event) = event.event {
        slack::handle_message(app_state, msg_event).await;
    }

    Ok(())
}
