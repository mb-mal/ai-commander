//! Application state, event loop, key/mouse handling.

pub mod board;
pub mod editor;
pub mod markdown;
pub mod modals;
pub mod theme;
pub mod widgets;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::backend::{
    self, AttachmentMeta, Backend, BackendEvent, ChatMsg, ModelInfo, PartKind, RenderPart, Role,
    SendOpts, SessionInfo,
};
use crate::config::{Config, ModelRef};
use crate::i18n::{Lang, Str, t};
use crate::kanban;

use editor::Editor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Composer,
}

/// Which screen occupies the chat area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiMode {
    #[default]
    Chat,
    Board,
}

/// Clickable zones registered during rendering; the mouse handler matches
/// clicks/wheel against them. Rebuilt every frame.
#[derive(Clone)]
pub enum HitTarget {
    NewChat,
    Session {
        harness: String,
        native_id: String,
    },
    SettingsBtn,
    ComposerFocus,
    /// status-bar key index: 0=F1 1=F2 2=^N 3=^K 4=^P 5=^H 6=^O 7=F10
    StatusKey(u8),
    ModalSettingsRow(usize),
    ModalModelRow(usize),
    ModalHarnessRow(usize),
    ModalFileRow(usize),
    ModalListRow(usize),
    TaskCard(String),
}

#[derive(Default)]
pub struct HitAreas {
    pub targets: Vec<(ratatui::layout::Rect, HitTarget)>,
    /// whole modal rect — wheel over it moves the modal list selection
    pub modal_rect: Option<ratatui::layout::Rect>,
    pub chat_rect: ratatui::layout::Rect,
}

pub struct CurrentChat {
    pub harness: String,
    /// "" = draft (not created server-side yet)
    pub native_id: String,
    /// id before a pending->real rebind: StreamDone with the old id must
    /// still finish this chat's turn
    pub prev_native_id: Option<String>,
    pub title: String,
    pub msgs: Vec<ChatMsg>,
    pub loading: bool,
    pub model_override: Option<ModelRef>,
}

impl CurrentChat {
    pub fn draft(harness: &str) -> Self {
        CurrentChat {
            harness: harness.into(),
            native_id: String::new(),
            prev_native_id: None,
            title: String::new(),
            msgs: Vec::new(),
            loading: false,
            model_override: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PickerEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone)]
pub struct FilePicker {
    /// None = drive list
    pub dir: Option<PathBuf>,
    pub entries: Vec<PickerEntry>,
    pub sel: usize,
    pub marked: HashSet<PathBuf>,
    /// selecting a directory (for workdir setting) instead of files
    pub dir_mode: bool,
    /// harness whose workdir we are choosing (dir_mode only)
    pub dir_target: Option<String>,
    pub err: Option<String>,
}

#[derive(Clone)]
pub enum InputAction {
    RenameSession { harness: String, native_id: String },
    ProfileName,
    ServerUrl { harness: String },
    ServerPassword { harness: String },
    NewTaskTitle,
    BoardTitle { task_id: String },
}

#[derive(Clone)]
pub enum ConfirmAction {
    DeleteSession { harness: String, native_id: String },
}

#[derive(Clone)]
pub enum ListAction {
    PickAgent {
        harness: String,
    },
    /// answer a harness permission request: sel 0=once 1=always 2=reject
    PermissionReply {
        harness: String,
        session_id: String,
        request_id: String,
    },
    AssignHarness,
    AssignRole,
}

#[derive(Clone)]
pub enum ModelCtx {
    CurrentChat,
    HarnessDefault { harness: String },
}

#[derive(Clone)]
pub enum Modal {
    Help,
    About,
    Settings {
        sel: usize,
    },
    ModelPicker {
        filter: Editor,
        sel: usize,
        ctx: ModelCtx,
    },
    HarnessPicker {
        sel: usize,
    },
    FilePicker(FilePicker),
    Input {
        title: String,
        editor: Editor,
        action: InputAction,
    },
    Confirm {
        title: String,
        message: String,
        action: ConfirmAction,
    },
    List {
        title: String,
        items: Vec<(String, String)>,
        sel: usize,
        action: ListAction,
    },
}

#[derive(Default, Clone)]
pub struct SettingsRow {
    pub label: String,
    pub value: String,
    pub kind: SettingsKind,
}

#[derive(Default, Clone, Copy, PartialEq)]
pub enum SettingsKind {
    #[default]
    Info,
    Lang,
    Profile,
    ActiveHarness,
    Enabled,
    ServerUrl,
    ServerPassword,
    Workdir,
    Agent,
    DefaultModel,
    AutoApprove,
}

pub struct App {
    pub cfg: Config,
    pub backends: std::collections::HashMap<String, Arc<dyn Backend>>,
    pub versions: std::collections::HashMap<String, String>,
    pub backend_err: std::collections::HashMap<String, String>,
    pub connecting: HashSet<String>,
    pub sessions: std::collections::HashMap<String, Vec<SessionInfo>>,
    pub models: std::collections::HashMap<String, Vec<ModelInfo>>,
    pub agents: std::collections::HashMap<String, Vec<crate::backend::AgentInfo>>,

    pub active_harness: String,
    pub cur: CurrentChat,
    /// highlighted row in sidebar: (harness, native_id)
    pub sidebar_sel: Option<(String, String)>,
    pub sidebar_scroll: u16,

    pub editor: Editor,
    pub attachments: Vec<AttachmentMeta>,
    /// (harness, native_id) pairs with a turn in flight — busy state is
    /// per-chat, not global, so switching chats never wedges the composer
    pub busy: std::collections::HashSet<(String, String)>,
    /// text of the last optimistic message, for rollback on send error
    pub last_sent: Option<String>,
    /// false after Esc: live deltas are hidden, final history loads on StreamDone
    pub follow: bool,

    pub focus: Focus,
    pub show_sidebar: bool,
    /// lines hidden above the bottom edge (0 = pinned to bottom)
    pub bottom_offset: u16,

    pub modal: Option<Modal>,
    pub toast: Option<(String, bool, std::time::Instant)>,
    pub quit: bool,
    /// clickable zones, refreshed on every draw
    pub hit: HitAreas,
    /// kanban screen state
    pub mode: UiMode,
    pub board: Arc<std::sync::Mutex<kanban::store::BoardStore>>,
    pub board_col: usize,
    pub board_row: usize,
    pub board_sel: Option<String>,
    /// (harness, session) -> accumulated agent reply for a task turn
    pub task_replies: std::collections::HashMap<(String, String), String>,
    /// render cache: (msg_id, part_id) -> (width, text_len, rendered lines)
    pub render_cache: std::collections::HashMap<
        (String, String),
        (usize, usize, Vec<ratatui::text::Line<'static>>),
    >,

    pub events_tx: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
    events_rx: tokio::sync::mpsc::UnboundedReceiver<BackendEvent>,
    input_rx: tokio::sync::mpsc::UnboundedReceiver<crossterm::event::Event>,
}

impl App {
    pub fn new(
        cfg: Config,
        input_rx: tokio::sync::mpsc::UnboundedReceiver<crossterm::event::Event>,
    ) -> Self {
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let active = cfg.default_harness_id();
        let mut app = App {
            cur: CurrentChat::draft(&active),
            active_harness: active.clone(),
            sidebar_sel: None,
            sidebar_scroll: 0,
            cfg,
            backends: Default::default(),
            versions: Default::default(),
            backend_err: Default::default(),
            connecting: Default::default(),
            sessions: Default::default(),
            models: Default::default(),
            agents: Default::default(),
            editor: Editor::new(),
            attachments: Vec::new(),
            busy: std::collections::HashSet::new(),
            last_sent: None,
            follow: true,
            focus: Focus::Composer,
            show_sidebar: true,
            bottom_offset: 0,
            modal: None,
            toast: None,
            quit: false,
            mode: UiMode::Chat,
            board: Arc::new(std::sync::Mutex::new(
                kanban::store::BoardStore::open_redb(&kanban::Kanban::path())
                    .unwrap_or_else(|_| kanban::store::BoardStore::open_memory()),
            )),
            board_col: 0,
            board_row: 0,
            board_sel: None,
            task_replies: std::collections::HashMap::new(),
            hit: HitAreas::default(),
            render_cache: std::collections::HashMap::new(),
            events_tx,
            events_rx,
            input_rx,
        };
        app.ensure_backend(&active);
        app
    }

    pub fn lang(&self) -> Lang {
        self.cfg.lang
    }

    /// Is a turn in flight for the *currently open* chat?
    pub fn busy_now(&self) -> bool {
        self.busy
            .contains(&(self.cur.harness.clone(), self.cur.native_id.clone()))
    }

    pub fn tr(&self, s: Str) -> &'static str {
        t(self.cfg.lang, s)
    }

