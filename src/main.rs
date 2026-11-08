//! ai-commander — ChatGPT-style TUI for AI agent harnesses.
//!
//! Usage:
//!   ai-commander                 interactive TUI (autostarts `opencode serve`)
//!   ai-commander --check         headless backend check: health, models, sessions
//!   ai-commander --server URL    connect to a running opencode server
//!   ai-commander --lang ru|en    UI language override
//!   ai-commander --workdir PATH  working directory for the harness

use ai_commander::{backend, config, i18n, ui};

use std::io::stdout;

use anyhow::Result;
use crossterm::event::Event as CEvent;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{event as cev, execute};
use ratatui::Terminal;

use config::Config;

fn print_help() {
    println!(
        "ai-commander {} — ChatGPT-style TUI for AI harnesses (opencode…)
  (no args)         run TUI
  --check           headless: connect to harness, print models & sessions
  --send TEXT       headless E2E: send a message, stream the live reply
  --server URL      use this opencode server instead of autostart
  --lang ru|en      UI language (default en; switchable in settings)
  --workdir PATH    harness working directory
  --version         print version

Developer: Mikhail Malyshev — Telegram & YouTube: @tsingular",
        env!("CARGO_PKG_VERSION")
    );
}

#[derive(Default)]
struct Args {
    check: bool,
    send: String,
    task: String,
    daemon: bool,
    print_mcp: bool,
    wait_secs: u64,
    server: String,
    lang: String,
    workdir: String,
    harness: String,
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => args.check = true,
            "--send" => args.send = it.next().unwrap_or_default(),
            "--task" => args.task = it.next().unwrap_or_default(),
            "--daemon" => args.daemon = true,
            "--print-mcp-config" => args.print_mcp = true,
            "--wait" => args.wait_secs = it.next().and_then(|s| s.parse().ok()).unwrap_or(60),
            "--server" => args.server = it.next().unwrap_or_default(),
            "--lang" => args.lang = it.next().unwrap_or_default(),
            "--workdir" => args.workdir = it.next().unwrap_or_default(),
            "--harness" => args.harness = it.next().unwrap_or_default(),
            "--version" | "-V" => {
                println!("ai-commander {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => eprintln!("unknown arg: {other} (see --help)"),
        }
    }
    args
}

