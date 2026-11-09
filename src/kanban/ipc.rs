//! Б1: single board owner + local IPC, hardened (review v4).
//!
//! The redb store takes an exclusive lock, so exactly ONE process may have
//! the board open — the owner (TUI or daemon). The owner serves board tool
//! calls on a loopback TCP socket. Secondary processes — the board-mcp
//! server spawned per agent role — connect as clients instead of failing on
//! the exclusive lock (and instead of opening a second store, which used to
//! duplicate task ids).
//!
//! v4 hardening — the socket is reachable by any local process, and
//! browsers can shoot blind `no-cors` POSTs at ephemeral ports, so:
//!
//! * the port file carries a 32-byte transport token (file mode 0600);
//!   every request must present it,
//! * the first line that is not a JSON object closes the connection —
//!   this kills stray HTTP requests on sight,
//! * the acting role comes from a per-role token the owner mints at agent
//!   launch ([`issue_role_token`]) — the `role` field of a request body is
//!   ignored, so a client cannot impersonate another role.

use parking_lot::Mutex;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
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
/// The actor is the resolved identity (role or human) — v6 #3.
pub type ToolHandler =
    Arc<dyn Fn(&str, &Value, super::Actor) -> Result<String, String> + Send + Sync>;

/// 32 random bytes, hex-encoded.
fn new_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("system entropy");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Role tokens minted by the owner, mapped to the role they authorize.
static ROLE_TOKENS: Mutex<Option<std::collections::HashMap<String, String>>> = Mutex::new(None);

fn role_tokens_mut<R>(f: impl FnOnce(&mut std::collections::HashMap<String, String>) -> R) -> R {
    let mut g = ROLE_TOKENS.lock();
    let map = g.get_or_insert_with(Default::default);
    f(map)
}

/// Mint (or return the existing) token that authorizes requests acting as
/// `role`. The owner passes it to the agent's board-mcp process via the
/// AI_COMMANDER_BOARD_TOKEN env at launch; the body `role` field of IPC
/// requests is never trusted.
pub fn issue_role_token(role: &str) -> String {
    role_tokens_mut(|map| {
        if let Some((tok, _)) = map.iter().find(|(_, r)| r.as_str() == role) {
            return tok.clone();
        }
        let tok = new_token();
        map.insert(tok.clone(), role.to_string());
        tok
    })
}

fn role_of_token(token: &str) -> Option<String> {
    role_tokens_mut(|map| map.get(token).cloned())
}

/// Become the board owner: serve tool calls on a loopback socket and
/// publish the port + transport token. Runs until the process exits; call
/// [`cleanup`] on a graceful shutdown.
pub fn serve(handle: ToolHandler) -> std::io::Result<u16> {
    serve_at(&port_file(), handle)
}

/// Path-explicit variant (tests use their own port file instead of the
/// process-global env override, which races other tests).
pub fn serve_at(path: &std::path::Path, handle: ToolHandler) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = new_token();
    // v6 #3: the HUMAN token — whoever can read this 0600 file is the
    // local user; it authorizes human-only actions (board_unpark)
    let human_token = new_token();
    std::fs::write(path, format!("{port}\n{token}\n{human_token}"))?;
    restrict_to_owner(path);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handle = handle.clone();
            let token = token.clone();
            let human_token = human_token.clone();
            std::thread::spawn(move || {
                serve_conn(stream, handle, token, human_token)
            });
        }
    });
    Ok(port)
}

/// Port file readable by the owning user only (on Unix; on Windows the
/// config dir under the user profile is already per-user).
fn restrict_to_owner(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Convenience: serve directly from a shared BoardStore (TUI shape).
pub fn serve_board(board: Arc<Mutex<BoardStore>>) -> std::io::Result<u16> {
    let handle: ToolHandler = Arc::new(move |tool, args, actor| {
        let mut b = board.lock();
        super::board_tool_call(tool, args, &actor, &mut b)
    });
    serve(handle)
}

fn serve_conn(
    stream: TcpStream,
    handle: ToolHandler,
    transport_token: String,
    human_token: String,
) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let reader = BufReader::new(reader);
    let mut stream = stream;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        // v4: the first line that is not a JSON object closes the
        // connection — an HTTP request dies on its request line
        // ("POST / HTTP/1.1"), long before any body could execute
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            break;
        };
        if !v.is_object() {
            break;
        }
        // v4: transport token is mandatory and must match the port file
        let given = v.get("token").and_then(Value::as_str).unwrap_or("");
        if given != transport_token {
            let _ = writeln!(
                stream,
                "{}",
                json!({"ok": false, "error": "bad transport token"})
            );
            let _ = stream.flush();
            break;
        }
        // v4: the acting role comes from the role token only;
        // v6 #3: the HUMAN token is a separate authority
        let role_token = v.get("role_token").and_then(Value::as_str).unwrap_or("");
        let actor = if role_token == human_token {
            Some(super::Actor::Human)
        } else {
            role_of_token(role_token).map(super::Actor::Role)
        };
        let Some(role) = actor else {
            let _ = writeln!(
                stream,
                "{}",
                json!({"ok": false, "error": "unknown role token"})
            );
            let _ = stream.flush();
            continue;
        };
        let tool = v.get("tool").and_then(Value::as_str).unwrap_or("");
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
    let Some((port, _token, _human)) = read_port_file(&port_file()) else {
        return false;
    };
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// The HUMAN token from the port file (local user authority), if present.
pub fn human_token() -> Option<String> {
    read_port_file(&port_file()).and_then(|(_, _, h)| h)
}

