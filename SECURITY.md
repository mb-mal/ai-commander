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

## Threat model (v7)

**What ai-commander's permissions protect against:** mistakes by the model
and prompt injection through tool outputs — the "approve this shell command?"
dialog stops an agent from quietly running something the user did not intend.

**What they do NOT protect against:** a malicious agent that already has
arbitrary code execution. If you allow `execute cargo *`, the agent can
write a `build.rs` that reads any file the user can read and exfiltrates it.
For untrusted code, use OS-level isolation (containers, separate user
accounts) or the agent harness's own sandbox.

**Board IPC security layers:**

1. The redb store is single-owner (exclusive lock); only the TUI or daemon
   has it open.
2. The IPC transport token (in `board.port`, 0600) authenticates every
   request; the acting role is identified by a per-role token minted at
   agent launch — the `role` field of a request body is never trusted.
3. The human token (for `board_unpark`, i.e. approving a parked permission)
   is NOT in `board.port` — it lives in a separate temp file outside the
   config dir, read once by the TUI and deleted.
4. Agents cannot read, write or execute anything inside the ai-commander
   config directory (`~/.config/ai-commander/`): built-in deny rules in the
   policy engine block all such requests regardless of user or project
   policy.yaml settings. This protects `board.port`, `policy.yaml`, and
   `config.toml` from tampering by the agent.
5. Un-parking a NeedsHuman task is human-only: the `board_unpark` tool
   requires the human token, and the `promote` MCP action is refused for
   agents.
