//! MCP board server (stdio). Agents get the board as MCP tools by spawning
//! this binary — the role identity comes from the `AI_COMMANDER_ROLE` env
//! (the per-session token: every mutation is authored by that role).
//!
//! Protocol: newline-delimited JSON-RPC 2.0 over stdin/stdout (MCP stdio):
//!   initialize → tools/list → tools/call
//! Tools: board_create_task, board_update_task, board_comment, board_handoff,
//!        board_read_inbox, board_ask_human — all hitting the same redb
//! event-sourced store the TUI and daemon use.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use std::io::{BufRead, Write};

use ai_commander::kanban::store::BoardStore;
use ai_commander::kanban::{Kanban, TaskAction};
use serde_json::{Value, json};

fn main() {
    // v4: the acting role comes from the role TOKEN the owner minted at
    // launch (AI_COMMANDER_BOARD_TOKEN) — never from an untrusted body
    // field. AI_COMMANDER_ROLE stays for direct mode (no owner).
    let role_token = std::env::var("AI_COMMANDER_BOARD_TOKEN").unwrap_or_default();
    let role = std::env::var("AI_COMMANDER_ROLE").unwrap_or_default();
    // Б1: if a board owner (TUI or daemon) is serving IPC, work as its
    // client — never open the redb store (exclusive lock) and never create
    // a second store. Direct mode only when nobody owns the board.
    let mut board = if ai_commander::kanban::ipc::owner_alive() {
        if role_token.is_empty() {
            eprintln!(
                "board-mcp: an owner is serving IPC but AI_COMMANDER_BOARD_TOKEN is not set;"
            );
            eprintln!("       the owner must launch this server with a minted role token.");
            std::process::exit(1);
        }
        None
    } else {
        match BoardStore::open_redb(&Kanban::path()) {
            Ok(b) => Some(b),
            Err(e) => {
                // redb takes an exclusive lock (review v2 Б1): the TUI or
                // daemon owns the DB but serves no IPC (older owner build).
                // Graceful exit with a clear message.
                eprintln!("board-mcp: cannot open the kanban store: {e:#}");
                eprintln!("hint: the board is likely open in ai-commander (TUI or daemon).");
                eprintln!(
                    "       stop the owner process or set AI_COMMANDER_KANBAN_DB to a separate copy."
                );
                std::process::exit(1);
            }
        }
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_owned) else {
            continue; // responses to server-initiated requests never arrive here
        };
        let id = v.get("id").cloned();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let result = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ai-commander-board", "version": env!("CARGO_PKG_VERSION")}
            })),
            "notifications/initialized" | "initialized" => {
                continue; // notification, no response
            }
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let call = match board.as_mut() {
                    Some(b) => call_tool(&name, &args, &role, b),
                    // Б1 IPC client: forward to the owner's store, acting
                    // as the role the token authorizes (v4)
                    None => {
                        match ai_commander::kanban::ipc::call_owner(&name, &args, &role_token) {
                            Some(Ok(text)) => Ok(text),
                            Some(Err(e)) => Err(e),
                            None => {
                                Err("board owner is not reachable (TUI/daemon stopped?)".into())
                            }
                        }
                    }
                };
                match call {
                    Ok(text) => Ok(json!({"content": [{"type": "text", "text": text}]})),
                    Err(e) => Ok(json!({
                        "content": [{"type": "text", "text": format!("error: {e}")}],
                        "isError": true
                    })),
                }
            }
            _ => Err(format!("unknown method {method}")),
        };
        let Some(id) = id else { continue };
        let msg = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": e}}),
        };
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }
}

fn tools() -> Value {
    json!([
        {"name": "board_create_task",
         "description": "Create a kanban task (Backlog). Optional depends_on: task ids that must finish first; parent_id: delegate from your task (depth is capped).",
         "inputSchema": {"type": "object", "properties": {
             "title": {"type": "string"},
             "depends_on": {"type": "array", "items": {"type": "string"}},
             "parent_id": {"type": "string"}},
             "required": ["title"]}},
        {"name": "board_update_task",
         "description": "Advance a task: action = promote|start|finish|approve|rework|cancel. You may only start/finish a task assigned to your role.",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"},
             "action": {"type": "string", "enum": ["promote","start","finish","approve","rework","cancel"]}},
             "required": ["task_id", "action"]}},
        {"name": "board_comment",
         "description": "Post a comment to the task thread (visible to humans and other agents).",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"}, "text": {"type": "string"}},
             "required": ["task_id", "text"]}},
        {"name": "board_handoff",
         "description": "Hand the task to another role. summary (>=10 chars) and open_questions form the handoff contract; the task returns to Ready for the new owner.",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"}, "to_role": {"type": "string"},
             "summary": {"type": "string"}, "open_questions": {"type": "string"}},
             "required": ["task_id", "to_role", "summary"]}},
        {"name": "board_artifact",
         "description": "Attach a concrete output of your work to the task: a file path, branch name, link or short text. Artifacts survive handoffs and are shown to the next owner.",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"},
             "kind": {"type": "string", "enum": ["file","branch","link","text"]},
             "value": {"type": "string"}},
             "required": ["task_id", "value"]}},
        {"name": "board_read_inbox",
         "description": "Read task thread entries newer than since_ms (epoch ms, default 0).",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"}, "since_ms": {"type": "integer"}},
             "required": ["task_id"]}},
        {"name": "board_ask_human",
         "description": "Escalate: mark the task NeedsHuman (only while it is in progress) with your question in text.",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"}, "text": {"type": "string"}},
             "required": ["task_id", "text"]}}
    ])
}

