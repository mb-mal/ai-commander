//! DeepSeek Harness (dsh) backend — verified against @deepseek-ai/dsh 0.2.2-rc.1.
//!
//! dsh is a profile-based CLI; ai-commander drives the headless profile:
//!   dsh --profile headless --json [--session-id <id>] "<task>"
//! stdout is NDJSON run events:
//!   {"type":"session","sessionId":"session-…","cwd":…}          (first line)
//!   {"type":"thinking","text":"…"}                              (reasoning, replace)
//!   {"type":"text","text":"…"}                                  (answer, cumulative)
//!   {"type":"status","phase":"turn_end","reason":{"kind":…}}
//!   {"type":"final","sessionId":"…","reason":{…},"text":"…"}
//! Caveat (0.2.2-rc.1): `--session-id` adopts the session but does NOT feed
//! prior turns into the model context — so this backend replays the stored
//! conversation inside the task text to keep continuity. The DEEPSEEK_API_KEY
//! is taken from the environment, or from the local opencode auth store.

use std::io::BufRead;
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

use super::{
    AgentInfo, AttachmentMeta, Backend, BackendEvent, ChatMsg, ModelInfo, PartKind, RenderPart,
    Role, SendOpts, SessionInfo,
};
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
    store: Mutex<DshStore>,
    child: Arc<Mutex<Option<Child>>>,
    connected: AtomicBool,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
            store: Mutex::new(store),
            child: Arc::new(Mutex::new(None)),
            connected: AtomicBool::new(false),
        }
    }

    fn persist(&self) {
        if let Ok(g) = self.store.lock() {
            if let Ok(raw) = serde_json::to_vec_pretty(&*g) {
                let _ = std::fs::write(&self.store_path, raw);
            }
        }
    }

    /// Locate the dsh entry point: prefer a direct `node …/dsh/lib/bin.js`
    /// (clean child kill), fall back to `cmd /c npx …` (tree kill).
    fn launch(&self) -> (PathBuf, Vec<String>) {
        // explicit override via the harness "server_url" field (dsh bin.js path)
        if !self.cfg.server_url.is_empty() {
            let p = PathBuf::from(&self.cfg.server_url);
            if p.exists() {
                return (node_exe(), vec![p.to_string_lossy().into_owned()]);
            }
        }
        // npx cache layout: %LOCALAPPDATA%\npm-cache\_npx\<hash>\node_modules\@deepseek-ai\dsh\lib\bin.js
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let npx_dir = PathBuf::from(&local).join("npm-cache").join("_npx");
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
        // global npm layout: …\npm\node_modules\@deepseek-ai\dsh\lib\bin.js
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let bin = PathBuf::from(&appdata)
                .join("npm")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh")
                .join("lib")
                .join("bin.js");
            if bin.exists() {
                return (node_exe(), vec![bin.to_string_lossy().into_owned()]);
            }
        }
        // last resort: npx via cmd (kills need the whole tree)
        (
            PathBuf::from("cmd.exe"),
            vec!["/d".into(), "/s".into(), "/c".into(), "npx -y @deepseek-ai/dsh".into()],
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

    fn build_command(&self, task_args: &[String]) -> Command {
        let (exe, mut prefix) = self.launch();
        let mut cmd = Command::new(&exe);
        let mut args: Vec<String> = Vec::new();
        let via_cmd = exe.file_name().map(|f| f == "cmd.exe").unwrap_or(false);
        if via_cmd {
            // cmd /c "npx -y @deepseek-ai/dsh --profile headless …"
            let mut line = prefix.remove(0);
            line.push_str(" --profile headless --json");
            for a in task_args {
                line.push(' ');
                line.push_str(a);
            }
            args.extend(prefix);
            args.push(line);
        } else {
            args.extend(prefix);
            args.push("--profile".into());
            args.push("headless".into());
            args.push("--json".into());
            args.extend(task_args.iter().cloned());
        }
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
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
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
        self.store.lock().unwrap().sessions.push(s);
        self.persist();
        Ok(info)
    }

    async fn rename_session(&self, native_id: &str, title: &str) -> Result<()> {
        if let Some(s) = self.store.lock().unwrap().sessions.iter_mut().find(|s| s.native_id == native_id) {
            s.title = title.to_string();
        }
        self.persist();
        Ok(())
    }

    async fn delete_session(&self, native_id: &str) -> Result<()> {
        self.store.lock().unwrap().sessions.retain(|s| s.native_id != native_id);
        self.persist();
        Ok(())
    }

    async fn load_history(&self, native_id: &str) -> Result<Vec<ChatMsg>> {
        let store = self.store.lock().unwrap();
        let Some(s) = store.sessions.iter().find(|s| s.native_id == native_id) else {
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

    async fn send(&self, native_id: &str, text: &str, attachments: &[AttachmentMeta], opts: SendOpts<'_>) -> Result<()> {
        if !attachments.is_empty() {
            // headless dsh has no attachment flag yet
            let _ = attachments;
        }
        let _ = opts; // single provider route; model selection is done on the dsh side

        // task text = replayed history + new message
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

        let mut task_args: Vec<String> = Vec::new();
        if !native_id.is_empty() && !native_id.starts_with("pending-") {
            task_args.push("--session-id".into());
            task_args.push(native_id.to_string());
        }
        task_args.push(task);

        let mut child = self
            .build_command(&task_args)
            .spawn()
            .map_err(|e| anyhow!("spawn dsh headless: {e}"))?;
        let stdout = child.stdout.take().expect("piped stdout");
        *self.child.lock().unwrap() = Some(child);

        let events = self.events.clone();
        let harness = self.cfg.id.clone();
        let native_id = native_id.to_string();
        let user_text = text.to_string();
        let store_path = self.store_path.clone();
        let child_slot = Arc::clone(&self.child);
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            let mut session_id: Option<String> = None;
            let mut final_text = String::new();
            let mut error: Option<String> = None;
            let msg_key = |sid: &Option<String>| format!("dsh_{}", sid.clone().unwrap_or_else(|| native_id.clone()));

            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Some(v) = parse_json_line(&line) else { continue };
                let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "session" => {
                        session_id = v.get("sessionId").and_then(Value::as_str).map(str::to_owned);
                        if let Some(sid) = &session_id {
                            let _ = events.send(BackendEvent::SessionCreated {
                                harness: harness.clone(),
                                native_id: sid.clone(),
                                title: user_text.chars().take(48).collect(),
                            });
                        }
                    }
                    "thinking" => {
                        let t = v.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            let _ = events.send(BackendEvent::PartUpdated {
                                harness: harness.clone(),
                                native_id: native_id.clone(),
                                msg_id: msg_key(&session_id),
                                part: RenderPart { part_id: "r0".into(), kind: PartKind::Reasoning, text: t.to_string() },
                            });
                        }
                    }
                    "text" => {
                        // cumulative replace, not deltas
                        let t = v.get("text").and_then(Value::as_str).unwrap_or("");
                        let _ = events.send(BackendEvent::PartUpdated {
                            harness: harness.clone(),
                            native_id: native_id.clone(),
                            msg_id: msg_key(&session_id),
                            part: RenderPart { part_id: "t0".into(), kind: PartKind::Text, text: t.to_string() },
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

            if let Some(mut ch) = child_slot.lock().unwrap().take() {
                let _ = ch.kill();
                let _ = ch.wait();
            }

            let real_id = session_id.clone().unwrap_or_default();
            let mut store: DshStore = std::fs::read(&store_path)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .unwrap_or_default();
            match store.sessions.iter_mut().find(|s| s.native_id == native_id) {
                Some(s) => {
                    if !real_id.is_empty() {
                        s.native_id = real_id.clone();
                    }
                    s.msgs.push(StoredMsg { role: "user".into(), text: user_text.clone() });
                    s.msgs.push(StoredMsg { role: "assistant".into(), text: final_text.clone() });
                    s.updated_ms = now_ms();
                }
                None => {
                    store.sessions.push(StoredSession {
                        native_id: if real_id.is_empty() { native_id.clone() } else { real_id.clone() },
                        title: user_text.chars().take(48).collect(),
                        updated_ms: now_ms(),
                        msgs: vec![
                            StoredMsg { role: "user".into(), text: user_text.clone() },
                            StoredMsg { role: "assistant".into(), text: final_text.clone() },
                        ],
                    });
                }
            }
            let _ = std::fs::write(&store_path, serde_json::to_vec_pretty(&store).unwrap_or_default());

            if let Some(err) = error {
                let _ = events.send(BackendEvent::StreamError {
                    harness: harness.clone(),
                    native_id: if real_id.is_empty() { native_id.clone() } else { real_id.clone() },
                    error: err,
                });
            }
            let _ = events.send(BackendEvent::StreamDone {
                harness,
                native_id: if real_id.is_empty() { native_id } else { real_id },
            });
        });
        Ok(())
    }

    async fn abort(&self, _native_id: &str) -> Result<()> {
        if let Some(mut ch) = self.child.lock().unwrap().take() {
            let _ = ch.kill();
            let _ = ch.wait();
        }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // dsh routes through a single "deepseek-official" provider; its model
        // list lives in the web UI, so expose the obvious one.
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
            let _ = ch.kill();
            let _ = ch.wait();
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
