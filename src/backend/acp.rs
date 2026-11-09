//! ACP backend — Agent Client Protocol over stdio via `opencode acp`.
//!
//! The architecturally-correct harness connection: one long-lived agent
//! process, typed ACP messages (schema crate), real cancellation
//! (`session/cancel`) and real permission requests
//! (`session/request_permission`). JSON-RPC framing is line-based over the
//! child's stdio; request responses are routed through per-id channels.
//!
//! Flow: initialize → session/new (first turn; the real sessionId is
//! announced via `SessionCreated`) → session/prompt → session/update
//! notifications stream agent_message_chunk / agent_thought_chunk / tool_call
//! → the prompt response (stopReason) finishes the turn.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::{
    AgentInfo, AttachmentMeta, Backend, BackendEvent, ChatMsg, MAX_ATTACHMENT_BYTES, ModelInfo,
    PartKind, RenderPart, Role, SendOpts, SessionInfo, kill_tree,
};
use crate::config::HarnessCfg;

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, EnvVariable, ImageContent, InitializeRequest,
    LoadSessionRequest, McpServer, McpServerStdio, NewSessionRequest, PermissionOption,
    PermissionOptionKind, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    TextContent, ToolCallStatus, ToolKind,
};

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct StoredMsg {
    role: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct StoredSession {
    native_id: String,
    title: String,
    updated_ms: i64,
    msgs: Vec<StoredMsg>,
}

#[derive(Debug, Default, Serialize, serde::Deserialize)]
struct AcpStore {
    sessions: Vec<StoredSession>,
}

/// A server→client permission request awaiting the user's answer.
/// v4 Б2: options keep the schema kind so the reply can match exactly, and
/// the jsonrpc id stays raw (numbers AND strings are legal request ids).
#[derive(Clone)]
struct PendingPermission {
    jsonrpc_id: Value,
    /// v5: only the owning session's abort may cancel this request
    session_id: String,
    options: Vec<PermissionOption>,
}

pub struct AcpBackend {
    cfg: HarnessCfg,
    events: UnboundedSender<BackendEvent>,
    store_path: PathBuf,
    store: Arc<Mutex<AcpStore>>,
    connected: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<ChildStdin>>>,
    next_id: Arc<AtomicU64>,
    /// jsonrpc id -> channel for the waiting caller
    pending: Arc<Mutex<HashMap<u64, mpsc::SyncSender<Value>>>>,
    /// real sessionId -> id the UI tracks (pending-* on the first turn)
    route: Arc<Mutex<HashMap<String, String>>>,
    /// sent id -> accumulated assistant text of the current turn
    acc: Arc<Mutex<HashMap<String, String>>>,
    permissions: Arc<Mutex<Vec<PendingPermission>>>,
    /// model list captured from the session/new response (configOptions)
    models: Arc<Mutex<Vec<ModelInfo>>>,
    child: Mutex<Option<Child>>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn persist_atomic(path: &std::path::Path, store: &AcpStore) {
    let Ok(raw) = serde_json::to_vec_pretty(store) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &raw).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

impl AcpBackend {
    pub fn new(cfg: HarnessCfg, events: UnboundedSender<BackendEvent>) -> Self {
        let store_path =
            crate::config::config_dir().join(format!("harness-{}-sessions.json", cfg.id));
        let store = std::fs::read(&store_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        AcpBackend {
            cfg,
            events,
            store_path,
            store: Arc::new(Mutex::new(store)),
            connected: Arc::new(AtomicBool::new(false)),
            writer: Arc::new(Mutex::new(None)),
            next_id: Arc::new(AtomicU64::new(1)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            route: Arc::new(Mutex::new(HashMap::new())),
            acc: Arc::new(Mutex::new(HashMap::new())),
            permissions: Arc::new(Mutex::new(Vec::new())),
            models: Arc::new(Mutex::new(Vec::new())),
            child: Mutex::new(None),
        }
    }

    fn exe_path(&self) -> Result<PathBuf> {
        if !self.cfg.server_url.is_empty() {
            let p = PathBuf::from(&self.cfg.server_url);
            if p.exists() {
                return Ok(p);
            }
        }
        if let Ok(p) = which::which("opencode") {
            return Ok(p);
        }
        #[cfg(windows)]
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let candidate = PathBuf::from(&appdata)
                .join("npm")
                .join("node_modules")
                .join("@opencode")
                .join("cli")
                .join("bin")
                .join("opencode.exe");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        bail!("opencode executable not found for the ACP harness")
    }

    fn cwd(&self) -> PathBuf {
        if self.cfg.workdir.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            PathBuf::from(&self.cfg.workdir)
        }
    }

    /// Fire a JSON-RPC request; the response is delivered to the returned
    /// receiver by the reader thread.
    fn call(&self, method: &str, params: Value) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::sync_channel(1);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Some(w) = self.writer.lock().as_mut() {
            let _ = writeln!(w, "{msg}");
            let _ = w.flush();
        }
        rx
    }

    fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Some(w) = self.writer.lock().as_mut() {
            let _ = writeln!(w, "{msg}");
            let _ = w.flush();
        }
    }

    fn write_raw(&self, v: &Value) {
        if let Some(w) = self.writer.lock().as_mut() {
            let _ = writeln!(w, "{v}");
            let _ = w.flush();
        }
    }

    /// Map a session/update notification to UI events (acc accumulates text).
    fn handle_update(
        events: &UnboundedSender<BackendEvent>,
        acc: &Arc<Mutex<HashMap<String, String>>>,
        harness: &str,
        sent_id: &str,
        note: SessionNotification,
    ) {
        let msg_id = format!("acp_{sent_id}");
        match note.update {
            SessionUpdate::AgentMessageChunk(c) => {
                if let ContentBlock::Text(t) = c.content {
                    if !t.text.is_empty() {
                        acc.lock()
                            .entry(sent_id.to_string())
                            .or_default()
                            .push_str(&t.text);
                        let _ = events.send(BackendEvent::PartDelta {
                            harness: harness.into(),
                            native_id: sent_id.into(),
                            msg_id,
                            part_id: "t0".into(),
                            kind: PartKind::Text,
                            delta: t.text,
                        });
                    }
                }
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                if let ContentBlock::Text(t) = c.content {
                    if !t.text.is_empty() {
                        let _ = events.send(BackendEvent::PartDelta {
                            harness: harness.into(),
                            native_id: sent_id.into(),
                            msg_id,
                            part_id: "r0".into(),
                            kind: PartKind::Reasoning,
                            delta: t.text,
                        });
                    }
                }
            }
            SessionUpdate::ToolCall(tc) => {
                let _ = events.send(BackendEvent::PartUpdated {
                    harness: harness.into(),
                    native_id: sent_id.into(),
                    msg_id,
                    part: RenderPart {
                        part_id: format!("c_{}", tc.tool_call_id),
                        kind: PartKind::Tool,
                        text: format!("⚙ {}…", tc.title),
                    },
                });
            }
            SessionUpdate::ToolCallUpdate(u) => {
                let status = u.fields.status.unwrap_or_default();
                let mark = match status {
                    ToolCallStatus::Pending => "…",
                    ToolCallStatus::InProgress => "⚙",
                    ToolCallStatus::Completed => "✓",
                    ToolCallStatus::Failed => "⚠",
                    _ => "⚙",
                };
                let title = u.fields.title.clone().unwrap_or_default();
                let _ = events.send(BackendEvent::PartUpdated {
                    harness: harness.into(),
                    native_id: sent_id.into(),
                    msg_id,
                    part: RenderPart {
                        part_id: format!("c_{}", u.tool_call_id),
                        kind: PartKind::Tool,
                        text: if title.is_empty() {
                            format!("{mark} tool")
                        } else {
                            format!("{mark} {title}")
                        },
                    },
                });
            }
            SessionUpdate::Plan(p) => {
                let entries: Vec<String> = p.entries.iter().map(|e| e.content.clone()).collect();
                let _ = events.send(BackendEvent::PartUpdated {
                    harness: harness.into(),
                    native_id: sent_id.into(),
                    msg_id,
                    part: RenderPart {
                        part_id: "plan".into(),
                        kind: PartKind::Tool,
                        text: format!("▤ plan: {}", entries.join(" · ")),
                    },
                });
            }
            _ => {}
        }
    }
}

#[async_trait]
impl Backend for AcpBackend {
    fn harness_id(&self) -> &str {
        &self.cfg.id
    }

    async fn connect(&self) -> Result<String> {
        if self.connected.load(Ordering::Relaxed) {
            return Ok("acp".into());
        }
        let exe = self.exe_path()?;
        let mut cmd = Command::new(&exe);
        cmd.arg("acp").current_dir(self.cwd());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        super::spawn_prep(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("spawn opencode acp: {e}"))?;
        *self.writer.lock() = child.stdin.take();
        let stdout = child.stdout.take();
        *self.child.lock() = Some(child);

        // initialize; the response flips `connected` from the reader thread
        // (Б2: params built from the schema types, not hand-rolled json!)
        let init = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);
        let _init = self.call(
            "initialize",
            serde_json::to_value(&init).unwrap_or(Value::Null),
        );

        let events = self.events.clone();
        let harness = self.cfg.id.clone();
        let pending = Arc::clone(&self.pending);
        let route = Arc::clone(&self.route);
        let acc = Arc::clone(&self.acc);
        let permissions = Arc::clone(&self.permissions);
        let connected = Arc::clone(&self.connected);
        let writer = Arc::clone(&self.writer);
        std::thread::spawn(move || {
            let Some(stdout) = stdout else { return };
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let method = v
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !method.is_empty() {
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    match method.as_str() {
                        "session/update" => {
                            if let Ok(note) = serde_json::from_value::<SessionNotification>(params)
                            {
                                let real = note.session_id.to_string();
                                let sent = route.lock().get(&real).cloned().unwrap_or(real.clone());
                                Self::handle_update(&events, &acc, &harness, &sent, note);
                            }
                        }
                        "session/request_permission" => {
                            // v4 Б2: typed params; the raw jsonrpc id is kept
                            // (string ids are legal and must not be dropped)
                            let Ok(req) =
                                serde_json::from_value::<RequestPermissionRequest>(params.clone())
                            else {
                                // v5: answer a malformed request instead of
                                // silence — the agent would wait forever
                                if let Some(id) = v.get("id") {
                                    let reply = json!({"jsonrpc": "2.0", "id": id,
                                        "error": {"code": -32602,
                                                  "message": "invalid params for session/request_permission"}});
                                    if let Some(w) = writer.lock().as_mut() {
                                        let _ = writeln!(w, "{reply}");
                                        let _ = w.flush();
                                    }
                                }
                                continue;
                            };
                            let raw_id = v.get("id").cloned().unwrap_or(Value::Null);
                            if raw_id.is_null() {
                                continue;
                            }
                            let sid = req.session_id.to_string();
                            // v5 issues #1: the policy engine must see WHAT
                            // the agent wants — action from the tool kind,
                            // resource from locations / the raw command, and
                            // a human-readable title from the tool call
                            let f = &req.tool_call.fields;
                            let kind = f.kind.unwrap_or_default();
                            let action = tool_kind_name(kind).to_string();
                            // v6 #2: every touched location matters — the
                            // policy engine takes the strictest across them
                            let resources = if action == "execute" {
                                let cmd = raw_input_text(f.raw_input.as_ref());
                                if cmd.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![cmd]
                                }
                            } else {
                                f.locations
                                    .as_ref()
                                    .map(|locs| {
                                        locs.iter()
                                            .map(|l| l.path.to_string_lossy().into_owned())
                                            .collect()
                                    })
                                    .unwrap_or_default()
                            };
                            let resource = resources.first().cloned().unwrap_or_default();
                            let title = f
                                .title
                                .clone()
                                .filter(|t| !t.trim().is_empty())
                                .unwrap_or_else(|| {
                                    // v4 Б2: fall back to option names
                                    req.options
                                        .iter()
                                        .map(|o| o.name.clone())
                                        .collect::<Vec<_>>()
                                        .join(" / ")
                                });
                            // register BEFORE notifying — the UI's reply must
                            // always find the pending entry (no lost race)
                            permissions.lock().push(PendingPermission {
                                jsonrpc_id: raw_id.clone(),
                                session_id: sid.clone(),
                                options: req.options,
                            });
                            let _ = events.send(BackendEvent::PermissionRequest {
                                harness: harness.clone(),
                                session_id: sid,
                                request_id: format!(
                                    "jsonrpc:{}",
                                    serde_json::to_string(&raw_id).unwrap_or_default()
                                ),
                                title,
                                action,
                                resource,
                                resources,
                            });
                        }
                        _ => {
                            // fs/* and other client-side requests we do not serve
                            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                                let reply = json!({"jsonrpc": "2.0", "id": id,
                                    "error": {"code": -32601, "message": "method not supported by ai-commander"}});
                                if let Some(w) = writer.lock().as_mut() {
                                    let _ = writeln!(w, "{reply}");
                                    let _ = w.flush();
                                }
                            }
                        }
                    }
                    continue;
                }
                // a response to one of our requests
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let is_init = v
                        .get("result")
                        .map(|r| r.get("agentCapabilities").is_some())
                        .unwrap_or(false);
                    if is_init {
                        connected.store(true, Ordering::Relaxed);
                    }
                    if let Some(tx) = pending.lock().remove(&id) {
                        let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
            }
            connected.store(false, Ordering::Relaxed);
            let _ = events.send(BackendEvent::Error {
                harness,
                error: "acp agent process exited".into(),
            });
        });

        // bounded wait for the initialize response (reader flips `connected`)
        for _ in 0..150 {
            if self.connected.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !self.connected.load(Ordering::Relaxed) {
            bail!("opencode acp did not answer initialize in 15s");
        }
        Ok("acp".into())
    }

    async fn refresh_sessions(&self) -> Result<Vec<SessionInfo>> {
        Ok(self
            .store
            .lock()
            .sessions
            .iter()
            .map(|s| SessionInfo {
                native_id: s.native_id.clone(),
                title: s.title.clone(),
                updated_ms: s.updated_ms,
            })
            .collect())
    }

    async fn create_session(&self, title: &str) -> Result<SessionInfo> {
        let s = StoredSession {
            native_id: format!("pending-{}", now_ms()),
            title: title.to_string(),
            updated_ms: now_ms(),
            msgs: Vec::new(),
        };
        let info = SessionInfo {
            native_id: s.native_id.clone(),
            title: s.title.clone(),
            updated_ms: s.updated_ms,
        };
        let mut g = self.store.lock();
        g.sessions.push(s);
        persist_atomic(&self.store_path, &g);
        Ok(info)
    }

    async fn rename_session(&self, native_id: &str, title: &str) -> Result<()> {
        let mut g = self.store.lock();
        if let Some(s) = g.sessions.iter_mut().find(|s| s.native_id == native_id) {
            s.title = title.to_string();
        }
        persist_atomic(&self.store_path, &g);
        Ok(())
    }

    async fn delete_session(&self, native_id: &str) -> Result<()> {
        let mut g = self.store.lock();
        g.sessions.retain(|s| s.native_id != native_id);
        persist_atomic(&self.store_path, &g);
        Ok(())
    }

    async fn load_history(&self, native_id: &str) -> Result<Vec<ChatMsg>> {
        let g = self.store.lock();
        let Some(s) = g.sessions.iter().find(|s| s.native_id == native_id) else {
            return Ok(Vec::new());
        };
        Ok(s.msgs
            .iter()
            .enumerate()
            .map(|(i, m)| ChatMsg {
                msg_id: format!("acp{i}"),
                role: if m.role == "user" {
                    Role::User
                } else {
                    Role::Assistant
                },
                parts: vec![RenderPart {
                    part_id: "t0".into(),
                    kind: PartKind::Text,
                    text: m.text.clone(),
                }],
                attachments: Vec::new(),
            })
            .collect())
    }

    async fn send(
        &self,
        native_id: &str,
        text: &str,
        attachments: &[AttachmentMeta],
        opts: SendOpts<'_>,
    ) -> Result<()> {
        for a in attachments {
            if a.size > MAX_ATTACHMENT_BYTES {
                bail!(
                    "attachment {} is {} — over the {} MB limit",
                    a.filename,
                    a.size_label(),
                    MAX_ATTACHMENT_BYTES / (1024 * 1024)
                );
            }
        }
        if !self.connected.load(Ordering::Relaxed) {
            bail!("acp agent is not running");
        }

        let resume = !native_id.is_empty() && !native_id.starts_with("pending-");
        let cwd = self.cwd().to_string_lossy().into_owned();

        // Б2: prompt content blocks are schema ContentBlock values
        let mut blocks: Vec<ContentBlock> = vec![ContentBlock::Text(TextContent::new(text))];
        for a in attachments {
            if a.mime.starts_with("image/") {
                if let Ok(data) = std::fs::read(&a.path) {
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                    blocks.push(ContentBlock::Image(ImageContent::new(b64, &a.mime)));
                }
            }
        }

        // session/new -> real id, then prompt; a helper thread sequences both
        let writer = Arc::clone(&self.writer);
        let next_id = Arc::clone(&self.next_id);
        let pending = Arc::clone(&self.pending);
        let route = Arc::clone(&self.route);
        let acc = Arc::clone(&self.acc);
        let store = Arc::clone(&self.store);
        let store_path = self.store_path.clone();
        let events = self.events.clone();
        let harness = self.cfg.id.clone();
        let models = Arc::clone(&self.models);
        let sent_id = native_id.to_string();
        let user_text = text.to_string();
        let mcp_role = opts.mcp_role.map(str::to_owned);
        let blocks_clone = blocks.clone();
        let cwd_base = cwd.clone();
        // Н3: owned copy for the thread (opts.cwd borrows from the caller)
        let cwd_override = opts.cwd.map(|p| p.to_path_buf());

        std::thread::spawn(move || {
            let rpc = |method: &str, params: Value| -> mpsc::Receiver<Value> {
                let (tx, rx) = mpsc::sync_channel(1);
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                pending.lock().insert(id, tx);
                let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                if let Some(w) = writer.lock().as_mut() {
                    let _ = writeln!(w, "{msg}");
                    let _ = w.flush();
                }
                rx
            };

            // board MCP server: same binaries dir, role token via env
            // (Б2: typed McpServer list, not json!)
            let mcp_servers = |role: Option<&str>| -> Vec<McpServer> {
                role.and_then(board_mcp_entry).into_iter().collect()
            };
            // Н3: prefer the task's worktree cwd over the harness default
            let cwd_str = cwd_override
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(cwd_base.clone());
            let real_id: String = if resume {
                // Б2: schema-typed session/load params
                let load = LoadSessionRequest::new(sent_id.clone(), cwd_str.clone())
                    .mcp_servers(mcp_servers(mcp_role.as_deref()));
                let _load = rpc(
                    "session/load",
                    serde_json::to_value(&load).unwrap_or(Value::Null),
                );
                sent_id.clone()
            } else {
                // Б2: schema-typed session/new params
                let new = NewSessionRequest::new(cwd_str.clone())
                    .mcp_servers(mcp_servers(mcp_role.as_deref()));
                let rx = rpc(
                    "session/new",
                    serde_json::to_value(&new).unwrap_or(Value::Null),
                );
                match rx.recv_timeout(Duration::from_secs(20)) {
                    Ok(v) => {
                        // capture the model list for Ctrl+K
                        if let Some(opts) = v.get("configOptions").and_then(Value::as_array) {
                            let mut ms = Vec::new();
                            for o in opts {
                                if o.get("id").and_then(Value::as_str) != Some("model") {
                                    continue;
                                }
                                if let Some(options) = o.get("options").and_then(Value::as_array) {
                                    for m in options {
                                        let value =
                                            m.get("value").and_then(Value::as_str).unwrap_or("");
                                        if let Some((prov, model)) = value.split_once('/') {
                                            ms.push(ModelInfo {
                                                provider_id: prov.to_string(),
                                                model_id: model.to_string(),
                                                name: m
                                                    .get("name")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or(value)
                                                    .to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                            *models.lock() = ms;
                        }
                        v.get("sessionId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    }
                    Err(_) => String::new(),
                }
            };
            if real_id.is_empty() {
                let _ = events.send(BackendEvent::StreamError {
                    harness: harness.clone(),
                    native_id: sent_id.clone(),
                    error: "acp: session id was not received".into(),
                });
                let _ = events.send(BackendEvent::StreamDone {
                    harness,
                    native_id: sent_id,
                });
                return;
            }
            route.lock().insert(real_id.clone(), sent_id.clone());
            if real_id != sent_id {
                let _ = events.send(BackendEvent::SessionCreated {
                    harness: harness.clone(),
                    native_id: real_id.clone(),
                    title: user_text.chars().take(48).collect(),
                });
            }

            // Б2: schema-typed session/prompt params
            let prompt = PromptRequest::new(real_id.clone(), blocks_clone);
            let rx = rpc(
                "session/prompt",
                serde_json::to_value(&prompt).unwrap_or(Value::Null),
            );
            match rx.recv_timeout(Duration::from_secs(900)) {
                Ok(result) => {
                    let stop = result
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn");
                    let error = if stop == "cancelled" {
                        None
                    } else if stop == "refusal" || stop == "max_tokens" {
                        Some(format!("turn stopped: {stop}"))
                    } else {
                        None
                    };
                    let assistant = acc.lock().remove(&sent_id).unwrap_or_default();
                    {
                        let mut g = store.lock();
                        match g.sessions.iter_mut().find(|s| s.native_id == sent_id) {
                            Some(s) => {
                                if real_id != sent_id {
                                    s.native_id = real_id.clone();
                                }
                                s.msgs.push(StoredMsg {
                                    role: "user".into(),
                                    text: user_text.clone(),
                                });
                                s.msgs.push(StoredMsg {
                                    role: "assistant".into(),
                                    text: assistant,
                                });
                                s.updated_ms = now_ms();
                            }
                            None => g.sessions.push(StoredSession {
                                native_id: if real_id == sent_id {
                                    sent_id.clone()
                                } else {
                                    real_id.clone()
                                },
                                title: user_text.chars().take(48).collect(),
                                updated_ms: now_ms(),
                                msgs: vec![
                                    StoredMsg {
                                        role: "user".into(),
                                        text: user_text.clone(),
                                    },
                                    StoredMsg {
                                        role: "assistant".into(),
                                        text: assistant,
                                    },
                                ],
                            }),
                        }
                        persist_atomic(&store_path, &g);
                    }
                    if let Some(err) = error {
                        let _ = events.send(BackendEvent::StreamError {
                            harness: harness.clone(),
                            native_id: sent_id.clone(),
                            error: err,
                        });
                    }
                }
                Err(_) => {
                    let _ = events.send(BackendEvent::StreamError {
                        harness: harness.clone(),
                        native_id: sent_id.clone(),
                        error: "acp: prompt response timed out".into(),
                    });
                }
            }
            let _ = events.send(BackendEvent::StreamDone {
                harness,
                native_id: sent_id,
            });
        });
        Ok(())
    }

    /// ACP session/cancel — a real, working abort. `native_id` may be the
    /// agent-side session id (route key — what the board stores) or the
    /// client-side sent id (pending-* draft); both resolve to the real id.
    async fn abort(&self, native_id: &str) -> Result<()> {
        let real = {
            let route = self.route.lock();
            if route.contains_key(native_id) {
                Some(native_id.to_string())
            } else {
                route
                    .iter()
                    .find(|(_, v)| v.as_str() == native_id)
                    .map(|(k, _)| k.clone())
            }
        };
        if let Some(real) = &real {
            // Б2: schema-typed CancelNotification (sessionId only — the old
            // hand-rolled payload carried an off-schema "reason" field)
            let cancel = CancelNotification::new(real.to_string());
            self.notify(
                "session/cancel",
                serde_json::to_value(&cancel).unwrap_or(Value::Null),
            );
        }
        // v5 Б2: the spec REQUIRES answering pending permission requests
        // with `cancelled` when their turn is cancelled — but ONLY the ones
        // of THIS session; another session's open questions stay alive
        let (mine, others): (Vec<PendingPermission>, Vec<PendingPermission>) = {
            let all = std::mem::take(&mut *self.permissions.lock());
            all.into_iter()
                .partition(|p| Some(&p.session_id) == real.as_ref())
        };
        *self.permissions.lock() = others;
        for p in mine {
            let resp = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
            let msg = json!({"jsonrpc": "2.0", "id": p.jsonrpc_id,
                "result": serde_json::to_value(&resp).unwrap_or(Value::Null)});
            self.write_raw(&msg);
        }
        Ok(())
    }

    /// Map the user's answer to the schema option kinds (v4 Б2):
    /// "once"→allow_once, "always"→allow_always, "reject"→reject_once.
    /// No matching option → `cancelled` (never a blind first-option pick).
    async fn permission_reply(
        &self,
        _native_id: &str,
        request_id: &str,
        reply: &str,
    ) -> Result<()> {
        let jsonrpc_id = request_id
            .strip_prefix("jsonrpc:")
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(|v| !v.is_null())
            .ok_or_else(|| anyhow!("bad permission request id"))?;
        let mut perms = self.permissions.lock();
        let Some(idx) = perms.iter().position(|p| p.jsonrpc_id == jsonrpc_id) else {
            bail!("permission request not found");
        };
        let p = perms.remove(idx);
        drop(perms);

        // v4 Б2: choose strictly by the schema kind, in preference order;
        // no fallback to "whatever option is first"
        let want: &[PermissionOptionKind] = match reply {
            "reject" => &[
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ],
            "always" => &[PermissionOptionKind::AllowAlways],
            // "once" and anything else the UI sends for a plain allow
            _ => &[PermissionOptionKind::AllowOnce],
        };
        let outcome = match want
            .iter()
            .find_map(|k| p.options.iter().find(|o| o.kind == *k))
        {
            Some(o) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                o.option_id.clone(),
            )),
            None => {
                // v4 Б2: nothing matched — cancel (never a blind allow)
                crate::log_warn!(
                    "acp",
                    "no permission option of kind {:?} offered — cancelling",
                    reply
                );
                RequestPermissionOutcome::Cancelled
            }
        };
        // v4 Б2: typed response; the raw id (number or string) round-trips
        let resp = RequestPermissionResponse::new(outcome);
        let msg = json!({"jsonrpc": "2.0", "id": jsonrpc_id,
            "result": serde_json::to_value(&resp).unwrap_or(Value::Null)});
        self.write_raw(&msg);
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // captured from the session/new configOptions (empty until a session exists)
        Ok(self.models.lock().clone())
    }

    async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        Ok(Vec::new())
    }

    fn shutdown(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.writer.lock().take();
        if let Some(mut ch) = self.child.lock().take() {
            kill_tree(&mut ch);
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Board MCP server descriptor for session/new — typed (Б2): the old
/// hand-rolled json sent `env` as a map, but the schema (and strict agents)
/// require an array of {name, value} and a server `name`.
fn board_mcp_entry(role: &str) -> Option<McpServer> {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))?;
    board_mcp_at(&dir, role)
}

/// Policy action name for a tool kind (v5 issues #1): matches the ACP
/// snake_case kinds; an absent kind is `other`, to which `allow` rules
/// never apply (the policy engine decides ask/deny only).
fn tool_kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
}

/// Human-readable command for an `execute` permission from the tool's
/// raw input: {"command": "..."} / {"cmd": "..."} / bare string.
fn raw_input_text(raw: Option<&Value>) -> String {
    match raw {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => ["command", "cmd", "script", "input"]
            .iter()
            .find_map(|k| obj.get(*k).and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn board_mcp_at(dir: &std::path::Path, role: &str) -> Option<McpServer> {
    let name = if cfg!(windows) {
        "board-mcp.exe"
    } else {
        "board-mcp"
    };
    let bin = dir.join(name);
    if !bin.exists() {
        return None;
    }
    // v4: mint a role token (this process is the board owner) so the
    // spawned board-mcp authenticates to the owner's IPC and acts as
    // exactly this role — the body `role` field is never trusted
    let role_token = crate::kanban::ipc::issue_role_token(role);
    Some(McpServer::Stdio(McpServerStdio::new("board", bin).env(
        vec![
            EnvVariable::new("AI_COMMANDER_BOARD_TOKEN", role_token),
            EnvVariable::new("AI_COMMANDER_ROLE", role),
        ],
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Б2: the board MCP descriptor embedded in session/new must be
    /// schema-canonical — strict agents reject the old hand-rolled json
    /// (env as a map, missing server name). Round-trip through the typed
    /// NewSessionRequest proves the payload a strict agent accepts.
    #[test]
    fn board_mcp_entry_is_schema_canonical() {
        let dir = std::env::temp_dir().join(format!("aic-b2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = if cfg!(windows) {
            dir.join("board-mcp.exe")
        } else {
            dir.join("board-mcp")
        };
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();

        let entry = board_mcp_at(&dir, "coder").expect("entry when the binary exists");
        let req = NewSessionRequest::new(&dir).mcp_servers(vec![entry]);
        let v = serde_json::to_value(&req).unwrap();

        // strict-agent check: deserialize + exact round-trip
        let parsed: NewSessionRequest = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), v, "canonical");

        // env must be the schema array form, not the old map; v4: the minted
        // role token rides along so board-mcp never self-declares a role
        let env = &v["mcpServers"][0]["env"];
        assert!(
            env.is_array(),
            "env must be an array of {{name,value}}: {env}"
        );
        assert_eq!(env[0]["name"], "AI_COMMANDER_BOARD_TOKEN");
        assert_eq!(
            env[0]["value"].as_str().map(str::len),
            Some(64),
            "32-byte hex role token"
        );
        assert_eq!(env[1]["name"], "AI_COMMANDER_ROLE");
        assert_eq!(env[1]["value"], "coder");
        // the server must carry a name (absent in the old payload)
        assert_eq!(v["mcpServers"][0]["name"], "board");

        assert!(board_mcp_at(&dir, "x").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