fn apply_overrides(cfg: &mut Config, args: &Args) {
    if !args.lang.is_empty() {
        cfg.lang = i18n::Lang::parse(&args.lang);
    }
    if !args.server.is_empty() || !args.workdir.is_empty() {
        if let Some(h) = cfg.harnesses.iter_mut().find(|h| h.kind == "opencode") {
            if !args.server.is_empty() {
                h.server_url = args.server.trim_end_matches('/').to_string();
            }
            if !args.workdir.is_empty() {
                h.workdir = args.workdir.clone();
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    let mut cfg = Config::load()?;
    apply_overrides(&mut cfg, &args);

    if args.check {
        return run_check(cfg, &args.harness).await;
    }
    if args.daemon {
        return run_daemon(cfg).await;
    }
    if args.print_mcp {
        return print_mcp_config();
    }
    if !args.task.is_empty() {
        return run_task(cfg, &args.task, args.wait_secs.max(5), &args.harness).await;
    }
    if !args.send.is_empty() {
        return run_send(cfg, &args.send, args.wait_secs.max(5), &args.harness).await;
    }

    run_tui(cfg).await
}

// ---------------- headless E2E: send a message, watch the live stream ----------------

async fn run_send(cfg: Config, text: &str, wait_secs: u64, harness_id: &str) -> Result<()> {
    use backend::{BackendEvent, PartKind, SendOpts, create_backend};
    use std::time::Instant;

    let harness_cfg = cfg
        .harnesses
        .iter()
        .find(|h| {
            h.enabled
                && backend::supported_kind(&h.kind)
                && (harness_id.is_empty() || h.id == harness_id)
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no enabled harness '{}' in config", harness_id))?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&harness_cfg, tx)?;
    let version = b.connect().await?;
    println!("connected: {} v{version}", harness_cfg.kind);

    let session = b.create_session("ai-commander e2e").await?;
    println!("session: {}", session.native_id);

    let model = harness_cfg.default_model.clone();
    let agent = if harness_cfg.agent.is_empty() {
        None
    } else {
        Some(harness_cfg.agent.clone())
    };
    b.send(
        &session.native_id,
        text,
        &[],
        SendOpts {
            model: model.as_ref(),
            agent: agent.as_deref(),
            mcp_role: None,
        },
    )
    .await?;
    println!("sent, streaming up to {wait_secs}s…");

    let mut saw_delta = false;
    let mut last_text = String::new();
    let deadline = Instant::now() + std::time::Duration::from_secs(wait_secs);
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        let ev = match tokio::time::timeout(remain, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(_) => break, // deadline
        };
        match ev {
            BackendEvent::PartDelta {
                native_id,
                kind,
                delta,
                ..
            } if native_id == session.native_id && kind == PartKind::Text => {
                saw_delta = true;
                print!("{delta}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            BackendEvent::PartUpdated {
                native_id, part, ..
            } if native_id == session.native_id && part.kind == PartKind::Text && !saw_delta => {
                // replace-style event without deltas seen
                last_text = part.text.clone();
            }
            BackendEvent::StreamDone { native_id, .. } if native_id == session.native_id => {
                println!();
                break;
            }
            BackendEvent::StreamError {
                native_id, error, ..
            } if native_id == session.native_id => {
                println!("\n[stream error] {error}");
                break;
            }
            BackendEvent::Error { error, .. } => {
                println!("\n[error] {error}");
                break;
            }
            _ => {}
        }
    }
    if saw_delta {
        println!("\n--- E2E OK: live deltas received");
    } else if !last_text.is_empty() {
        println!("{last_text}");
        println!("--- E2E OK: reply received via replace-events");
    } else {
        // fall back to history
        let msgs = b.load_history(&session.native_id).await?;
        let assistant: Vec<String> = msgs
            .iter()
            .filter(|m| m.role == backend::Role::Assistant)
            .flat_map(|m| {
                m.parts
                    .iter()
                    .filter(|p| p.kind == PartKind::Text)
                    .map(|p| p.text.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        let joined = assistant.join("\n");
        println!(
            "{}",
            if joined.is_empty() {
                "(no reply)"
            } else {
                &joined
            }
        );
        if joined.is_empty() {
            let _ = b.delete_session(&session.native_id).await;
            b.shutdown();
            anyhow::bail!("E2E FAILED: no assistant reply within {wait_secs}s");
        }
        println!("--- E2E OK (via history fallback)");
    }
    let _ = b.delete_session(&session.native_id).await;
    b.shutdown();
    Ok(())
}

// ---------------- headless check ----------------

async fn run_check(cfg: Config, harness_id: &str) -> Result<()> {
    use backend::create_backend;

    let harness_cfg = cfg
        .harnesses
        .iter()
        .find(|h| {
            h.enabled
                && backend::supported_kind(&h.kind)
                && (harness_id.is_empty() || h.id == harness_id)
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no enabled harness '{}' in config", harness_id))?;

    println!(
        "ai-commander --check: harness {} [{}] kind={}",
        harness_cfg.name, harness_cfg.id, harness_cfg.kind
    );
    println!(
        "server: {}",
        if harness_cfg.server_url.is_empty() {
            "autostart".into()
        } else {
            harness_cfg.server_url.clone()
        }
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&harness_cfg, tx)?;

    let version = b.connect().await?;
    println!("connected: {} v{version}", harness_cfg.kind);

    let models = b.list_models().await?;
    println!("\nmodels ({})", models.len());
    for m in models.iter() {
        println!("  {}", m.display());
    }

    let sessions = b.refresh_sessions().await?;
    println!("\nsessions ({})", sessions.len());
    for s in sessions.iter().take(15) {
        println!("  [{}] {}", s.native_id, s.title);
    }

    println!(
        "\nattachment mimes: png={} md={}",
        backend::mime_for(std::path::Path::new("x.png")),
        backend::mime_for(std::path::Path::new("x.md"))
    );

    // drain stray events
    while rx.try_recv().is_ok() {}
    b.shutdown();
    println!("\ncheck OK");
    Ok(())
}

// ---------------- headless kanban task ----------------

/// Create a board task, run it on a harness, report the FSM outcome.
async fn run_task(cfg: Config, title: &str, wait_secs: u64, harness_id: &str) -> Result<()> {
    use ai_commander::kanban::{TaskAction, classify_reply, store::BoardStore};
    use backend::{BackendEvent, PartKind, SendOpts, create_backend};
    use std::time::Instant;

    let harness_cfg = cfg
        .harnesses
        .iter()
        .find(|h| {
            h.enabled
                && backend::supported_kind(&h.kind)
                && (harness_id.is_empty() || h.id == harness_id)
        })
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no enabled harness '{harness_id}' in config"))?;

    let mut board = BoardStore::open_redb(&ai_commander::kanban::Kanban::path())?;
    let task = board.add_task(title);
    let role = std::env::var("AI_COMMANDER_ROLE")
        .ok()
        .filter(|r| !r.is_empty());
    board
        .assign_with_role(&task.id, Some(harness_cfg.id.clone()), role)
        .map_err(anyhow::Error::msg)?;
    let task = board
        .transition(&task.id, TaskAction::Promote)
        .map_err(anyhow::Error::msg)?;
    println!("task {} '{}' → {:?}", task.id, task.title, task.status);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&harness_cfg, tx.clone())?;
    let _version = b.connect().await?;

    let session = b.create_session(title).await?;
    board.link_session(&task.id, &session.native_id);
    let task = board
        .transition(&task.id, TaskAction::Start)
        .map_err(anyhow::Error::msg)?;
    println!(
        "running on {} [{}] attempt {}",
        harness_cfg.id, session.native_id, task.attempts
    );

    let prompt = ai_commander::kanban::task_prompt(&task);
    let model = harness_cfg.default_model.clone();
    let agent = if harness_cfg.agent.is_empty() {
        None
    } else {
        Some(harness_cfg.agent.clone())
    };
    b.send(
        &session.native_id,
        &prompt,
        &[],
        SendOpts {
            model: model.as_ref(),
            agent: agent.as_deref(),
            mcp_role: None,
        },
    )
    .await?;

    let mut reply = String::new();
    let deadline = Instant::now() + std::time::Duration::from_secs(wait_secs);
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        let ev = match tokio::time::timeout(remain, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) | Err(_) => break,
        };
        match ev {
            BackendEvent::PartDelta {
                native_id,
                kind,
                delta,
                ..
            } if native_id == session.native_id && kind == PartKind::Text => {
                print!("{delta}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                reply.push_str(&delta);
            }
            BackendEvent::StreamDone { native_id, .. } if native_id == session.native_id => {
                println!();
                break;
            }
            BackendEvent::StreamError {
                native_id, error, ..
            } if native_id == session.native_id => {
                println!(
                    "
[error] {error}"
                );
                break;
            }
            _ => {}
        }
    }

    let action = match classify_reply(&reply) {
        ai_commander::kanban::TurnOutcome::Finished => TaskAction::Finish,
        ai_commander::kanban::TurnOutcome::Blocked => TaskAction::Block,
        ai_commander::kanban::TurnOutcome::NeedsHuman => TaskAction::Ask,
    };
    let final_task = board
        .transition(&task.id, action)
        .map_err(anyhow::Error::msg)?;
    println!(
        "
{} final status: {:?}",
        final_task.id, final_task.status
    );
    b.shutdown();
    Ok(())
}

fn print_mcp_config() -> Result<()> {
    let exe = std::env::current_exe()?;
    let bin = exe
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("board-mcp.exe");
    let bin_str = bin.to_string_lossy();
    println!(
        "# opencode.jsonc — add to your config so agent sessions get the board:
\"mcp\": {{
  \"board\": {{
    \"type\": \"local\",
    \"command\": [\"{bin_str}\"],
    \"environment\": {{\"AI_COMMANDER_ROLE\": \"coder\"}}
  }}
}}

# hermes / dsh: point their MCP config at the same binary with
# AI_COMMANDER_ROLE=<role> in the environment."
    );
    Ok(())
}

// ---------------- daemon ----------------

/// Background executor: runs Ready tasks (harness assigned) through their
/// harnesses, retries stuck ones via the watchdog, keeps the board moving
/// while no TUI is open. Stop with Ctrl+C.
async fn run_daemon(cfg: Config) -> Result<()> {
    use ai_commander::kanban::{TaskAction, TaskStatus, store::BoardStore};
    use backend::{BackendEvent, SendOpts, create_backend};
    use std::time::Instant;

    let mut backends: std::collections::HashMap<String, std::sync::Arc<dyn backend::Backend>> =
        std::collections::HashMap::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    for h in cfg
        .harnesses
        .iter()
        .filter(|h| h.enabled && backend::supported_kind(&h.kind))
    {
        if let Ok(b) = create_backend(h, tx.clone()) {
            if b.connect().await.is_ok() {
                backends.insert(h.id.clone(), b);
            }
        }
    }
    if backends.is_empty() {
        anyhow::bail!("daemon: no usable harness backends");
    }
    let mut board = BoardStore::open_redb(&ai_commander::kanban::Kanban::path())?;

    // A2A gateway: share the daemon's single redb connection
    if let Ok(g) = ai_commander::kanban::a2a::A2aGateway::spawn(board.gateway_handle()) {
        println!("a2a gateway: {}", g.agent_card_url());
    }

    let mut otel_last_seq: u64 = 0;
    let mut busy: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        // 1) drain finished turns
        while let Ok(ev) = rx.try_recv() {
            if let backend::BackendEvent::StreamDone { native_id, .. } = &ev {
                busy.remove(native_id);
            }
        }

        // 1.5) cron triggers: spawn due recurring tasks
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for id in board.spawn_due_scheduled(now_secs) {
            println!("[trigger] spawned {id} from schedule");
        }

        // 2) watchdog pass (retry / escalate)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        for (id, label) in board.watchdog_pass(now) {
            println!("[watchdog] {id}: {label}");
            busy.remove(&id);
        }

        // 3) launch one Ready task per idle harness
        let ready: Vec<ai_commander::kanban::Task> =
            board.projection().by_status(TaskStatus::Ready);
        for task in ready {
            let Some(harness) = task.harness.clone() else {
                continue;
            };
            let Some(b) = backends.get(&harness) else {
                continue;
            };
            if busy.contains(&task.id) {
                continue;
            }
            let prompt = ai_commander::kanban::task_prompt(&task);
            let session = match b.create_session(&task.title).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            board.link_session(&task.id, &session.native_id);
            let cwd = std::env::current_dir().unwrap_or_default();
            if board
                .transition_in(&cwd, &task.id, TaskAction::Start)
                .is_err()
            {
                continue;
            }
            let model = cfg.harness(&harness).and_then(|h| h.default_model.clone());
            let agent = cfg
                .harness(&harness)
                .map(|h| h.agent.clone())
                .filter(|a| !a.is_empty());
            let _ = b
                .send(
                    &session.native_id,
                    &prompt,
                    &[],
                    SendOpts {
                        model: model.as_ref(),
                        agent: agent.as_deref(),
                        mcp_role: None,
                    },
                )
                .await;
            busy.insert(task.id.clone());
            println!("[run] {} '{}' on {harness}", task.id, task.title);
            break; // one at a time keeps the log readable
        }

        // 4) stop when everything is terminal
        let all_done = board.projection().tasks.iter().all(|t| {
            matches!(
                t.status,
                TaskStatus::Done
                    | TaskStatus::Cancelled
                    | TaskStatus::NeedsHuman
                    | TaskStatus::Blocked
            )
        });
        if all_done && busy.is_empty() && !board.projection().tasks.is_empty() {
            println!("daemon: board settled, exiting");
            for b in backends.values() {
                b.shutdown();
            }
            return Ok(());
        }
        // OTel export: new events since the last pass -> spans.jsonl
        {
            let last = board.last_events(usize::MAX);
            if let Some(newest) = last.last() {
                if newest.seq > otel_last_seq {
                    let fresh: Vec<_> = last
                        .iter()
                        .filter(|e| e.seq > otel_last_seq)
                        .cloned()
                        .collect();
                    if let Ok(n) = ai_commander::kanban::otel::export_events(
                        &ai_commander::kanban::Kanban::path()
                            .parent()
                            .unwrap_or(std::path::Path::new("."))
                            .join("otel"),
                        &fresh,
                        "ai-commander",
                    ) {
                        otel_last_seq = newest.seq;
                        if n > 0 {
                            println!("[otel] exported {n} spans");
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let _ = Instant::now();
    }
}

// ---------------- TUI ----------------

fn spawn_input_thread(tx: tokio::sync::mpsc::UnboundedSender<CEvent>) {
    std::thread::spawn(move || {
        loop {
            match cev::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
}

/// RAII terminal guard: raw mode, alternate screen, mouse and paste capture
/// are restored on *every* exit path — panics, errors, Ctrl+C, normal quit.
struct TerminalGuard {
    terminal: Option<Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>,
}

impl TerminalGuard {
    fn release(&mut self) {
        let _ = disable_raw_mode();
        if let Some(t) = self.terminal.as_mut() {
            let _ = execute!(
                t.backend_mut(),
                LeaveAlternateScreen,
                cev::DisableMouseCapture,
                cev::DisableBracketedPaste
            );
            let _ = t.show_cursor();
        }
        self.terminal = None;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.release();
    }
}

async fn run_tui(cfg: Config) -> Result<()> {
    // panic hook restores everything too (incl. mouse + paste)
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            LeaveAlternateScreen,
            cev::DisableMouseCapture,
            cev::DisableBracketedPaste
        );
        orig_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        cev::EnableMouseCapture,
        cev::EnableBracketedPaste
    )?;
    let terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;
    let mut guard = TerminalGuard {
        terminal: Some(terminal),
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_input_thread(tx);

    let mut app = ui::App::new(cfg, rx);
    let run_result = app.run(guard.terminal.as_mut().expect("terminal")).await;

    // kill harness servers on every path, then restore the terminal via Drop
    for b in app.backends.values() {
        b.shutdown();
    }
    guard.release();
    run_result
}
