//! OpenCode harness backend (verified against opencode CLI 2.2.6, the v2 API).
//!
//! Live-tested contract:
//! - `opencode serve --port N --hostname 127.0.0.1` with env
//!   `OPENCODE_SERVER_PASSWORD=<pw>`; API under `/api/*` with
//!   `Authorization: Basic base64("opencode:<pw>")`.
//! - health: `GET /global/health` (no prefix, no auth)
//! - sessions: `GET/POST /api/session`, `PATCH/DELETE /api/session/{id}` (204)
//! - history: `GET /api/session/{id}/message` → `{data:[{id,type,text,content[]}]}`;
//!   user msgs have `text`, assistant msgs have `content:[{type:text|reasoning|tool,...}]`
//! - send: `POST /api/session/{id}/model` {model:{providerID,modelID}},
//!   `POST /api/session/{id}/agent` {agent}, then
//!   `POST /api/session/{id}/prompt` `{text, files?:[{uri:"data:...;base64,..", name}]}`
//! - models: `GET /api/model`; agents: `GET /api/agent`
//! - SSE `GET /api/event`: `{id,created,type,data:{...}}` events:
//!   session.text.started/delta/ended, session.reasoning.*, session.tool.input.started,
//!   session.tool.called/success, session.step.*, session.usage.updated,
//!   session.execution.succeeded (stream done), session.error,
//!   session.created/updated/deleted.
//! - abort: not exposed by this server build — Esc locally detaches; the next
//!   prompt "steers" (interrupts) the current turn server-side.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine as _;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::{
    AgentInfo, AttachmentMeta, Backend, BackendEvent, ChatMsg, ModelInfo, PartKind, RenderPart,
    Role, SendOpts, SessionInfo,
};
use crate::config::HarnessCfg;

pub struct OpenCodeBackend {
    cfg: HarnessCfg,
    /// Basic-auth password (own generated for autospawn, from config for external);
    /// also passed as OPENCODE_SERVER_PASSWORD when spawning the server
    password: String,
    /// Basic-auth client for everything under /api — REST **and** SSE
    /// (the SSE endpoint returns 401 without the Authorization header).
    http: reqwest::Client,
    /// unauthenticated client for /global/health
    bare: reqwest::Client,
    events: UnboundedSender<BackendEvent>,
    base_url: Mutex<Option<String>>,
    child: Mutex<Option<Child>>,
    connected: Arc<AtomicBool>,
    /// sessions we stream events for (others are ignored)
    known_sessions: Arc<Mutex<HashSet<String>>>,
    /// bumps on every connect/shutdown so stale SSE loops exit
    sse_gen: Arc<std::sync::atomic::AtomicU64>,
}

