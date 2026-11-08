//! ai-commander — ChatGPT-style TUI for AI agent harnesses.
//!
//! Usage:
//!   ai-commander                 interactive TUI (autostarts `opencode serve`)
//!   ai-commander --check         headless backend check: health, models, sessions
//!   ai-commander --server URL    connect to a running opencode server
//!   ai-commander --lang ru|en    UI language override
//!   ai-commander --workdir PATH  working directory for the harness

mod backend;
mod config;
mod i18n;
mod ui;

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
