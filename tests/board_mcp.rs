//! Integration test for the board-mcp MCP server: spawn the binary, speak
//! JSON-RPC over stdio like a real MCP client, verify the tools work against
//! a throwaway redb store.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use ai_commander::kanban::store::BoardStore;

struct McpClient {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn spawn(db: &PathBuf, role: &str) -> McpClient {
        let mut child = Command::new(env!("CARGO_BIN_EXE_board-mcp"))
            .env("AI_COMMANDER_KANBAN_DB", db)
            .env("AI_COMMANDER_ROLE", role)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn board-mcp");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        McpClient {
            child,
            stdin,
            reader,
            next_id: 0,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params
        });
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read response");
        serde_json::from_str(&line).expect("parse response")
    }

    fn init(&mut self) {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        );
        self.notify_initialized();
    }

    fn notify_initialized(&mut self) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        });
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn call(&mut self, name: &str, args: serde_json::Value) -> (String, bool) {
        let resp = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        );
        // rmcp surfaces tool errors as JSON-RPC errors; success carries content
        if let Some(e) = resp.get("error") {
            let msg = e["message"].as_str().unwrap_or_default().to_string();
            return (msg, true);
        }
        let result = resp["result"].clone();
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (text, result["isError"].as_bool().unwrap_or(false))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tmpdb(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("aic-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("kanban.redb")
}

#[test]
fn mcp_board_full_dialog() {
    let db = tmpdb("dialog");
    let mut c = McpClient::spawn(&db, "coder");

    // initialize + tools/list. v5 #8: the server is on the official rmcp
    // SDK — a current client must negotiate a CURRENT protocol version
    // (the hand-rolled one was stuck on 2024-11-05 forever)
    let init = c.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "ai-commander-board");
    let negotiated = init["result"]["protocolVersion"].as_str().unwrap_or("");
    assert!(
        negotiated >= "2025-06-18",
        "the SDK must negotiate a current protocol version, got {negotiated}"
    );
    // handshake completion notification (rmcp expects it before calls)
    c.notify_initialized();
    let tools = c.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in &names {
        assert!(
            !n.contains('.'),
            "tool names must fit [a-zA-Z0-9_-] (no dots): {n}"
        );
    }
    for expected in [
        "board_create_task",
        "board_update_task",
        "board_comment",
        "board_handoff",
        "board_read_inbox",
        "board_ask_human",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }

    // create + advance + comment + inbox
    let (text, err) = c.call(
        "board_create_task",
        serde_json::json!({"title": "write the report"}),
    );
    assert!(!err, "create failed: {text}");
    let task_id = text.split(' ').nth(1).unwrap().to_string();
    assert!(task_id.starts_with("t-"), "bad id in {text}");

    let (text, err) = c.call(
        "board_update_task",
        serde_json::json!({"task_id": task_id, "action": "promote"}),
    );
    assert!(!err, "promote failed: {text}");
    assert!(text.contains("Ready"), "{text}");

    let (_, err) = c.call(
        "board_comment",
        serde_json::json!({"task_id": task_id, "text": "starting now"}),
    );
    assert!(!err);

    let (inbox, err) = c.call("board_read_inbox", serde_json::json!({"task_id": task_id}));
    assert!(!err);
    assert!(inbox.contains("starting now"), "{inbox}");

    // handoff contract: too-short summary is rejected, proper one works
    let (text, err) = c.call(
        "board_handoff",
        serde_json::json!({
        "task_id": task_id, "to_role": "reviewer", "summary": "short"}),
    );
    assert!(err, "handoff without a summary must fail: {text}");

    let (text, err) = c.call(
        "board_handoff",
        serde_json::json!({
        "task_id": task_id, "to_role": "reviewer",
        "summary": "report draft written, needs a review pass", "open_questions": "tone ok?"}),
    );
    assert!(!err, "handoff failed: {text}");

    // state check straight through the store
    drop(c);
    let board = BoardStore::open_redb(&db).unwrap();
    let t = &board.projection().tasks[0];
    assert_eq!(t.role.as_deref(), Some("reviewer"));
    assert!(
        t.comments
            .iter()
            .any(|c| c.kind == "handoff" && c.text.contains("tone ok?"))
    );
    drop(board);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn mcp_requires_role_token_and_ownership() {
    let db = tmpdb("role");

    // no role token -> mutations refused
    let mut anon = McpClient::spawn(&db, "");
    anon.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    anon.notify_initialized();
    let (text, err) = anon.call("board_create_task", serde_json::json!({"title": "x"}));
    assert!(err && text.contains("role token"), "{text}");
    drop(anon);

    // coder creates a task assigned to reviewer; coder cannot start it
    let mut coder = McpClient::spawn(&db, "coder");
    coder.init();
    let (text, _) = coder.call("board_create_task", serde_json::json!({"title": "owned"}));
    let id = text.split(' ').nth(1).unwrap().to_string();
    let (text, _) = coder.call(
        "board_handoff",
        serde_json::json!({
        "task_id": id, "to_role": "reviewer", "summary": "please own this task now"}),
    );
    assert!(!text.contains("error"), "{text}");
    let (text, err) = coder.call(
        "board_update_task",
        serde_json::json!({"task_id": id, "action": "promote"}),
    );
    assert!(!err, "{text}");
    let (text, err) = coder.call(
        "board_update_task",
        serde_json::json!({"task_id": id, "action": "start"}),
    );
    assert!(err, "ownership must block start: {text}");
    drop(coder);

    // reviewer can start it
    let mut reviewer = McpClient::spawn(&db, "reviewer");
    reviewer.init();
    let (text, err) = reviewer.call(
        "board_update_task",
        serde_json::json!({"task_id": id, "action": "start"}),
    );
    assert!(!err, "assignee must be able to start: {text}");
    drop(reviewer);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}
