//! Fake ACP agent for conformance tests (review v3 Б2, extended in v4).
//!
//! Speaks the Agent Client Protocol over stdio like a real agent, but
//! validates EVERY incoming client message against the schema types —
//! strictly: the params must deserialize into the typed request AND
//! re-serialize to the same JSON (no extra/off-schema fields). The old
//! hand-rolled payloads (e.g. `session/cancel` with a stray `reason`) fail
//! here, which is exactly what the tests in tests/acp_fake_agent.rs check.
//!
//! v4: before answering `session/prompt`, the agent sends a
//! `session/request_permission` request with a STRING jsonrpc id and all
//! three option kinds, and finishes the turn only after the client's typed
//! response (selected option / cancelled) — driving the permission
//! scenarios end to end. The read loop is a single state machine: nested
//! stdin locks deadlock on Windows.
//!
//! Results are appended to the file named by FAKE_ACP_LOG:
//!   OK <method>            — message is schema-canonical
//!   FAIL <method> <error>  — validation error
//!   PERM selected <id> / PERM cancelled — the client's permission outcome

use std::io::{BufRead, Write};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PermissionOption, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification, SessionUpdate, TextContent, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::{Value, json};

fn log_line(line: &str) {
    if let Ok(path) = std::env::var("FAKE_ACP_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Strict schema check: deserialize into the typed params and require the
/// round-trip to reproduce the payload exactly (extra fields = off-schema).
fn validate<T>(method: &str, params: Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    match serde_json::from_value::<T>(params.clone()) {
        Err(e) => log_line(&format!("FAIL {method} deserialize: {e}")),
        Ok(typed) => {
            let round = serde_json::to_value(&typed).unwrap_or(Value::Null);
            if round != params {
                log_line(&format!(
                    "FAIL {method} not schema-canonical: sent {params} vs schema {round}"
                ));
            } else {
                log_line(&format!("OK {method}"));
            }
        }
    }
}

/// A prompt turn paused on the open permission request.
struct AwaitingPerm {
    prompt_id: Value,
    session_id: String,
    echo: String,
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut next_session = 0u32;
    let mut awaiting: Option<AwaitingPerm> = None;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let id = v.get("id").cloned();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let is_request = id.is_some();

        // the client answered our permission request → finish the turn
        if method.is_empty() && id == Some(Value::String("perm-1".into())) {
            if let Some(a) = awaiting.take() {
                let mut outcome_text = String::new();
                match serde_json::from_value::<RequestPermissionResponse>(
                    v.get("result").cloned().unwrap_or(Value::Null),
                ) {
                    Ok(resp) => match resp.outcome {
                        RequestPermissionOutcome::Selected(sel) => {
                            outcome_text = format!("PERM selected {}", sel.option_id);
                            log_line(&outcome_text);
                        }
                        RequestPermissionOutcome::Cancelled => {
                            outcome_text = "PERM cancelled".into();
                            log_line(&outcome_text);
                        }
                        _ => log_line("FAIL unknown outcome variant"),
                    },
                    Err(e) => log_line(&format!("FAIL perm response not schema: {e}")),
                }
                let stop = if outcome_text.contains("cancelled") {
                    "cancelled"
                } else {
                    "end_turn"
                };
                let note = SessionNotification::new(
                    a.session_id.clone(),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(format!(
                            "echo: {} — {} — DONE: fake agent replied",
                            a.echo, outcome_text
                        )),
                    ))),
                );
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc": "2.0", "method": "session/update",
                           "params": serde_json::to_value(&note).unwrap_or(Value::Null)})
                );
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": a.prompt_id,
                           "result": {"stopReason": stop}})
                );
                let _ = stdout.flush();
            }
            continue;
        }

        match method {
            "initialize" => {
                validate::<InitializeRequest>("initialize", params);
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc": "2.0", "id": id.unwrap(),
                        "result": {"protocolVersion": 1, "agentCapabilities": {}}
                    })
                );
                let _ = stdout.flush();
            }
            "session/new" => {
                validate::<NewSessionRequest>("session/new", params);
                next_session += 1;
                let sid = format!("ses_fake_{next_session}");
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc": "2.0", "id": id.unwrap(),
                        "result": {"sessionId": sid, "configOptions": []}
                    })
                );
                let _ = stdout.flush();
            }
            "session/load" => {
                validate::<LoadSessionRequest>("session/load", params.clone());
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("ses_fake_load")
                    .to_string();
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": id.unwrap(), "result": {"sessionId": sid}})
                );
                let _ = stdout.flush();
            }
            "session/prompt" => {
                validate::<PromptRequest>("session/prompt", params.clone());
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("ses_fake_1")
                    .to_string();
                let echo = params
                    .get("prompt")
                    .and_then(Value::as_array)
                    .and_then(|b| b.first())
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                // v4 Б2: ask permission (typed, STRING id) before answering;
                // the turn completes when the client replies (see above)
                let mut fields = ToolCallUpdateFields::default();
                fields.title = Some("rm -rf probe".into());
                let req = RequestPermissionRequest::new(
                    sid,
                    ToolCallUpdate::new("tc_probe_1", fields),
                    vec![
                        PermissionOption::new(
                            "allow_once",
                            "Allow",
                            PermissionOptionKind::AllowOnce,
                        ),
                        PermissionOption::new(
                            "allow_always",
                            "Always allow",
                            PermissionOptionKind::AllowAlways,
                        ),
                        PermissionOption::new(
                            "reject_once",
                            "Reject",
                            PermissionOptionKind::RejectOnce,
                        ),
                    ],
                );
                awaiting = Some(AwaitingPerm {
                    prompt_id: id.clone().unwrap_or(Value::Null),
                    session_id: params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("ses_fake_1")
                        .to_string(),
                    echo,
                });
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission",
                           "params": serde_json::to_value(&req).unwrap_or(Value::Null)})
                );
                let _ = stdout.flush();
            }
            "session/cancel" => {
                validate::<CancelNotification>("session/cancel", params);
                // notification: no response; a pending turn is finished by
                // the client's `cancelled` permission replies
            }
            _ => {
                if is_request {
                    let _ = writeln!(
                        stdout,
                        "{}",
                        json!({"jsonrpc": "2.0", "id": id.unwrap(),
                               "error": {"code": -32601, "message": format!("unknown method {method}")}})
                    );
                    let _ = stdout.flush();
                }
            }
        }
    }
}
