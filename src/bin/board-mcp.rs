//! MCP board server (stdio). Agents get the board as MCP tools by spawning
//! this binary — the role identity comes from the `AI_COMMANDER_ROLE` env
//! (the per-session token: every mutation is authored by that role).
//!
//! Protocol: newline-delimited JSON-RPC 2.0 over stdin/stdout (MCP stdio):
//!   initialize → tools/list → tools/call
//! Tools: board.create_task, board.update_task, board.comment, board.handoff,
//!        board.read_inbox, board.ask_human — all hitting the same redb
//! event-sourced store the TUI and daemon use.

use std::io::{BufRead, Write};

use ai_commander::kanban::store::BoardStore;
use ai_commander::kanban::{Kanban, TaskAction};
use serde_json::{Value, json};

fn main() {
    let role = std::env::var("AI_COMMANDER_ROLE").unwrap_or_default();
    let mut board = BoardStore::open_redb(&Kanban::path()).expect("open kanban store");
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
                match call_tool(&name, &args, &role, &mut board) {
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
        {"name": "board.create_task",
         "description": "Create a kanban task (Backlog). Optional depends_on: task ids that must finish first.",
         "inputSchema": {"type": "object", "properties": {
             "title": {"type": "string"},
             "depends_on": {"type": "array", "items": {"type": "string"}}},
             "required": ["title"]}},
        {"name": "board.update_task",
         "description": "Advance a task: action = promote|start|finish|approve|rework|cancel. You may only start/finish a task assigned to your role.",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"},
             "action": {"type": "string", "enum": ["promote","start","finish","approve","rework","cancel"]}},
             "required": ["task_id", "action"]}},
        {"name": "board.comment",
         "description": "Post a comment to the task thread (visible to humans and other agents).",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"}, "text": {"type": "string"}},
             "required": ["task_id", "text"]}},
        {"name": "board.handoff",
         "description": "Hand the task to another role. summary (>=10 chars) and open_questions form the handoff contract; the task returns to Ready for the new owner.",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"}, "to_role": {"type": "string"},
             "summary": {"type": "string"}, "open_questions": {"type": "string"}},
             "required": ["task_id", "to_role", "summary"]}},
        {"name": "board.read_inbox",
         "description": "Read task thread entries newer than since_ms (epoch ms, default 0).",
         "inputSchema": {"type": "object", "properties": {
             "task_id": {"type": "string"}, "since_ms": {"type": "integer"}},
             "required": ["task_id"]}},
        {"name": "board.ask_human",
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
        "board.create_task" => {
            let title = str_arg("title");
            if title.trim().is_empty() {
                return Err("title is required".into());
            }
            let t = board.add_task(&title);
            if let Some(deps) = args.get("depends_on").and_then(Value::as_array) {
                let ids: Vec<String> = deps
                    .iter()
                    .filter_map(|d| d.as_str().map(str::to_owned))
                    .collect();
                board.set_depends(&t.id, ids)?;
            }
            Ok(format!("created {} '{}' in Backlog", t.id, t.title))
        }
        "board.update_task" => {
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
            // ownership: start/finish only by the assignee
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
            let t = board.transition(&id, action)?;
            Ok(format!("{} → {:?}", t.id, t.status))
        }
        "board.comment" => {
            let id = str_arg("task_id");
            let text = str_arg("text");
            if text.trim().is_empty() {
                return Err("text is required".into());
            }
            board.comment(&id, role, "comment", &text)?;
            Ok("comment posted".into())
        }
        "board.handoff" => {
            let t = board.handoff(
                &str_arg("task_id"),
                role,
                &str_arg("to_role"),
                &str_arg("summary"),
                &str_arg("open_questions"),
            )?;
            Ok(format!("handed off to {:?}; status {:?}", t.role, t.status))
        }
        "board.read_inbox" => {
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
        "board.ask_human" => {
            let id = str_arg("task_id");
            board.comment(&id, role, "system", &str_arg("text"))?;
            board.transition(&id, TaskAction::Ask)?;
            Ok("escalated to NeedsHuman".into())
        }
        other => Err(format!("unknown tool {other}")),
    }
}
