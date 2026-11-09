//! Б1: single board owner + IPC. When an owner process (TUI/daemon shape:
//! a BoardStore behind Arc<Mutex> serving IPC) is alive, board-mcp must
//! work as its CLIENT: tool calls land in the owner's store, and board-mcp
//! must never open the redb file itself (exclusive lock, and the old
//! failure mode — a second store with duplicated task ids).

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use ai_commander::kanban::store::BoardStore;

fn send(
    child: &mut std::process::Child,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    use std::io::BufReader;
    let msg = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }
    let mut line = String::new();
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    reader.read_line(&mut line).expect("read response");
    let stdout = reader.into_inner();
    child.stdout.replace(stdout);
    serde_json::from_str(&line).expect("parse response")
}

#[test]
fn board_mcp_uses_owner_store_over_ipc() {
    // 1) become the board owner: IPC server over a temp port file
    let dir = std::env::temp_dir().join(format!("aic-ipc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let port_file = dir.join("board.port");
    // SAFETY: env override set before spawning the child; single test binary
    unsafe { std::env::set_var("AI_COMMANDER_BOARD_PORT", &port_file) };
    let owner = Arc::new(Mutex::new(BoardStore::open_memory()));
    let port = ai_commander::kanban::ipc::serve_board(owner.clone()).expect("serve");
    assert!(port > 0);
    // v4: mint the role token the child must present
    let role_token = ai_commander::kanban::ipc::issue_role_token("coder");

    // the redb path board-mcp would open in direct mode — it must NOT be
    // touched while the owner serves IPC
    let db: PathBuf = dir.join("kanban.redb");

    // 2) spawn board-mcp like an agent harness would (role token via env)
    let mut child = Command::new(env!("CARGO_BIN_EXE_board-mcp"))
        .env("AI_COMMANDER_KANBAN_DB", &db)
        .env("AI_COMMANDER_ROLE", "coder")
        .env("AI_COMMANDER_BOARD_TOKEN", &role_token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn board-mcp");

    let init = send(
        &mut child,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "ai-commander-board");

    let created = send(
        &mut child,
        2,
        "tools/call",
        serde_json::json!({"name": "board_create_task",
                           "arguments": {"title": "created via the ipc owner"}}),
    );
    let text = created["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        created["result"]["isError"].is_null(),
        "create over IPC failed: {text}"
    );
    assert!(text.contains("Backlog"), "created: {text}");

    let listed = send(
        &mut child,
        3,
        "tools/call",
        serde_json::json!({"name": "board_artifact",
                           "arguments": {"task_id": "t-0001", "kind": "file", "value": "src/x.rs"}}),
    );
    let atext = listed["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        listed["result"]["isError"].is_null(),
        "artifact over IPC failed: {atext}"
    );

    let _ = child.kill();
    let _ = child.wait();
    ai_commander::kanban::ipc::cleanup();

    // 3) the task and artifact live in the OWNER's store — one store,
    //    no duplicates, no second projection
    {
        let b = owner.lock().unwrap();
        let t = b
            .projection()
            .tasks
            .iter()
            .find(|t| t.title == "created via the ipc owner")
            .expect("task must exist in the owner's store (Б1)");
        assert_eq!(t.artifacts.len(), 1, "artifact reached the owner's store");
        assert_eq!(t.artifacts[0].by, "coder");
    }

    // 4) board-mcp never opened the redb file (IPC mode, not direct mode)
    assert!(
        !db.exists(),
        "board-mcp opened its own redb store while an owner was serving (Б1 violation)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
