//! Pluggable AI harness backends. A *harness* (opencode, hermes-agent, deepseek
//! harness, …) is a running agent runtime we chat with. All backends stream
//! their updates through a single `BackendEvent` channel tagged with harness id.

pub mod acp;
pub mod dsh;
pub mod hermes;
pub mod opencode;

use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::HarnessCfg;
use crate::config::ModelRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub native_id: String,
    pub title: String,
    pub updated_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub provider_id: String,
    pub model_id: String,
    pub name: String,
}

impl ModelInfo {
    pub fn display(&self) -> String {
        if self.name.is_empty() {
            format!("{}/{}", self.provider_id, self.model_id)
        } else {
            format!("{}/{} — {}", self.provider_id, self.model_id, self.name)
        }
    }

    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        self.display().to_lowercase().contains(&needle)
    }
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub path: PathBuf,
    pub filename: String,
    pub size: u64,
    pub mime: String,
}

impl AttachmentMeta {
    pub fn from_path(path: &Path) -> AttachmentMeta {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        AttachmentMeta {
            path: path.to_path_buf(),
            filename: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            size,
            mime: mime_for(path),
        }
    }

    pub fn size_label(&self) -> String {
        if self.size >= 1_048_576 {
            format!("{:.1}M", self.size as f64 / 1_048_576.0)
        } else if self.size >= 1024 {
            format!("{:.1}k", self.size as f64 / 1024.0)
        } else {
            format!("{}b", self.size)
        }
    }
}

/// MIME by extension — good enough for chat attachments.
pub fn mime_for(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" | "csv" | "toml" | "ini" | "cfg" | "yaml" | "yml" | "json" | "xml"
        | "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "go" | "java"
        | "kt" | "sh" | "bat" | "ps1" | "lua" | "rb" | "php" | "cs" | "html" | "css" | "scss"
        | "sql" | "r" | "m" => "text/plain",
        "mp3" | "wav" | "ogg" | "flac" => "audio/ogg", // generic; opencode decides
        "mp4" | "mkv" | "webm" | "mov" => "video/mp4",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Text,
    Reasoning,
    Tool,
}

#[derive(Debug, Clone)]
pub struct RenderPart {
    pub part_id: String,
    pub kind: PartKind,
    pub text: String,
}

/// A chat message rendered in UI. Assistant messages keep ordered parts
/// (reasoning / tool calls / text) so agent activity is visible.
#[derive(Debug, Clone)]
pub struct ChatMsg {
    pub msg_id: String,
    pub role: Role,
    pub parts: Vec<RenderPart>,
    pub attachments: Vec<AttachmentMeta>,
}

#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// Connected and healthy; carries server version
    Connected { harness: String, version: String },
    /// Session list refresh (all sessions of one harness)
    Sessions {
        harness: String,
        list: Vec<SessionInfo>,
    },
    /// Sessions of this harness changed server-side — UI should re-fetch
    SessionsChanged { harness: String },
    /// A draft chat got a real session after first message
    SessionCreated {
        harness: String,
        native_id: String,
        title: String,
    },
    /// A kanban task got its harness session (spawned by the board runner)
    TaskLinked {
        harness: String,
        task_id: String,
        session_id: String,
    },
    /// The agent asks the user to approve an action (opencode permission flow)
    PermissionRequest {
        harness: String,
        session_id: String,
        request_id: String,
        title: String,
        /// action name (e.g. "bash") for the policy engine
        action: String,
        /// primary resource (path/command) for pattern rules
        resource: String,
    },
    /// SSE stream reconnected after a drop — UI should reload current history
    Reconnected { harness: String },
    /// History loaded for a session
    History {
        harness: String,
        native_id: String,
        msgs: Vec<ChatMsg>,
    },
    /// Streaming delta for one assistant part (append `delta` chars)
    PartDelta {
        harness: String,
        native_id: String,
        msg_id: String,
        part_id: String,
        kind: PartKind,
        delta: String,
    },
    /// A part appeared/changed without a delta (replace text)
    PartUpdated {
        harness: String,
        native_id: String,
        msg_id: String,
        part: RenderPart,
    },
    /// Assistant message finished (or aborted) for the session
    StreamDone { harness: String, native_id: String },
    StreamError {
        harness: String,
        native_id: String,
        error: String,
    },
    Models {
        harness: String,
        models: Vec<ModelInfo>,
    },
    Agents {
        harness: String,
        agents: Vec<AgentInfo>,
    },
    /// Global backend error (connection etc.)
    Error { harness: String, error: String },
}

