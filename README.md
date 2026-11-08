# ai-commander

[![screenshot](docs/screenshot.png)](docs/screenshot.png)

*(the image above is a rendered mockup of the real UI, generated from the
theme constants by `scripts/make-screenshot.py` — no real session data)*

ChatGPT-style TUI client for AI agent harnesses with the aesthetics of
DOS-era file managers (Norton Commander: box-drawing frames, F-key bar,
blue screen + silver dialogs) — written in Rust (ratatui + crossterm + tokio).

## Support matrix

| Harness | Kind | Streaming | Sessions | Attachments | Esc |
|---|---|---|---|---|---|
| OpenCode ≥ 2.2 | HTTP+SSE server (auto-started) | live deltas | server-side | images & files (≤8 MB) | detach* |
| OpenCode via **ACP** | Agent Client Protocol over stdio (`opencode acp`) | live deltas | server-side + local | images (≤8 MB) | **real cancel** |
| Hermes Agent | CLI subprocess | live deltas | local registry | images via `--image` | real kill |
| DeepSeek Harness (`dsh`) | CLI subprocess (headless) | replace events | local registry | — | real kill |

\* opencode 2.2 exposes no abort endpoint: Esc stops following the turn; the
result loads when it finishes, and the next message interrupts it.

## How it works

The second harness is **Hermes Agent** (CLI): ai-commander drives the
`hermes chat` subprocess per message, streams its `--format stream-json`
deltas, keeps a local session registry with accumulated history (the CLI has
no session-list API), attaches images via `--image`, picks models from its
provider cache, and `Esc` really kills the running turn. Enable it with
`Ctrl+H` in the app or `enabled = true` in config.

The third harness is **DeepSeek Harness (`dsh`)** — also subprocess-driven via
`dsh --profile headless --json` (NDJSON `session`/`thinking`/`text`/`final`
events; thinking and answer arrive as cumulative replacements). The headless
profile's `--session-id` adopts a session but does not replay prior turns
into the model context, so the backend folds the stored conversation into the
task text itself. `DEEPSEEK_API_KEY` is taken from the environment or reused
from the local opencode auth store; `Esc` kills the running turn.

## Features

- ChatGPT layout: session history on the left, chat in the centre, composer
  at the bottom; profile + settings in the bottom-left corner
- Live token streaming (text, reasoning, tool calls) with markdown rendering
  and code blocks
- Pluggable harness architecture (`trait Backend`): opencode today,
  hermes-agent / deepseek-harness slots reserved in config and UI
- Model picker with a live list from the active harness, per-harness defaults
  and per-chat overrides (`Ctrl+K`)
- Attachments via an NC-style file browser (`Ins` marks files, `Tab` toggles
  drives)
- Full mouse support: clickable sessions, menus, modal rows and F-key buttons;
  wheel scrolling
- RU/EN interface with live switching (F2 → Language)

## Build

Windows without MSVC Build Tools? The repo pins the fully self-contained
`stable-x86_64-pc-windows-gnu` toolchain (see `rust-toolchain.toml`).

```bash
bash scripts/fetch-tools.sh # once: fetch dlltool.exe + as.exe from w64devkit
source scripts/gnu-env.sh   # puts tools/mingw-bin on PATH
cargo build --release
```

On MSVC machines simply delete `rust-toolchain.toml` and build with the
default toolchain. `dlltool`/`as` are needed only for the GNU target (raw-dylib
import libraries); they are not committed to the repo.

## Run

```bash
target/release/ai-commander                 # TUI (auto-starts opencode serve)
target/release/ai-commander --check         # headless: health, models, sessions
target/release/ai-commander --send "hello"  # E2E: real streamed model reply
target/release/ai-commander --server http://127.0.0.1:4096 --lang ru
```

