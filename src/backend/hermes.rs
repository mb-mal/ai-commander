//! Hermes Agent backend — verified against the local `hermes` CLI.
//!
//! Drives the `hermes chat` subprocess per message:
//!   hermes chat --query-file - --format stream-json --oneshot -Q [--yolo]
//!              [--resume <session>] [--image p]… [-m model --provider p]
//! The task text goes over **stdin** (`--query-file -`): argv would break on
//! messages starting with `-` and on the 32k Windows command-line limit.
//! stdout carries NDJSON events (incremental `text` deltas, final `result`
//! with the real session id); stderr is drained into a small ring buffer so
//! the pipe never deadlocks and errors reach the UI.
//!
//! The store is a single source of truth shared with the reader thread
//! (`Arc<Mutex<…>>`), written atomically (temp file + rename). All UI events
//! carry the session id the UI already knows (pending-* on the first turn),
//! and `SessionCreated` announces the real id for rebinding — the exact
//! scenario that previously lost history and wedged the UI forever.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::{
    AgentInfo, AttachmentMeta, Backend, BackendEvent, ChatMsg, MAX_ATTACHMENT_BYTES, ModelInfo,
    PartKind, RenderPart, Role, SendOpts, SessionInfo, kill_tree,
};
use crate::config::HarnessCfg;

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
struct HermesStore {
    sessions: Vec<StoredSession>,
}

