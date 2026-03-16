# Hermes

[![CI](https://github.com/jbyoung12/hermes-bot/workflows/CI/badge.svg)](https://github.com/jbyoung12/hermes-bot/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust Version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2Fjbyoung12%2Fhermes-bot%2Fmain%2FCargo.toml&query=%24.package%5B%22rust-version%22%5D&label=rust&suffix=%2B&color=blue)](https://www.rust-lang.org)

**Control Claude Code from Slack.** Each channel is a repo, each Slack thread is a session.

Access Claude from anywhere — start on your laptop, continue on your phone. Sessions persist across messages and restarts, local CLI sessions auto-sync to Slack, and fine-grained tool permissions control what Claude can run. Zero infrastructure needed (Socket Mode, no webhooks).

## Quick Start

```bash
# Install
cargo install hermes-bot

# Set up Slack app (see below), then:
cp .env.example .env
cp config.toml.example config.toml
hermes
```

In Slack, type in a repo channel:

```
fix the failing tests in the auth module
```

Hermes replies in a Slack thread. Reply to continue — full history is preserved.

## Features

- 🔄 **Persistent sessions** — Conversations survive restarts, resume automatically
- 📱 **Mobile access** — Control Claude from your phone via Slack
- 🔗 **Local sync** — Start sessions with `claude` CLI, continue in Slack
- 🔒 **Fine-grained permissions** — Control which tools Claude can use per repo
- 👥 **Team-friendly** — Deploy on a VPS for shared access
- ⚡ **Zero infrastructure** — Socket Mode (no webhooks, no public URLs)
- 🧵 **Thread-based** — One channel = one repo, one Slack thread = one session

## How It Works

```
┌─────────┐                    ┌─────────┐                    ┌─────────────┐
│  Slack  │ ◄─── Socket Mode──►│ Hermes  │ ◄─── stdin/out ───►│ Claude CLI  │
│ Channel │                    │         │                    │   Process   │
└─────────┘                    └─────────┘                    └───────┬─────┘
     │                              │                                 │
     │  1. User types message       │                                 │
     ├──────────────────────────────►                                 │
     │                              │  2. Spawn agent                 │
     │                              ├─────────────────────────────────►
     │                              │                                 │
     │                              │  3. Stream events (tools, text) │
     │                              │◄────────────────────────────────┤
     │  4. Post results in Slack thread                               │
     │◄─────────────────────────────┤                                 │
     │                              │                                 │
     │  5. User replies in Slack thread                               ▼
     ├──────────────────────────────►                           ┌──────────┐
     │                              │  6. Resume session        │ Local    │
     │                              ├──────────────────────────►│ Git Repo │
     │  7. Continue conversation    │                           └──────────┘
     │◄─────────────────────────────┘
```

## Installation

### Prerequisites

- [Rust](https://rustup.rs/) (see `rust-version` in `Cargo.toml` for MSRV)
- [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code) installed and authenticated
- A Slack workspace where you can create apps

### Slack App Setup

1. Go to [api.slack.com/apps](https://api.slack.com/apps) → **Create New App** → **From an app manifest**
2. Pick your workspace
3. Paste `slack-manifest.json` from this repo
4. Click **Create**
5. **Install to Workspace** (OAuth & Permissions)
6. Copy **Bot Token** (`xoxb-...`) from OAuth & Permissions
7. Generate **App Token** (`xapp-...`) from Basic Information → App-Level Tokens → add `connections:write` scope
8. **Get your Slack User ID** — Profile → three dots → **Copy member ID**

### Configuration

**1. Slack tokens:**

```bash
cp .env.example .env
```

Edit `.env`:

```env
SLACK_APP_TOKEN=xapp-1-...
SLACK_BOT_TOKEN=xoxb-...
```

**2. Configure repos:**

```bash
cp config.toml.example config.toml
```

Edit `config.toml`:

```toml
[slack]
allowed_users = ["U01234567"]  # Your Slack user ID

[defaults]
streaming_mode = "batch"  # or "live" for real-time
allowed_tools = ["Read", "Glob", "Grep", "WebSearch"]

[repos.my-project]
path = "/absolute/path/to/my-project"
allowed_tools = ["Edit", "Write", "Bash(cargo *)"]
```

**3. Run:**

```bash
cargo run  # or 'hermes' if installed
```

Hermes auto-creates channels from repo names (e.g., `repos.backend` → `#backend`).

## Usage

### Commands

**In threads:**

- `/status` — Show session info
- `/stop` — Stop session
- `/model` — Show current model
- `/model opus` — Switch to Opus (also: `sonnet`, `haiku`)
- `/execute` — Run last plan with fresh context

**Slash commands:**

- `/claude sessions` — List active sessions
- `/claude help` — Show help

### Local Session Sync

Run `claude` locally in a configured repo, and Hermes auto-detects it:

```
Local session detected (branch: main)
> fix the tests
```

Reply in the Slack thread to continue.

### Security & Permissions

**User allowlist** — Only users in `allowed_users` can interact.

**Tool permissions** — Claude can only run pre-approved tools:

- **Global** (all repos): `Read`, `Glob`, `Grep`, `WebSearch` — safe, read-only
- **Per-repo**: `Edit`, `Write`, `Bash(cargo *)` — scoped to specific repos

Example: `frontend` repo allows `Bash(npm *)` but not `Bash(rm *)`.

**Audit trail** — All commands visible in Slack threads.

### Team Usage

**Personal use:** Run on your laptop, message from your phone.

**Team use:** Run on a VPS. Multiple people can use Claude, conversations are shared in Slack.

## Comparison


| Feature            | Hermes                                   | OpenClaw / Alternatives                 |
| ------------------ | ---------------------------------------- | --------------------------------------- |
| **Infrastructure** | Socket Mode — laptop or VPS, no webhooks | Requires public URLs, server deployment |
| **Mental model**   | Channel = repo, Slack thread = session   | Varies                                  |
| **Implementation** | Uses Claude CLI — gets updates free      | Often reimplements integration          |
| **Local + Remote** | Auto-syncs local sessions                | Typically Slack-only or CLI-only        |
| **Security**       | Per-repo tool permissions                | Often all-or-nothing                    |


## Troubleshooting

**"Failed to spawn claude CLI"**
→ Install: `npm install -g @anthropic-ai/claude-code`
→ Verify: `claude --version`

**"Socket Mode connection failed"**
→ Check `SLACK_APP_TOKEN` starts with `xapp-` and has `connections:write` scope
→ Check `SLACK_BOT_TOKEN` starts with `xoxb-`
→ Enable Socket Mode in app settings

**Bot doesn't respond**
→ Check `allowed_users` includes your Slack user ID
→ Verify bot is in the channel

**Sessions not resuming**
→ Ensure `sessions.json` is writable
→ Agent processes killed on shutdown, auto-recover on next message

## Architecture

- **Socket Mode** — No public URL, works on laptop or VPS
- **Agent trait** — Extensible backend (Claude Code implemented)
- **Session persistence** — `sessions.json` survives restarts
- **Concurrency guard** — One agent per Slack thread
- **Local session sync** — Filesystem watcher imports CLI sessions

```
src/
  main.rs          Socket Mode setup, shutdown handling
  config.rs        Config loading and validation
  session.rs       Session persistence (sessions.json)
  sync.rs          Local CLI session sync (notify crate)
  slack/           Slack API, message handlers, formatting
  agent/           Agent trait, Claude CLI integration, protocol parser
```

## Contributing

Contributions welcome!

```bash
git clone https://github.com/jbyoung12/hermes-bot.git
cd hermes-bot
cp .env.example .env
cp config.toml.example config.toml
cargo test
```

**Before submitting:**

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

**Guidelines:**

- Use `thiserror` for errors
- Add tests for new features
- Keep commits focused

Open PRs against `main`. For bugs, include steps to reproduce and your Rust version.

## License

MIT — see [LICENSE](LICENSE)