Run inside **Windows Terminal** (UTF-8, emoji, mouse). The first launch creates
`config.toml` in the OS config directory (e.g. `%APPDATA%\ai-commander\`).

## Keys

| Key | Action |
|---|---|
| `Enter` | send · `Alt+Enter`/`Ctrl+J` — newline |
| `Esc` | stop following (the turn finishes in background; the next message steers it — the server has no abort) |
| `Ctrl+N` | new chat · `Ctrl+K` — model picker (live list) · `Ctrl+H` — harness |
| `Ctrl+P` | attach files (NC browser: `Ins` mark, `Tab` drives, `Enter` pick) |
| `Ctrl+O` | show/hide history · `Ctrl+D` — drop last attachment |
| `Tab` | focus history ↔ input · `PgUp/PgDn` — scroll · `End` — bottom |
| In history: `E` | rename session, `D`/`Del` — delete session |
| `F1` / `A` | help / About · `F2` — settings · `F10` — quit |

**Mouse**: click sessions / “+ New chat” / ⚙ Settings; the bottom F-keys are
real buttons; rows inside settings, model/harness/agent pickers and the file
browser are clickable; the wheel scrolls the chat and moves modal selection;
clicking the composer focuses it.

Settings (F2): language RU/EN (live), profile name, and per harness — server
URL (empty = autostart), server password (Basic auth), working directory,
agent (build/plan/…), default model. Everything persists in `config.toml`.

## Architecture

- `src/backend/mod.rs` — `trait Backend` (sessions, history, send, models,
  agents, event channel). New harnesses = new trait impls + a `[[harness]]`
  config entry.
- `src/backend/opencode.rs` — OpenCode v2 API: `/api/*` with Basic auth,
  `prompt` + SSE `/api/event` (`session.text.delta`, `session.reasoning.*`,
  `session.tool.*`, `session.execution.succeeded`), attachments as
  `files:[{uri:"data:…;base64", name}]`.
- `src/ui/*` — app state machine, sidebar/chat/composer, modals (NC file
  browser, settings, model/harness/agent pickers, 90s-style About), a custom
  text editor, markdown rendering (pulldown-cmark), the NC theme.

## Tests

```bash
cargo test   # 25: event mapping (SSE/NDJSON/ACP), App regression tests (busy rebind, rollback, permissions), markdown, i18n, config, wrap, grapheme editor, mime, atomic store
```

## Kanban board (Ctrl+B) — agent task execution

Tasks live in an embedded **redb** database (pure-Rust, ACID, crash-safe — no
C toolchain needed) behind the `EventStore` trait: an append-only event log
plus a projection snapshot written in the **same transaction**, with schema
versioning and a one-time migration from the old JSON file. A hard-kill
crash test (`tests/kanban_crash.rs`) verifies reopen consistency: no holes in
the log, snapshot == fold(log).

- F7 new task · F4 edit · F6 advance status (auto-runs the task) · `a` assign
  harness · Enter opens the task's chat session · `d` approve · `r` rework ·
  F8 cancel · arrows/mouse navigate
- Moving a task to In Progress creates a harness session and sends a task
  prompt (title, description, acceptance criteria); the agent's reply is
  classified (`DONE:` → Review, `BLOCKED:` → Blocked, `ASK:` → Needs human)
  and drives the FSM automatically
- Headless: `ai-commander --task "do something" --harness oc`
- **Watchdog**: InProgress tasks silent for 10 minutes are retried
  (attempts++) or escalated to Needs human after `max_attempts` (default 3)
- **Roles**: `~/.ai-commander/agents/<id>.md` (YAML frontmatter: name/harness/
  model/autonomy + markdown instructions) and `.ai-commander/agents/…` at
  project level (project wins). Assign with `o` on the board; the role's
  instructions prefix the task prompt. Teams: `.ai-commander/teams/<id>.yaml`
  (name/lead/members).
- **Daemon**: `ai-commander --daemon` runs the board without a UI — launches
  Ready tasks, retries stuck ones via the watchdog, exits when the board
  settles. The TUI sees the same store, so tasks continue while it is closed.
  Headless task runs honor `AI_COMMANDER_ROLE=<id>` to pick a role.

### Agent-to-agent board (MCP)

The board is exposed to agents as MCP tools by the `board-mcp` binary
(newline JSON-RPC over stdio, MCP 2024-11-05): `board.create_task`,
`board.update_task`, `board.comment`, `board.handoff`, `board.artifact`,
`board.read_inbox`, `board.ask_human`. The role identity is the per-session
token (`AI_COMMANDER_ROLE` env): every mutation is authored by that role,
and start/finish are ownership-checked. Handoffs carry a validated contract
(summary ≥ 10 chars + open questions) and return the task to Ready for the
new owner. Artifacts (`file`/`branch`/`link`/`text`) are first-class task
outputs — they survive handoffs, so the next owner finds the work.

**Single owner + IPC**: the redb store takes an exclusive lock, so exactly
one process (TUI or daemon) owns the board and serves board tool calls on a
loopback socket (port published in `<config>/board.port`, overridable with
`AI_COMMANDER_BOARD_PORT`). `board-mcp` connects to that owner as a client
— it never opens the redb file while an owner is alive and never creates a
second store. Only when nobody owns the board does it open the store
directly.

- ACP harness: the board server is injected automatically into every task
  session with a role (`session/new mcpServers`, typed schema entries)
- any harness: `ai-commander --print-mcp-config` prints the snippet for
  its own MCP config

### Activity feed, agents panel, A2A, OTel

- **Ctrl+L** — collapsible activity feed (bottom): the board's event log tail,
  task ids + actions + statuses
- **Ctrl+U** — agents & teams panel: roles (name/harness/status), harness
  backends (ready/busy/off); `T` opens the role wizard (coder/reviewer/
  planner/tester → `.ai-commander/agents/<id>.md`)
- **Approvals counter** in the header (`⚠ approvals:N`) while permission
  requests are pending
- **A2A gateway** (daemon): `GET /.well-known/agent.json` Agent Card;
  `message/send` creates a board task — external A2A agents join the board
- **OTel export** (daemon): board events → OTLP JSON spans in
  `<config>/otel/ai-commander-spans.jsonl` (one resourceSpans line per event)

### Worktree isolation, triggers, budgets

- **Git worktree per code task**: Start creates `ai-commander/t-XXXX` in
  `.aic-worktrees/t-XXXX` (base = current branch); Review→Done merges the
  branch back (`--no-ff`) and removes the worktree; a conflict aborts the
  merge and keeps the task in Review; Cancel drops the worktree unmerged.
  Engages via `transition_in` in the TUI runner and the daemon (plain
  `transition` never touches git)
- **Triggers**: `Task.schedule` (5-field cron string) — the daemon spawns a
  fresh one-shot child task per due window (`spawn_due_scheduled`)
- **Budgets & limits**: `budget_usd` × `unit_cost_usd` blocks Starts beyond
  the budget; `run_budget_secs` escalates a silent turn straight to
  NeedsHuman (instead of the retry path); `max_turns` caps thread turns;
  delegation chains (`parent_id`, MCP `board.create_task parent_id`) are
  capped at depth 3

### Task DAG & policy engine

- Tasks may declare `depends_on` (task ids; forward refs allowed): the
  daemon only launches Ready tasks whose dependencies are Done/Cancelled
- Permission requests (opencode) are evaluated against
  `.ai-commander/policy.yaml`: `allow / ask / deny` rules with `*` wildcard
  patterns over action + resource; strictest match wins, no rule → ask.
  Allow auto-replies, deny auto-rejects, ask opens the modal

## Logging, tests, quality

- **File logging**: `--log-level off|error|warn|info|debug` writes to
  `<config>/ai-commander.log` (backends, daemon lifecycle, model/agent
  switch failures); the daemon mirrors its `[run]`/`[watchdog]` lines there
- **Property tests**: deterministic pseudo-random strings verify the editor
  cursor invariant, grapheme-delete completeness and word-wrap safety
  (width, word preservation, hard-break character conservation) — they
  caught and fixed two real bugs: tabs measuring 0 in unicode-width, and
  `Editor::is_empty()` true on multi-empty-line text
- **HTTP mock tests**: hand-rolled mock servers exercise the opencode backend
  against 401 (health), 500 (REST) and a broken SSE stream (mid-stream
  disconnect) without any real harness
- **A2A flake fix**: the TCP half-close race on Windows (WSAECONNRESET) is
  handled with connect/read retries in the gateway test

## v2 review fixes

Blockers from the v2 review addressed:
- **Б1** single board owner + IPC: the owner (TUI/daemon) serves board tool
  calls on a loopback socket; `board-mcp` is a client while an owner is
  alive (no second store, no duplicate ids) and only opens the redb store
  directly when nobody owns it
- **Б2** ACP backend builds every message from the `agent-client-protocol`
  schema types (initialize, session/new, session/load, session/prompt,
  session/cancel, mcpServers entries); the `fake-acp-agent` conformance
  binary validates every client message against the schema
- **Б3** cron schedules disabled (were spawning a task every 60 s) until a
  real parser is in place
- **Б4** worktree: commits agent work before merge, refuses `--force` on a
  dirty worktree (task → NeedsHuman, files preserved)
- **Б8** daemon: transitions tasks on StreamDone (classify_reply), busy keyed
  by task.id (was native_id → never cleared)
- **Б9** daemon: uses the shared TaskRunner (DAG), role instructions prefix
  the prompt, role model overrides the harness default, `mcp_role` passed

Security fixes:
- A2A gateway disabled by default (env opt-in)
- Project policies can only tighten user policies (deny > ask > allow;
  a project `allow` never weakens a user `deny`)
- config written with 0600 permissions on Unix
- MCP: an agent cannot `approve` or `rework` its own task

## v3 review fixes

- **Н1** retry cap: a Failed turn retries with exponential backoff
  (30 s → 2 m → 8 m → 15 m cap) and escalates to NeedsHuman after
  `max_attempts` (default 3) — no infinite retry loops
- **Н2** strict DONE: only an explicit `DONE:` prefix finishes a task;
  error text (e.g. `401 Unauthorized`) counts as Failed, not Finished
- **Н3** worktree isolation end-to-end: Start creates the worktree, the
  session is created with `cwd` = worktree (subprocess harnesses), and the
  opencode HTTP backend moves the session to the worktree directory before
  the prompt (`POST /api/session/{id}/move`)
- **Д1** lease heartbeat on every progress event (text, reasoning, tools)
- **Д2** watchdog aborts are awaited (`abort_stuck` joins every handle,
  bounded 10 s) before transitions apply — no re-launch while the old
  agent process is still alive
- **Д4** the DAG gates every Start, manual board actions included
- **Д5** a failed send still ends the turn (empty reply → retry cap)
- **Д7** role files are cached by fingerprint (path, mtime, len) and
  re-read only on change
- **Д8** artifacts: first-class task outputs, `board.artifact` MCP tool,
  survive handoffs

## Limitations & roadmap

- dsh headless does not replay session context on `--session-id`, so the
  backend folds stored history into the task text; `Esc` for opencode only
  detaches (no server abort); reasoning/tool history is not persisted for
  CLI harnesses (text only).
- On the roadmap: workspace crate split, cargo-dist releases, ratatui-image,
  mermaid rendering, vhs-captured screenshots, fluent i18n files.
- The `opencode-acp` harness (Agent Client Protocol) is the newest connection:
  it uses the `agent-client-protocol` schema crate, real `session/cancel` and
  real permission requests; the older HTTP backend stays as the battle-tested
  default until ACP gets more mileage. Remaining plans: typed JSON models for
  the HTTP backend, a render cache for very long chats.
- MCP is the agent's concern: sessions can be pointed at MCP servers by the
  harness config; A2A is agent-to-agent and out of scope for a chat client.
- The screenshot is a mockup; a real capture (vhs) is on the wishlist.

## Author

**Mikhail Malyshev** — Telegram & YouTube: **@tsingular**

In-app About: `F1` → `A` (or `Ctrl+A`). © 2026 Mikhail Malyshev
