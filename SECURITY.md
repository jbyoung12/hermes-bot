# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Hermes, please report it responsibly.

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, use GitHub's private security advisory feature:

1. Go to the [Security tab](https://github.com/jbyoung12/hermes-bot/security/advisories)
2. Click "Report a vulnerability"
3. Fill in the details

Alternatively, you can email me directly (see my GitHub profile for contact info).

Include:
- A description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

I'll respond as soon as possible and work with you to coordinate a fix before public disclosure.

## Security Considerations

Hermes executes Claude Code CLI processes on behalf of Slack users. Keep in mind:

- **`allowed_users`** in `config.toml` controls who can trigger agent sessions. Keep this list minimal.
- **`allowed_tools`** controls which tools Claude can use per repo. Be restrictive with `Bash` permissions.
- **Tokens** (`SLACK_APP_TOKEN`, `SLACK_BOT_TOKEN`) should be stored in `.env` or environment variables, never committed to version control.
- **Repo paths** give Claude filesystem access. Only configure repos you trust Claude to modify.
