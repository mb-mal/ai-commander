//! Hermes Agent backend (verified against the local `hermes` CLI).
//!
//! Hermes is a CLI harness, not a server, so this backend drives the
//! `hermes chat` subprocess per message:
//!   hermes chat -q <text> --format stream-json --oneshot -Q --yolo
//!              [--resume <session>] [--image p]… [-m model --provider p] [--in dir]
//! stdout carries NDJSON events:
//!   {"type":"system","subtype":"init",…}
//!   {"type":"text","text":"<delta>"}            (incremental deltas)
//!   {"type":"result","session_id":"local-…","text":"<full>","exit_code":0,tokens}
//! plus non-JSON noise lines ("Session: …", "session_id: …") which are ignored.
//!
//! Hermes has no session-list/history API, so ai-commander keeps its own
//! registry + accumulated history per harness in a small JSON store
//! (`<config_dir>/harness-<id>-sessions.json`). Esc really kills the child.

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

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct StoredMsg {
    role: String, // "user" | "assistant"
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
struct HermesStore {
    sessions: Vec<StoredSession>,
}

pub struct HermesBackend {
    cfg: HarnessCfg,
    events: UnboundedSender<BackendEvent>,
    store_path: PathBuf,
    store: Mutex<HermesStore>,
    /// (native session id of the running turn, child process) — abort kills it
    child: Arc<Mutex<Option<(String, Child)>>>,
    connected: AtomicBool,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl HermesBackend {
    pub fn new(cfg: HarnessCfg, events: UnboundedSender<BackendEvent>) -> Self {
        let store_path = crate::config::config_dir().join(format!("harness-{}-sessions.json", cfg.id));
        let store = std::fs::read(&store_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        HermesBackend {
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

    fn exe_path(&self) -> Result<PathBuf> {
        // explicit override via the harness "server_url" field (acts as exe path)
        if !self.cfg.server_url.is_empty() {
            let p = PathBuf::from(&self.cfg.server_url);
            if p.exists() {
                return Ok(p);
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let candidate = PathBuf::from(&local).join("hermes").join("bin").join("hermes.exe");
            if candidate.exists() {
                return Ok(candidate);
            }
            let candidate = PathBuf::from(&local).join("hermes").join("bin").join("hermes");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        Ok(PathBuf::from("hermes"))
    }

    fn build_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(self.exe_path().unwrap_or_else(|_| PathBuf::from("hermes")));
        cmd.args(args);
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
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }
}

#[async_trait]
impl Backend for HermesBackend {
    fn harness_id(&self) -> &str {
        &self.cfg.id
    }

    async fn connect(&self) -> Result<String> {
        let exe = self.exe_path()?;
        if !exe.exists() {
            bail!("hermes executable not found (looked at {:?}); install hermes-agent or set the harness exe path in settings", exe);
        }
        // hermes --version prints something like "hermes-agent 1.2.3"
        let out = self
            .build_command(&["--version"])
            .output()
            .map_err(|e| anyhow!("run {}: {e}", exe.display()))?;
        // output may include banners ("Update available…"); keep the semver-ish line
        let stdout = String::from_utf8_lossy(&out.stdout);
        let version = stdout
            .lines()
            .map(str::trim)
            .find(|l| l.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) || l.starts_with('v'))
            .unwrap_or("hermes")
            .to_string();
        self.connected.store(true, Ordering::Relaxed);
        Ok(version)
    }

    async fn refresh_sessions(&self) -> Result<Vec<SessionInfo>> {
        let list: Vec<SessionInfo> = self
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
            .collect();
        Ok(list)
    }

    async fn create_session(&self, title: &str) -> Result<SessionInfo> {
        // real native id arrives with the first `result` event; for now a local placeholder
        let placeholder = format!("pending-{}", now_ms());
        let s = StoredSession {
            native_id: placeholder,
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
                msg_id: format!("hm{i}"),
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
        let mut args: Vec<String> = vec![
            "chat".into(),
            "-q".into(),
            text.to_string(),
            "--format".into(),
            "stream-json".into(),
            "--oneshot".into(),
            "-Q".into(),
            "--yolo".into(),
        ];
        // images attach via --image; other file kinds are not supported by the CLI
        for a in attachments {
            if a.mime.starts_with("image/") {
                args.push("--image".into());
                args.push(a.path.to_string_lossy().into_owned());
            }
        }
        if !native_id.is_empty() && !native_id.starts_with("pending-") {
            args.push("--resume".into());
            args.push(native_id.to_string());
        }
        if let Some(m) = opts.model {
            args.push("-m".into());
            args.push(m.model_id.clone());
            args.push("--provider".into());
            args.push(m.provider_id.clone());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut cmd = self.build_command(&arg_refs);
        let mut child = cmd.spawn().map_err(|e| anyhow!("spawn hermes chat: {e}"))?;
        let stdout = child.stdout.take().expect("piped stdout");
        *self.child.lock().unwrap() = Some((native_id.to_string(), child));

        // spawn a blocking reader that forwards parsed events into the channel
        let events = self.events.clone();
        let harness = self.cfg.id.clone();
        let native_id = native_id.to_string();
        let user_text = text.to_string();
        let store_path = self.store_path.clone();
        let child_slot = Arc::clone(&self.child);
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            let mut assistant_text = String::new();
            let mut result_sid: Option<String> = None;
            let mut had_error: Option<String> = None;
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Some(v) = parse_json_line(&line) else { continue };
                let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "text" => {
                        let delta = v.get("text").and_then(Value::as_str).unwrap_or("");
                        if !delta.is_empty() {
                            let _ = events.send(BackendEvent::PartDelta {
                                harness: harness.clone(),
                                native_id: native_id.clone(),
                                msg_id: format!("hm_{native_id}"),
                                part_id: "t0".into(),
                                kind: PartKind::Text,
                                delta: delta.to_string(),
                            });
                        }
                    }
                    "result" => {
                        result_sid = v.get("session_id").and_then(Value::as_str).map(str::to_owned);
                        let full = v.get("text").and_then(Value::as_str).unwrap_or("");
                        assistant_text = full.to_string();
                        let code = v.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
                        if code != 0 {
                            had_error = Some(format!("exit code {code}"));
                        }
                        let _ = events.send(BackendEvent::PartUpdated {
                            harness: harness.clone(),
                            native_id: native_id.clone(),
                            msg_id: format!("hm_{native_id}"),
                            part: RenderPart {
                                part_id: "t0".into(),
                                kind: PartKind::Text,
                                text: full.to_string(),
                            },
                        });
                    }
                    "system" => {}
                    other => {
                        // tool-ish events surface as one-line activity entries
                        let name = v
                            .get("name")
                            .or_else(|| v.get("tool"))
                            .and_then(Value::as_str)
                            .unwrap_or(other);
                        let _ = events.send(BackendEvent::PartUpdated {
                            harness: harness.clone(),
                            native_id: native_id.clone(),
                            msg_id: format!("hm_{native_id}"),
                            part: RenderPart {
                                part_id: format!("tool_{}", v.get("id").and_then(Value::as_str).unwrap_or(other)),
                                kind: PartKind::Tool,
                                text: format!("⚙ {name}"),
                            },
                        });
                    }
                }
            }
            // reap the child
            if let Some((_, mut ch)) = child_slot.lock().unwrap().take() {
                let _ = ch.wait();
            }
            // persist the turn into the local registry
            let real_id = result_sid.unwrap_or_default();
            let mut store: HermesStore = std::fs::read(&store_path)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .unwrap_or_default();
            let sid_for_store = if !real_id.is_empty() { real_id.clone() } else { native_id.clone() };
            let entry = store.sessions.iter_mut().find(|s| s.native_id == native_id);
            match entry {
                Some(s) => {
                    if !real_id.is_empty() {
                        s.native_id = real_id.clone();
                    }
                    s.msgs.push(StoredMsg { role: "user".into(), text: user_text.clone() });
                    s.msgs.push(StoredMsg { role: "assistant".into(), text: assistant_text.clone() });
                    s.updated_ms = now_ms();
                }
                None => {
                    store.sessions.push(StoredSession {
                        native_id: sid_for_store.clone(),
                        title: user_text.chars().take(48).collect(),
                        updated_ms: now_ms(),
                        msgs: vec![
                            StoredMsg { role: "user".into(), text: user_text.clone() },
                            StoredMsg { role: "assistant".into(), text: assistant_text.clone() },
                        ],
                    });
                }
            }
            let _ = std::fs::write(&store_path, serde_json::to_vec_pretty(&store).unwrap_or_default());

            if let Some(err) = had_error {
                let _ = events.send(BackendEvent::StreamError {
                    harness: harness.clone(),
                    native_id: if !real_id.is_empty() { real_id.clone() } else { native_id.clone() },
                    error: err,
                });
            }
            // the UI tracks the session under the id it sent; if the server minted
            // a new one (first turn), tell it about the rename
            if !real_id.is_empty() && real_id != native_id {
                let _ = events.send(BackendEvent::SessionCreated {
                    harness: harness.clone(),
                    native_id: real_id.clone(),
                    title: user_text.chars().take(48).collect(),
                });
            }
            let _ = events.send(BackendEvent::StreamDone {
                harness,
                native_id: if !real_id.is_empty() { real_id } else { native_id },
            });
        });
        Ok(())
    }

    /// Esc really terminates the running hermes turn.
    async fn abort(&self, native_id: &str) -> Result<()> {
        if let Some((sid, mut ch)) = self.child.lock().unwrap().take() {
            let _ = &sid;
            let _ = ch.kill();
            let _ = ch.wait();
        }
        let _ = native_id;
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // hermes caches the live provider catalog here
        let path = std::env::var_os("LOCALAPPDATA")
            .map(|l| PathBuf::from(&l).join("hermes").join("provider_models_cache.json"))
            .ok_or_else(|| anyhow!("LOCALAPPDATA not set"))?;
        let raw = std::fs::read(&path).map_err(|e| anyhow!("read {}: {e}", path.display()))?;
        let v: Value = serde_json::from_slice(&raw)?;
        let mut out = Vec::new();
        if let Some(obj) = v.as_object() {
            for (provider, entry) in obj {
                if let Some(models) = entry.get("models").and_then(Value::as_array) {
                    for m in models {
                        if let Some(id) = m.as_str() {
                            out.push(ModelInfo {
                                provider_id: provider.clone(),
                                model_id: id.to_string(),
                                name: id.to_string(),
                            });
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| a.provider_id.cmp(&b.provider_id).then(a.model_id.cmp(&b.model_id)));
        Ok(out)
    }

    async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        Ok(Vec::new()) // CLI has no agent list; the model/provider pair covers it
    }

    fn shutdown(&self) {
        self.connected.store(false, Ordering::Relaxed);
        if let Some((_, mut ch)) = self.child.lock().unwrap().take() {
            let _ = ch.kill();
            let _ = ch.wait();
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Parse one stdout line if it is JSON (noise lines like "Session: x" are skipped).
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
    fn parses_text_delta_and_result() {
        let v = parse_json_line(r#"{"type":"text","text":"ана"}"#).unwrap();
        assert_eq!(v.get("text").and_then(Value::as_str), Some("ана"));

        let v = parse_json_line(r#"{"type":"result","session_id":"local-abc","text":"OK","exit_code":0}"#).unwrap();
        assert_eq!(v.get("session_id").and_then(Value::as_str), Some("local-abc"));
    }

    #[test]
    fn skips_noise_lines() {
        assert!(parse_json_line("Session: local-x").is_none());
        assert!(parse_json_line("session_id: local-x").is_none());
        assert!(parse_json_line("").is_none());
        assert!(parse_json_line("not json {").is_none());
    }

    #[test]
    fn store_roundtrip() {
        let mut st = HermesStore::default();
        st.sessions.push(StoredSession {
            native_id: "local-1".into(),
            title: "demo".into(),
            updated_ms: 42,
            msgs: vec![StoredMsg { role: "user".into(), text: "hi".into() }],
        });
        let raw = serde_json::to_vec(&st).unwrap();
        let back: HermesStore = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.sessions[0].msgs[0].text, "hi");
    }
}
