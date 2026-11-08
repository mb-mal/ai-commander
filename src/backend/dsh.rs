//! DeepSeek Harness (dsh) backend — verified against @deepseek-ai/dsh 0.2.2-rc.1.
//!
//! dsh is a profile-based CLI; ai-commander drives the headless profile:
//!   dsh --profile headless --json [--session-id <id>] -   (task over stdin)
//! stdout is NDJSON run events:
//!   {"type":"session","sessionId":"session-…","cwd":…}          (first line)
//!   {"type":"thinking","text":"…"}                              (reasoning, replace)
//!   {"type":"text","text":"…"}                                  (answer, cumulative)
//!   {"type":"status","phase":"turn_end","reason":{"kind":…}}
//!   {"type":"final","sessionId":"…","reason":{…},"text":"…"}
//! Caveat (0.2.2-rc.1): `--session-id` adopts the session but does NOT feed
//! prior turns into the model context — so this backend replays the stored
//! conversation inside the task text. The DEEPSEEK_API_KEY comes from the
//! environment or the local opencode auth store. Store handling mirrors
//! hermes.rs: one shared `Arc<Mutex<…>>` source of truth, atomic writes,
//! UI events carry the id the UI already knows.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::{kill_tree, AgentInfo, AttachmentMeta, Backend, BackendEvent, ChatMsg, ModelInfo, PartKind, RenderPart, Role, SendOpts, SessionInfo};
use crate::config::HarnessCfg;

const MAX_HISTORY_TURNS: usize = 30;

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
struct DshStore {
    sessions: Vec<StoredSession>,
}

pub struct DshBackend {
    cfg: HarnessCfg,
    events: UnboundedSender<BackendEvent>,
    store_path: PathBuf,
    store: Arc<Mutex<DshStore>>,
    child: Mutex<Option<Child>>,
    connected: AtomicBool,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn persist_atomic(path: &std::path::Path, store: &DshStore) {
    let Ok(raw) = serde_json::to_vec_pretty(store) else { return };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &raw).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Fold the stored conversation into the task text (headless resume does not
/// replay prior turns into the model context on its own).
fn compose_task(history: &[StoredMsg], text: &str) -> String {
    if history.is_empty() {
        return text.to_string();
    }
    let tail = &history[history.len().saturating_sub(MAX_HISTORY_TURNS * 2)..];
    let mut out = String::from("[Conversation so far — for context]\n");
    for m in tail {
        out.push_str(if m.role == "user" { "User: " } else { "Assistant: " });
        out.push_str(&m.text);
        out.push_str("\n\n");
    }
    out.push_str("[New message — answer only to this]\n");
    out.push_str(text);
    out
}

impl DshBackend {
    pub fn new(cfg: HarnessCfg, events: UnboundedSender<BackendEvent>) -> Self {
        let store_path = crate::config::config_dir().join(format!("harness-{}-sessions.json", cfg.id));
        let store = std::fs::read(&store_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        DshBackend {
            cfg,
            events,
            store_path,
            store: Arc::new(Mutex::new(store)),
            child: Mutex::new(None),
            connected: AtomicBool::new(false),
        }
    }

    /// Locate the dsh entry point: prefer a direct `node …/dsh/lib/bin.js`
    /// (clean child kill), fall back to `cmd /c npx …` on Windows.
    fn launch(&self) -> (PathBuf, Vec<String>) {
        if !self.cfg.server_url.is_empty() {
            let p = PathBuf::from(&self.cfg.server_url);
            if p.exists() {
                return (node_exe(), vec![p.to_string_lossy().into_owned()]);
            }
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        #[cfg(windows)]
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(&local).join("npm-cache").join("_npx"));
        }
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            candidates.push(PathBuf::from(&home).join(".npm").join("_npx"));
        }
        for npx_dir in candidates {
            if let Ok(entries) = std::fs::read_dir(&npx_dir) {
                for e in entries.flatten() {
                    let bin = e.path()
                        .join("node_modules")
                        .join("@deepseek-ai")
                        .join("dsh")
                        .join("lib")
                        .join("bin.js");
                    if bin.exists() {
                        return (node_exe(), vec![bin.to_string_lossy().into_owned()]);
                    }
                }
            }
        }
        // global npm layout
        if let Some(prefix) = npm_prefix() {
            let bin = prefix
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            if bin.exists() {
                return (node_exe(), vec![bin.to_string_lossy().into_owned()]);
            }
        }
        (
            PathBuf::from("npx"),
            vec!["-y".into(), "@deepseek-ai/dsh".into()],
        )
    }

    fn api_key(&self) -> Option<String> {
        if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") {
            if !k.is_empty() {
                return Some(k);
            }
        }
        // reuse the key stored by opencode (~/.local/share/opencode/auth.json → deepseek.key)
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
        let auth = PathBuf::from(&home)
            .join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json");
        let raw = std::fs::read(auth).ok()?;
        let v: Value = serde_json::from_slice(&raw).ok()?;
        v.get("deepseek")
            .and_then(|d| d.get("key"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|k| !k.is_empty())
    }

    fn build_command(&self, extra: &[String]) -> Command {
        let (exe, prefix) = self.launch();
        let mut cmd = Command::new(&exe);
        let mut args: Vec<String> = prefix;
        args.push("--profile".into());
        args.push("headless".into());
        args.push("--json".into());
        args.extend(extra.iter().cloned());
        cmd.args(&args);
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
        if let Some(key) = self.api_key() {
            cmd.env("DEEPSEEK_API_KEY", key);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }
}

fn node_exe() -> PathBuf {
    if let Ok(p) = std::env::var("NODE") {
        return PathBuf::from(p);
    }
    PathBuf::from("node")
}

fn npm_prefix() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("npm"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".npm-global"))
    }
}

