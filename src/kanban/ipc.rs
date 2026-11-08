//! Б1: single board owner + local IPC.
//!
//! The redb store takes an exclusive lock, so exactly ONE process may have
//! the board open — the owner (TUI or daemon). The owner serves board tool
//! calls on a loopback TCP socket; the port is published in a well-known
//! port file. Secondary processes — the board-mcp server spawned per agent
//! role — connect as clients instead of failing on the exclusive lock (and
//! instead of opening a second store, which used to duplicate task ids).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::store::BoardStore;

/// Port file location: `<config>/board.port`, overridable with
/// AI_COMMANDER_BOARD_PORT (tests, per-project setups).
pub fn port_file() -> PathBuf {
    if let Ok(p) = std::env::var("AI_COMMANDER_BOARD_PORT") {
        return PathBuf::from(p);
    }
    crate::config::config_dir().join("board.port")
}

/// Handler that executes one board tool call against the owner's store.
pub type ToolHandler = Arc<dyn Fn(&str, &Value, &str) -> Result<String, String> + Send + Sync>;

/// Become the board owner: serve tool calls on a loopback socket and
/// publish the port. Runs until the process exits; call [`cleanup`] on a
/// graceful shutdown.
pub fn serve(handle: ToolHandler) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    std::fs::write(port_file(), port.to_string())?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handle = handle.clone();
            std::thread::spawn(move || serve_conn(stream, handle));
        }
    });
    Ok(port)
}

/// Convenience: serve directly from a shared BoardStore (TUI shape).
pub fn serve_board(board: Arc<Mutex<BoardStore>>) -> std::io::Result<u16> {
    serve(Arc::new(move |tool, args, role| {
        let mut b = board.lock().unwrap();
        super::board_tool_call(tool, args, role, &mut b)
    }))
}

fn serve_conn(stream: TcpStream, handle: ToolHandler) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let reader = BufReader::new(reader);
    let mut stream = stream;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let tool = v.get("tool").and_then(Value::as_str).unwrap_or("");
        let role = v.get("role").and_then(Value::as_str).unwrap_or("");
        let args = v.get("args").cloned().unwrap_or(Value::Null);
        let reply = if tool.is_empty() {
            json!({"ok": false, "error": "missing tool"})
        } else {
            match handle(tool, &args, role) {
                Ok(text) => json!({"ok": true, "result": text}),
                Err(e) => json!({"ok": false, "error": e}),
            }
        };
        if writeln!(stream, "{reply}").is_err() {
            break;
        }
        let _ = stream.flush();
    }
}

/// Remove the port file on owner shutdown (best-effort).
pub fn cleanup() {
    let _ = std::fs::remove_file(port_file());
}

/// Is a board owner listening? (port file present + connectable)
pub fn owner_alive() -> bool {
    call_owner("board.ping", &json!({}), "").is_some()
}

/// One board tool call to the owner. `None` = no owner reachable.
pub fn call_owner(tool: &str, args: &Value, role: &str) -> Option<Result<String, String>> {
    let port: u16 = std::fs::read_to_string(port_file())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let req = json!({"tool": tool, "args": args, "role": role});
    writeln!(stream, "{req}").ok()?;
    stream.flush().ok()?;
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Some(Ok(v.get("result").and_then(Value::as_str)?.to_string()))
    } else {
        Some(Err(v.get("error").and_then(Value::as_str)?.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Б1: a client call reaches the OWNER's store — one store, no
    /// duplicates. Served and called in-process here; the cross-process
    /// proof lives in tests/board_ipc.rs (board-mcp over MCP stdio).
    #[test]
    fn tool_call_hits_the_owners_store() {
        let board = Arc::new(Mutex::new(BoardStore::open_memory()));
        let port_file = std::env::temp_dir().join(format!("aic-ipc-{}", std::process::id()));
        let _ = std::fs::remove_file(&port_file);
        // SAFETY: env override before any threads reference it in this test
        unsafe { std::env::set_var("AI_COMMANDER_BOARD_PORT", &port_file) };

        let port = serve_board(board.clone()).expect("serve");
        assert!(port > 0);
        assert_eq!(
            std::fs::read_to_string(&port_file).unwrap(),
            port.to_string()
        );

        let res = call_owner("board.create_task", &json!({"title": "via ipc"}), "coder")
            .expect("owner reachable")
            .expect("tool ok");
        assert!(res.contains("Backlog"), "created: {res}");

        // the task lives in the owner's store — no second store involved
        let b = board.lock().unwrap();
        assert!(
            b.projection().tasks.iter().any(|t| t.title == "via ipc"),
            "task must be in the owner's store"
        );

        // ping-style unknown tool surfaces the error, not silence
        assert!(call_owner("board.nope", &json!({}), "coder").is_some());

        cleanup();
        assert!(!port_file.exists(), "cleanup removes the port file");
    }
}