fn call_tool(
    name: &str,
    args: &Value,
    role: &str,
    board: &mut BoardStore,
) -> Result<String, String> {
    if role.is_empty() {
        return Err("no role token: set AI_COMMANDER_ROLE for this MCP server".into());
    }
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match name {
        "board_create_task" => {
            let title = str_arg("title");
            if title.trim().is_empty() {
                return Err("title is required".into());
            }
            let t = match args.get("parent_id").and_then(Value::as_str) {
                Some(parent) if !parent.is_empty() => board.add_subtask(parent, &title)?,
                _ => board.add_task(&title),
            };
            if let Some(deps) = args.get("depends_on").and_then(Value::as_array) {
                let ids: Vec<String> = deps
                    .iter()
                    .filter_map(|d| d.as_str().map(str::to_owned))
                    .collect();
                board.set_depends(&t.id, ids)?;
            }
            Ok(format!("created {} '{}' in Backlog", t.id, t.title))
        }
        "board_update_task" => {
            let id = str_arg("task_id");
            let action = match str_arg("action").as_str() {
                "promote" => TaskAction::Promote,
                "start" => TaskAction::Start,
                "finish" => TaskAction::Finish,
                "approve" => TaskAction::Approve,
                "rework" => TaskAction::Rework,
                "cancel" => TaskAction::Cancel,
                other => return Err(format!("unknown action {other}")),
            };
            // ownership: start/finish only by the assignee; approve/reject
            // NEVER by the assignee (review v2: agent can't self-approve)
            let t = board
                .projection()
                .tasks
                .iter()
                .find(|t| t.id == id)
                .cloned()
                .ok_or_else(|| format!("task {id} not found"))?;
            if matches!(action, TaskAction::Start | TaskAction::Finish)
                && t.role.as_deref().is_some_and(|r| r != role)
            {
                return Err(format!("task is assigned to {:?}, not to you", t.role));
            }
            if matches!(action, TaskAction::Approve | TaskAction::Rework)
                && t.role.as_deref() == Some(role)
            {
                return Err(
                    "you cannot approve your own work — a different role must review".into(),
                );
            }
            let t = board.transition(&id, action)?;
            Ok(format!("{} → {:?}", t.id, t.status))
        }
        "board_comment" => {
            let id = str_arg("task_id");
            let text = str_arg("text");
            if text.trim().is_empty() {
                return Err("text is required".into());
            }
            board.comment(&id, role, "comment", &text)?;
            Ok("comment posted".into())
        }
        "board_handoff" => {
            let t = board.handoff(
                &str_arg("task_id"),
                role,
                &str_arg("to_role"),
                &str_arg("summary"),
                &str_arg("open_questions"),
            )?;
            Ok(format!("handed off to {:?}; status {:?}", t.role, t.status))
        }
        "board_artifact" => {
            let t = board.add_artifact(
                &str_arg("task_id"),
                &str_arg("kind"),
                &str_arg("value"),
                role,
            )?;
            Ok(format!(
                "artifact attached to {} ({} total)",
                t.id,
                t.artifacts.len()
            ))
        }
        "board_read_inbox" => {
            let id = str_arg("task_id");
            let since = args.get("since_ms").and_then(Value::as_i64).unwrap_or(0);
            let inbox = board
                .read_inbox(&id, since)
                .ok_or_else(|| format!("task {id} not found"))?;
            if inbox.is_empty() {
                return Ok("inbox is empty".into());
            }
            Ok(inbox
                .iter()
                .map(|c| format!("[{}] {}: {}", c.kind, c.author, c.text.replace('\n', " ")))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "board_ask_human" => {
            let id = str_arg("task_id");
            board.comment(&id, role, "system", &str_arg("text"))?;
            board.transition(&id, TaskAction::Ask)?;
            Ok("escalated to NeedsHuman".into())
        }
        other => Err(format!("unknown tool {other}")),
    }
}
