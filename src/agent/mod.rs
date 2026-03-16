//! Agent abstraction and event model for AI coding agents.
//!
//! This module defines the [`Agent`] trait for spawning and communicating with
//! AI coding agents (like Claude Code CLI). Agents run as subprocesses and
//! communicate via a bidirectional event stream.
//!
//! # Architecture
//!
//! - [`Agent`] trait: Interface for spawning agents
//! - [`AgentHandle`]: Handle for bidirectional communication with a running agent
//! - [`AgentEvent`]: Events emitted by the agent (text, tool use, completion, etc.)
//! - [`AgentResponse`]: Final response format for Slack display
//!
//! # Implementations
//!
//! - [`claude::ClaudeAgent`]: Claude Code CLI integration

pub mod claude;
pub mod protocol;

use crate::error::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use tokio::sync::{mpsc, oneshot};

// ── Event-based agent model ───────────────────────────────────────────

/// Events emitted by an agent during a session.
///
/// Agents communicate asynchronously via events. Handlers receive these
/// events through the `AgentHandle.receiver` channel and respond by:
/// - Displaying text to the user
/// - Posting tool activity notifications
/// - Waiting for user input (questions)
/// - Finalizing the turn when complete
///
/// # Event Flow
///
/// 1. `SessionInit` - Session starts (provides session_id)
/// 2. `Text`, `ToolUse`, `ToolProgress` - Activity during the turn (may repeat)
/// 3. `QuestionPending` - If Claude asks a question (blocks until answered)
/// 4. `TurnComplete` - Turn finishes successfully
///
/// Alternatively, `ProcessExited` can occur at any time if the agent crashes.
#[derive(Debug)]
pub enum AgentEvent {
    /// Session initialized (carries session_id, model).
    SessionInit { session_id: String, model: String },
    /// Text content from Claude.
    Text(String),
    /// Claude is using a tool.
    ToolUse { name: String, input: Value },
    /// Claude is asking the user a question via control_request (AskUserQuestion tool).
    QuestionPending {
        request_id: String,
        questions: Value,
    },
    /// A turn completed.
    TurnComplete {
        result: Option<String>,
        subtype: String,
        num_turns: u32,
        duration_ms: u64,
        is_error: bool,
        session_id: String,
    },
    /// Claude wants to use a tool that requires user approval.
    ToolApprovalPending {
        request_id: String,
        tool_name: String,
        tool_input: Value,
    },
    /// Tool progress heartbeat.
    ToolProgress { tool_name: String },
    /// Agent process exited unexpectedly.
    ProcessExited { code: Option<i32> },
}

/// Handle to a running agent session for bidirectional communication.
///
/// Provides channels for:
/// - Sending user prompts to the agent (`sender`)
/// - Receiving events from the agent (`receiver`)
/// - Sending raw protocol messages like control responses (`stdin_tx`)
/// - Killing the agent process (`kill_tx`)
///
/// The handle is returned by [`Agent::spawn`] and remains valid until the
/// agent process exits or is killed.
pub struct AgentHandle {
    /// Send user messages to the agent.
    pub sender: mpsc::Sender<String>,
    /// Receive events from the agent.
    pub receiver: mpsc::Receiver<AgentEvent>,
    /// Kill the agent process.
    pub kill_tx: Option<oneshot::Sender<()>>,
    /// Session ID (set after SessionInit event).
    pub session_id: Option<String>,
    /// Send raw JSON lines to the agent's stdin (for control responses).
    pub stdin_tx: mpsc::Sender<String>,
}

/// Trait for agent backends (e.g., Claude Code CLI).
///
/// Implementations provide the interface to spawn and communicate with
/// AI coding agents. The agent runs as a subprocess and communicates via
/// stdin/stdout using a stream-json protocol.
///
/// # Example
///
/// ```ignore
/// let agent = ClaudeAgent;
/// let handle = agent.spawn(
///     Path::new("/path/to/repo"),
///     &["Read", "Write", "Bash"],
///     Some("You are a helpful assistant"),
///     None,  // New session
///     Some("claude-opus-4-6"),
/// ).await?;
///
/// // Send a prompt
/// handle.sender.send("Fix the login bug".to_string()).await?;
///
/// // Receive events
/// while let Some(event) = handle.receiver.recv().await {
///     match event {
///         AgentEvent::Text(text) => println!("{}", text),
///         AgentEvent::TurnComplete { .. } => break,
///         _ => {}
///     }
/// }
/// ```
#[async_trait]
pub trait Agent: Send + Sync {
    /// Spawns a new agent session in the specified repository.
    ///
    /// # Arguments
    ///
    /// * `repo_path` - Working directory for the agent (git repo root)
    /// * `allowed_tools` - List of tools the agent can use without asking permission
    /// * `system_prompt` - Optional additional system prompt to append
    /// * `resume_session_id` - If set, resumes an existing session instead of starting new
    /// * `model` - Model to use (e.g., "claude-opus-4-6"). Only used for new sessions.
    ///
    /// # Returns
    ///
    /// An `AgentHandle` for communicating with the spawned agent.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The agent binary is not found in PATH
    /// - The subprocess fails to spawn
    /// - Required stdio streams cannot be captured
    async fn spawn(
        &self,
        repo_path: &Path,
        allowed_tools: &[String],
        system_prompt: Option<&str>,
        resume_session_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<AgentHandle>;
}