    pub fn toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), err, std::time::Instant::now()));
    }

    // ---------------- backends ----------------

    pub fn ensure_backend(&mut self, harness: &str) -> Option<Arc<dyn Backend>> {
        if let Some(b) = self.backends.get(harness) {
            return Some(Arc::clone(b));
        }
        let cfg = self.cfg.harness(harness)?.clone();
        if !cfg.enabled || !backend::supported_kind(&cfg.kind) {
            return None;
        }
        let backend = match backend::create_backend(&cfg, self.events_tx.clone()) {
            Ok(b) => b,
            Err(e) => {
                self.toast(format!("{}: {e}", cfg.name), true);
                return None;
            }
        };
        self.backends
            .insert(harness.to_string(), Arc::clone(&backend));
        self.connecting.insert(harness.to_string());
        let b = Arc::clone(&backend);
        let harness = harness.to_string();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            match b.connect().await {
                Ok(version) => {
                    let _ = tx.send(BackendEvent::Connected {
                        harness: harness.clone(),
                        version,
                    });
                    let _ = tx.send(BackendEvent::SessionsChanged { harness });
                }
                Err(e) => {
                    let _ = tx.send(BackendEvent::Error {
                        harness,
                        error: format!("{e:#}"),
                    });
                }
            }
        });
        Some(backend)
    }

    fn spawn_refresh_sessions(&self, harness: &str) {
        if let Some(b) = self.backends.get(harness).cloned() {
            let tx = self.events_tx.clone();
            let harness = harness.to_string();
            tokio::spawn(async move {
                match b.refresh_sessions().await {
                    Ok(list) => {
                        let _ = tx.send(BackendEvent::Sessions { harness, list });
                    }
                    Err(e) => {
                        let _ = tx.send(BackendEvent::Error {
                            harness,
                            error: format!("sessions: {e:#}"),
                        });
                    }
                }
            });
        }
    }

    fn spawn_refresh_models(&self, harness: &str) {
        if let Some(b) = self.backends.get(harness).cloned() {
            let tx = self.events_tx.clone();
            let harness = harness.to_string();
            tokio::spawn(async move {
                match b.list_models().await {
                    Ok(models) => {
                        let _ = tx.send(BackendEvent::Models { harness, models });
                    }
                    Err(e) => {
                        let _ = tx.send(BackendEvent::Error {
                            harness,
                            error: format!("models: {e:#}"),
                        });
                    }
                }
            });
        }
    }

    fn spawn_refresh_agents(&self, harness: &str) {
        if let Some(b) = self.backends.get(harness).cloned() {
            let tx = self.events_tx.clone();
            let harness = harness.to_string();
            tokio::spawn(async move {
                match b.list_agents().await {
                    Ok(agents) => {
                        let _ = tx.send(BackendEvent::Agents { harness, agents });
                    }
                    Err(e) => {
                        let _ = tx.send(BackendEvent::Error {
                            harness,
                            error: format!("agents: {e:#}"),
                        });
                    }
                }
            });
        }
    }

    /// All sessions of enabled harnesses in sidebar order.
    pub fn flattened_sessions(&self) -> Vec<(String, String, String, i64)> {
        let mut out = Vec::new();
        for h in &self.cfg.harnesses {
            if !h.enabled || !backend::supported_kind(&h.kind) {
                continue;
            }
            if let Some(list) = self.sessions.get(&h.id) {
                for s in list {
                    out.push((
                        h.id.clone(),
                        s.native_id.clone(),
                        s.title.clone(),
                        s.updated_ms,
                    ));
                }
            }
        }
        out
    }

    // ---------------- chat actions ----------------

    pub fn new_chat(&mut self) {
        self.cur = CurrentChat::draft(&self.active_harness.clone());
        self.bottom_offset = 0;
        self.focus = Focus::Composer;
        self.sidebar_sel = None;
    }

    pub fn open_session(&mut self, harness: &str, native_id: &str) {
        if self.cur.harness == harness && self.cur.native_id == native_id {
            return;
        }
        let title = self
            .sessions
            .get(harness)
            .and_then(|l| l.iter().find(|s| s.native_id == native_id))
            .map(|s| s.title.clone())
            .unwrap_or_default();
        self.cur = CurrentChat {
            harness: harness.into(),
            native_id: native_id.into(),
            prev_native_id: None,
            title,
            msgs: Vec::new(),
            loading: true,
            model_override: None,
        };
        self.bottom_offset = 0;
        self.focus = Focus::Composer;
        self.sidebar_sel = Some((harness.into(), native_id.into()));
        if let Some(b) = self.backends.get(harness).cloned() {
            let tx = self.events_tx.clone();
            let harness = harness.to_string();
            let native_id = native_id.to_string();
            tokio::spawn(async move {
                match b.load_history(&native_id).await {
                    Ok(msgs) => {
                        let _ = tx.send(BackendEvent::History {
                            harness,
                            native_id,
                            msgs,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(BackendEvent::Error {
                            harness,
                            error: format!("history: {e:#}"),
                        });
                    }
                }
            });
        }
    }

    pub fn send_current(&mut self) {
        if self.busy_now() {
            return;
        }
        let text = self.editor.text();
        let atts = std::mem::take(&mut self.attachments);
        if text.trim().is_empty() && atts.is_empty() {
            return;
        }
        let harness = self.cur.harness.clone();
        let Some(b) = self.ensure_backend(&harness) else {
            self.toast(self.tr(Str::NotConnected), true);
            self.attachments = atts;
            return;
        };
        self.editor.set_text("");

        // draft title from the first message
        if self.cur.native_id.is_empty() && self.cur.title.is_empty() {
            let first = text.lines().next().unwrap_or("chat").trim();
            let cut: String = first.chars().take(48).collect();
            self.cur.title = cut;
        }

        // optimistic user message
        let mut parts = Vec::new();
        if !text.trim().is_empty() {
            parts.push(RenderPart {
                part_id: "local".into(),
                kind: PartKind::Text,
                text: text.clone(),
            });
        }
        self.cur.msgs.push(ChatMsg {
            msg_id: format!("local_{}", self.cur.msgs.len()),
            role: Role::User,
            parts,
            attachments: atts.clone(),
        });
        self.busy
            .insert((harness.clone(), self.cur.native_id.clone()));
        self.last_sent = Some(text.clone());
        self.follow = true;
        self.bottom_offset = 0;

        let native_id = self.cur.native_id.clone();
        let title = self.cur.title.clone();
        let model = self.cur.model_override.clone().or_else(|| {
            self.cfg
                .harness(&harness)
                .and_then(|h| h.default_model.clone())
        });
        let agent = self
            .cfg
            .harness(&harness)
            .map(|h| h.agent.clone())
            .filter(|a| !a.is_empty());
        let tx = self.events_tx.clone();
        let harness_clone = harness.clone();
        tokio::spawn(async move {
            let native_id = if native_id.is_empty() {
                match b.create_session(&title).await {
                    Ok(s) => {
                        let _ = tx.send(BackendEvent::SessionCreated {
                            harness: harness_clone.clone(),
                            native_id: s.native_id.clone(),
                            title: s.title.clone(),
                        });
                        s.native_id
                    }
                    Err(e) => {
                        let _ = tx.send(BackendEvent::Error {
                            harness: harness_clone,
                            error: format!("create session: {e:#}"),
                        });
                        return;
                    }
                }
            } else {
                native_id
            };
            if let Err(e) = b
                .send(
                    &native_id,
                    &text,
                    &atts,
                    SendOpts {
                        model: model.as_ref(),
                        agent: agent.as_deref(),
                    },
                )
                .await
            {
                let _ = tx.send(BackendEvent::Error {
                    harness: harness_clone,
                    error: format!("send: {e:#}"),
                });
            }
        });
    }

    pub fn abort_current(&mut self) {
        if !self.busy_now() {
            return;
        }
        // opencode's server exposes no abort endpoint: Esc detaches, the
        // finished turn reloads via history on StreamDone, the next prompt
        // steers. Hermes is a child process — Esc really kills it.
        if let Some(b) = self.backends.get(&self.cur.harness).cloned() {
            if !self.cur.native_id.is_empty() && !self.cur.native_id.starts_with("pending-") {
                let native_id = self.cur.native_id.clone();
                tokio::spawn(async move {
                    let _ = b.abort(&native_id).await;
                });
            }
        }
        self.busy
            .remove(&(self.cur.harness.clone(), self.cur.native_id.clone()));
        self.follow = false;
        self.toast(self.tr(Str::StopNote), false);
    }

    pub fn current_model(&self) -> Option<ModelRef> {
        self.cur.model_override.clone().or_else(|| {
            self.cfg
                .harness(&self.cur.harness)
                .and_then(|h| h.default_model.clone())
        })
    }

    // ---------------- main loop ----------------

    pub async fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        loop {
            terminal.draw(|f| widgets::draw(self, f))?;
            tokio::select! {
                ev = self.events_rx.recv() => {
                    if let Some(ev) = ev {
                        self.on_backend_event(ev);
                    }
                    // coalesce a burst of stream events into one frame
                    for _ in 0..64 {
                        match self.events_rx.try_recv() {
                            Ok(ev) => self.on_backend_event(ev),
                            Err(_) => break,
                        }
                    }
                }
                ev = self.input_rx.recv() => {
                    if let Some(ev) = ev {
                        self.on_input(ev);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                    // kanban watchdog: once a second is plenty for 10-minute windows
                    let acted = {
                        let mut b = self.board.lock().unwrap();
                        b.watchdog_pass(std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0))
                    };
                    for (id, label) in acted {
                        self.toast(format!("⚠ {id}: {label} (watchdog)"), true);
                    }
                    let expired = self
                        .toast
                        .as_ref()
                        .map(|(_, _, at)| at.elapsed() > Duration::from_secs(4))
                        .unwrap_or(false);
                    if expired {
                        self.toast = None;
                    }
                }
            }
            if self.quit {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn on_backend_event(&mut self, ev: BackendEvent) {
        match ev {
            BackendEvent::Connected { harness, version } => {
                self.connecting.remove(&harness);
                self.backend_err.remove(&harness);
                self.versions.insert(harness.clone(), version);
                self.spawn_refresh_sessions(&harness);
                self.spawn_refresh_models(&harness);
                self.spawn_refresh_agents(&harness);
            }
            BackendEvent::SessionsChanged { harness } => {
                self.spawn_refresh_sessions(&harness);
            }
            BackendEvent::Sessions { harness, list } => {
                // remember titles for open chat
                if let Some(s) = list
                    .iter()
                    .find(|s| self.cur.harness == harness && s.native_id == self.cur.native_id)
                {
                    self.cur.title = s.title.clone();
                }
                self.sessions.insert(harness, list);
            }
            BackendEvent::SessionCreated {
                harness,
                native_id,
                title,
            } => {
                {
                    let mut b = self.board.lock().unwrap();
                    let pendings: Vec<String> = b
                        .projection()
                        .tasks
                        .iter()
                        .filter_map(|t| t.session_id.clone())
                        .filter(|sid| sid.starts_with("pending-"))
                        .collect();
                    for p in pendings {
                        b.rebind_session(&p, &native_id);
                    }
                }
                let old = if self.cur.harness == harness {
                    if self.cur.native_id.is_empty() || self.cur.native_id.starts_with("pending-") {
                        self.cur.prev_native_id = Some(self.cur.native_id.clone());
                        Some(std::mem::replace(
                            &mut self.cur.native_id,
                            native_id.clone(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(prev_id) = old {
                    // keep per-chat busy state attached to the real id
                    if self.busy.remove(&(harness.clone(), prev_id.clone())) {
                        self.busy.insert((harness.clone(), native_id.clone()));
                    }
                    self.cur.title = title;
                }
                self.spawn_refresh_sessions(&harness);
            }
            BackendEvent::History {
                harness,
                native_id,
                msgs,
            } => {
                if self.cur.harness == harness && self.cur.native_id == native_id {
                    self.cur.msgs = msgs;
                    self.cur.loading = false;
                }
            }
            BackendEvent::TaskLinked {
                harness,
                task_id,
                session_id,
            } => {
                let mut b = self.board.lock().unwrap();
                b.link_session(&task_id, &session_id);
                self.busy.insert((harness.clone(), session_id.clone()));
            }
            BackendEvent::PartDelta {
                harness,
                native_id,
                msg_id,
                part_id,
                kind,
                delta,
            } => {
                if self.follow {
                    self.apply_part((harness, native_id), msg_id, part_id, kind, delta, true);
                }
            }
            BackendEvent::PartUpdated {
                harness,
                native_id,
                msg_id,
                part,
            } => {
                if self.follow {
                    let (kind, text) = (part.kind, part.text);
                    self.apply_part(
                        (harness, native_id),
                        msg_id,
                        part.part_id,
                        kind,
                        text,
                        false,
                    );
                }
            }
            BackendEvent::PermissionRequest {
                harness,
                session_id,
                request_id,
                title,
            } => {
                // the agent asks before acting — surface a modal (NC silver)
                let lang = self.cfg.lang;
                let items = vec![
                    ("once".into(), t(lang, Str::PermOnce).to_string()),
                    ("always".into(), t(lang, Str::PermAlways).to_string()),
                    ("reject".into(), t(lang, Str::PermReject).to_string()),
                ];
                self.modal = Some(Modal::List {
                    title: format!("⚠ {title}"),
                    items,
                    sel: 0,
                    action: ListAction::PermissionReply {
                        harness,
                        session_id,
                        request_id,
                    },
                });
            }
            BackendEvent::Reconnected { harness } => {
                // SSE dropped and recovered: resync list + current history
                self.spawn_refresh_sessions(&harness);
                if self.cur.harness == harness && !self.cur.native_id.is_empty() {
                    let native_id = self.cur.native_id.clone();
                    self.cur.loading = true;
                    if let Some(b) = self.backends.get(&harness).cloned() {
                        let tx = self.events_tx.clone();
                        let h = harness.clone();
                        tokio::spawn(async move {
                            match b.load_history(&native_id).await {
                                Ok(msgs) => {
                                    let _ = tx.send(BackendEvent::History {
                                        harness: h,
                                        native_id,
                                        msgs,
                                    });
                                }
                                Err(e) => {
                                    let _ = tx.send(BackendEvent::Error {
                                        harness: h,
                                        error: format!("history: {e:#}"),
                                    });
                                }
                            }
                        });
                    }
                }
            }
            BackendEvent::StreamDone { harness, native_id } => {
                // a task turn finished: classify DONE/BLOCKED/ASK -> FSM
                let task = {
                    let b = self.board.lock().unwrap();
                    b.find_by_session(&harness, &native_id)
                };
                if let Some(task) = task {
                    let reply = self
                        .task_replies
                        .remove(&(harness.clone(), native_id.clone()))
                        .unwrap_or_default();
                    let action = match kanban::classify_reply(&reply) {
                        kanban::TurnOutcome::Finished => kanban::TaskAction::Finish,
                        kanban::TurnOutcome::Blocked => kanban::TaskAction::Block,
                        kanban::TurnOutcome::NeedsHuman => kanban::TaskAction::Ask,
                    };
                    let mut b = self.board.lock().unwrap();
                    if let Ok(t) = b.transition(&task.id, action) {
                        drop(b);
                        self.toast(format!("{} → {:?}", t.id, t.status), false);
                    }
                }
                // busy is per (harness, session) — clear regardless of the open chat
                self.busy.remove(&(harness.clone(), native_id.clone()));
                // the turn finished under the pre-rebind (pending) id: finish
                // the rebound current chat as well
                if self.cur.harness == harness
                    && self.cur.prev_native_id.as_deref() == Some(native_id.as_str())
                {
                    self.busy
                        .remove(&(harness.clone(), self.cur.native_id.clone()));
                    self.cur.prev_native_id = None;
                }
                if self.cur.harness == harness && self.cur.native_id == native_id {
                    self.last_sent = None;
                    if !self.follow {
                        // detached earlier — pull the finished turn from history
                        self.follow = true;
                        self.cur.loading = true;
                        self.spawn_history_reload(&harness, &native_id);
                    }
                }
                self.spawn_refresh_sessions(&harness);
            }
            BackendEvent::StreamError {
                harness,
                native_id,
                error,
            } => {
                self.busy.remove(&(harness.clone(), native_id.clone()));
                if self.cur.harness == harness && self.cur.native_id == native_id {
                    self.follow = true;
                    let msg = ChatMsg {
                        msg_id: format!("err_{}", self.cur.msgs.len()),
                        role: Role::Assistant,
                        parts: vec![RenderPart {
                            part_id: "err".into(),
                            kind: PartKind::Tool,
                            text: format!("⚠ {error}"),
                        }],
                        attachments: Vec::new(),
                    };
                    self.cur.msgs.push(msg);
                    self.bottom_offset = 0;
                }
            }
            BackendEvent::Models { harness, models } => {
                self.models.insert(harness, models);
            }
            BackendEvent::Agents { harness, agents } => {
                self.agents.insert(harness, agents);
            }
            BackendEvent::Error { harness, error } => {
                self.connecting.remove(&harness);
                self.backend_err.insert(harness.clone(), error.clone());
                // rollback the optimistic message: put the text back into the composer
                let is_cur = self.cur.harness == harness;
                if is_cur {
                    if let Some(text) = self.last_sent.take() {
                        let last_is_local = self
                            .cur
                            .msgs
                            .last()
                            .map(|m| m.msg_id.starts_with("local_"))
                            .unwrap_or(false);
                        if last_is_local {
                            self.cur.msgs.pop();
                        }
                        if !text.trim().is_empty() && self.editor.is_empty() {
                            self.editor.set_text(&text);
                        }
                    }
                }
                let name = self
                    .cfg
                    .harness(&harness)
                    .map(|h| h.name.clone())
                    .unwrap_or(harness);
                self.toast(format!("{name}: {error}"), true);
            }
        }
    }

    fn spawn_history_reload(&self, harness: &str, native_id: &str) {
        if let Some(b) = self.backends.get(harness).cloned() {
            let tx = self.events_tx.clone();
            let (h, id) = (harness.to_string(), native_id.to_string());
            tokio::spawn(async move {
                match b.load_history(&id).await {
                    Ok(msgs) => {
                        let _ = tx.send(BackendEvent::History {
                            harness: h,
                            native_id: id,
                            msgs,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(BackendEvent::Error {
                            harness: h,
                            error: format!("history: {e:#}"),
                        });
                    }
                }
            });
        }
    }

    fn apply_part(
        &mut self,
        (harness, native_id): (String, String),
        msg_id: String,
        part_id: String,
        kind: PartKind,
        text: String,
        is_delta: bool,
    ) {
        if self.cur.harness != harness || self.cur.native_id != native_id {
            return;
        }

        self.bottom_offset = 0;
        let msg = match self.cur.msgs.iter_mut().find(|m| m.msg_id == msg_id) {
            Some(m) => m,
            None => {
                self.cur.msgs.push(ChatMsg {
                    msg_id: msg_id.clone(),
                    role: Role::Assistant,
                    parts: Vec::new(),
                    attachments: Vec::new(),
                });
                self.cur.msgs.last_mut().expect("just pushed")
            }
        };
        match msg.parts.iter_mut().find(|p| p.part_id == part_id) {
            Some(p) => {
                if is_delta {
                    p.text.push_str(&text);
                } else {
                    p.text = text;
                }
            }
            None => {
                msg.parts.push(RenderPart {
                    part_id: part_id.clone(),
                    kind,
                    text,
                });
            }
        }
    }

    fn on_input(&mut self, ev: crossterm::event::Event) {
        use crossterm::event::Event;
        match ev {
            Event::Key(k) => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) => self.on_paste(s),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    fn on_paste(&mut self, s: String) {
        match self.modal.as_mut() {
            Some(Modal::Input { editor, .. }) => editor.insert_str(&s),
            Some(Modal::ModelPicker { filter, .. }) => filter.insert_str(&s),
            _ => {
                if self.focus == Focus::Composer {
                    self.editor.insert_str(&s);
                }
            }
        }
    }

    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        let pos = ratatui::layout::Position {
            x: m.column,
            y: m.row,
        };
        let in_modal = |app: &Self, p: ratatui::layout::Position| {
            app.hit.modal_rect.map(|r| r.contains(p)).unwrap_or(false)
        };

        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if matches!(m.kind, MouseEventKind::ScrollUp) {
                    -1
                } else {
                    1
                };
                if self.modal.is_some() && in_modal(self, pos) {
                    self.modal_move(dir);
                } else if self.hit.chat_rect.contains(pos) {
                    if dir < 0 {
                        self.bottom_offset = self.bottom_offset.saturating_add(3);
                    } else {
                        self.bottom_offset = self.bottom_offset.saturating_sub(3);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // find the innermost target under the cursor
                let hit = self
                    .hit
                    .targets
                    .iter()
                    .rev()
                    .find(|(r, _)| r.contains(pos))
                    .map(|(_, t)| t.clone());
                let Some(target) = hit else { return };

                let modal_open = self.modal.is_some();
                match target {
                    HitTarget::StatusKey(n) if !modal_open => self.status_key_action(n),
                    HitTarget::NewChat if !modal_open => self.new_chat(),
                    HitTarget::Session { harness, native_id } if !modal_open => {
                        self.sidebar_sel = Some((harness.clone(), native_id.clone()));
                        self.open_session(&harness, &native_id);
                    }
                    HitTarget::SettingsBtn if !modal_open => {
                        self.modal = Some(Modal::Settings { sel: 0 });
                    }
                    HitTarget::ComposerFocus if !modal_open => self.focus = Focus::Composer,
                    HitTarget::ModalSettingsRow(i) if modal_open => {
                        self.modal = Some(Modal::Settings { sel: i });
                        self.modal_key(crossterm::event::KeyEvent::from(
                            crossterm::event::KeyCode::Enter,
                        ));
                    }
                    HitTarget::ModalModelRow(i) if modal_open => {
                        if let Some(Modal::ModelPicker { sel, .. }) = self.modal.as_mut() {
                            *sel = i;
                        }
                        self.modal_key(crossterm::event::KeyEvent::from(
                            crossterm::event::KeyCode::Enter,
                        ));
                    }
                    HitTarget::ModalHarnessRow(i) if modal_open => {
                        if let Some(Modal::HarnessPicker { sel, .. }) = self.modal.as_mut() {
                            *sel = i;
                        }
                        self.modal_key(crossterm::event::KeyEvent::from(
                            crossterm::event::KeyCode::Enter,
                        ));
                    }
                    HitTarget::ModalFileRow(i) if modal_open => {
                        if let Some(Modal::FilePicker(picker)) = self.modal.as_mut() {
                            picker.sel = i;
                        }
                        self.modal_key(crossterm::event::KeyEvent::from(
                            crossterm::event::KeyCode::Enter,
                        ));
                    }
                    HitTarget::ModalListRow(i) if modal_open => {
                        if let Some(Modal::List { sel, .. }) = self.modal.as_mut() {
                            *sel = i;
                        }
                        self.modal_key(crossterm::event::KeyEvent::from(
                            crossterm::event::KeyCode::Enter,
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Move the selection of the currently open modal list (wheel support).
    fn modal_move(&mut self, dir: i32) {
        let clamp_move = |sel: &mut usize, len: usize, dir: i32| {
            if len == 0 {
                return;
            }
            let next = (*sel as i64 + dir as i64).clamp(0, len as i64 - 1);
            *sel = next as usize;
        };
        let settings_len = self.settings_rows().len();
        let model_len = self.filtered_models().len();
        match self.modal.as_mut() {
            Some(Modal::Settings { sel }) => clamp_move(sel, settings_len, dir),
            Some(Modal::ModelPicker { sel, .. }) => clamp_move(sel, model_len, dir),
            Some(Modal::HarnessPicker { sel }) => clamp_move(sel, self.cfg.harnesses.len(), dir),
            Some(Modal::FilePicker(p)) => clamp_move(&mut p.sel, p.entries.len(), dir),
            Some(Modal::List { sel, items, .. }) => clamp_move(sel, items.len(), dir),
            _ => {}
        }
    }

    fn status_key_action(&mut self, n: u8) {
        match n {
            0 => self.modal = Some(Modal::Help),
            1 => self.modal = Some(Modal::Settings { sel: 0 }),
            2 => self.new_chat(),
            3 => {
                self.modal = Some(Modal::ModelPicker {
                    filter: Editor::new(),
                    sel: 0,
                    ctx: ModelCtx::CurrentChat,
                })
            }
            4 => self.open_file_picker(false, None),
            5 => self.modal = Some(Modal::HarnessPicker { sel: 0 }),
            6 => self.show_sidebar = !self.show_sidebar,
            7 => self.quit = true,
            _ => {}
        }
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        if k.kind != crossterm::event::KeyEventKind::Press {
            return;
        }
        if self.modal.is_some() {
            self.modal_key(k);
            return;
        }

        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);

        // global
        match (k.code, ctrl, alt) {
            (KeyCode::F(1), _, _) => return self.open_modal(Modal::Help),
            (KeyCode::F(2), _, _) => return self.open_modal(Modal::Settings { sel: 0 }),
            (KeyCode::F(10), _, _)
            | (KeyCode::Char('q'), true, _)
            | (KeyCode::Char('c'), true, _) => {
                self.quit = true;
                return;
            }
            (KeyCode::Char('n'), true, _) => return self.new_chat(),
            (KeyCode::Char('k'), true, _) => {
                return self.open_modal(Modal::ModelPicker {
                    filter: Editor::new(),
                    sel: 0,
                    ctx: ModelCtx::CurrentChat,
                });
            }
            (KeyCode::Char('h'), true, _) => {
                return self.open_modal(Modal::HarnessPicker { sel: 0 });
            }
            (KeyCode::Char('o'), true, _) => {
                self.show_sidebar = !self.show_sidebar;
                return;
            }
            (KeyCode::Char('p'), true, _) => {
                return self.open_file_picker(false, None);
            }
            (KeyCode::Char('a'), true, _) => {
                return self.open_modal(Modal::About);
            }
            (KeyCode::Char('b'), true, _) => {
                self.mode = if self.mode == UiMode::Board {
                    UiMode::Chat
                } else {
                    UiMode::Board
                };
                return;
            }
            _ => {}
        }

        if self.mode == UiMode::Board {
            self.board_key(k);
            return;
        }

        match self.focus {
            Focus::Composer => match (k.code, ctrl, alt) {
                (KeyCode::Enter, false, true) | (KeyCode::Char('j' | 'J'), true, false) => {
                    self.editor.newline()
                }
                (KeyCode::Enter, false, false) => self.send_current(),
                (KeyCode::Esc, _, _) => self.abort_current(),
                (KeyCode::Tab, _, _) => self.focus = Focus::Sidebar,
                (KeyCode::BackTab, _, _) => self.focus = Focus::Sidebar,
                (KeyCode::Char('d'), true, _) => {
                    self.attachments.pop();
                }
                (KeyCode::PageUp, _, _) => {
                    self.bottom_offset = self.bottom_offset.saturating_add(10)
                }
                (KeyCode::PageDown, _, _) => {
                    self.bottom_offset = self.bottom_offset.saturating_sub(10)
                }
                (KeyCode::End, _, _) => self.bottom_offset = 0,
                (KeyCode::Char('c'), _, _) if k.modifiers.contains(KeyModifiers::CONTROL) => {}
                (code, false, _) => self.editor_key(code),
                _ => {}
            },
            Focus::Sidebar => match (k.code, ctrl, alt) {
                (KeyCode::Up, _, _) => self.sidebar_move(-1),
                (KeyCode::Down, _, _) => self.sidebar_move(1),
                (KeyCode::Enter, _, _) => {
                    if let Some((h, id)) = self.sidebar_sel.clone() {
                        self.open_session(&h, &id);
                    }
                }
                (KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc, _, _) => {
                    self.focus = Focus::Composer
                }
                (KeyCode::Char('e' | 'E'), false, _) => self.rename_selected(),
                (KeyCode::Char('d' | 'D') | KeyCode::F(8) | KeyCode::Delete, _, false) => {
                    self.delete_selected()
                }
                _ => {}
            },
        }
    }

    fn editor_key(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Char(c) => self.editor.insert_char(c),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Up => self.editor.up(),
            KeyCode::Down => self.editor.down(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            _ => {}
        }
    }

    pub fn sidebar_move(&mut self, dir: i32) {
        let items = self.flattened_sessions();
        if items.is_empty() {
            self.sidebar_sel = None;
            return;
        }
        let pos = self.sidebar_sel.as_ref().and_then(|(h, id)| {
            items
                .iter()
                .position(|(ih, iid, _, _)| ih == h && iid == id)
        });
        let next = match pos {
            Some(p) => (p as i32 + dir).clamp(0, items.len() as i32 - 1) as usize,
            None => {
                if dir > 0 {
                    0
                } else {
                    items.len() - 1
                }
            }
        };
        let (h, id, _, _) = items[next].clone();
        self.sidebar_sel = Some((h, id));
    }

    fn rename_selected(&mut self) {
        let Some((h, id)) = self.sidebar_sel.clone() else {
            return;
        };
        let title = self
            .sessions
            .get(&h)
            .and_then(|l| l.iter().find(|s| s.native_id == id))
            .map(|s| s.title.clone())
            .unwrap_or_default();
        self.open_modal(Modal::Input {
            title: self.tr(Str::RenameSession).to_string(),
            editor: Editor::with_text(&title),
            action: InputAction::RenameSession {
                harness: h,
                native_id: id,
            },
        });
    }

    fn delete_selected(&mut self) {
        let Some((h, id)) = self.sidebar_sel.clone() else {
            return;
        };
        self.open_modal(Modal::Confirm {
            title: self.tr(Str::DeleteSession).to_string(),
            message: self.tr(Str::DeleteSessionQ).to_string(),
            action: ConfirmAction::DeleteSession {
                harness: h,
                native_id: id,
            },
        });
    }

    // ---------------- modals ----------------

    fn open_modal(&mut self, m: Modal) {
        self.modal = Some(m);
    }

    pub fn open_file_picker(&mut self, dir_mode: bool, dir_target: Option<String>) {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let mut picker = FilePicker {
            dir: Some(home),
            entries: Vec::new(),
            sel: 0,
            marked: HashSet::new(),
            dir_mode,
            dir_target,
            err: None,
        };
        Self::load_picker_dir(&mut picker);
        self.open_modal(Modal::FilePicker(picker));
    }

    pub fn load_picker_dir(picker: &mut FilePicker) {
        picker.sel = 0;
        picker.err = None;
        let Some(dir) = picker.dir.clone() else {
            picker.entries = Self::list_drives();
            return;
        };
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                let mut dirs = Vec::new();
                let mut files = Vec::new();
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let Ok(meta) = e.metadata() else { continue };
                    let entry = PickerEntry {
                        name: name.clone(),
                        path: e.path(),
                        is_dir: meta.is_dir(),
                        size: meta.len(),
                    };
                    if entry.is_dir {
                        dirs.push(entry);
                    } else {
                        files.push(entry);
                    }
                }
                dirs.sort_by_key(|a| a.name.to_lowercase());
                files.sort_by_key(|a| a.name.to_lowercase());
                picker.entries = dirs;
                picker.entries.extend(files);
            }
            Err(e) => {
                picker.err = Some(e.to_string());
                picker.entries = Vec::new();
            }
        }
    }

    pub fn list_drives() -> Vec<PickerEntry> {
        let mut out = Vec::new();
        for l in b'C'..=b'Z' {
            let p = PathBuf::from(format!("{}:\\", l as char));
            if p.exists() {
                out.push(PickerEntry {
                    name: format!("[{}:]", l as char),
                    path: p,
                    is_dir: true,
                    size: 0,
                });
            }
        }
        out
    }

    /// Enter pressed in the file picker
    pub fn picker_activate(&mut self) {
        let Some(Modal::FilePicker(picker)) = &mut self.modal else {
            return;
        };
        let Some(entry) = picker.entries.get(picker.sel).cloned() else {
            return;
        };
        if entry.name == ".." {
            let parent = picker
                .dir
                .clone()
                .and_then(|d| d.parent().map(|p| p.to_path_buf()));
            if let Some(p) = parent {
                picker.dir = Some(p);
            } else {
                picker.dir = None;
            }
            Self::load_picker_dir(picker);
            return;
        }
        if entry.is_dir {
            picker.dir = Some(entry.path.clone());
            Self::load_picker_dir(picker);
        } else if picker.dir_mode {
            // in dir mode Enter on a file does nothing
        } else {
            let meta = AttachmentMeta::from_path(&entry.path);
            if meta.size > crate::backend::MAX_ATTACHMENT_BYTES {
                self.toast(
                    format!(
                        "{}: {} > {} MB",
                        meta.filename,
                        meta.size_label(),
                        crate::backend::MAX_ATTACHMENT_BYTES / (1024 * 1024)
                    ),
                    true,
                );
                return;
            }
            self.attachments.push(meta);
            self.modal = None;
        }
    }

    pub fn picker_attach_marked(&mut self) {
        let Some(Modal::FilePicker(picker)) = &mut self.modal else {
            return;
        };
        if picker.dir_mode {
            // choose current directory as workdir
            if let Some(dir) = picker.dir.clone() {
                let target = picker.dir_target.clone();
                let dir_str = dir.to_string_lossy().into_owned();
                self.modal = None;
                if let Some(harness) = target {
                    if let Some(h) = self.cfg.harness_mut(&harness) {
                        h.workdir = dir_str;
                    }
                    let _ = self.cfg.save();
                }
            }
            return;
        }
        let marked: Vec<PathBuf> = picker.marked.iter().cloned().collect();
        if marked.is_empty() {
            let Some(entry) = picker.entries.get(picker.sel) else {
                return;
            };
            if !entry.is_dir {
                self.attachments
                    .push(AttachmentMeta::from_path(&entry.path));
                self.modal = None;
            }
            return;
        }
        for p in marked {
            let meta = AttachmentMeta::from_path(&p);
            if meta.size > crate::backend::MAX_ATTACHMENT_BYTES {
                self.toast(format!("{}: oversize skipped", meta.filename), true);
                continue;
            }
            self.attachments.push(meta);
        }
        self.modal = None;
    }

    // ---------------- kanban board ----------------

    fn selected_board_task(&self) -> Option<kanban::Task> {
        let b = self.board.lock().unwrap();
        let cols = kanban::TaskStatus::COLUMN_ORDER;
        let status = *cols.get(self.board_col)?;
        let mut list = b.projection().by_status(status);
        if status == kanban::TaskStatus::Ready {
            list.extend(b.projection().by_status(kanban::TaskStatus::Blocked));
            list.extend(b.projection().by_status(kanban::TaskStatus::NeedsHuman));
        }
        list.into_iter().nth(self.board_row)
    }

    fn board_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if k.kind != crossterm::event::KeyEventKind::Press {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match (k.code, ctrl) {
            (KeyCode::Char('b'), true) => self.mode = UiMode::Chat,
            (KeyCode::Left, _) => {
                self.board_col = self.board_col.saturating_sub(1);
                self.board_row = 0;
            }
            (KeyCode::Right, _) => {
                self.board_col =
                    (self.board_col + 1).min(kanban::TaskStatus::COLUMN_ORDER.len() - 1);
                self.board_row = 0;
            }
            (KeyCode::Up, _) => self.board_row = self.board_row.saturating_sub(1),
            (KeyCode::Down, _) => self.board_row = self.board_row.saturating_add(1),
            (KeyCode::F(7), _) | (KeyCode::Char('n' | 'N'), false) => {
                self.modal = Some(Modal::Input {
                    title: "New task".into(),
                    editor: Editor::new(),
                    action: InputAction::NewTaskTitle,
                });
            }
            (KeyCode::F(4), _) | (KeyCode::Char('e' | 'E'), false) => {
                if let Some(t) = self.selected_board_task() {
                    self.modal = Some(Modal::Input {
                        title: "Edit task".into(),
                        editor: Editor::with_text(&t.title),
                        action: InputAction::BoardTitle { task_id: t.id },
                    });
                }
            }
            // status advance: the natural next action for the current status
            (KeyCode::F(6), _)
            | (KeyCode::Char('m' | 'M'), false)
            | (KeyCode::Char('p' | 'P'), false) => {
                self.board_advance();
            }
            (KeyCode::Char('a' | 'A'), false) => {
                // assign a harness from the enabled list
                let items: Vec<(String, String)> = self
                    .cfg
                    .harnesses
                    .iter()
                    .filter(|h| h.enabled && backend::supported_kind(&h.kind))
                    .map(|h| (h.id.clone(), h.name.clone()))
                    .collect();
                self.modal = Some(Modal::List {
                    title: "Assign harness".into(),
                    items,
                    sel: 0,
                    action: ListAction::AssignHarness,
                });
            }
            (KeyCode::Char('o' | 'O'), false) => {
                // assign a role from .ai-commander/agents/*.md
                let rs = kanban::roles::RoleSet::load(&std::env::current_dir().unwrap_or_default());
                let items: Vec<(String, String)> = rs
                    .roles
                    .iter()
                    .map(|(id, r)| (id.clone(), r.name.clone()))
                    .collect();
                if items.is_empty() {
                    self.toast("no roles: add .ai-commander/agents/<id>.md", true);
                } else {
                    self.modal = Some(Modal::List {
                        title: "Assign role".into(),
                        items,
                        sel: 0,
                        action: ListAction::AssignRole,
                    });
                }
            }
            (KeyCode::Char('d' | 'D'), false) => {
                self.board_act(kanban::TaskAction::Approve);
            }
            (KeyCode::Char('r' | 'R'), false) => {
                self.board_act(kanban::TaskAction::Rework);
            }
            (KeyCode::F(8), _) | (KeyCode::Delete, _) => {
                self.board_act(kanban::TaskAction::Cancel);
            }
            (KeyCode::Enter, _) => {
                if let Some(t) = self.selected_board_task() {
                    if let (Some(h), Some(sid)) = (t.harness.clone(), t.session_id.clone()) {
                        self.mode = UiMode::Chat;
                        self.open_session(&h, &sid);
                    } else {
                        self.toast("assign a harness first (a)", true);
                    }
                }
            }
            _ => {}
        }
    }

    fn board_act(&mut self, action: kanban::TaskAction) {
        let Some(t) = self.selected_board_task() else {
            return;
        };
        let outcome = {
            let mut b = self.board.lock().unwrap();
            b.transition(&t.id, action)
        };
        match outcome {
            Ok(updated) => self.toast(format!("{} → {:?}", updated.id, updated.status), false),
            Err(e) => self.toast(e, true),
        }
    }

    /// Promote/Start/Approve with auto-execution: entering Ready runs the
    /// task on its harness (if assigned) and the turn's outcome drives the
    /// next transition automatically.
    fn board_advance(&mut self) {
        use kanban::TaskAction as A;
        use kanban::TaskStatus as S;
        let Some(t) = self.selected_board_task() else {
            return;
        };
        let action = match t.status {
            S::Backlog | S::Blocked | S::NeedsHuman => A::Promote,
            S::Ready => A::Start,
            S::InProgress => A::Finish,
            S::Review => A::Approve,
            S::Done | S::Cancelled => return,
        };
        // Start immediately launches the agent turn
        if action == A::Start {
            self.run_task(&t);
            return;
        }
        self.board_act(action);
    }

    /// Execute a task: create a harness session and send the task prompt.
    fn run_task(&mut self, task: &kanban::Task) {
        let Some(harness) = task.harness.clone() else {
            self.toast("assign a harness first (a)", true);
            return;
        };
        let Some(b) = self.ensure_backend(&harness) else {
            self.toast("harness unavailable", true);
            return;
        };
        {
            let started = {
                let mut board = self.board.lock().unwrap();
                board.transition(&task.id, kanban::TaskAction::Start).ok()
            };
            if let Some(t) = started {
                self.toast(format!("{} ▶ running on {harness}", t.id), false);
            }
        }
        let prompt = kanban::task_prompt(task);
        let tx = self.events_tx.clone();
        let harness_clone = harness.clone();
        let task_id = task.id.clone();
        let model = self
            .cfg
            .harness(&harness)
            .and_then(|h| h.default_model.clone());
        let agent = self
            .cfg
            .harness(&harness)
            .map(|h| h.agent.clone())
            .filter(|a| !a.is_empty());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("rt");
            rt.block_on(async move {
                match b.create_session(&format!("task {task_id}")).await {
                    Ok(session) => {
                        let _ = tx.send(BackendEvent::TaskLinked {
                            harness: harness_clone,
                            task_id,
                            session_id: session.native_id.clone(),
                        });
                        let _ = b
                            .send(
                                &session.native_id,
                                &prompt,
                                &[],
                                SendOpts {
                                    model: model.as_ref(),
                                    agent: agent.as_deref(),
                                },
                            )
                            .await;
                    }
                    Err(e) => {
                        let _ = tx.send(BackendEvent::Error {
                            harness: harness_clone,
                            error: format!("task {task_id}: {e:#}"),
                        });
                    }
                }
            });
        });
    }

    // ---------------- modal key handling ----------------

    fn modal_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

        // Esc closes any modal (except when typing — still closes)
        if k.code == KeyCode::Esc {
            self.modal = None;
            return;
        }

        match self.modal.clone().expect("checked above") {
            Modal::Help => match k.code {
                KeyCode::Enter => self.modal = None,
                // classic 90s: About screen reachable from Help
                KeyCode::Char('a' | 'A') => self.modal = Some(Modal::About),
                _ => {}
            },
            // About: "press any key to close", like it always was
            Modal::About => {
                self.modal = None;
            }
            Modal::Confirm { action, .. } => {
                if k.code == KeyCode::Enter {
                    self.modal = None;
                    self.apply_confirm(action);
                }
            }
            Modal::Settings { sel } => {
                let rows = self.settings_rows();
                let len = rows.len().max(1);
                match k.code {
                    KeyCode::Up => {
                        self.modal = Some(Modal::Settings {
                            sel: (sel + len - 1) % len,
                        })
                    }
                    KeyCode::Down => {
                        self.modal = Some(Modal::Settings {
                            sel: (sel + 1) % len,
                        })
                    }
                    KeyCode::Enter => {
                        let row = rows.get(sel).cloned();
                        self.modal = None;
                        if let Some(row) = row {
                            self.settings_activate(sel, row);
                        }
                    }
                    _ => {}
                }
            }
            Modal::HarnessPicker { sel } => {
                let len = self.cfg.harnesses.len().max(1);
                match k.code {
                    KeyCode::Up => {
                        self.modal = Some(Modal::HarnessPicker {
                            sel: (sel + len - 1) % len,
                        })
                    }
                    KeyCode::Down => {
                        self.modal = Some(Modal::HarnessPicker {
                            sel: (sel + 1) % len,
                        })
                    }
                    KeyCode::Enter => {
                        self.modal = None;
                        self.switch_harness(sel);
                    }
                    _ => {}
                }
            }
            Modal::ModelPicker {
                filter: mut f,
                sel,
                ctx,
            } => {
                let items = self.filtered_models();
                match k.code {
                    KeyCode::Up => {
                        if !items.is_empty() {
                            let s = (sel + items.len() - 1) % items.len();
                            self.modal = Some(Modal::ModelPicker {
                                filter: f,
                                sel: s,
                                ctx,
                            });
                        }
                    }
                    KeyCode::Down => {
                        if !items.is_empty() {
                            let s = (sel + 1) % items.len();
                            self.modal = Some(Modal::ModelPicker {
                                filter: f,
                                sel: s,
                                ctx,
                            });
                        }
                    }
                    KeyCode::Backspace => {
                        f.backspace();
                        self.modal = Some(Modal::ModelPicker {
                            filter: f,
                            sel: 0,
                            ctx,
                        });
                    }
                    KeyCode::Char(c) if !ctrl => {
                        f.insert_char(c);
                        self.modal = Some(Modal::ModelPicker {
                            filter: f,
                            sel: 0,
                            ctx,
                        });
                    }
                    KeyCode::Enter => {
                        self.modal = None;
                        if let Some(info) = items.get(sel).cloned() {
                            self.apply_model(info, ctx);
                        }
                    }
                    _ => {}
                }
            }
            Modal::FilePicker(_) => {
                // actions needing &mut self are handled before borrowing the picker
                match k.code {
                    KeyCode::Enter => {
                        self.picker_activate();
                    }
                    KeyCode::F(5) => {
                        self.picker_attach_marked();
                    }
                    _ => {
                        if let Some(Modal::FilePicker(picker)) = self.modal.as_mut() {
                            let len = picker.entries.len();
                            match k.code {
                                KeyCode::Up => picker.sel = picker.sel.saturating_sub(1),
                                KeyCode::Down => {
                                    if picker.sel + 1 < len {
                                        picker.sel += 1;
                                    }
                                }
                                KeyCode::Home => picker.sel = 0,
                                KeyCode::End => picker.sel = len.saturating_sub(1),
                                KeyCode::PageUp => picker.sel = picker.sel.saturating_sub(8),
                                KeyCode::PageDown => {
                                    picker.sel = (picker.sel + 8).min(len.saturating_sub(1))
                                }
                                KeyCode::Insert => {
                                    if let Some(e) = picker.entries.get(picker.sel) {
                                        if !e.is_dir {
                                            if !picker.marked.insert(e.path.clone()) {
                                                picker.marked.remove(&e.path);
                                            }
                                            if picker.sel + 1 < len {
                                                picker.sel += 1;
                                            }
                                        }
                                    }
                                }
                                KeyCode::Delete => {
                                    if let Some(e) = picker.entries.get(picker.sel) {
                                        picker.marked.remove(&e.path);
                                    }
                                }
                                KeyCode::Tab => {
                                    if picker.dir.is_some() {
                                        picker.dir = None;
                                    } else {
                                        picker.dir = Some(dirs::home_dir().unwrap_or_default());
                                    }
                                    Self::load_picker_dir(picker);
                                }
                                KeyCode::Backspace => {
                                    let parent = picker
                                        .dir
                                        .clone()
                                        .and_then(|d| d.parent().map(|p| p.to_path_buf()));
                                    if let Some(p) = parent {
                                        picker.dir = Some(p);
                                    } else {
                                        picker.dir = None;
                                    }
                                    Self::load_picker_dir(picker);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            Modal::Input {
                title,
                mut editor,
                action,
            } => match k.code {
                KeyCode::Enter => {
                    self.modal = None;
                    let value = editor.text();
                    self.apply_input(value, action);
                }
                _ => {
                    match k.code {
                        KeyCode::Backspace => editor.backspace(),
                        KeyCode::Delete => editor.delete(),
                        KeyCode::Left => editor.left(),
                        KeyCode::Right => editor.right(),
                        KeyCode::Up => editor.up(),
                        KeyCode::Down => editor.down(),
                        KeyCode::Home => editor.home(),
                        KeyCode::End => editor.end(),
                        KeyCode::Char(c) if !ctrl => editor.insert_char(c),
                        _ => {}
                    }
                    self.modal = Some(Modal::Input {
                        title,
                        editor,
                        action,
                    });
                }
            },
            Modal::List {
                title,
                items,
                sel,
                action,
            } => {
                let len = items.len().max(1);
                match k.code {
                    KeyCode::Up => {
                        self.modal = Some(Modal::List {
                            title,
                            items,
                            sel: (sel + len - 1) % len,
                            action,
                        })
                    }
                    KeyCode::Down => {
                        self.modal = Some(Modal::List {
                            title,
                            items,
                            sel: (sel + 1) % len,
                            action,
                        })
                    }
                    KeyCode::Enter => {
                        self.modal = None;
                        if let Some((id, _)) = items.into_iter().nth(sel) {
                            self.apply_list(id, action);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn filtered_models(&self) -> Vec<ModelInfo> {
        let harness = match &self.modal {
            Some(Modal::ModelPicker { ctx, .. }) => match ctx {
                ModelCtx::CurrentChat => self.cur.harness.clone(),
                ModelCtx::HarnessDefault { harness } => harness.clone(),
            },
            _ => self.active_harness.clone(),
        };
        let filter = match &self.modal {
            Some(Modal::ModelPicker { filter, .. }) => filter.text().to_lowercase(),
            _ => String::new(),
        };
        self.models
            .get(&harness)
            .map(|list| {
                list.iter()
                    .filter(|m| filter.is_empty() || m.matches(&filter))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn apply_model(&mut self, info: ModelInfo, ctx: ModelCtx) {
        let r = ModelRef {
            provider_id: info.provider_id,
            model_id: info.model_id,
        };
        match ctx {
            ModelCtx::CurrentChat => {
                self.cur.model_override = Some(r.clone());
                if let Some(h) = self.cfg.harness_mut(&self.cur.harness) {
                    h.default_model = Some(r);
                }
                let _ = self.cfg.save();
                self.toast(
                    format!(
                        "✓ {}",
                        self.current_model()
                            .map(|m| m.display())
                            .unwrap_or_default()
                    ),
                    false,
                );
            }
            ModelCtx::HarnessDefault { harness } => {
                if let Some(h) = self.cfg.harness_mut(&harness) {
                    h.default_model = Some(r);
                }
                let _ = self.cfg.save();
            }
        }
    }

    fn switch_harness(&mut self, idx: usize) {
        let Some(h) = self.cfg.harnesses.get(idx).cloned() else {
            return;
        };
        if !backend::supported_kind(&h.kind) {
            self.toast(
                format!("{}: {}", h.name, self.tr(Str::SettingsDisabledKind)),
                true,
            );
            return;
        }
        if !h.enabled {
            if let Some(hh) = self.cfg.harness_mut(&h.id) {
                hh.enabled = true;
            }
            let _ = self.cfg.save();
        }
        self.active_harness = h.id.clone();
        self.ensure_backend(&h.id);
        self.new_chat();
    }

    fn settings_activate(&mut self, sel: usize, row: SettingsRow) {
        match row.kind {
            SettingsKind::Info => {
                // re-open settings on the same row
                self.modal = Some(Modal::Settings { sel });
            }
            SettingsKind::Lang => {
                self.cfg.lang = self.cfg.lang.next();
                let _ = self.cfg.save();
                self.modal = Some(Modal::Settings { sel });
            }
            SettingsKind::Profile => {
                self.modal = Some(Modal::Input {
                    title: self.tr(Str::SettingsProfile).to_string(),
                    editor: Editor::with_text(&self.cfg.profile_name),
                    action: InputAction::ProfileName,
                });
            }
            SettingsKind::ActiveHarness => {
                let sel0 = self
                    .cfg
                    .harnesses
                    .iter()
                    .position(|h| h.id == self.active_harness)
                    .unwrap_or(0);
                self.modal = Some(Modal::HarnessPicker { sel: sel0 });
            }
            SettingsKind::Enabled => {
                let id = self.active_harness.clone();
                if let Some(h) = self.cfg.harness_mut(&id) {
                    h.enabled = !h.enabled;
                }
                let _ = self.cfg.save();
                self.modal = Some(Modal::Settings { sel });
            }
            SettingsKind::ServerUrl => {
                let id = self.active_harness.clone();
                let cur = self
                    .cfg
                    .harness(&id)
                    .map(|h| h.server_url.clone())
                    .unwrap_or_default();
                self.modal = Some(Modal::Input {
                    title: self.tr(Str::SettingsServerUrl).to_string(),
                    editor: Editor::with_text(&cur),
                    action: InputAction::ServerUrl { harness: id },
                });
            }
            SettingsKind::ServerPassword => {
                let id = self.active_harness.clone();
                let cur = self
                    .cfg
                    .harness(&id)
                    .map(|h| h.server_password.clone())
                    .unwrap_or_default();
                self.modal = Some(Modal::Input {
                    title: self.tr(Str::SettingsServerPassword).to_string(),
                    editor: Editor::with_text(&cur),
                    action: InputAction::ServerPassword { harness: id },
                });
            }
            SettingsKind::Workdir => {
                let id = self.active_harness.clone();
                self.open_file_picker(true, Some(id));
            }
            SettingsKind::Agent => {
                let id = self.active_harness.clone();
                let agents = self.agents.get(&id).cloned().unwrap_or_default();
                let items: Vec<(String, String)> = agents
                    .iter()
                    .map(|a| (a.id.clone(), a.name.clone()))
                    .collect();
                self.modal = Some(Modal::List {
                    title: self.tr(Str::SettingsAgent).to_string(),
                    items,
                    sel: 0,
                    action: ListAction::PickAgent { harness: id },
                });
            }
            SettingsKind::AutoApprove => {
                let id = self.active_harness.clone();
                if let Some(h) = self.cfg.harness_mut(&id) {
                    h.auto_approve = !h.auto_approve;
                }
                let _ = self.cfg.save();
                self.modal = Some(Modal::Settings { sel });
            }
            SettingsKind::DefaultModel => {
                let id = self.active_harness.clone();
                self.modal = Some(Modal::ModelPicker {
                    filter: Editor::new(),
                    sel: 0,
                    ctx: ModelCtx::HarnessDefault { harness: id },
                });
            }
        }
    }

    fn apply_input(&mut self, value: String, action: InputAction) {
        match action {
            InputAction::RenameSession { harness, native_id } => {
                let title = value.trim().to_string();
                if title.is_empty() {
                    return;
                }
                if self.cur.harness == harness && self.cur.native_id == native_id {
                    self.cur.title = title.clone();
                }
                if let Some(sessions) = self.sessions.get_mut(&harness) {
                    if let Some(s) = sessions.iter_mut().find(|s| s.native_id == native_id) {
                        s.title = title.clone();
                    }
                }
                if let Some(b) = self.backends.get(&harness).cloned() {
                    tokio::spawn(async move {
                        let _ = b.rename_session(&native_id, &title).await;
                    });
                }
            }
            InputAction::ProfileName => {
                self.cfg.profile_name = value.trim().to_string();
                let _ = self.cfg.save();
            }
            InputAction::ServerUrl { harness } => {
                if let Some(h) = self.cfg.harness_mut(&harness) {
                    h.server_url = value.trim().to_string();
                }
                let _ = self.cfg.save();
                // restart backend
                if let Some(b) = self.backends.remove(&harness) {
                    b.shutdown();
                }
                self.ensure_backend(&harness);
                self.toast(self.tr(Str::SettingsRestartNote), false);
            }
            InputAction::NewTaskTitle => {
                let title = value.trim().to_string();
                if !title.is_empty() {
                    let t = {
                        let mut b = self.board.lock().unwrap();
                        b.add_task(&title)
                    };
                    self.toast(format!("{} added to Backlog", t.id), false);
                }
            }
            InputAction::BoardTitle { task_id } => {
                let title = value.trim().to_string();
                if !title.is_empty() {
                    let mut b = self.board.lock().unwrap();
                    let _ = b.edit_task(&task_id, &title);
                }
            }
            InputAction::ServerPassword { harness } => {
                if let Some(h) = self.cfg.harness_mut(&harness) {
                    h.server_password = value.trim().to_string();
                }
                let _ = self.cfg.save();
                if let Some(b) = self.backends.remove(&harness) {
                    b.shutdown();
                }
                self.ensure_backend(&harness);
                self.toast(self.tr(Str::SettingsRestartNote), false);
            }
        }
    }

    fn apply_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::DeleteSession { harness, native_id } => {
                if self.cur.harness == harness && self.cur.native_id == native_id {
                    self.cur = CurrentChat::draft(&self.active_harness.clone());
                }
                self.sessions
                    .entry(harness.clone())
                    .and_modify(|l| l.retain(|s| s.native_id != native_id));
                self.sidebar_sel = None;
                if let Some(b) = self.backends.get(&harness).cloned() {
                    tokio::spawn(async move {
                        let _ = b.delete_session(&native_id).await;
                    });
                }
                self.spawn_refresh_sessions(&harness);
                self.toast(self.tr(Str::SessionDeleted), false);
            }
        }
    }

    fn apply_list(&mut self, id: String, action: ListAction) {
        match action {
            ListAction::PickAgent { harness } => {
                if let Some(h) = self.cfg.harness_mut(&harness) {
                    h.agent = id;
                }
                let _ = self.cfg.save();
            }
            ListAction::AssignHarness => {
                if let Some(t) = self.selected_board_task() {
                    let mut b = self.board.lock().unwrap();
                    let _ = b.assign(&t.id, Some(id));
                }
            }
            ListAction::AssignRole => {
                if let Some(t) = self.selected_board_task() {
                    let mut b = self.board.lock().unwrap();
                    // role carries its harness; keep the current one if unset
                    let harness = b
                        .projection()
                        .tasks
                        .iter()
                        .find(|x| x.id == t.id)
                        .and_then(|x| x.harness.clone());
                    let _ = b.assign_with_role(&t.id, harness, Some(id));
                }
            }
            ListAction::PermissionReply {
                harness,
                session_id,
                request_id,
            } => {
                let reply = match id.as_str() {
                    "always" => "always",
                    "reject" => "reject",
                    _ => "once",
                };
                if let Some(b) = self.backends.get(&harness).cloned() {
                    tokio::spawn(async move {
                        if let Err(e) = b.permission_reply(&session_id, &request_id, reply).await {
                            eprintln!("permission reply failed: {e:#}");
                        }
                    });
                }
            }
        }
    }

    /// Rows of the settings modal for the active harness.
    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        let lang = self.cfg.lang;
        let mut rows = Vec::new();
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsLanguage).into(),
            value: lang.title().to_string(),
            kind: SettingsKind::Lang,
        });
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsProfile).into(),
            value: self.cfg.profile_name.clone(),
            kind: SettingsKind::Profile,
        });
        let h = self.cfg.harness(&self.active_harness);
        let (hname, hkind, henabled, hsrv, hpw, hwd, hagent, hmodel, hauto, supported) = match h {
            Some(h) => (
                format!("{} [{}]", h.name, h.id),
                h.kind.clone(),
                h.enabled,
                h.server_url.clone(),
                h.server_password.clone(),
                h.workdir.clone(),
                h.agent.clone(),
                h.default_model
                    .as_ref()
                    .map(|m| m.display())
                    .unwrap_or_default(),
                h.auto_approve,
                backend::supported_kind(&h.kind),
            ),
            None => (
                "—".into(),
                String::new(),
                false,
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                false,
                false,
            ),
        };
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsActiveHarness).into(),
            value: hname.clone(),
            kind: SettingsKind::ActiveHarness,
        });
        let enabled_val = if !supported {
            t(lang, Str::SettingsDisabledKind).to_string()
        } else if henabled {
            "✓".into()
        } else {
            "✗".into()
        };
        rows.push(SettingsRow {
            label: format!("{}: {}", t(lang, Str::SettingsEnabled), hkind),
            value: enabled_val,
            kind: SettingsKind::Enabled,
        });
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsServerUrl).into(),
            value: if hsrv.is_empty() {
                t(lang, Str::None_).to_string()
            } else {
                hsrv.clone()
            },
            kind: SettingsKind::ServerUrl,
        });
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsServerPassword).into(),
            value: if hpw.is_empty() {
                t(lang, Str::None_).to_string()
            } else {
                "••••".into()
            },
            kind: SettingsKind::ServerPassword,
        });
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsWorkdir).into(),
            value: if hwd.is_empty() {
                ".".into()
            } else {
                hwd.clone()
            },
            kind: SettingsKind::Workdir,
        });
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsAgent).into(),
            value: hagent.clone(),
            kind: SettingsKind::Agent,
        });
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsAutoApprove).into(),
            value: if hauto { "✓".into() } else { "✗".into() },
            kind: SettingsKind::AutoApprove,
        });
        rows.push(SettingsRow {
            label: t(lang, Str::SettingsDefaultModel).into(),
            value: if hmodel.is_empty() {
                t(lang, Str::None_).to_string()
            } else {
                hmodel.clone()
            },
            kind: SettingsKind::DefaultModel,
        });
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        AgentInfo, AttachmentMeta, Backend, BackendEvent, ChatMsg, ModelInfo, PartKind, RenderPart,
        SendOpts, SessionInfo,
    };
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// Scenario-driven fake harness backend: no IO, just the event contract.
    struct ScriptedBackend {
        harness: String,
        last_permission: StdMutex<Option<(String, String)>>, // (session, request id)
    }

    #[async_trait]
    impl Backend for ScriptedBackend {
        fn harness_id(&self) -> &str {
            &self.harness
        }
        async fn connect(&self) -> anyhow::Result<String> {
            Ok("scripted".into())
        }
        async fn refresh_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }
        async fn create_session(&self, _title: &str) -> anyhow::Result<SessionInfo> {
            Ok(SessionInfo {
                native_id: "pending-1".into(),
                title: "t".into(),
                updated_ms: 0,
            })
        }
        async fn rename_session(&self, _id: &str, _t: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete_session(&self, _id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn load_history(&self, _id: &str) -> anyhow::Result<Vec<ChatMsg>> {
            Ok(Vec::new())
        }
        async fn send(
            &self,
            _id: &str,
            _text: &str,
            _attachments: &[AttachmentMeta],
            _opts: SendOpts<'_>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn abort(&self, _id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn permission_reply(&self, sid: &str, rid: &str, _reply: &str) -> anyhow::Result<()> {
            *self.last_permission.lock().unwrap() = Some((sid.into(), rid.into()));
            Ok(())
        }
        async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
            Ok(Vec::new())
        }
        async fn list_agents(&self) -> anyhow::Result<Vec<AgentInfo>> {
            Ok(Vec::new())
        }
        fn shutdown(&self) {}
        fn is_connected(&self) -> bool {
            true
        }
    }

    fn app_with(kind: &str) -> App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = crate::config::Config {
            lang: crate::i18n::Lang::En,
            profile_name: "tester".into(),
            harnesses: vec![crate::config::HarnessCfg {
                id: kind.into(),
                name: "Scripted".into(),
                // unsupported kind on purpose: App::new must not connect or
                // spawn any real harness process from unit tests
                kind: "stub".into(),
                enabled: true,
                server_url: String::new(),
                server_password: String::new(),
                workdir: String::new(),
                agent: String::new(),
                auto_approve: false,
                default_model: None,
            }],
        };
        App::new(cfg, rx)
    }

    // Regression 1 (the review's wedge): a pending id is rebound to the real
    // one; StreamDone must still clear the busy state.
    #[tokio::test]
    async fn busy_clears_after_pending_rebind() {
        let mut app = app_with("hermes");
        app.backends.insert(
            "hermes".into(),
            std::sync::Arc::new(ScriptedBackend {
                harness: "hermes".into(),
                last_permission: StdMutex::new(None),
            }),
        );
        app.cur.harness = "hermes".into();
        app.cur.native_id = "pending-1".into();
        app.busy.insert(("hermes".into(), "pending-1".into()));

        app.on_backend_event(BackendEvent::PartDelta {
            harness: "hermes".into(),
            native_id: "pending-1".into(),
            msg_id: "hm".into(),
            part_id: "t0".into(),
            kind: PartKind::Text,
            delta: "hi".into(),
        });
        assert!(app.busy_now(), "busy while streaming");

        app.on_backend_event(BackendEvent::SessionCreated {
            harness: "hermes".into(),
            native_id: "local-real".into(),
            title: "t".into(),
        });
        assert_eq!(app.cur.native_id, "local-real", "cur rebound to real id");
        assert!(app.busy_now(), "busy follows the rebound id");

        app.on_backend_event(BackendEvent::StreamDone {
            harness: "hermes".into(),
            native_id: "pending-1".into(), // backend sends the id the UI tracked
        });
        assert!(!app.busy_now(), "busy cleared — the old wedge is gone");
    }

    // Regression 2: send error rolls the optimistic message back into the editor.
    #[tokio::test]
    async fn send_error_rolls_back_optimistic_message() {
        let mut app = app_with("hermes");
        app.backends.insert(
            "hermes".into(),
            std::sync::Arc::new(ScriptedBackend {
                harness: "hermes".into(),
                last_permission: StdMutex::new(None),
            }),
        );
        app.cur.harness = "hermes".into();
        app.cur.native_id = "pending-1".into();
        app.editor.set_text("");
        app.last_sent = Some("hello?".into());
        app.cur.msgs.push(ChatMsg {
            msg_id: "local_0".into(),
            role: crate::backend::Role::User,
            parts: vec![RenderPart {
                part_id: "t0".into(),
                kind: PartKind::Text,
                text: "hello?".into(),
            }],
            attachments: Vec::new(),
        });

        app.on_backend_event(BackendEvent::Error {
            harness: "hermes".into(),
            error: "send failed".into(),
        });
        assert!(app.cur.msgs.is_empty(), "optimistic message removed");
        assert_eq!(app.editor.text(), "hello?", "text restored to the composer");
    }

    // Regression 3: permission requests open the modal and the answer reaches
    // the backend.
    #[tokio::test]
    async fn permission_request_opens_modal_and_replies() {
        let mut app = app_with("opencode");
        let backend = std::sync::Arc::new(ScriptedBackend {
            harness: "opencode".into(),
            last_permission: StdMutex::new(None),
        });
        app.backends.insert("opencode".into(), backend.clone());
        app.cur.harness = "opencode".into();
        app.cur.native_id = "ses_1".into();

        app.on_backend_event(BackendEvent::PermissionRequest {
            harness: "opencode".into(),
            session_id: "ses_1".into(),
            request_id: "jsonrpc:7".into(),
            title: "bash: rm -rf".into(),
        });
        assert!(
            matches!(
                &app.modal,
                Some(Modal::List {
                    action: ListAction::PermissionReply { .. },
                    ..
                })
            ),
            "permission modal opened"
        );
        let Some(Modal::List {
            items, sel, action, ..
        }) = app.modal.clone()
        else {
            unreachable!()
        };
        assert_eq!(items.len(), 3, "once / always / reject");

        let id = items.get(sel).map(|(i, _)| i.clone()).unwrap_or_default();
        app.apply_list(id, action);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            backend.last_permission.lock().unwrap().clone(),
            Some(("ses_1".into(), "jsonrpc:7".into())),
            "reply reached the backend"
        );
    }

    // Regression 4: events of a foreign session must not clear another chat's
    // busy state (and must not render into it).
    #[tokio::test]
    async fn foreign_stream_done_does_not_touch_current_chat() {
        let mut app = app_with("hermes");
        app.backends.insert(
            "hermes".into(),
            std::sync::Arc::new(ScriptedBackend {
                harness: "hermes".into(),
                last_permission: StdMutex::new(None),
            }),
        );
        app.cur.harness = "hermes".into();
        app.cur.native_id = "local-A".into();
        app.busy.insert(("hermes".into(), "local-A".into()));

        app.on_backend_event(BackendEvent::StreamDone {
            harness: "hermes".into(),
            native_id: "local-B".into(),
        });
        assert!(app.busy_now(), "chat A is still busy after B finished");
        assert!(app.cur.msgs.is_empty(), "B's events do not render into A");
    }
}