impl OpenCodeBackend {
    pub fn new(cfg: HarnessCfg, events: UnboundedSender<BackendEvent>) -> Self {
        let password = if cfg.server_url.is_empty() {
            format!("ai-cmd-{}", rand_suffix())
        } else if cfg.server_password.is_empty() {
            String::new() // external server without password: no auth header
        } else {
            cfg.server_password.clone()
        };
        let mut b = reqwest::Client::builder().no_proxy();
        if !password.is_empty() {
            let auth = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"))
            );
            b = b.default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                if let Ok(v) = reqwest::header::HeaderValue::from_str(&auth) {
                    h.insert(reqwest::header::AUTHORIZATION, v);
                }
                h
            });
        }
        OpenCodeBackend {
            cfg,
            password,
            http: b.build().expect("reqwest client"),
            bare: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("reqwest client"),
            events,
            base_url: Mutex::new(None),
            child: Mutex::new(None),
            connected: Arc::new(AtomicBool::new(false)),
            known_sessions: Arc::new(Mutex::new(HashSet::new())),
            sse_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    async fn base(&self) -> Result<String> {
        self.base_url
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("opencode server is not connected yet"))
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let base = self.base().await?;
        let resp = self
            .http
            .get(format!("{base}{path}"))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("GET {path} -> {status}: {}", truncate(&text, 300));
        }
        serde_json::from_str(&text)
            .with_context(|| format!("parse GET {path}: {}", truncate(&text, 300)))
    }

    async fn send_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let base = self.base().await?;
        let mut req = self.http.request(method, format!("{base}{path}"));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .with_context(|| format!("req {path}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{path} -> {status}: {}", truncate(&text, 300));
        }
        Ok(if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        })
    }

    // --- server lifecycle ---

    fn resolve_opencode_exe() -> Result<PathBuf> {
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
        if let Ok(out) = Command::new("where").arg("opencode.cmd").output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    let shim = PathBuf::from(line);
                    let derived = shim.parent().map(|p| {
                        p.join("node_modules")
                            .join("@opencode")
                            .join("cli")
                            .join("bin")
                            .join("opencode.exe")
                    });
                    if let Some(exe) = derived.filter(|p| p.exists()) {
                        return Ok(exe);
                    }
                    let sibling = shim.with_file_name("opencode.exe");
                    if sibling.exists() {
                        return Ok(sibling);
                    }
                }
            }
        }
        Ok(PathBuf::from("opencode.exe"))
    }

    fn free_port() -> Result<u16> {
        std::net::TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()
            .map(|a| a.port())
            .context("local addr")
    }

    /// Health probe: v2 serves it under /api (auth); plain /global/health
    /// answers 200 with SPA HTML on some builds, so it's only a fallback signal.
    async fn healthy(&self, base: &str) -> Option<Value> {
        let looks_alive = |body: &str| !body.trim_start().starts_with('<'); // SPA HTML is not the API
        if let Ok(r) = self
            .http
            .get(format!("{base}/api/global/health"))
            .timeout(Duration::from_millis(1500))
            .send()
            .await
        {
            if r.status().is_success() {
                let body = r.text().await.unwrap_or_default();
                if !looks_alive(&body) {
                    return None; // an HTML page answered, not the API
                }
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    if v.get("data").is_some()
                        || v.get("healthy").is_some()
                        || v.get("version").is_some()
                    {
                        return Some(v);
                    }
                }
                return Some(Value::Null); // 200, non-HTML (some builds send an empty body)
            }
        }
        // legacy fallback
        if let Ok(r) = self
            .bare
            .get(format!("{base}/global/health"))
            .timeout(Duration::from_millis(1500))
            .send()
            .await
        {
            if r.status().is_success() {
                let body = r.text().await.unwrap_or_default();
                if looks_alive(&body) {
                    return Some(Value::Null);
                }
            }
        }
        None
    }

    async fn spawn_server(&self) -> Result<String> {
        let exe = Self::resolve_opencode_exe()?;
        let port = Self::free_port()?;
        let mut cmd = Command::new(&exe);
        cmd.args([
            "serve",
            "--port",
            &port.to_string(),
            "--hostname",
            "127.0.0.1",
        ]);
        cmd.env("OPENCODE_SERVER_PASSWORD", &self.password);
        if !self.cfg.workdir.is_empty() {
            if let Ok(wd) = std::env::current_dir() {
                let dir = if PathBuf::from(&self.cfg.workdir).is_absolute() {
                    PathBuf::from(&self.cfg.workdir)
                } else {
                    wd.join(&self.cfg.workdir)
                };
                cmd.current_dir(dir);
            }
        }
        let log_path = std::env::temp_dir().join(format!("ai-commander-opencode-{}.log", port));
        cmd.stdin(Stdio::null());
        match std::fs::File::create(&log_path) {
            Ok(f) => {
                let out = f
                    .try_clone()
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::null());
                let err = f
                    .try_clone()
                    .map(Stdio::from)
                    .unwrap_or_else(|_| Stdio::null());
                cmd.stdout(out).stderr(err);
            }
            Err(_) => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", exe.display()))?;
        *self.child.lock().unwrap() = Some(child);

        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..200 {
            if self.healthy(&base).await.is_some() {
                return Ok(base);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            {
                let mut guard = self.child.lock().unwrap();
                if let Some(c) = guard.as_mut() {
                    if c.try_wait().map(|s| s.is_some()).unwrap_or(false) {
                        let log_note = std::fs::read_to_string(&log_path).unwrap_or_default();
                        let tail: String = log_note
                            .lines()
                            .rev()
                            .take(8)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("\n");
                        bail!("opencode serve exited early.\n{tail}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        bail!(
            "opencode server did not become healthy in 200s (log: {})",
            log_path.display()
        );
    }

    // --- SSE ---

    fn start_event_stream(&self, base: String) {
        let events = self.events.clone();
        let known = Arc::clone(&self.known_sessions);
        let connected = Arc::clone(&self.connected);
        let sse_gen = Arc::clone(&self.sse_gen);
        let harness = self.cfg.id.clone();
        let my_gen = self.sse_gen.fetch_add(1, Ordering::Relaxed) + 1;
        // same authenticated client as REST — /api/event rejects anonymous streams
        let client = self.http.clone();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            loop {
                if !connected.load(Ordering::Relaxed) || sse_gen.load(Ordering::Relaxed) != my_gen {
                    return; // superseded by a newer stream or shutdown
                }
                let Ok(req) = client
                    .get(format!("{base}/api/event"))
                    .header("accept", "text/event-stream")
                    .build()
                else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                };
                let Ok(resp) = client.execute(req).await else {
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(8);
                    continue;
                };
                if !resp.status().is_success() {
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(8);
                    continue;
                }
                let mut stream = resp.bytes_stream().eventsource();
                let mut got_any = false;
                while let Some(Ok(ev)) = stream.next().await {
                    if !got_any {
                        got_any = true;
                        if backoff_secs > 1 {
                            // stream dropped and recovered — ask the UI to resync
                            let _ = events.send(BackendEvent::Reconnected {
                                harness: harness.clone(),
                            });
                        }
                        backoff_secs = 1;
                    }
                    handle_sse_event(&events, &known, &harness, &ev.data);
                }
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(8);
            }
        });
    }

    fn map_session(&self, v: &Value) -> SessionInfo {
        SessionInfo {
            native_id: v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .unwrap_or("untitled")
                .to_string(),
            updated_ms: v
                .pointer("/time/updated")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        }
    }
}

fn health_version(v: &Value) -> String {
    // /api/global/health → {data:{version}} or {version}; fallback "?"
    v.pointer("/data/version")
        .and_then(Value::as_str)
        .or_else(|| v.get("version").and_then(Value::as_str))
        .unwrap_or("?")
        .to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut cut = n;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

fn rand_suffix() -> String {
    // 16 bytes of OS randomness — a nanosecond timestamp is guessable
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("OS randomness");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[async_trait]
impl Backend for OpenCodeBackend {
    fn harness_id(&self) -> &str {
        &self.cfg.id
    }

    async fn connect(&self) -> Result<String> {
        let existing = self.base_url.lock().unwrap().clone();
        if let Some(base) = existing {
            if let Some(v) = self.healthy(&base).await {
                let version = health_version(&v);
                self.connected.store(true, Ordering::Relaxed);
                return Ok(version);
            }
        }

        let base = if !self.cfg.server_url.is_empty() {
            let url = self.cfg.server_url.trim_end_matches('/').to_string();
            match self.healthy(&url).await {
                Some(_) => url,
                None => bail!("server at {url} is not healthy"),
            }
        } else {
            self.spawn_server().await?
        };

        let v = self.healthy(&base).await.unwrap_or(Value::Null);
        let version = health_version(&v);

        *self.base_url.lock().unwrap() = Some(base.clone());
        self.connected.store(true, Ordering::Relaxed);

        // seed known sessions so SSE filtering works before the first refresh lands
        if let Ok(sessions) = self.get_json("/api/session").await {
            let mut ks = self.known_sessions.lock().unwrap();
            ks.clear();
            if let Some(arr) = sessions.get("data").and_then(Value::as_array) {
                for s in arr {
                    if let Some(id) = s.get("id").and_then(Value::as_str) {
                        ks.insert(id.to_string());
                    }
                }
            }
        }

        self.start_event_stream(base);
        Ok(version)
    }

    async fn refresh_sessions(&self) -> Result<Vec<SessionInfo>> {
        let v = self.get_json("/api/session").await?;
        let mut list: Vec<SessionInfo> = v
            .get("data")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|s| self.map_session(s)).collect())
            .unwrap_or_default();
        list.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        {
            // union, not replace: a session created between the REST call and
            // this write must not lose its events
            let mut ks = self.known_sessions.lock().unwrap();
            ks.extend(list.iter().map(|s| s.native_id.clone()));
        }
        Ok(list)
    }

    async fn create_session(&self, title: &str) -> Result<SessionInfo> {
        let v = self
            .send_json(
                reqwest::Method::POST,
                "/api/session",
                Some(serde_json::json!({ "title": title })),
            )
            .await?;
        let s = v.get("data").cloned().unwrap_or(v);
        let s = self.map_session(&s);
        self.known_sessions
            .lock()
            .unwrap()
            .insert(s.native_id.clone());
        Ok(s)
    }

    async fn rename_session(&self, native_id: &str, title: &str) -> Result<()> {
        self.send_json(
            reqwest::Method::PATCH,
            &format!("/api/session/{native_id}"),
            Some(serde_json::json!({ "title": title })),
        )
        .await?;
        Ok(())
    }

    async fn delete_session(&self, native_id: &str) -> Result<()> {
        self.send_json(
            reqwest::Method::DELETE,
            &format!("/api/session/{native_id}"),
            None,
        )
        .await?;
        self.known_sessions.lock().unwrap().remove(native_id);
        Ok(())
    }

    async fn load_history(&self, native_id: &str) -> Result<Vec<ChatMsg>> {
        let v = self
            .get_json(&format!("/api/session/{native_id}/message"))
            .await?;
        let arr = v
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut msgs = Vec::new();
        for item in &arr {
            let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
            let msg_id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match ty {
                "user" => {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if text.trim().is_empty() {
                        continue;
                    }
                    msgs.push(ChatMsg {
                        msg_id,
                        role: Role::User,
                        parts: vec![RenderPart {
                            part_id: "t0".into(),
                            kind: PartKind::Text,
                            text,
                        }],
                        attachments: Vec::new(),
                    });
                }
                "assistant" => {
                    let mut parts = Vec::new();
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for (i, c) in content.iter().enumerate() {
                            let cty = c.get("type").and_then(Value::as_str).unwrap_or("");
                            match cty {
                                "text" => {
                                    let text = c.get("text").and_then(Value::as_str).unwrap_or("");
                                    if !text.trim().is_empty() {
                                        parts.push(RenderPart {
                                            part_id: format!("t{i}"),
                                            kind: PartKind::Text,
                                            text: text.into(),
                                        });
                                    }
                                }
                                "reasoning" => {
                                    let text = c.get("text").and_then(Value::as_str).unwrap_or("");
                                    if !text.trim().is_empty() {
                                        parts.push(RenderPart {
                                            part_id: format!("r{i}"),
                                            kind: PartKind::Reasoning,
                                            text: text.into(),
                                        });
                                    }
                                }
                                "tool" => {
                                    let name =
                                        c.get("name").and_then(Value::as_str).unwrap_or("tool");
                                    let status = c
                                        .pointer("/state/status")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    let label = tool_label(
                                        name,
                                        status,
                                        c.pointer("/state/error").and_then(Value::as_str),
                                    );
                                    parts.push(RenderPart {
                                        part_id: format!("c{i}"),
                                        kind: PartKind::Tool,
                                        text: label,
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    if !parts.is_empty() {
                        msgs.push(ChatMsg {
                            msg_id,
                            role: Role::Assistant,
                            parts,
                            attachments: Vec::new(),
                        });
                    }
                }
                _ => {} // system / idle — not rendered
            }
        }
        Ok(msgs)
    }

    async fn send(
        &self,
        native_id: &str,
        text: &str,
        attachments: &[AttachmentMeta],
        opts: SendOpts<'_>,
    ) -> Result<()> {
        for a in attachments {
            if a.size > super::MAX_ATTACHMENT_BYTES {
                bail!(
                    "attachment {} is {} — over the {} MB limit",
                    a.filename,
                    a.size_label(),
                    super::MAX_ATTACHMENT_BYTES / (1024 * 1024)
                );
            }
        }
        // switch model/agent for subsequent turns when requested
        if let Some(m) = opts.model {
            if let Err(e) = self
                .send_json(
                    reqwest::Method::POST,
                    &format!("/api/session/{native_id}/model"),
                    Some(serde_json::json!({ "model": { "providerID": m.provider_id, "modelID": m.model_id } })),
                )
                .await
            {
                let _ = self.events.send(BackendEvent::Error {
                    harness: self.cfg.id.clone(),
                    error: format!("model switch failed: {e:#}"),
                });
            }
        }
        if let Some(agent) = opts.agent {
            if let Err(e) = self
                .send_json(
                    reqwest::Method::POST,
                    &format!("/api/session/{native_id}/agent"),
                    Some(serde_json::json!({ "agent": agent })),
                )
                .await
            {
                let _ = self.events.send(BackendEvent::Error {
                    harness: self.cfg.id.clone(),
                    error: format!("agent switch failed: {e:#}"),
                });
            }
        }

        let mut body = serde_json::json!({ "text": text });
        if !attachments.is_empty() {
            let files: Vec<Value> = attachments
                .iter()
                .map(|a| {
                    let data = std::fs::read(&a.path).unwrap_or_default();
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                    serde_json::json!({ "uri": format!("data:{};base64,{}", a.mime, b64), "name": a.filename })
                })
                .collect();
            body["files"] = Value::Array(files);
        }
        self.known_sessions
            .lock()
            .unwrap()
            .insert(native_id.to_string());
        self.send_json(
            reqwest::Method::POST,
            &format!("/api/session/{native_id}/prompt"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    /// This server build exposes no abort endpoint; the next prompt "steers"
    /// (interrupts) the current turn. The UI detaches locally on Esc.
    async fn abort(&self, _native_id: &str) -> Result<()> {
        Ok(())
    }

    /// Answer a permission request: POST /api/session/{id}/permission/{rid}/reply
    /// with {reply: "once" | "always" | "reject"} (v2 contract).
    async fn permission_reply(&self, native_id: &str, request_id: &str, reply: &str) -> Result<()> {
        self.send_json(
            reqwest::Method::POST,
            &format!("/api/session/{native_id}/permission/{request_id}/reply"),
            Some(serde_json::json!({ "reply": reply })),
        )
        .await?;
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let v = self.get_json("/api/model").await?;
        let mut out: Vec<ModelInfo> = v
            .get("data")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|m| m.get("type").and_then(Value::as_str).unwrap_or("chat") == "chat")
                    .map(|m| ModelInfo {
                        provider_id: m
                            .get("providerID")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        model_id: m
                            .get("modelID")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: m
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.retain(|m| !m.provider_id.is_empty() && !m.model_id.is_empty());
        out.sort_by(|a, b| {
            a.provider_id
                .cmp(&b.provider_id)
                .then(a.model_id.cmp(&b.model_id))
        });
        Ok(out)
    }

    async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let v = self.get_json("/api/agent").await?;
        let mut out: Vec<AgentInfo> = v
            .get("data")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| AgentInfo {
                        id: x
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: x
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.retain(|a| !a.id.is_empty());
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn shutdown(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.sse_gen.fetch_add(1, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            super::kill_tree(&mut child);
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

fn tool_label(name: &str, status: &str, error: Option<&str>) -> String {
    match status {
        "running" => format!("⚙ {name}…"),
        "error" => format!("⚠ {name}: {}", error.unwrap_or("error")),
        "completed" => format!("✓ {name}"),
        _ => format!("⚙ {name}"),
    }
}

/// SSE dispatch — payload lives in the `data` field of each line's JSON.
fn handle_sse_event(
    events: &UnboundedSender<BackendEvent>,
    known: &Mutex<HashSet<String>>,
    harness: &str,
    line: &str,
) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    // payload is in `data` for live events; some events use `properties`
    let props = v
        .get("data")
        .cloned()
        .or_else(|| v.get("properties").cloned())
        .unwrap_or(Value::Null);
    let sid = props
        .get("sessionID")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let send = |ev: BackendEvent| {
        let _ = events.send(ev);
    };
    let h = harness.to_string();

    // session-list changes always pass through
    match ty {
        "session.created" | "session.updated" | "session.deleted" => {
            send(BackendEvent::SessionsChanged { harness: h });
            return;
        }
        _ => {}
    }

    let watched = sid.is_empty() || known.lock().unwrap().contains(&sid);
    if !watched {
        return;
    }

    let msg_id = || {
        props
            .get("assistantMessageID")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    match ty {
        "permission.v2.asked" | "permission.updated" | "permission.asked" => {
            let request_id = props
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let action = props
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("action");
            let resource = props
                .get("resources")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .or_else(|| props.get("pattern").and_then(|p| p.as_str()))
                .unwrap_or("");
            let title = if resource.is_empty() {
                action.to_string()
            } else {
                format!("{action}: {resource}")
            };
            if !sid.is_empty() && !request_id.is_empty() {
                send(BackendEvent::PermissionRequest {
                    harness: harness.into(),
                    session_id: sid,
                    request_id,
                    title,
                });
            }
        }
        "session.text.started" => {
            let ordinal = props.get("ordinal").and_then(Value::as_i64).unwrap_or(0);
            send(BackendEvent::PartUpdated {
                harness: h,
                native_id: sid,
                msg_id: msg_id(),
                part: RenderPart {
                    part_id: format!("t{ordinal}"),
                    kind: PartKind::Text,
                    text: String::new(),
                },
            });
        }
        "session.text.delta" => {
            let ordinal = props.get("ordinal").and_then(Value::as_i64).unwrap_or(0);
            let delta = props.get("delta").and_then(Value::as_str).unwrap_or("");
            if !delta.is_empty() {
                send(BackendEvent::PartDelta {
                    harness: h,
                    native_id: sid,
                    msg_id: msg_id(),
                    part_id: format!("t{ordinal}"),
                    kind: PartKind::Text,
                    delta: delta.to_string(),
                });
            }
        }
        "session.text.ended" => {
            let ordinal = props.get("ordinal").and_then(Value::as_i64).unwrap_or(0);
            let text = props.get("text").and_then(Value::as_str).unwrap_or("");
            send(BackendEvent::PartUpdated {
                harness: h,
                native_id: sid,
                msg_id: msg_id(),
                part: RenderPart {
                    part_id: format!("t{ordinal}"),
                    kind: PartKind::Text,
                    text: text.to_string(),
                },
            });
        }
        "session.reasoning.started" => {
            let ordinal = props.get("ordinal").and_then(Value::as_i64).unwrap_or(0);
            send(BackendEvent::PartUpdated {
                harness: h,
                native_id: sid,
                msg_id: msg_id(),
                part: RenderPart {
                    part_id: format!("r{ordinal}"),
                    kind: PartKind::Reasoning,
                    text: String::new(),
                },
            });
        }
        "session.reasoning.delta" => {
            let ordinal = props.get("ordinal").and_then(Value::as_i64).unwrap_or(0);
            let delta = props.get("delta").and_then(Value::as_str).unwrap_or("");
            if !delta.is_empty() {
                send(BackendEvent::PartDelta {
                    harness: h,
                    native_id: sid,
                    msg_id: msg_id(),
                    part_id: format!("r{ordinal}"),
                    kind: PartKind::Reasoning,
                    delta: delta.to_string(),
                });
            }
        }
        "session.reasoning.ended" => {
            let ordinal = props.get("ordinal").and_then(Value::as_i64).unwrap_or(0);
            let text = props.get("text").and_then(Value::as_str).unwrap_or("");
            send(BackendEvent::PartUpdated {
                harness: h,
                native_id: sid,
                msg_id: msg_id(),
                part: RenderPart {
                    part_id: format!("r{ordinal}"),
                    kind: PartKind::Reasoning,
                    text: text.to_string(),
                },
            });
        }
        "session.tool.input.started" | "session.tool.called" => {
            let call = props
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = props.get("name").and_then(Value::as_str).unwrap_or("tool");
            let running = ty == "session.tool.input.started";
            send(BackendEvent::PartUpdated {
                harness: h,
                native_id: sid,
                msg_id: msg_id(),
                part: RenderPart {
                    part_id: if call.is_empty() {
                        format!(
                            "c{}",
                            props.get("timestamp").and_then(Value::as_i64).unwrap_or(0)
                        )
                    } else {
                        call
                    },
                    kind: PartKind::Tool,
                    text: if running {
                        format!("⚙ {name}…")
                    } else {
                        format!("⚙ {name}")
                    },
                },
            });
        }
        "session.tool.success" => {
            let call = props
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // name isn't in this event; reuse call id, look up is done in UI by part id
            send(BackendEvent::PartUpdated {
                harness: h,
                native_id: sid,
                msg_id: msg_id(),
                part: RenderPart {
                    part_id: call,
                    kind: PartKind::Tool,
                    text: "✓ tool".to_string(),
                },
            });
        }
        "session.tool.error" => {
            let call = props
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let err = props
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| props.get("error").and_then(Value::as_str))
                .unwrap_or("error");
            send(BackendEvent::PartUpdated {
                harness: h,
                native_id: sid,
                msg_id: msg_id(),
                part: RenderPart {
                    part_id: call,
                    kind: PartKind::Tool,
                    text: format!("⚠ {err}"),
                },
            });
        }
        "session.execution.succeeded" => {
            send(BackendEvent::StreamDone {
                harness: h,
                native_id: sid,
            });
        }
        "session.execution.failed" => {
            let msg = props
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| props.get("message").and_then(Value::as_str))
                .unwrap_or("execution failed")
                .to_string();
            send(BackendEvent::StreamError {
                harness: h.clone(),
                native_id: sid.clone(),
                error: msg,
            });
            send(BackendEvent::StreamDone {
                harness: h,
                native_id: sid,
            });
        }
        "session.error" => {
            let msg = props
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| props.get("message").and_then(Value::as_str))
                .unwrap_or("unknown harness error")
                .to_string();
            if !sid.is_empty() {
                send(BackendEvent::StreamError {
                    harness: h.clone(),
                    native_id: sid.clone(),
                    error: msg,
                });
                send(BackendEvent::StreamDone {
                    harness: h,
                    native_id: sid,
                });
            } else {
                send(BackendEvent::Error {
                    harness: h,
                    error: msg,
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Mutex<HashSet<String>> {
        let mut s = HashSet::new();
        s.insert("ses_1".into());
        Mutex::new(s)
    }

    #[test]
    fn sse_text_delta_maps() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let known = known();
        let data = r#"{"id":"e1","type":"session.text.delta","data":{"sessionID":"ses_1","assistantMessageID":"m1","ordinal":0,"delta":"hello"}}"#;
        handle_sse_event(&tx, &known, "oc", data);
        match rx.try_recv().unwrap() {
            BackendEvent::PartDelta {
                delta,
                part_id,
                kind,
                ..
            } => {
                assert_eq!(delta, "hello");
                assert_eq!(part_id, "t0");
                assert_eq!(kind, PartKind::Text);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sse_filters_foreign_sessions() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let known = known();
        let data =
            r#"{"id":"e2","type":"session.execution.succeeded","data":{"sessionID":"ses_other"}}"#;
        handle_sse_event(&tx, &known, "oc", data);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sse_done_signal() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let known = known();
        let data =
            r#"{"id":"e3","type":"session.execution.succeeded","data":{"sessionID":"ses_1"}}"#;
        handle_sse_event(&tx, &known, "oc", data);
        match rx.try_recv().unwrap() {
            BackendEvent::StreamDone { native_id, .. } => assert_eq!(native_id, "ses_1"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sse_reasoning_delta_maps() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let known = known();
        let data = r#"{"id":"e4","type":"session.reasoning.delta","data":{"sessionID":"ses_1","assistantMessageID":"m1","ordinal":0,"delta":"thinking..."}}"#;
        handle_sse_event(&tx, &known, "oc", data);
        match rx.try_recv().unwrap() {
            BackendEvent::PartDelta { kind, delta, .. } => {
                assert_eq!(kind, PartKind::Reasoning);
                assert_eq!(delta, "thinking...");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
