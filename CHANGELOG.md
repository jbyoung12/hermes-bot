# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-03-15

### Added
- Initial release of Hermes
- Claude Code CLI integration via Slack Socket Mode
- Multi-repo support with configurable channel mapping
- Per-repo tool permissions (Read, Write, Edit, Bash, etc.)
- Session persistence across restarts via `sessions.json`
- Thread-based conversation management with automatic session resumption
- Local CLI session sync via filesystem watcher (FSEvents/inotify)
- Plan mode support with `/execute` command for fresh context execution
- Live and batch streaming modes for real-time or final response posting
- Thread commands: `/status`, `/stop`, `/model`, `/execute`
- Slash command: `/claude sessions` and `/claude help`
- Per-thread model override support (opus, sonnet, haiku)
- Question/answer flow for interactive Claude prompts
- Markdown to Slack mrkdwn conversion
- Comprehensive error handling and recovery
- Rate limiting for Slack API calls
- Graceful shutdown with process cleanup
- 86 unit tests with full test coverage
- GitHub Actions CI/CD pipeline
- Dependabot integration for dependency updates
- Complete documentation (README, CONTRIBUTING, CODE_OF_CONDUCT, SECURITY)

[unreleased]: https://github.com/jbyoung12/hermes-bot/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jbyoung12/hermes-bot/releases/tag/v0.1.0