pub struct SendOpts<'a> {
    pub model: Option<&'a ModelRef>,
    pub agent: Option<&'a str>,
    /// role token: the ACP backend injects the board MCP server bound to it
    pub mcp_role: Option<&'a str>,
    /// Н3: working directory for this session (the task's worktree);
    /// backends pass it as `session/new` cwd / process current_dir
    pub cwd: Option<&'a std::path::Path>,
}

#[async_trait]
#[allow(dead_code)]
pub trait Backend: Send + Sync {
    fn harness_id(&self) -> &str;

    /// Ensure the harness server is running; returns its version.
    async fn connect(&self) -> Result<String>;

    async fn refresh_sessions(&self) -> Result<Vec<SessionInfo>>;
    async fn create_session(&self, title: &str) -> Result<SessionInfo>;
    async fn rename_session(&self, native_id: &str, title: &str) -> Result<()>;
    async fn delete_session(&self, native_id: &str) -> Result<()>;
    async fn load_history(&self, native_id: &str) -> Result<Vec<ChatMsg>>;
    async fn send(
        &self,
        native_id: &str,
        text: &str,
        attachments: &[AttachmentMeta],
        opts: SendOpts<'_>,
    ) -> Result<()>;
    async fn abort(&self, native_id: &str) -> Result<()>;

    /// Answer a PermissionRequest (opencode: once/always/reject).
    async fn permission_reply(
        &self,
        _native_id: &str,
        _request_id: &str,
        _reply: &str,
    ) -> Result<()> {
        anyhow::bail!("permissions are not supported by this harness")
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn list_agents(&self) -> Result<Vec<AgentInfo>>;

    /// Kill spawned server (if any). Called on app exit.
    fn shutdown(&self);

    fn is_connected(&self) -> bool;
}

/// Instantiate a backend by harness config kind.
pub fn create_backend(
    cfg: &HarnessCfg,
    events: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
) -> Result<std::sync::Arc<dyn Backend>> {
    match cfg.kind.as_str() {
        "opencode" => Ok(std::sync::Arc::new(opencode::OpenCodeBackend::new(
            cfg.clone(),
            events,
        ))),
        "hermes" => Ok(std::sync::Arc::new(hermes::HermesBackend::new(
            cfg.clone(),
            events,
        ))),
        "deepseek-harness" => Ok(std::sync::Arc::new(dsh::DshBackend::new(
            cfg.clone(),
            events,
        ))),
        "opencode-acp" => Ok(std::sync::Arc::new(acp::AcpBackend::new(
            cfg.clone(),
            events,
        ))),
        other => anyhow::bail!("harness kind '{other}' is not supported yet"),
    }
}

/// Kinds the app can currently drive.
pub fn supported_kind(kind: &str) -> bool {
    matches!(
        kind,
        "opencode" | "opencode-acp" | "hermes" | "deepseek-harness"
    )
}

/// Hard cap for a single attachment — protects RAM from multi-GB files
/// (base64 inflates by ~4/3).
pub const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

/// Platform spawn preparation for harness child processes: no console
/// window on Windows, own process group on Unix (v5 #5 — so `kill_tree`
/// can take the whole tree down with one signal).
pub fn spawn_prep(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

/// Kill a child process *tree*: `Child::kill` only hits the direct process,
/// leaving interpreters/node spawned underneath alive as orphans.
/// Unix (v5 #5): the child runs in its own process group — SIGTERM the
/// group, grace period, then SIGKILL.
pub fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        unsafe {
            // TERM the group, give the children a moment, then KILL
            if libc::kill(-pgid, libc::SIGTERM) != 0 {
                let _ = libc::kill(pgid, libc::SIGTERM);
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = libc::kill(-pgid, libc::SIGKILL);
            let _ = libc::kill(pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_detect() {
        assert_eq!(mime_for(Path::new("/x/photo.PNG")), "image/png");
        assert_eq!(mime_for(Path::new("/x/doc.md")), "text/plain");
        assert_eq!(mime_for(Path::new("/x/report.pdf")), "application/pdf");
        assert_eq!(
            mime_for(Path::new("/x/blob.xyz")),
            "application/octet-stream"
        );
    }
}
