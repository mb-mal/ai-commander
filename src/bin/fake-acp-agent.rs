//! Fake ACP agent for conformance tests (review v3 Б2).
//!
//! Speaks the Agent Client Protocol over stdio like a real agent, but
//! validates EVERY incoming client message against the schema types —
//! strictly: the params must deserialize into the typed request AND
//! re-serialize to the same JSON (no extra/off-schema fields). The old
//! hand-rolled payloads (e.g. `session/cancel` with a stray `reason`) fail
//! here, which is exactly what the test in tests/acp_fake_agent.rs checks.
//!
//! Results are appended to the file named by FAKE_ACP_LOG:
//!   OK <method>            — message is schema-canonical
//!   FAIL <method> <error>  — validation error (also answered as JSON-RPC error)

use std::io::{BufRead, Write};

use agent_client_protocol::schema::v1::{
    CancelNotification, InitializeRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    SessionNotification, SessionUpdate,
};
use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};
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

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut next_session = 0u32;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let id = v.get("id").cloned();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let is_request = id.is_some();

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
                // echo the first text block back as an agent_message_chunk
                let echo = params
                    .get("prompt")
                    .and_then(Value::as_array)
                    .and_then(|b| b.first())
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let note = SessionNotification::new(
                    sid.clone(),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(format!("echo: {echo}DONE: fake agent replied")),
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
                    json!({"jsonrpc": "2.0", "id": id.unwrap(),
                           "result": {"stopReason": "end_turn"}})
                );
                let _ = stdout.flush();
            }
            "session/cancel" => {
                validate::<CancelNotification>("session/cancel", params);
                // notification: no response
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
