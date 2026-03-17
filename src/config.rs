use crate::error::{HermesError, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

pub const DEFAULT_MODEL: &str = "claude-opus-4-6";

#[derive(Deserialize)]
pub struct Config {
    pub slack: SlackConfig,
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub tuning: TuningConfig,
    #[serde(default)]
    pub repos: HashMap<String, RepoConfig>,
    /// Path to the session store database. Defaults to "sessions.db" in the working directory.
    #[serde(default = "default_sessions_file")]
    pub sessions_file: PathBuf,
}

fn default_sessions_file() -> PathBuf {
    PathBuf::from("sessions.db")
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("slack", &self.slack)
            .field("defaults", &self.defaults)
            .field("tuning", &self.tuning)
            .field("repos", &self.repos)
            .field("sessions_file", &self.sessions_file)
            .finish()
    }
}

#[derive(Deserialize)]
pub struct SlackConfig {
    #[serde(default)]
    pub app_token: String,
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("app_token", &"[REDACTED]")
            .field("bot_token", &"[REDACTED]")
            .field("allowed_users", &self.allowed_users)
            .finish()
    }
}

// ── Security Notes ─────────────────────────────────────────────────────

// IMPORTANT: Token Security
//
// In production, tokens should NEVER be committed to version control.
// Use one of these methods:
//
// 1. Environment variables (recommended):
//    export SLACK_APP_TOKEN=xapp-...
//    export SLACK_BOT_TOKEN=xoxb-...
//
// 2. Secret management service:
//    - AWS Secrets Manager
//    - HashiCorp Vault
//    - Kubernetes Secrets
//
// 3. Encrypted .env file (not committed to git):
//    - Add .env to .gitignore
//    - Use tools like git-crypt or SOPS for encryption
//
// The config.toml file should only contain non-sensitive configuration.
// Tokens loaded from environment variables will override config file values.

#[derive(Debug, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default)]
    pub append_system_prompt: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub streaming_mode: StreamingMode,
    #[serde(default)]
    pub model: Option<String>,
    /// Enable syncing local Claude Code sessions into Slack. Default: true.
    #[serde(default = "default_true")]
    pub sync_local_sessions: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamingMode {
    #[default]
    Batch,
    Live,
}

impl std::fmt::Display for StreamingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamingMode::Batch => write!(f, "batch"),
            StreamingMode::Live => write!(f, "live"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoConfig {
    pub path: PathBuf,
    #[serde(default = "default_agent")]
    pub agent: AgentKind,
    /// Custom Slack channel name. Defaults to the repo key name (dots replaced with hyphens).
    pub channel: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Override the global sync_local_sessions setting for this repo.
    #[serde(default)]
    pub sync_local_sessions: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
}

fn default_agent() -> AgentKind {
    AgentKind::Claude
}

/// Performance and behavior tuning parameters.
/// All fields have sensible defaults and are optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TuningConfig {
    /// Slack's approximate max message length (characters) for chat.postMessage. Default: 39000
    pub slack_max_message_chars: usize,
    /// Session time-to-live in days. Sessions older than this are pruned. Default: 7
    pub session_ttl_days: i64,
    /// Live-mode message update interval in seconds. Default: 2
    pub live_update_interval_secs: u64,
    /// Minimum interval between Slack API write calls per channel (ms). Default: 1100
    pub rate_limit_interval_ms: u64,
    /// Maximum accumulated text size in bytes before flushing (prevents unbounded growth). Default: 1000000
    pub max_accumulated_text_bytes: usize,
    /// Max retries for posting the first chunk of a thread. Default: 3
    pub first_chunk_max_retries: u32,
    /// Max length of message text shown in log previews. Default: 100
    pub log_preview_max_len: usize,
}

impl Default for TuningConfig {
    fn default() -> Self {
        Self {
            slack_max_message_chars: 39_000,
            session_ttl_days: 7,
            live_update_interval_secs: 2,
            rate_limit_interval_ms: 1100,
            max_accumulated_text_bytes: 1_000_000,
            first_chunk_max_retries: 3,
            log_preview_max_len: 100,
        }
    }
}