pub struct HermesBackend {
    cfg: HarnessCfg,
    events: UnboundedSender<BackendEvent>,
    store_path: PathBuf,
    store: Arc<Mutex<HermesStore>>,
    child: Mutex<Option<Child>>,
    connected: AtomicBool,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Atomic persist: write a temp file next to the target, then rename.
fn persist_atomic(path: &std::path::Path, store: &HermesStore) {
    let Ok(raw) = serde_json::to_vec_pretty(store) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &raw).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

impl HermesBackend {
    pub fn new(cfg: HarnessCfg, events: UnboundedSender<BackendEvent>) -> Self {
        let store_path =
            crate::config::config_dir().join(format!("harness-{}-sessions.json", cfg.id));
        let store = std::fs::read(&store_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        HermesBackend {
            cfg,
            events,
            store_path,
            store: Arc::new(Mutex::new(store)),
            child: Mutex::new(None),
            connected: AtomicBool::new(false),
        }
    }

    /// Path to the hermes launcher: config override → PATH → known layouts.
    fn exe_path(&self) -> Result<PathBuf> {
        if !self.cfg.server_url.is_empty() {
            let p = PathBuf::from(&self.cfg.server_url);
            if p.exists() {
                return Ok(p);
            }
        }
        if let Ok(p) = which::which("hermes") {
            return Ok(p);
        }
        #[cfg(windows)]
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            for name in ["hermes.exe", "hermes"] {
                let candidate = PathBuf::from(&local).join("hermes").join("bin").join(name);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let candidate = PathBuf::from(&home)
                .join(".hermes")
                .join("bin")
                .join("hermes");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        bail!("hermes executable not found; install hermes-agent or set the exe path in settings")
    }

    fn build_command(&self, args: &[&str]) -> Command {
        let exe = self.exe_path().unwrap_or_else(|_| PathBuf::from("hermes"));
        let mut cmd = Command::new(exe);
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
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
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
        let out = self
            .build_command(&["--version"])
            .output()
            .map_err(|e| anyhow!("run {}: {e}", exe.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let version = stdout
            .lines()
            .map(str::trim)
            .find(|l| {
                l.chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
                    || l.starts_with('v')
            })
            .unwrap_or("hermes")
            .to_string();
        self.connected.store(true, Ordering::Relaxed);
        Ok(version)
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
                msg_id: format!("hm{i}"),
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
            if a.mime.starts_with("image/") && a.size > MAX_ATTACHMENT_BYTES {
                bail!(
                    "attachment {} is {} — over the {} MB limit",
                    a.filename,
                    a.size_label(),
                    MAX_ATTACHMENT_BYTES / (1024 * 1024)
                );
            }
        }

        let mut args: Vec<String> = vec![
            "chat".into(),
            "--query-file".into(),
            "-".into(), // task text over stdin, not argv
            "--format".into(),
            "stream-json".into(),
            "--oneshot".into(),
            "-Q".into(),
        ];
        if self.cfg.auto_approve {
            // unattended tool execution — opt-in only, see README
            args.push("--yolo".into());
        }
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
        // Н3: prefer the task's worktree cwd over the harness default
        if let Some(cwd) = opts.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| anyhow!("spawn hermes chat: {e}"))?;
        let mut stdin = child.stdin.take();
        if let Some(si) = stdin.as_mut() {
            let _ = si.write_all(text.as_bytes());
            let _ = si.flush();
        }
        drop(stdin);
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take();
        *self.child.lock().unwrap() = Some(child);

        // drain stderr so the pipe never fills; keep a tail for error reports
        let err_tail = Arc::new(Mutex::new(Vec::<String>::new()));
        if let Some(se) = stderr {
            let err_tail = Arc::clone(&err_tail);
            std::thread::spawn(move || {
                let r = std::io::BufReader::new(se);
                for line in r.lines().map_while(Result::ok) {
                    let mut t = err_tail.lock().unwrap();
                    t.push(line);
                    let len = t.len();
                    t.drain(..len.saturating_sub(5));
                }
            });
        }

        let events = self.events.clone();
        let harness = self.cfg.id.clone();
        let sent_id = native_id.to_string();
        let user_text = text.to_string();
        let store = Arc::clone(&self.store);
        let store_path = self.store_path.clone();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            let mut deltas = String::new();
            let mut result_sid: Option<String> = None;
            let mut result_text: Option<String> = None;
            let mut exit_error: Option<String> = None;

            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Some(v) = parse_json_line(&line) else {
                    continue;
                };
                let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "text" => {
                        let delta = v.get("text").and_then(Value::as_str).unwrap_or("");
                        if !delta.is_empty() {
                            deltas.push_str(delta);
                            let _ = events.send(BackendEvent::PartDelta {
                                harness: harness.clone(),
                                native_id: sent_id.clone(),
                                msg_id: format!("hm_{sent_id}"),
                                part_id: "t0".into(),
                                kind: PartKind::Text,
                                delta: delta.to_string(),
                            });
                        }
                    }
                    "result" => {
                        result_sid = v
                            .get("session_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        result_text = Some(
                            v.get("text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        );
                        let code = v.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
                        if code != 0 {
                            exit_error = Some(format!(
                                "hermes exited with code {code}: {}",
                                err_tail.lock().unwrap().join(" | ")
                            ));
                        }
                    }
                    "system" => {}
                    _ => {}
                }
            }

            // the process ended: settle the ONE shared store, then signal completion
            let assistant_text = result_text.clone().unwrap_or(deltas.clone());
            let real_id = result_sid.clone().unwrap_or_default();
            {
                let mut g = store.lock().unwrap();
                match g.sessions.iter_mut().find(|s| s.native_id == sent_id) {
                    Some(s) => {
                        if !real_id.is_empty() {
                            s.native_id = real_id.clone();
                        }
                        s.msgs.push(StoredMsg {
                            role: "user".into(),
                            text: user_text.clone(),
                        });
                        s.msgs.push(StoredMsg {
                            role: "assistant".into(),
                            text: assistant_text.clone(),
                        });
                        s.updated_ms = now_ms();
                    }
                    None => {
                        g.sessions.push(StoredSession {
                            native_id: if real_id.is_empty() {
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
                                    text: assistant_text.clone(),
                                },
                            ],
                        });
                    }
                }
                persist_atomic(&store_path, &g);
            }

            crate::log_info!(
                "hermes",
                "turn finished: real_id={real_id:?} bytes={}",
                assistant_text.len()
            );
            if let Some(err) = exit_error {
                let _ = events.send(BackendEvent::StreamError {
                    harness: harness.clone(),
                    native_id: sent_id.clone(),
                    error: err,
                });
            } else if result_text.is_none() && deltas.is_empty() {
                let tail = err_tail.lock().unwrap().join(" | ");
                let _ = events.send(BackendEvent::StreamError {
                    harness: harness.clone(),
                    native_id: sent_id.clone(),
                    error: if tail.is_empty() {
                        "hermes finished without any output".to_string()
                    } else {
                        tail
                    },
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

    /// Esc really terminates the running hermes turn (whole process tree).
    async fn abort(&self, _native_id: &str) -> Result<()> {
        if let Some(mut ch) = self.child.lock().unwrap().take() {
            kill_tree(&mut ch);
        }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        #[cfg(windows)]
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(&local)
                    .join("hermes")
                    .join("provider_models_cache.json"),
            );
        }
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            candidates.push(
                PathBuf::from(&home)
                    .join(".hermes")
                    .join("provider_models_cache.json"),
            );
            candidates.push(
                PathBuf::from(&home)
                    .join(".local")
                    .join("share")
                    .join("hermes")
                    .join("provider_models_cache.json"),
            );
        }
        let path = candidates
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| anyhow!("hermes provider models cache not found"))?;
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
        out.sort_by(|a, b| {
            a.provider_id
                .cmp(&b.provider_id)
                .then(a.model_id.cmp(&b.model_id))
        });
        Ok(out)
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

/// Parse one stdout line if it is JSON (noise like "Session: x" is skipped).
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
        let v = parse_json_line(
            r#"{"type":"result","session_id":"local-abc","text":"OK","exit_code":0}"#,
        )
        .unwrap();
        assert_eq!(
            v.get("session_id").and_then(Value::as_str),
            Some("local-abc")
        );
    }

    #[test]
    fn skips_noise_lines() {
        assert!(parse_json_line("Session: local-x").is_none());
        assert!(parse_json_line("session_id: local-x").is_none());
        assert!(parse_json_line("").is_none());
        assert!(parse_json_line("not json {").is_none());
    }

    #[test]
    fn store_persists_atomically() {
        let dir = std::env::temp_dir().join(format!("aic-hermes-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        let mut st = HermesStore::default();
        st.sessions.push(StoredSession {
            native_id: "local-1".into(),
            title: "demo".into(),
            updated_ms: 42,
            msgs: vec![StoredMsg {
                role: "user".into(),
                text: "hi".into(),
            }],
        });
        persist_atomic(&path, &st);
        let back: HermesStore = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.sessions[0].msgs[0].text, "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
