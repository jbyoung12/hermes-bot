use thiserror::Error;

/// Errors that can occur in Hermes.
#[derive(Debug, Error)]
pub enum HermesError {
    /// Configuration file error (missing, invalid, etc.)
    #[error("Configuration error: {0}")]
    Config(String),

    /// Session not found for the given thread
    #[error("Session not found for thread '{0}'")]
    SessionNotFound(String),

    /// Claude CLI binary not found in PATH
    #[error("Claude CLI not found in PATH. Install it from: https://docs.anthropic.com/en/docs/claude-code")]
    ClaudeNotFound,

    /// Failed to spawn the Claude CLI process
    #[error("Failed to spawn Claude CLI: {reason}")]
    AgentSpawnFailed { reason: String },

    /// Slack API call failed
    #[error("Slack API error: {0}")]
    SlackApi(String),

    /// Filesystem I/O error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML configuration parse error
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, HermesError>;
