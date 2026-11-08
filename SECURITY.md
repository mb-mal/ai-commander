# Security Policy

ai-commander drives AI agent harnesses that can execute tools, edit files and
run shell commands on your machine. Please report security issues responsibly.

## Reporting a Vulnerability

Contact the maintainer via Telegram: **@tsingular** (or open a private
security advisory on GitHub: Security → Report a vulnerability).

Include: description, affected version/commit, reproduction steps, impact.
You will get an acknowledgement within 72 hours.

## Security-relevant design notes

- **Auto-approval is off by default.** Hermes runs without `--yolo` unless you
  enable "Auto-approve tools" in settings; opencode permission requests pop a
  modal (allow once / always / deny) instead of being silently dropped.
- The opencode server spawned by the app binds to 127.0.0.1 only, with a
  random per-run password from OS entropy.
- Harness passwords are stored in plain text in `config.toml` — keep the file
  private; prefer pointing `server_url` at your own already-running server.
- Attachments are capped at 8 MB and sent as data-URLs to the harness.
- `scripts/fetch-tools.sh` verifies the SHA-256 of the downloaded archive.