fn read_port_file(path: &std::path::Path) -> Option<(u16, String, Option<String>)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut lines = raw.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let token = lines.next()?.trim().to_string();
    let human = lines.next().map(|l| l.trim().to_string());
    Some((port, token, human))
}

/// One board tool call to the owner as the bearer of `role_token`.
/// `None` = no owner reachable.
pub fn call_owner(tool: &str, args: &Value, role_token: &str) -> Option<Result<String, String>> {
    call_owner_at(&port_file(), tool, args, role_token)
}

/// Path-explicit variant for tests (see [`serve_at`]).
pub fn call_owner_at(
    port_file: &std::path::Path,
    tool: &str,
    args: &Value,
    role_token: &str,
) -> Option<Result<String, String>> {
    let (port, token, _human) = read_port_file(port_file)?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let req = json!({"token": token, "role_token": role_token, "tool": tool, "args": args});
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
    use std::io::Read as _;

    fn handler_for(board: Arc<Mutex<BoardStore>>) -> (ToolHandler, Arc<Mutex<BoardStore>>) {
        let b2 = board.clone();
        let h: ToolHandler = Arc::new(move |tool, args, role| {
            let mut b = b2.lock();
            super::super::board_tool_call(tool, args, &role, &mut b)
        });
        (h, board)
    }

    /// v4: an HTTP request aimed at the IPC port must NOT execute its JSON
    /// body — the connection is closed on the request line and nothing is
    /// created.
    #[test]
    fn http_request_is_rejected_and_connection_closed() {
        let board = Arc::new(Mutex::new(BoardStore::open_memory()));
        let (handle, board) = handler_for(board);
        let pf = std::env::temp_dir().join(format!("aic-ipc-http-{}", std::process::id()));
        let _ = std::fs::remove_file(&pf);
        let port = serve_at(&pf, handle).expect("serve");

        // what a web page sends: fetch(url, {method: 'POST', mode: 'no-cors', body})
        let body = "{\"tool\":\"board_create_task\",\"args\":{\"title\":\"injected from a web page\"},\"role\":\"anyone\"}\n";
        let req = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\n\
             Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let _ = s.shutdown(std::net::Shutdown::Write);
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);

        assert!(
            out.trim().is_empty(),
            "HTTP must be cut off without a reply, got: {out}"
        );
        assert!(
            board.lock().projection().tasks.is_empty(),
            "the HTTP body must not create a task"
        );
        let _ = std::fs::remove_file(&pf);
    }

    /// v4: a request without the transport token (or with a wrong one) is
    /// rejected and the connection closed.
    #[test]
    fn request_without_or_wrong_token_rejected() {
        let board = Arc::new(Mutex::new(BoardStore::open_memory()));
        let (handle, board) = handler_for(board);
        let pf = std::env::temp_dir().join(format!("aic-ipc-tok-{}", std::process::id()));
        let _ = std::fs::remove_file(&pf);
        let port = serve_at(&pf, handle).expect("serve");
        let role_tok = issue_role_token("coder");

        for body in [
            // no token at all
            format!(
                "{{\"tool\":\"board_create_task\",\"args\":{{\"title\":\"x\"}},\"role_token\":\"{role_tok}\"}}\n"
            ),
            // wrong token
            format!(
                "{{\"token\":\"deadbeef\",\"tool\":\"board_create_task\",\"args\":{{\"title\":\"x\"}},\"role_token\":\"{role_tok}\"}}\n"
            ),
        ] {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(body.as_bytes()).unwrap();
            let _ = s.shutdown(std::net::Shutdown::Write);
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            assert!(
                out.contains("bad transport token"),
                "must be rejected: {out}"
            );
        }
        assert!(
            board.lock().projection().tasks.is_empty(),
            "no task may be created without a valid token"
        );
        let _ = std::fs::remove_file(&pf);
    }

    /// v4: the acting role is taken from the role token, never from the
    /// request body — `approve` with a spoofed `role` field must fail.
    #[test]
    fn approve_with_spoofed_role_rejected() {
        let board = Arc::new(Mutex::new(BoardStore::open_memory()));
        let (handle, board) = handler_for(board);
        let pf = std::env::temp_dir().join(format!("aic-ipc-role-{}", std::process::id()));
        let _ = std::fs::remove_file(&pf);
        serve_at(&pf, handle).expect("serve");
        let coder_tok = issue_role_token("coder");
        let reviewer_tok = issue_role_token("reviewer");

        // coder builds an ASSIGNED task into Review (handoff assigns the role)
        let r = call_owner_at(
            &pf,
            "board_create_task",
            &json!({"title": "spoof probe"}),
            &coder_tok,
        )
        .unwrap()
        .unwrap();
        let id = r.split(' ').nth(1).unwrap().to_string();
        call_owner_at(
            &pf,
            "board_handoff",
            &json!({"task_id": id, "to_role": "coder", "summary": "assigned to coder for the probe"}),
            &coder_tok,
        )
        .unwrap()
        .unwrap();
        call_owner_at(
            &pf,
            "board_update_task",
            &json!({"task_id": id, "action": "promote"}),
            &coder_tok,
        )
        .unwrap()
        .unwrap();
        call_owner_at(
            &pf,
            "board_update_task",
            &json!({"task_id": id, "action": "start"}),
            &coder_tok,
        )
        .unwrap()
        .unwrap();
        call_owner_at(
            &pf,
            "board_update_task",
            &json!({"task_id": id, "action": "finish"}),
            &coder_tok,
        )
        .unwrap()
        .unwrap();

        // coder holds coder_tok but claims to be the reviewer in the body:
        // the server must act as the token's role (coder) and refuse
        let (msg, ok) = match call_owner_at(
            &pf,
            "board_update_task",
            &json!({"task_id": id, "action": "approve", "role": "reviewer"}),
            &coder_tok,
        )
        .unwrap()
        {
            Ok(text) => (text, true),
            Err(e) => (e, false),
        };
        assert!(!ok, "self-approve via spoofed body role must fail: {msg}");
        assert!(
            msg.contains("cannot approve your own work"),
            "rejection must come from the token's role: {msg}"
        );
        // the real reviewer token still approves
        call_owner_at(
            &pf,
            "board_update_task",
            &json!({"task_id": id, "action": "approve"}),
            &reviewer_tok,
        )
        .unwrap()
        .unwrap();
        assert!(
            board
                .lock()
                .projection()
                .tasks
                .iter()
                .any(|t| t.id == id && t.status == super::super::TaskStatus::Done),
            "the genuine reviewer token must approve"
        );
        let _ = std::fs::remove_file(&pf);
    }

    /// v4: unknown role token gets a clear error, no execution.
    #[test]
    fn unknown_role_token_rejected() {
        let board = Arc::new(Mutex::new(BoardStore::open_memory()));
        let (handle, board) = handler_for(board);
        let pf = std::env::temp_dir().join(format!("aic-ipc-urole-{}", std::process::id()));
        let _ = std::fs::remove_file(&pf);
        serve_at(&pf, handle).expect("serve");

        let res = call_owner_at(
            &pf,
            "board_create_task",
            &json!({"title": "x"}),
            "not-a-real-token",
        )
        .unwrap();
        assert!(res.is_err(), "unknown role token must be rejected");
        assert!(
            board.lock().projection().tasks.is_empty(),
            "nothing executed for an unknown role token"
        );
        let _ = std::fs::remove_file(&pf);
    }

    /// v4: the port file (transport token inside) is owner-only on Unix.
    #[cfg(unix)]
    #[test]
    fn port_file_is_owner_only() {
        let board = Arc::new(Mutex::new(BoardStore::open_memory()));
        let (handle, _) = handler_for(board);
        let pf = std::env::temp_dir().join(format!("aic-ipc-mode-{}", std::process::id()));
        let _ = std::fs::remove_file(&pf);
        serve_at(&pf, handle).expect("serve");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&pf).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "port file must be 0600");
        let _ = std::fs::remove_file(&pf);
    }
}
