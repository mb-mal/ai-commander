//! A2A gateway (review "Позже"): publish the board as an A2A agent —
//! `GET /.well-known/agent.json` returns the Agent Card; incoming A2A
//! `message/send` requests create board tasks. Minimal, file-free HTTP over
//! a std TcpListener, no external server dependency.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use super::store::BoardStore;

pub struct A2aGateway {
    pub port: u16,
}

/// Read the port of a running gateway (written by A2aGateway::spawn).
pub fn gateway_port() -> Option<u16> {
    let port_file = crate::kanban::Kanban::path().parent()?.join("board-port");
    let raw = std::fs::read_to_string(port_file).ok()?;
    raw.trim().parse().ok()
}

impl A2aGateway {
    /// Bind 127.0.0.1:0 and serve in background threads until dropped.
    pub fn spawn(board: std::sync::Arc<std::sync::Mutex<BoardStore>>) -> Result<A2aGateway> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| anyhow!("bind: {e}"))?;
        let port = listener.local_addr()?.port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let board = std::sync::Arc::clone(&board);
                std::thread::spawn(move || handle(stream, board));
            }
        });
        // write the port so board-mcp can find the proxy when redb is locked
        let port_file = crate::kanban::Kanban::path()
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("board-port");
        let _ = std::fs::write(&port_file, port.to_string());
        Ok(A2aGateway { port })
    }

    pub fn agent_card_url(&self) -> String {
        format!("http://127.0.0.1:{}/.well-known/agent.json", self.port)
    }
}

fn handle(mut stream: TcpStream, board: std::sync::Arc<std::sync::Mutex<BoardStore>>) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut buf = BufReader::new(reader);
    let mut request_line = String::new();
    if buf.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // read headers for content-length
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if buf.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = buf.read_exact(&mut body);
    }

    let (status, payload) = route(&method, &path, &body, &board);
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    // half-close: signal EOF to the client, then wait for it to finish reading
    // (a full Both-shutdown races the client's read on Windows → WSAECONNRESET)
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink);
}

fn route(
    method: &str,
    path: &str,
    body: &[u8],
    board: &std::sync::Arc<std::sync::Mutex<BoardStore>>,
) -> (&'static str, Value) {
    match (method, path) {
        ("GET", "/.well-known/agent.json") => ("200 OK", agent_card()),
        // board-mcp proxy: POST /board {tool: "board.create_task", args: {...}}
        ("POST", "/board") => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return ("400 Bad Request", json!({"error": "invalid JSON"}));
            };
            let tool = v
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = v.get("args").cloned().unwrap_or(json!({}));
            let role = v
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut b = board.lock().unwrap();
            match crate::kanban::board_tool_call(&tool, &args, &role, &mut b) {
                Ok(text) => ("200 OK", json!({"result": text})),
                Err(e) => ("200 OK", json!({"error": e})),
            }
        }
        ("POST", "/") => {
            let Ok(v) = serde_json::from_slice::<Value>(body) else {
                return ("400 Bad Request", json!({"error": "invalid JSON"}));
            };
            // A2A message/send: params.message.parts[].text
            let text = v
                .pointer("/params/message/parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                return ("400 Bad Request", json!({"error": "no message text"}));
            }
            let mut b = board.lock().unwrap();
            let t = b.add_task(&text);
            (
                "200 OK",
                json!({"result": {"taskId": t.id, "status": t.status.to_string()}}),
            )
        }
        _ => ("404 Not Found", json!({"error": "not found"})),
    }
}

fn agent_card() -> Value {
    json!({
        "name": "ai-commander-board",
        "description": "Kanban board driven by AI agents: send a task as a message, it becomes a board task",
        "url": "http://127.0.0.1/",
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {"streaming": false},
        "defaultInputModes": ["text"],
        "defaultOutputModes": ["text"],
        "skills": [{"id": "create_task", "name": "Create task", "description": "Send a message; it becomes a board task"}]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect_retry(port: u16) -> std::net::TcpStream {
        // the listener thread may not have started yet on a loaded test runner
        for _ in 0..50 {
            if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("cannot connect to a2a gateway on port {port}");
    }

    /// HTTP round-trip with retry: Windows can reset the connection during
    /// the server's half-close race; retrying the request is safe (idempotent
    /// routes only in tests).
    fn roundtrip(port: u16, request: String) -> (u16, Value) {
        for attempt in 0..5 {
            let mut s = connect_retry(port);
            if write!(s, "{request}").is_err() {
                continue;
            }
            let _ = s.flush();
            let mut buf = String::new();
            match std::io::Read::read_to_string(&mut s, &mut buf) {
                Ok(_) => {
                    let status: u16 = buf
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("");
                    if let Ok(v) = serde_json::from_str(body) {
                        return (status, v);
                    }
                }
                Err(_) if attempt < 4 => continue, // WSAECONNRESET race
                Err(e) => panic!("read failed after retries: {e}"),
            }
        }
        panic!("a2a roundtrip exhausted retries");
    }

    fn post(port: u16, body: &str) -> (u16, Value) {
        roundtrip(
            port,
            format!(
                "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    fn get(port: u16, path: &str) -> (u16, Value) {
        roundtrip(
            port,
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
        )
    }

    #[test]
    fn agent_card_and_task_creation() {
        let board = BoardStore::open_memory();
        let gw = A2aGateway::spawn(std::sync::Arc::new(std::sync::Mutex::new(board))).unwrap();

        let (status, card) = get(gw.port, "/.well-known/agent.json");
        assert_eq!(status, 200);
        assert_eq!(card["name"], "ai-commander-board");

        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"message/send",
            "params":{"message":{"parts":[{"type":"text","text":"integrate the payment API"}]}}}"#;
        let (status, resp) = post(gw.port, msg);
        assert_eq!(status, 200);
        let id = resp["result"]["taskId"].as_str().unwrap().to_string();
        assert!(id.starts_with("t-"), "{resp}");

        // bad payload -> 400
        let (status, _) = post(gw.port, "{\"x\":1}");
        assert_eq!(status, 400);
    }
}
