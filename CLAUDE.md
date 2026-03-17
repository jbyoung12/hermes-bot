# Hermes

Remote Claude Code control via Slack. Each channel is a repo, each thread is a session.

## Build & Run

```bash
make build          # cargo build
make run            # cargo run (auto-loads .env)
make release        # cargo build --release
make test           # cargo test
make lint           # cargo clippy -- -D warnings
make fmt            # cargo fmt
make check          # fmt-check + lint + test
```

## Project Structure

```
src/
  main.rs          Entry point, Socket Mode setup, heartbeat loop, shutdown handling
  lib.rs           Library crate for integration tests (re-exports all modules)
  config.rs        Configuration loading/validation (config.toml, env vars, tuning params)
  error.rs         Error types (thiserror): Config, Session, Agent, Slack, IO, JSON, TOML
  session.rs       Session persistence (SQLite) with per-row updates and TTL pruning
  sync.rs          Local CLI session sync — filesystem watcher imports local sessions into Slack
  util.rs          Utilities (floor_char_boundary, panic hook)
  slack/
    mod.rs         AppState (shared state), message dispatch, rate limiting, model resolution
    api.rs         Slack API calls (channels, messages, reactions, auto-channel creation)
    handlers.rs    Message/thread/command handlers, event processing loop, plan execution
    formatting.rs  Markdown → Slack mrkdwn conversion, message splitting, question/tool formatting
  agent/
    mod.rs         Agent trait, AgentHandle (bidirectional comms), AgentEvent enum
    claude.rs      Claude Code CLI integration (spawn, stdin/stdout/stderr tasks, kill)
    protocol.rs    Stream-JSON protocol: parsing (NDJSON), outbound messages, control responses
tests/
  integration_test.rs
  common/mod.rs
```

## Code Conventions

- Async runtime: tokio (multi-threaded)
- Error handling: `thiserror` for error types, `Box<dyn Error>` at the top level
- Logging: `tracing` with `tracing-subscriber` (env-filter, default level `info`)
- Slack SDK: `slack-morphism` with hyper connector and Socket Mode
- Config format: TOML (`toml` crate), secrets via `.env` or environment variables
- Testing: `rstest` for parameterized tests, `tokio::test` for async tests
- Run `cargo fmt` and `cargo clippy -- -D warnings` before committing — zero warnings policy

## Key Architecture

- **Socket Mode** — no public URL needed; uses `slack-morphism` WebSocket listener
- **Agent trait** (`src/agent/mod.rs`) — async trait for agent backends; `ClaudeAgent` is the implementation
- **Stream-JSON protocol** — bidirectional NDJSON communication with Claude Code CLI via stdin/stdout
- **AppState** (`src/slack/mod.rs`) — shared state passed through Slack event handlers (wrapped in `Arc`)
- **Session persistence** — `SessionStore` uses SQLite (`sessions.db`) with WAL mode, per-row updates, and TTL-based pruning; auto-migrates legacy `sessions.json`
- **Concurrency** — `in_progress` set guards one agent per thread; new messages interrupt running agents; `kill_senders` for graceful shutdown
- **Local session sync** — filesystem watcher (FSEvents/inotify via `notify` crate) imports local CLI sessions into Slack threads with TOCTOU-safe claim logic
- **Rate limiting** — per-channel rate limiter prevents Slack API throttling (configurable interval)
- **Streaming modes** — `batch` (post full response after completion) or `live` (edit messages in real-time via chat.update)
- **Plan detection** — intercepts Write tool calls to `~/.claude/plans/`, posts plan content to Slack, supports `!execute` to re-run with fresh context
- **Interactive control requests** — AskUserQuestion and tool approval prompts forwarded to Slack threads, answered via thread replies
- **Heartbeat loop** — 60s interval for stale pending repo cleanup; 5-minute interval for expired session pruning

## Slack Interaction Model

- **New channel message** → spawns new Claude Code session, creates thread
- **Thread reply** → resumes session (auto-respawns with `--resume` if process exited)
- **Thread commands**: `!stop`, `!status`, `!model [name]`, `!execute`
- **Slash command**: `/claude sessions`, `/claude help`
- **Question flow** — Claude asks via AskUserQuestion → posted to thread → user replies → answer forwarded via control_response
- **Tool approval flow** — unapproved tool use → posted to thread → user replies yes/no → allow/deny via control_response
- **Model aliases** — `opus`, `sonnet`, `haiku` resolve to full model IDs; per-thread overrides via `!model`

## Configuration

- `config.toml` — repo definitions, tool permissions, defaults, tuning params (gitignored)
- `.env` — Slack tokens: `SLACK_APP_TOKEN` (xapp-), `SLACK_BOT_TOKEN` (xoxb-) (gitignored)
- `HERMES_CONFIG` env var — override config file path (defaults to `config.toml`)
- See `config.toml.example` and `.env.example` for templates
- **`sync_local_sessions`** — global (`[defaults]`, bool, default `true`) and per-repo (`[repos.*]`, `Option<bool>`); controls local CLI session sync to Slack
- **Tuning params** (`[tuning]`): `slack_max_message_chars`, `session_ttl_days`, `live_update_interval_secs`, `rate_limit_interval_ms`, `max_accumulated_text_bytes`, `first_chunk_max_retries`, `log_preview_max_len`

## Sensitive Files — Do Not Commit

- `.env` — Slack tokens
- `config.toml` — may contain tokens and repo paths
- `sessions.db` — runtime session state (SQLite)
