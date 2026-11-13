//! ai-commander — ChatGPT-style TUI for AI agent harnesses.
//!
//! Usage:
//!   ai-commander                 interactive TUI (autostarts `opencode serve`)
//!   ai-commander --check         headless backend check: health, models, sessions
//!   ai-commander --server URL    connect to a running opencode server
//!   ai-commander --lang ru|en    UI language override
//!   ai-commander --workdir PATH  working directory for the harness

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use std::time::Instant;

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
  --log-level LVL   file log: off|error|warn|info|debug (default info;
                    writes <config>/ai-commander.log)
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
    log_level: String,
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
            "--log-level" => args.log_level = it.next().unwrap_or_default(),
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
    ai_commander::logging::init(ai_commander::logging::parse_level(&args.log_level));
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
            cwd: None,
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
            cwd: None,
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
        ai_commander::kanban::TurnOutcome::Failed => TaskAction::Interrupt,
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
    use ai_commander::kanban::{TaskStatus, store::BoardStore};
    use backend::{BackendEvent, create_backend};

    let mut backends: std::collections::HashMap<String, std::sync::Arc<dyn backend::Backend>> =
        std::collections::HashMap::new();
    // v5: the same policy engine the TUI uses — the daemon decides
    // unattended permission requests (allow/deny auto, ask → NeedsHuman).
    // A broken USER policy refuses to start: silently dropping the user's
    // own deny rules would be a security hole (v5 issues #1).
    let policy = match ai_commander::policy::Policy::load_checked(
        &std::env::current_dir().unwrap_or_default(),
    ) {
        Ok(p) => p,
        Err(e) => anyhow::bail!("invalid user policy.yaml — fix it to run the daemon: {e}"),
    };
    for w in &policy.warnings {
        println!("[policy] {w}");
        ai_commander::log_warn!("daemon", "policy: {w}");
    }
    // v5 #6: role files that failed to load are loud — a typo must not
    // silently drop a role
    for e in
        ai_commander::kanban::roles::RoleSet::cached(&std::env::current_dir().unwrap_or_default())
            .errors
    {
        println!("[roles] {}: {}", e.path.display(), e.message);
        ai_commander::log_warn!("daemon", "role {}: {}", e.path.display(), e.message);
    }
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
    // Б1: the daemon owns the board exclusively and serves board tool calls
    // over IPC so secondary processes (board-mcp per agent) don't need the
    // redb lock and never open a second store.
    let board = std::sync::Arc::new(parking_lot::Mutex::new(BoardStore::open_redb(
        &ai_commander::kanban::Kanban::path(),
    )?));
    match ai_commander::kanban::ipc::serve_board(board.clone()) {
        Ok(port) => println!("board IPC owner on 127.0.0.1:{port}"),
        Err(e) => println!("[warn] board IPC server: {e}"),
    }
    // v5 #3: drop worktrees of tasks that no longer exist (crash leftovers)
    {
        let ids: Vec<String> = board
            .lock()
            .projection()
            .tasks
            .iter()
            .map(|t| t.id.clone())
            .collect();
        for id in ai_commander::kanban::worktree::prune_orphans(
            &std::env::current_dir().unwrap_or_default(),
            &ids,
        ) {
            println!("[worktree] pruned orphan {id}");
        }
    }

    let mut otel_last_seq: u64 = 0;
    let mut busy: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut task_replies: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // v4 Н2: task ids whose turn stream errored — their StreamDone is
    // Failed even if a DONE: marker was printed before the break
    let mut task_errors: std::collections::HashSet<String> = std::collections::HashSet::new();
    // v5 #10: permission asks parked in NeedsHuman + their one-time permits
    let mut permits = ai_commander::kanban::runner::PermitBook::new();
    let mut prev_statuses: std::collections::HashMap<String, TaskStatus> =
        std::collections::HashMap::new();
    // v6 #3: high-water mark of processed human  comments
    let mut last_permit_ts: i64 = 0;
    // v9 #2: (harness, pending_id) -> real_id for event normalization
    let mut rebound: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // v8 #8 / v9 #1: wait for EITHER an event (immediate) or the 2s
        // tick. The event from select! is KEPT and processed first —
        // the old code dropped it (every lone event was lost).
        let (first_ev, periodic) = tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(e) => (Some(e), false),
                    None => return Ok(()), // channel closed
                }
            }
            _ = tick.tick() => (None, true)
        };
        // v9 #6: periodic tasks (cron, watchdog, launch, OTel) run ONLY
        // on the timer tick or when a turn finishes — not on every PartDelta
        let mut do_periodic = periodic;
        // v9 #1: process the FIRST event from select! + drain the rest
        // 1) drain finished turns via TaskRunner (Б8: classify + transition)
        use backend::PartKind;
        let mut events: Vec<backend::BackendEvent> = Vec::new();
        if let Some(ev) = first_ev {
            events.push(ev);
        }
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        for ev in &events {
            // v9 #2: handle SessionCreated (rebind pending tasks on the board)
            if let backend::BackendEvent::SessionCreated {
                harness, native_id, ..
            } = ev
            {
                let mut b = board.lock();
                let pendings: Vec<String> = b
                    .projection()
                    .tasks
                    .iter()
                    .filter(|t| {
                        t.session_id
                            .as_deref()
                            .is_some_and(|sid| sid.starts_with("pending-"))
                            && t.harness.as_deref() == Some(harness.as_str())
                    })
                    .filter_map(|t| t.session_id.clone())
                    .collect();
                for pid in pendings {
                    b.rebind_session(&pid, native_id);
                    rebound.insert((harness.clone(), pid), native_id.clone());
                }
                continue;
            }
            // v9 #2: normalize native_id through the rebind map
            let ev = normalize_event_id(ev, &rebound);
            let ev = &ev;
            match ev {
                backend::BackendEvent::PartDelta {
                    native_id,
                    harness,
                    delta,
                    kind,
                    ..
                } => {
                    if *kind == PartKind::Text {
                        let task_id = {
                            let b = board.lock();
                            b.find_by_session(harness, native_id).map(|t| t.id)
                        };
                        if let Some(id) = task_id {
                            let mut reply = task_replies.remove(&id).unwrap_or_default();
                            {
                                let mut b = board.lock();
                                ai_commander::kanban::runner::on_progress(
                                    &mut b, &id, delta, &mut reply,
                                );
                            }
                            task_replies.insert(id, reply);
                        }
                    }
                }
                backend::BackendEvent::PartUpdated {
                    native_id,
                    harness,
                    part,
                    ..
                } => {
                    // v4 Д1: tool-call progress (PartUpdated, no delta text)
                    // is a liveness signal — renew the task lease.
                    // v8 #1: Text-kind PartUpdated REPLACES the reply (dsh)
                    let task_id = {
                        let b = board.lock();
                        b.find_by_session(harness, native_id).map(|t| t.id)
                    };
                    if let Some(id) = task_id {
                        let mut b = board.lock();
                        ai_commander::kanban::runner::on_activity(&mut b, &id);
                        drop(b);
                        if part.kind == backend::PartKind::Text {
                            // v8 #1: the daemon keys replies by TASK id
                            task_replies.insert(id, part.text.clone());
                        }
                    }
                }
                backend::BackendEvent::StreamDone { native_id, harness } => {
                    // v10 #5: a finished turn should immediately trigger the
                    // launch/watchdog cycle (no 2s wait for DAG dependencies)
                    do_periodic = true;
                    let task_id = {
                        let b = board.lock();
                        b.find_by_session(harness, native_id).map(|t| t.id)
                    };
                    if let Some(id) = task_id {
                        let reply = task_replies.remove(&id).unwrap_or_default();
                        let errored = task_errors.remove(&id);
                        // v5: only a task still InProgress is classified —
                        // a permission ASK may already have moved it to
                        // NeedsHuman mid-turn
                        let outcome = {
                            let mut b = board.lock();
                            let in_progress = b
                                .projection()
                                .tasks
                                .iter()
                                .any(|t| t.id == id && t.status == TaskStatus::InProgress);
                            if in_progress {
                                ai_commander::kanban::runner::on_stream_done_ex(
                                    &mut b, &id, &reply, errored,
                                )
                            } else {
                                Ok(b.projection()
                                    .tasks
                                    .iter()
                                    .find(|t| t.id == id)
                                    .cloned()
                                    .expect("task present"))
                            }
                        };
                        match outcome {
                            Ok(t) => println!("[done] {} → {:?}", t.id, t.status),
                            Err(e) => println!("[warn] {id} transition: {e}"),
                        }
                        busy.remove(&id);
                    }
                }
                backend::BackendEvent::PermissionRequest {
                    harness,
                    session_id,
                    request_id,
                    title,
                    action,
                    resource,
                    resources,
                } => {
                    // v5: the daemon runs unattended — the policy engine
                    // decides. allow/deny are answered at once; ASK moves
                    // the task to NeedsHuman with the question recorded,
                    // instead of hanging until the lease expires.
                    let Some(b) = backends.get(harness).cloned() else {
                        continue;
                    };
                    // v5 #10: the human already approved this exact ask when
                    // un-parking the task — honor it once
                    let task_for_permit = {
                        let bb = board.lock();
                        bb.find_by_session(harness, session_id).map(|t| t.id)
                    };
                    if let Some(tid) = &task_for_permit {
                        if permits.take_permit(tid, action, resource) {
                            println!("[policy] one-time permit for {tid}: {action} {resource}");
                            let _ = b.permission_reply(session_id, request_id, "once").await;
                            continue;
                        }
                    }
                    // v6 #2: strictest across every touched location
                    match policy.check_all(action, resources) {
                        ai_commander::policy::Effect::Allow => {
                            println!("[policy] auto-allow {action}: {resource}");
                            let _ = b.permission_reply(session_id, request_id, "once").await;
                        }
                        ai_commander::policy::Effect::Deny => {
                            println!("[policy] auto-deny {action}: {resource}");
                            let _ = b.permission_reply(session_id, request_id, "reject").await;
                        }
                        ai_commander::policy::Effect::Ask => {
                            let task_id = {
                                let bb = board.lock();
                                bb.find_by_session(harness, session_id).map(|t| t.id)
                            };
                            if let Some(id) = task_id {
                                permits.record_ask(&id, action, resource);
                                let question = format!(
                                    "agent asks permission: {title} ({action}: {resource})"
                                );
                                let outcome = {
                                    let mut bb = board.lock();
                                    ai_commander::kanban::runner::on_permission_ask(
                                        &mut bb, &id, &question,
                                    )
                                };
                                match outcome {
                                    Ok(t) => {
                                        println!("[policy] ask → {} NeedsHuman: {question}", t.id)
                                    }
                                    Err(e) => println!("[warn] {id} ask transition: {e}"),
                                }
                                busy.remove(&id);
                            }
                            // the agent's turn must not hang on the answer
                            let _ = b.permission_reply(session_id, request_id, "reject").await;
                        }
                    }
                }
                backend::BackendEvent::StreamError {
                    native_id, harness, ..
                } => {
                    // v4 Н2: remember the broken stream; the following
                    // StreamDone must not honor a printed DONE: marker
                    let task_id = {
                        let b = board.lock();
                        b.find_by_session(harness, native_id).map(|t| t.id)
                    };
                    if let Some(id) = task_id {
                        task_errors.insert(id);
                    }
                }
                _ => {}
            }
        }

        if !do_periodic {
            // v9 #6: skip the heavy periodic work on event-only iterations
            continue;
        }

        // 1.5) cron triggers: spawn due recurring tasks
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for id in board.lock().spawn_due_scheduled(now_secs) {
            println!("[trigger] spawned {id} from schedule");
            ai_commander::log_info!("daemon", "trigger spawned {id}");
        }

        // 2) watchdog: scan stuck tasks → abort their sessions → apply (Б6)
        // Д2: aborts are awaited (abort_stuck joins every handle) so a task
        // is never re-launched while its old agent process is still alive
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let stuck = board.lock().watchdog_scan(now);
        if !stuck.is_empty() {
            for st in &stuck {
                println!(
                    "[watchdog] aborting {} session {}",
                    st.task_id, st.session_id
                );
            }
            ai_commander::kanban::runner::abort_stuck(&backends, &stuck).await;
            for (id, label) in board.lock().watchdog_apply(&stuck) {
                println!("[watchdog] {id}: {label}");
                ai_commander::log_info!("daemon", "watchdog {id}: {label}");
                busy.remove(&id);
            }
        }

        // v6 #3: the ONLY grant path is an explicit human action — new
        // `permit` journal comments (written by the human-only board_unpark
        // tool over IPC). Merely observing NeedsHuman→Ready granted agents
        // their own permits via `promote`.
        {
            let b = board.lock();
            for t in b.projection().tasks.iter() {
                for c in t.comments.iter() {
                    if c.kind == "permit"
                        && c.author == "human"
                        && c.ts_ms > last_permit_ts
                        && permits.grant(&t.id)
                    {
                        println!(
                            "[permit] human granted the parked ask of {} (comment {})",
                            t.id, c.ts_ms
                        );
                        if c.ts_ms > last_permit_ts {
                            last_permit_ts = c.ts_ms;
                        }
                    }
                }
                prev_statuses.insert(t.id.clone(), t.status);
            }
        }

        // 3) launch one launchable task per idle harness (B9: DAG-aware)
        let ready: Vec<ai_commander::kanban::Task> = board.lock().launchable();
        let roles_snapshot = ai_commander::kanban::roles::RoleSet::cached(
            &std::env::current_dir().unwrap_or_default(),
        );
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
            // v5 #6: a task assigned to a role that failed to load (or does
            // not exist) must not be launched blind — escalate with the cause
            if let Some(role_id) = &task.role {
                if !roles_snapshot.roles.contains_key(role_id) {
                    let reason = match roles_snapshot.errors.iter().find(|e| {
                        e.path.file_stem().map(|s| s.to_string_lossy())
                            == Some(role_id.clone().into())
                    }) {
                        Some(e) => format!("role '{role_id}' failed to load: {}", e.message),
                        None => format!("role '{role_id}' is not defined"),
                    };
                    println!("[escalate] {} launch blocked: {reason}", task.id);
                    let mut b = board.lock();
                    ai_commander::kanban::runner::launch_failed(&mut b, &task.id, &reason);
                    busy.remove(&task.id);
                    continue;
                }
            }

            // B9+Н3: shared TaskRunner — Start creates the worktree, the
            // session is created with cwd=worktree so the agent works there
            match ai_commander::kanban::runner::launch_task(b, &board, &task, &cfg, &roles_snapshot)
                .await
            {
                Ok(_session_id) => {
                    busy.insert(task.id.clone());
                    println!("[run] {} '{}' on {harness}", task.id, task.title);
                    ai_commander::log_info!("daemon", "run {} on {harness}", task.id);
                    break; // one at a time keeps the log readable
                }
                Err(e) => {
                    println!("[warn] launch {}: {e:#}", task.id);
                    ai_commander::log_warn!("daemon", "launch {}: {e:#}", task.id);
                    // v4: Start itself failing (broken worktree/branch, task
                    // still Ready) escalates to a human — retrying the same
                    // broken Start every 30 s would loop forever (attempts
                    // never grow on a failed Start).
                    // Д5: a failure AFTER Start (session/send) still ends the
                    // turn via the Н1 retry cap.
                    let still_ready = {
                        let b = board.lock();
                        b.projection()
                            .tasks
                            .iter()
                            .any(|t| t.id == task.id && t.status == TaskStatus::Ready)
                    };
                    let mut b = board.lock();
                    if still_ready {
                        ai_commander::kanban::runner::launch_failed(
                            &mut b,
                            &task.id,
                            &format!("{e:#}"),
                        );
                        println!("[escalate] {} → NeedsHuman (launch failure)", task.id);
                    } else {
                        let _ = ai_commander::kanban::runner::on_stream_done(&mut b, &task.id, "");
                    }
                    busy.remove(&task.id);
                }
            }
        }

        // 4) stop when everything is terminal
        let all_done = {
            let b = board.lock();
            !b.projection().tasks.is_empty()
                && b.projection().tasks.iter().all(|t| {
                    matches!(
                        t.status,
                        TaskStatus::Done
                            | TaskStatus::Cancelled
                            | TaskStatus::NeedsHuman
                            | TaskStatus::Blocked
                    )
                })
        };
        if all_done && busy.is_empty() {
            println!("daemon: board settled, exiting");
            for b in backends.values() {
                b.shutdown();
            }
            ai_commander::kanban::ipc::cleanup();
            return Ok(());
        }
        // OTel export: new events since the last pass -> spans.jsonl
        {
            let last = board.lock().events_since(otel_last_seq);
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
        // v9 #6: no unconditional sleep — tick.tick() above IS the timer
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
    // Б1: the TUI owned the board — unpublish the IPC port file
    ai_commander::kanban::ipc::cleanup();
    guard.release();
    run_result
}

/// v9 #2: rewrite the native_id of an event through the rebind map so all
/// handlers see the real session id even when the backend sends pending-*.
fn normalize_event_id(
    ev: &backend::BackendEvent,
    rebound: &std::collections::HashMap<(String, String), String>,
) -> backend::BackendEvent {
    let fix = |harness: &str, id: &str| -> String {
        rebound
            .get(&(harness.to_string(), id.to_string()))
            .cloned()
            .unwrap_or_else(|| id.to_string())
    };
    let mut ev = ev.clone();
    match &mut ev {
        backend::BackendEvent::PartDelta {
            harness, native_id, ..
        }
        | backend::BackendEvent::PartUpdated {
            harness, native_id, ..
        }
        | backend::BackendEvent::StreamDone { harness, native_id }
        | backend::BackendEvent::StreamError {
            harness, native_id, ..
        }
        | backend::BackendEvent::Usage {
            harness, native_id, ..
        }
        | backend::BackendEvent::Compaction {
            harness, native_id, ..
        } => {
            *native_id = fix(harness, native_id);
        }
        backend::BackendEvent::PermissionRequest {
            harness,
            session_id,
            ..
        } => {
            *session_id = fix(harness, session_id);
        }
        _ => {}
    }
    ev
}
