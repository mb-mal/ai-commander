//! File logging for the TUI (stderr is invisible inside ratatui). Simple,
//! dependency-free: a global append-only file handle + level filter, plus a
//! `--log-level` flag. Backends and the app log through `log!` macros; the
//! daemon also mirrors its println output there.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static SINK: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn parse_level(s: &str) -> Level {
    match s.to_ascii_lowercase().as_str() {
        "off" | "none" => Level::Off,
        "error" => Level::Error,
        "warn" | "warning" => Level::Warn,
        "debug" | "trace" => Level::Debug,
        _ => Level::Info,
    }
}

/// Initialize: open `<config_dir>/ai-commander.log` for append.
pub fn init(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
    if level == Level::Off {
        return;
    }
    let path = crate::config::config_dir().join("ai-commander.log");
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        *SINK.lock().unwrap() = Some(f);
    }
}

pub fn enabled(level: Level) -> bool {
    (level as u8) <= LEVEL.load(Ordering::Relaxed)
        && LEVEL.load(Ordering::Relaxed) != Level::Off as u8
}

pub fn write(level: Level, target: &str, msg: &str) {
    if !enabled(level) {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Some(f) = SINK.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{ts} {level:?} [{target}] {msg}");
    }
}

#[macro_export]
macro_rules! log_error {
    ($target:expr, $($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Error, $target, &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_warn {
    ($target:expr, $($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Warn, $target, &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_info {
    ($target:expr, $($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Info, $target, &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_debug {
    ($target:expr, $($arg:tt)*) => { $crate::logging::write($crate::logging::Level::Debug, $target, &format!($($arg)*)) };
}