#[async_trait]
impl Backend for DshBackend {
    fn harness_id(&self) -> &str {
        &self.cfg.id
    }

    async fn connect(&self) -> Result<String> {
        if self.api_key().is_none() {
            bail!("DEEPSEEK_API_KEY not set and no deepseek key found in the opencode auth store; set the key in the environment or configure opencode");
        }
        self.connected.store(true, Ordering::Relaxed);
        Ok("0.2.x".into())
    }

    async fn refresh_sessions(&self) -> Result<Vec<SessionInfo>> {
        Ok(self
            .store
            .lock()
            .unwrap()
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
        let mut g = self.store.lock().unwrap();
        g.sessions.push(s);
        persist_atomic(&self.store_path, &g);
        Ok(info)
    }

    async fn rename_session(&self, native_id: &str, title: &str) -> Result<()> {
        let mut g = self.store.lock().unwrap();
        if let Some(s) = g.sessions.iter_mut().find(|s| s.native_id == native_id) {
            s.title = title.to_string();
        }
        persist_atomic(&self.store_path, &g);
        Ok(())
    }

    async fn delete_session(&self, native_id: &str) -> Result<()> {
        let mut g = self.store.lock().unwrap();
        g.sessions.retain(|s| s.native_id != native_id);
        persist_atomic(&self.store_path, &g);
        Ok(())
    }

    async fn load_history(&self, native_id: &str) -> Result<Vec<ChatMsg>> {
        let g = self.store.lock().unwrap();
        let Some(s) = g.sessions.iter().find(|s| s.native_id == native_id) else {
            return Ok(Vec::new());
        };
        Ok(s.msgs
            .iter()
            .enumerate()
            .map(|(i, m)| ChatMsg {
                msg_id: format!("dsh{i}"),
                role: if m.role == "user" { Role::User } else { Role::Assistant },
                parts: vec![RenderPart {
                    part_id: "t0".into(),
                    kind: PartKind::Text,
                    text: m.text.clone(),
                }],
                attachments: Vec::new(),
            })
            .collect())
    }

    async fn send(&self, native_id: &str, text: &str, _attachments: &[AttachmentMeta], _opts: SendOpts<'_>) -> Result<()> {
        // headless dsh has no attachment flag yet
        let history: Vec<StoredMsg> = self
            .store
            .lock()
            .unwrap()
            .sessions
            .iter()
            .find(|s| s.native_id == native_id)
            .map(|s| s.msgs.clone())
            .unwrap_or_default();
        let task = compose_task(&history, text);

        let mut extra: Vec<String> = Vec::new();
        if !native_id.is_empty() && !native_id.starts_with("pending-") {
            extra.push("--session-id".into());
            extra.push(native_id.to_string());
        }
        extra.push("-".into()); // task over stdin

        let mut child = self
            .build_command(&extra)
            .spawn()
            .map_err(|e| anyhow!("spawn dsh headless: {e}"))?;
        let mut stdin = child.stdin.take();
        if let Some(si) = stdin.as_mut() {
            let _ = si.write_all(task.as_bytes());
            let _ = si.flush();
        }
        drop(stdin);
        let stdout = child.stdout.take().expect("piped stdout");
        *self.child.lock().unwrap() = Some(child);

        let events = self.events.clone();
        let harness = self.cfg.id.clone();
        let sent_id = native_id.to_string();
        let user_text = text.to_string();
        let store = Arc::clone(&self.store);
        let store_path = self.store_path.clone();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            let mut session_id: Option<String> = None;
            let mut final_text = String::new();
            let mut error: Option<String> = None;
            let msg_key = |sid: &Option<String>| format!("dsh_{}", sid.clone().unwrap_or_else(|| sent_id.clone()));

            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Some(v) = parse_json_line(&line) else { continue };
                let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "session" => {
                        session_id = v.get("sessionId").and_then(Value::as_str).map(str::to_owned);
                    }
                    "thinking" => {
                        let t = v.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            let _ = events.send(BackendEvent::PartUpdated {
                                harness: harness.clone(),
                                native_id: sent_id.clone(),
                                msg_id: msg_key(&session_id),
                                part: RenderPart {
                                    part_id: "r0".into(),
                                    kind: PartKind::Reasoning,
                                    text: t.to_string(),
                                },
                            });
                        }
                    }
                    "text" => {
                        // cumulative replace, not deltas
                        let t = v.get("text").and_then(Value::as_str).unwrap_or("");
                        let _ = events.send(BackendEvent::PartUpdated {
                            harness: harness.clone(),
                            native_id: sent_id.clone(),
                            msg_id: msg_key(&session_id),
                            part: RenderPart {
                                part_id: "t0".into(),
                                kind: PartKind::Text,
                                text: t.to_string(),
                            },
                        });
                    }
                    "final" => {
                        final_text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                        if session_id.is_none() {
                            session_id = v.get("sessionId").and_then(Value::as_str).map(str::to_owned);
                        }
                        let reason = v.pointer("/reason/kind").and_then(Value::as_str).unwrap_or("");
                        if reason != "completed" {
                            error = v
                                .pointer("/reason/error/message")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .or_else(|| Some(format!("turn ended: {reason}")));
                        }
                    }
                    _ => {}
                }
            }

            // child reaping happens on the next send/shutdown via the slot
            let real_id = session_id.clone().unwrap_or_default();
            {
                let mut g = store.lock().unwrap();
                match g.sessions.iter_mut().find(|s| s.native_id == sent_id) {
                    Some(s) => {
                        if !real_id.is_empty() {
                            s.native_id = real_id.clone();
                        }
                        s.msgs.push(StoredMsg { role: "user".into(), text: user_text.clone() });
                        s.msgs.push(StoredMsg { role: "assistant".into(), text: final_text.clone() });
                        s.updated_ms = now_ms();
                    }
                    None => {
                        g.sessions.push(StoredSession {
                            native_id: if real_id.is_empty() { sent_id.clone() } else { real_id.clone() },
                            title: user_text.chars().take(48).collect(),
                            updated_ms: now_ms(),
                            msgs: vec![
                                StoredMsg { role: "user".into(), text: user_text.clone() },
                                StoredMsg { role: "assistant".into(), text: final_text.clone() },
                            ],
                        });
                    }
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
            if !real_id.is_empty() && real_id != sent_id {
                let _ = events.send(BackendEvent::SessionCreated {
                    harness: harness.clone(),
                    native_id: real_id,
                    title: user_text.chars().take(48).collect(),
                });
            }
            let _ = events.send(BackendEvent::StreamDone {
                harness,
                native_id: sent_id, // the id the UI tracks
            });
        });
        Ok(())
    }

    async fn abort(&self, _native_id: &str) -> Result<()> {
        if let Some(mut ch) = self.child.lock().unwrap().take() {
            kill_tree(&mut ch);
        }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // dsh routes through a single "deepseek-official" provider
        Ok(vec![ModelInfo {
            provider_id: "deepseek-official".into(),
            model_id: "deepseek-chat".into(),
            name: "DeepSeek (default route)".into(),
        }])
    }

    async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        Ok(Vec::new())
    }

    fn shutdown(&self) {
        self.connected.store(false, Ordering::Relaxed);
        if let Some(mut ch) = self.child.lock().unwrap().take() {
            kill_tree(&mut ch);
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

fn parse_json_line(line: &str) -> Option<Value> {
    let t = line.trim();
    if !t.starts_with('{') {
        return None;
    }
    serde_json::from_str(t).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events() {
        let v = parse_json_line(r#"{"type":"session","sessionId":"session-x","cwd":"C:\\"}"#).unwrap();
        assert_eq!(v.get("sessionId").and_then(Value::as_str), Some("session-x"));

        let v = parse_json_line(r#"{"type":"final","reason":{"kind":"completed"},"text":"ok"}"#).unwrap();
        assert_eq!(v.pointer("/reason/kind").and_then(Value::as_str), Some("completed"));
    }

    #[test]
    fn skips_noise() {
        assert!(parse_json_line("npm warn something").is_none());
        assert!(parse_json_line("").is_none());
    }

    #[test]
    fn compose_task_replays_history() {
        let h = vec![
            StoredMsg { role: "user".into(), text: "hi".into() },
            StoredMsg { role: "assistant".into(), text: "hello".into() },
        ];
        let t = compose_task(&h, "next?");
        assert!(t.contains("User: hi"));
        assert!(t.contains("Assistant: hello"));
        assert!(t.contains("[New message"));
        assert_eq!(compose_task(&[], "solo"), "solo");
    }
}