impl RepoConfig {
    /// Returns allowed_tools merged with the global defaults.
    pub fn merged_tools(&self, defaults: &DefaultsConfig) -> Vec<String> {
        let mut tools = defaults.allowed_tools.clone();
        for tool in &self.allowed_tools {
            if !tools.contains(tool) {
                tools.push(tool.clone());
            }
        }
        tools
    }

    /// Returns whether local session sync is enabled: repo override > global default (true).
    pub fn sync_enabled(&self, defaults: &DefaultsConfig) -> bool {
        self.sync_local_sessions
            .unwrap_or(defaults.sync_local_sessions)
    }

    /// Returns the model for this repo: repo override > global default > DEFAULT_MODEL.
    pub fn resolved_model(&self, defaults: &DefaultsConfig) -> String {
        self.model
            .clone()
            .or_else(|| defaults.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = std::env::var("HERMES_CONFIG").unwrap_or_else(|_| "config.toml".into());
        let contents = std::fs::read_to_string(&path).map_err(|e| {
            HermesError::Config(format!("Failed to read config file '{}': {}", path, e))
        })?;
        let mut config: Config = toml::from_str(&contents)?;

        // Env vars override config file for secrets.
        if let Ok(val) = std::env::var("SLACK_APP_TOKEN") {
            config.slack.app_token = val;
        }
        if let Ok(val) = std::env::var("SLACK_BOT_TOKEN") {
            config.slack.bot_token = val;
        }

        if config.slack.app_token.is_empty() {
            return Err(HermesError::Config(
                "Slack app token not set. Use SLACK_APP_TOKEN env var or slack.app_token in config."
                    .into(),
            ));
        }
        if config.slack.bot_token.is_empty() {
            return Err(HermesError::Config(
                "Slack bot token not set. Use SLACK_BOT_TOKEN env var or slack.bot_token in config."
                    .into(),
            ));
        }

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.slack.app_token.starts_with("xapp-") {
            return Err(HermesError::Config(
                "Slack app_token should start with 'xapp-'. Did you swap app_token and bot_token?"
                    .into(),
            ));
        }
        if !self.slack.bot_token.starts_with("xoxb-") {
            return Err(HermesError::Config(
                "Slack bot_token should start with 'xoxb-'. Did you swap app_token and bot_token?"
                    .into(),
            ));
        }

        if self.repos.is_empty() {
            return Err(HermesError::Config(
                "No repos configured. Add at least one [repos.<name>] section.".into(),
            ));
        }

        // Validate sessions_file path for security
        if let Some(path_str) = self.sessions_file.to_str() {
            if path_str.contains("..") {
                tracing::warn!(
                    "sessions_file contains '..': {}. This may be a path traversal risk.",
                    path_str
                );
            }
            // Warn if absolute path outside current directory (potential security issue)
            if self.sessions_file.is_absolute() {
                tracing::info!(
                    "sessions_file uses absolute path: {}. Ensure proper permissions.",
                    self.sessions_file.display()
                );
            }
        }

        for (name, repo) in &self.repos {
            if !repo.path.exists() {
                return Err(HermesError::Config(format!(
                    "Repo '{}' path does not exist: {}",
                    name,
                    repo.path.display()
                )));
            }

            // Security: Warn about relative paths with .. (path traversal risk)
            if let Some(path_str) = repo.path.to_str()
                && path_str.contains("..")
            {
                tracing::warn!(
                    "Repo '{}' path contains '..': {}. Verify this is intentional.",
                    name,
                    path_str
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn minimal_config(repos_path: &str) -> String {
        format!(
            r#"
[slack]
app_token = "xapp-test"
bot_token = "xoxb-test"
allowed_users = ["U123"]

[defaults]
streaming_mode = "batch"

[repos.test]
path = "{}"
"#,
            repos_path
        )
    }

    #[test]
    fn test_parse_minimal_config() {
        let toml = minimal_config("/tmp");
        let config: Config = toml::from_str(&toml).unwrap();
        assert_eq!(config.slack.app_token, "xapp-test");
        assert_eq!(config.slack.bot_token, "xoxb-test");
        assert_eq!(config.slack.allowed_users, vec!["U123"]);
        assert_eq!(config.defaults.streaming_mode, StreamingMode::Batch);
        assert!(config.repos.contains_key("test"));
    }

    #[test]
    fn test_streaming_mode_live() {
        let toml = r#"
[slack]
[defaults]
streaming_mode = "live"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.defaults.streaming_mode, StreamingMode::Live);
    }

    #[test]
    fn test_streaming_mode_defaults_to_batch() {
        let toml = r#"
[slack]
[defaults]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.defaults.streaming_mode, StreamingMode::Batch);
    }

    #[test]
    fn test_sessions_file_defaults() {
        let toml = r#"
[slack]
[defaults]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.sessions_file, PathBuf::from("sessions.db"));
    }

    #[test]
    fn test_sessions_file_custom() {
        let toml = r#"
sessions_file = "/var/lib/hermes/sessions.db"
[slack]
[defaults]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.sessions_file,
            PathBuf::from("/var/lib/hermes/sessions.db")
        );
    }

    // Helper to create defaults config for merged_tools tests
    fn make_defaults(tools: Vec<&str>) -> DefaultsConfig {
        DefaultsConfig {
            append_system_prompt: None,
            allowed_tools: tools.iter().map(|s| s.to_string()).collect(),
            streaming_mode: StreamingMode::Batch,
            model: None,
            sync_local_sessions: true,
        }
    }

    // Helper to create repo config for merged_tools tests
    fn make_repo(tools: Vec<&str>) -> RepoConfig {
        RepoConfig {
            path: PathBuf::from("/tmp"),
            agent: AgentKind::Claude,
            channel: None,
            allowed_tools: tools.iter().map(|s| s.to_string()).collect(),
            model: None,
            sync_local_sessions: None,
        }
    }

    #[rstest]
    #[case(
        vec!["Read", "Grep"],
        vec!["Edit", "Write"],
        vec!["Read", "Grep", "Edit", "Write"],
        "combines defaults and repo tools"
    )]
    #[case(
        vec!["Read", "Grep"],
        vec!["Read", "Edit"],
        vec!["Read", "Grep", "Edit"],
        "deduplicates tools"
    )]
    #[case(
        vec!["Read"],
        vec![],
        vec!["Read"],
        "empty repo tools uses only defaults"
    )]
    fn test_merged_tools(
        #[case] defaults_tools: Vec<&str>,
        #[case] repo_tools: Vec<&str>,
        #[case] expected: Vec<&str>,
        #[case] description: &str,
    ) {
        let defaults = make_defaults(defaults_tools);
        let repo = make_repo(repo_tools);
        let merged = repo.merged_tools(&defaults);
        assert_eq!(merged, expected, "{}", description);
    }

    #[test]
    fn test_debug_redacts_tokens() {
        let config: Config = toml::from_str(
            r#"
[slack]
app_token = "xapp-secret-123"
bot_token = "xoxb-secret-456"
[defaults]
"#,
        )
        .unwrap();
        let debug_output = format!("{:?}", config);
        assert!(!debug_output.contains("xapp-secret-123"));
        assert!(!debug_output.contains("xoxb-secret-456"));
        assert!(debug_output.contains("[REDACTED]"));
    }

    #[test]
    fn test_agent_kind_defaults_to_claude() {
        let toml = r#"
[slack]
[defaults]
[repos.test]
path = "/tmp"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.repos["test"].agent, AgentKind::Claude);
    }

    #[test]
    fn test_validate_rejects_no_repos() {
        let toml = r#"
[slack]
app_token = "xapp-test"
bot_token = "xoxb-test"
[defaults]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No repos configured")
        );
    }

    #[test]
    fn test_validate_rejects_nonexistent_path() {
        let toml = r#"
[slack]
app_token = "xapp-test"
bot_token = "xoxb-test"
[defaults]
[repos.test]
path = "/nonexistent/path/that/should/not/exist"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_streaming_mode_display() {
        assert_eq!(StreamingMode::Batch.to_string(), "batch");
        assert_eq!(StreamingMode::Live.to_string(), "live");
    }

    #[test]
    fn test_validate_rejects_bad_app_token_prefix() {
        let toml = r#"
[slack]
app_token = "xoxb-wrong-prefix"
bot_token = "xoxb-test"
[defaults]
[repos.test]
path = "/tmp"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("xapp-"));
    }

    #[test]
    fn test_validate_rejects_bad_bot_token_prefix() {
        let toml = r#"
[slack]
app_token = "xapp-test"
bot_token = "xapp-wrong-prefix"
[defaults]
[repos.test]
path = "/tmp"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("xoxb-"));
    }

    #[test]
    fn test_resolved_model_defaults() {
        let defaults = DefaultsConfig {
            append_system_prompt: None,
            allowed_tools: vec![],
            streaming_mode: StreamingMode::Batch,
            model: None,
            sync_local_sessions: true,
        };
        let repo = RepoConfig {
            path: PathBuf::from("/tmp"),
            agent: AgentKind::Claude,
            channel: None,
            allowed_tools: vec![],
            model: None,
            sync_local_sessions: None,
        };
        assert_eq!(repo.resolved_model(&defaults), DEFAULT_MODEL);
    }

    #[test]
    fn test_resolved_model_global_override() {
        let defaults = DefaultsConfig {
            append_system_prompt: None,
            allowed_tools: vec![],
            streaming_mode: StreamingMode::Batch,
            model: Some("claude-sonnet-4-5-20250929".to_string()),
            sync_local_sessions: true,
        };
        let repo = RepoConfig {
            path: PathBuf::from("/tmp"),
            agent: AgentKind::Claude,
            channel: None,
            allowed_tools: vec![],
            model: None,
            sync_local_sessions: None,
        };
        assert_eq!(repo.resolved_model(&defaults), "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn test_resolved_model_repo_override() {
        let defaults = DefaultsConfig {
            append_system_prompt: None,
            allowed_tools: vec![],
            streaming_mode: StreamingMode::Batch,
            model: Some("claude-sonnet-4-5-20250929".to_string()),
            sync_local_sessions: true,
        };
        let repo = RepoConfig {
            path: PathBuf::from("/tmp"),
            agent: AgentKind::Claude,
            channel: None,
            allowed_tools: vec![],
            model: Some("claude-haiku-4-5-20251001".to_string()),
            sync_local_sessions: None,
        };
        assert_eq!(repo.resolved_model(&defaults), "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_sync_enabled_defaults_to_true() {
        let defaults = make_defaults(vec![]);
        let repo = make_repo(vec![]);
        assert!(repo.sync_enabled(&defaults));
    }

    #[test]
    fn test_sync_enabled_global_disable() {
        let mut defaults = make_defaults(vec![]);
        defaults.sync_local_sessions = false;
        let repo = make_repo(vec![]);
        assert!(!repo.sync_enabled(&defaults));
    }

    #[test]
    fn test_sync_enabled_repo_override_enable() {
        let mut defaults = make_defaults(vec![]);
        defaults.sync_local_sessions = false;
        let mut repo = make_repo(vec![]);
        repo.sync_local_sessions = Some(true);
        assert!(repo.sync_enabled(&defaults));
    }

    #[test]
    fn test_sync_enabled_repo_override_disable() {
        let defaults = make_defaults(vec![]);
        let mut repo = make_repo(vec![]);
        repo.sync_local_sessions = Some(false);
        assert!(!repo.sync_enabled(&defaults));
    }
}
