//! Fake ACP agent for conformance tests (review v3 Б2, extended in v4/v5).
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
//! v5: permission requests are per-session (`perm-<sessionId>` ids), so
//! two concurrent sessions each keep their own open question; with
//! FAKE_ACP_MALFORMED_PERM=1 the agent sends an intentionally malformed
//! permission request and expects a -32602 error reply (not silence).
//!
//! Results are appended to the file named by FAKE_ACP_LOG:
//!   OK <method> / FAIL <method> <error> — schema validation
//!   PERM selected <id> <sid> / PERM cancelled <sid> — permission outcomes
//!   MALFORMED <code> — reply to the malformed permission request

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use std::collections::HashMap;
use std::io::{BufRead, Write};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PermissionOption, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification, SessionUpdate, TextContent, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind,
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
/// Exception: `clientCapabilities.session` — the compaction RFD (Preview)
/// is not in schema crate 1.1.0, so the client legitimately advertises it
/// as an extension; it is stripped before the canonical comparison.
fn validate<T>(method: &str, params: Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    match serde_json::from_value::<T>(params.clone()) {
        Err(e) => log_line(&format!("FAIL {method} deserialize: {e}")),
        Ok(typed) => {
            let round = serde_json::to_value(&typed).unwrap_or(Value::Null);
            let mut params = params;
            if method == "initialize" {
                if let Some(cc) = params
                    .get_mut("clientCapabilities")
                    .and_then(|c| c.as_object_mut())
                {
                    cc.remove("session");
                }
            }
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

/// A prompt turn paused on its open permission request.
struct AwaitingPerm {
    prompt_id: Value,
    echo: String,
}

fn finish_turn(stdout: &mut std::io::Stdout, sid: &str, a: &AwaitingPerm, outcome_text: &str) {
    let stop = if outcome_text.contains("cancelled") {
        "cancelled"
    } else {
        "end_turn"
    };
    let note = SessionNotification::new(
        sid.to_string(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            format!(
                "echo: {} — {} — DONE: fake agent replied",
                a.echo, outcome_text
            ),
        )))),
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
        json!({"jsonrpc": "2.0", "id": a.prompt_id.clone(),
               "result": {"stopReason": stop}})
    );
    // К1/К2 (env-gated): report usage and run an RFD compaction sequence —
    // usage_update, compaction_update in_progress, 3 summary chunks,
    // compaction_update completed, and the post-compaction usage
    if std::env::var("FAKE_ACP_COMPACTION").ok().as_deref() == Some("1") {
        let mut upd = |body: Value| {
            let _ = writeln!(
                stdout,
                "{}",
                json!({"jsonrpc": "2.0", "method": "session/update", "params": body})
            );
        };
        upd(json!({"sessionId": sid, "update": {
            "sessionUpdate": "usage_update", "used": 53000, "size": 200000}}));
        upd(json!({"sessionId": sid, "update": {
            "sessionUpdate": "compaction_update",
            "compactionId": "c-0042", "status": "in_progress"}}));
        for chunk in ["Summarizing ", "the ", "old ", "context..."] {
            upd(json!({"sessionId": sid, "update": {
                "sessionUpdate": "compaction_summary_chunk",
                "compactionId": "c-0042", "delta": chunk}}));
        }
        upd(json!({"sessionId": sid, "update": {
            "sessionUpdate": "compaction_update",
            "compactionId": "c-0042", "status": "completed"}}));
        upd(json!({"sessionId": sid, "update": {
            "sessionUpdate": "usage_update", "used": 41000, "size": 200000}}));
    }
    let _ = stdout.flush();
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut next_session = 0u32;
    // v5: per-session open questions (id = "perm-<sessionId>")
    let mut awaiting: HashMap<String, AwaitingPerm> = HashMap::new();
    let malformed = std::env::var("FAKE_ACP_MALFORMED_PERM").ok().as_deref() == Some("1");

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let id = v.get("id").cloned();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let is_request = id.is_some();

        // a reply to one of our permission requests → finish that session's turn
        if method.is_empty() {
            if let Some(idstr) = id.as_ref().and_then(Value::as_str) {
                if let Some(sid) = idstr.strip_prefix("perm-bad-") {
                    // v5: malformed probe — the client must answer -32602
                    let sid = sid.to_string();
                    {
                        // v5: malformed probe — the client must answer -32602
                        let code = v
                            .get("error")
                            .and_then(|e| e.get("code"))
                            .and_then(Value::as_i64);
                        log_line(&format!(
                            "MALFORMED {}",
                            code.map(|c| c.to_string()).unwrap_or_else(|| "none".into())
                        ));
                        if let Some(a) = awaiting.remove(&sid) {
                            finish_turn(&mut stdout, &sid, &a, "malformed answered");
                        }
                    }
                } else if let Some(sid) = idstr.strip_prefix("perm-") {
                    let sid = sid.to_string();
                    if let Some(a) = awaiting.remove(&sid) {
                        let mut outcome_text = String::new();
                        match serde_json::from_value::<RequestPermissionResponse>(
                            v.get("result").cloned().unwrap_or(Value::Null),
                        ) {
                            Ok(resp) => match resp.outcome {
                                RequestPermissionOutcome::Selected(sel) => {
                                    outcome_text = format!("PERM selected {} {sid}", sel.option_id);
                                    log_line(&outcome_text);
                                }
                                RequestPermissionOutcome::Cancelled => {
                                    outcome_text = format!("PERM cancelled {sid}");
                                    log_line(&outcome_text);
                                }
                                _ => log_line("FAIL unknown outcome variant"),
                            },
                            Err(e) => log_line(&format!("FAIL perm response not schema: {e}")),
                        }
                        finish_turn(&mut stdout, &sid, &a, &outcome_text);
                    }
                }
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
                        "jsonrpc": "2.0", "id": id.clone().expect("request id"),
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
                        "jsonrpc": "2.0", "id": id.clone().expect("request id"),
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
                    json!({"jsonrpc": "2.0", "id": id.clone().expect("request id"), "result": {"sessionId": sid}})
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

                awaiting.insert(
                    sid.clone(),
                    AwaitingPerm {
                        prompt_id: id.clone().unwrap_or(Value::Null),
                        echo,
                    },
                );

                if malformed {
                    // v5: intentionally off-schema params — the client must
                    // answer -32602 instead of silence
                    let _ = writeln!(
                        stdout,
                        "{}",
                        json!({"jsonrpc": "2.0", "id": format!("perm-bad-{sid}"),
                               "method": "session/request_permission",
                               "params": {"sessionId": sid, "options": "not-an-array"}})
                    );
                    let _ = stdout.flush();
                    continue;
                }

                // v4 Б2: ask permission (typed, STRING id) before answering;
                // the turn completes when the client replies (see above)
                let mut fields = ToolCallUpdateFields::default();
                fields.title = Some("rm -rf probe".into());
                // v5 issues #1: real kind + raw command so the client's
                // policy engine sees what the agent wants to do
                fields.kind = Some(ToolKind::Execute);
                fields.raw_input = Some(serde_json::json!({"command": "rm -rf probe"}));
                let req = RequestPermissionRequest::new(
                    sid.clone(),
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
                let _ = writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": format!("perm-{sid}"),
                           "method": "session/request_permission",
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
                        json!({"jsonrpc": "2.0", "id": id.clone().expect("request id"),
                               "error": {"code": -32601, "message": format!("unknown method {method}")}})
                    );
                    let _ = stdout.flush();
                }
            }
        }
    }
}
