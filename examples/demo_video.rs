//! Deterministic 1920x1080 demo screencast for ai-commander.
//!
//! Drives the REAL `App` (the same `widgets::draw` + `on_key`/`on_input`/
//! `on_backend_event` paths the production loop uses) through a scripted
//! scenario on a ratatui `TestBackend`, rasterizes every frame with
//! Cascadia Mono into an RGBA buffer and pipes raw frames into ffmpeg.
//!
//! No real harness is contacted (a scripted in-process backend), the config
//! dir / kanban store / worktrees live in a throwaway temp dir, and the
//! only non-deterministic pixels are the HH:MM wall clock.
//!
//! Run: `cargo run --example demo_video --release [-- <out.mp4>]`
//! Needs: ffmpeg on PATH, a monospace font with box-drawing glyphs
//! (Cascadia Mono / Consolas on Windows, checked in that order).

use std::collections::HashMap;
use std::io::Write as _;

use anyhow::{Context as _, Result};

use ai_commander::backend::{
    AgentInfo, AttachmentMeta, Backend, BackendEvent, ChatMsg, CompactionMethod, CompactionSource,
    CompactionStatus, ModelInfo, PartKind, RenderPart, Role, SendOpts, SessionInfo,
};
use ai_commander::config::{Config, HarnessCfg, ModelRef};
use ai_commander::kanban::TaskAction;
use ai_commander::ui::defrag::CompactionView;
use ai_commander::ui::theme::THEME;
use ai_commander::ui::widgets;
use ai_commander::ui::{App, CurrentChat};

// ---------- geometry ----------
const FPS: u64 = 25;
const CELL_W: usize = 12;
const CELL_H: usize = 24;
const COLS: u16 = 160; // 160 * 12 = 1920
const ROWS: u16 = 45; //  45 * 24 = 1080
const W: usize = COLS as usize * CELL_W;
const H: usize = ROWS as usize * CELL_H;

const HARNESS: &str = "demo";

// ---------- the scripted backend: no IO, just the trait contract ----------

struct DemoBackend {
    harness: String,
}

#[async_trait::async_trait]
impl Backend for DemoBackend {
    fn harness_id(&self) -> &str {
        &self.harness
    }
    async fn connect(&self) -> Result<String> {
        Ok("demo-agent 1.0".into())
    }
    async fn refresh_sessions(&self) -> Result<Vec<SessionInfo>> {
        Ok(Vec::new())
    }
    async fn create_session(&self, _title: &str) -> Result<SessionInfo> {
        Ok(SessionInfo {
            native_id: format!("tsk-{}", std::process::id() % 1000),
            title: _title.to_string(),
            updated_ms: now_ms(),
        })
    }
    async fn rename_session(&self, _id: &str, _t: &str) -> Result<()> {
        Ok(())
    }
    async fn delete_session(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn load_history(&self, _id: &str) -> Result<Vec<ChatMsg>> {
        Ok(Vec::new())
    }
    async fn send(
        &self,
        _id: &str,
        _text: &str,
        _attachments: &[AttachmentMeta],
        _opts: SendOpts<'_>,
    ) -> Result<()> {
        Ok(())
    }
    async fn abort(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn permission_reply(&self, _sid: &str, _rid: &str, _reply: &str) -> Result<()> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }
    async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        Ok(Vec::new())
    }
    fn shutdown(&self) {}
    fn is_connected(&self) -> bool {
        true
    }
}

fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

// ---------- cell rasterizer ----------

struct GlyphMask {
    w: usize,
    h: usize,
    alpha: Vec<u8>,
}

struct Painter {
    font: ab_glyph::FontRef<'static>,
    scale: ab_glyph::PxScale,
    baseline: f32,
    cache: HashMap<(char, usize), Option<GlyphMask>>,
}

fn load_font() -> ab_glyph::FontRef<'static> {
    let candidates = [
        "C:/Windows/Fonts/CascadiaMono.ttf",
        "C:/Windows/Fonts/consola.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(f) = ab_glyph::FontRef::try_from_slice(bytes.leak()) {
                eprintln!("demo font: {path}");
                return f;
            }
        }
    }
    panic!("no monospace font found for the demo renderer");
}

impl Painter {
    fn new() -> Self {
        let font = load_font();
        use ab_glyph::{Font, ScaleFont};
        // pick the scale so the advance is exactly CELL_W px (mono font):
        // advance at scale 1.0 gives px per scale unit
        let one = ab_glyph::PxScale { x: 1.0, y: 1.0 };
        let s1 = font.as_scaled(one);
        let adv = s1.h_advance(s1.glyph_id('M'));
        let mut scale = ab_glyph::PxScale {
            x: CELL_W as f32 / adv,
            y: CELL_W as f32 / adv,
        };
        // ab_glyph descent is NEGATIVE (below baseline); the line box is
        // ascent - descent + gap
        let mut line = {
            let s = font.as_scaled(scale);
            s.ascent() - s.descent() + s.line_gap()
        };
        // keep 2px headroom so descenders never clip on the cell edge
        if line > CELL_H as f32 - 2.0 {
            let k = (CELL_H as f32 - 2.0) / line;
            scale.x *= k;
            scale.y *= k;
            line = {
                let s = font.as_scaled(scale);
                s.ascent() - s.descent() + s.line_gap()
            };
        }
        let ascent = font.as_scaled(scale).ascent();
        let baseline = (CELL_H as f32 - line) / 2.0 + ascent;
        Painter {
            font,
            scale,
            baseline,
            cache: HashMap::new(),
        }
    }

    fn mask(&mut self, ch: char, cells: usize) -> Option<&GlyphMask> {
        if !self.cache.contains_key(&(ch, cells)) {
            let m = self.rasterize(ch, cells);
            self.cache.insert((ch, cells), m);
        }
        self.cache.get(&(ch, cells)).and_then(|m| m.as_ref())
    }

    fn rasterize(&self, ch: char, cells: usize) -> Option<GlyphMask> {
        use ab_glyph::{Font, Glyph, Point};
        let cw = cells * CELL_W;
        let gid = self.font.glyph_id(ch);
        if gid == ab_glyph::GlyphId(0) && ch != '\0' {
            return None; // not in the font
        }
        let glyph = Glyph {
            id: gid,
            scale: self.scale,
            position: Point {
                x: 0.0,
                y: self.baseline,
            },
        };
        let outlined = self.font.outline_glyph(glyph)?;
        let b = outlined.px_bounds();
        let mut alpha = vec![0u8; cw * CELL_H];
        outlined.draw(|x, y, c| {
            // draw() coords are relative to the glyph bbox top-left
            let px = b.min.x as i64 + x as i64;
            let py = b.min.y as i64 + y as i64;
            if px >= 0 && px < cw as i64 && py >= 0 && py < CELL_H as i64 {
                let v = (c.clamp(0.0, 1.0) * 255.0) as u8;
                let i = py as usize * cw + px as usize;
                if v > alpha[i] {
                    alpha[i] = v;
                }
            }
        });
        Some(GlyphMask {
            w: cw,
            h: CELL_H,
            alpha,
        })
    }

    fn paint(
        &mut self,
        buf: &ratatui::buffer::Buffer,
        out: &mut [u8],
        cursor: Option<(u16, u16)>,
        cursor_on: bool,
    ) {
        use ratatui::style::Modifier;
        let def_fg = color_rgb(&THEME.fg, [0xE8, 0xEC, 0xEC]);
        let def_bg = color_rgb(&THEME.bg, [0x00, 0x00, 0x8A]);
        let conv = |c: &ratatui::style::Color, def: [u8; 3]| -> [u8; 3] { color_rgb(c, def) };
        let area = buf.area();
        for cy in 0..area.height {
            for cx in 0..area.width {
                let cell = &buf[(cx, cy)];
                let sym = cell.symbol();
                if sym.is_empty() {
                    continue; // continuation of a wide cell
                }
                let mut fg = conv(&cell.fg, def_fg);
                let mut bg = conv(&cell.bg, def_bg);
                if cell.modifier.contains(Modifier::REVERSED) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.modifier.contains(Modifier::BOLD) {
                    fg = [fg[0] / 4 * 3 + 64, fg[1] / 4 * 3 + 64, fg[2] / 4 * 3 + 64];
                }
                let px0 = cx as usize * CELL_W;
                let py0 = cy as usize * CELL_H;
                // background fill
                for y in 0..CELL_H {
                    let row = (py0 + y) * W;
                    for x in 0..CELL_W {
                        let i = (row + px0 + x) * 4;
                        out[i] = bg[0];
                        out[i + 1] = bg[1];
                        out[i + 2] = bg[2];
                        out[i + 3] = 255;
                    }
                }
                // glyph coverage (each char of a multi-char grapheme takes
                // one cell; the mask spans the grapheme's full width)
                let cells_w = unicode_width::UnicodeWidthStr::width(sym).max(1);
                let mut pen = px0;
                for ch in sym.chars() {
                    if let Some(m) = self.mask(ch, cells_w) {
                        blend_mask(out, pen, py0, m, fg);
                    }
                    pen += CELL_W;
                }
            }
        }
        // the terminal cursor as a blinking block
        if cursor_on {
            if let Some((cx, cy)) = cursor {
                if cx < area.width && cy < area.height {
                    let cell = &buf[(cx, cy)];
                    let mut fg = conv(&cell.fg, def_fg);
                    let mut bg = conv(&cell.bg, def_bg);
                    std::mem::swap(&mut fg, &mut bg);
                    let px0 = cx as usize * CELL_W;
                    let py0 = cy as usize * CELL_H;
                    for y in 0..CELL_H {
                        for x in 0..CELL_W {
                            let i = ((py0 + y) * W + px0 + x) * 4;
                            out[i] = fg[0];
                            out[i + 1] = fg[1];
                            out[i + 2] = fg[2];
                            out[i + 3] = 255;
                        }
                    }
                    let sym = cell.symbol();
                    if let Some(ch) = sym.chars().next() {
                        if let Some(m) = self.mask(ch, 1) {
                            blend_mask(out, px0, py0, m, bg);
                        }
                    }
                }
            }
        }
    }
}

fn color_rgb(c: &ratatui::style::Color, def: [u8; 3]) -> [u8; 3] {
    use ratatui::style::Color;
    match c {
        Color::Rgb(r, g, b) => [*r, *g, *b],
        Color::Reset => def,
        Color::Black => [0x00, 0x00, 0x00],
        Color::Red => [0xAA, 0x00, 0x00],
        Color::Green => [0x00, 0xAA, 0x00],
        Color::Yellow => [0xAA, 0xAA, 0x00],
        Color::Blue => [0x00, 0x00, 0xAA],
        Color::Magenta => [0xAA, 0x00, 0xAA],
        Color::Cyan => [0x00, 0xAA, 0xAA],
        Color::Gray => [0xAA, 0xAA, 0xAA],
        Color::DarkGray => [0x55, 0x55, 0x55],
        Color::LightRed => [0xFF, 0x55, 0x55],
        Color::LightGreen => [0x55, 0xFF, 0x55],
        Color::LightYellow => [0xFF, 0xFF, 0x55],
        Color::LightBlue => [0x55, 0x55, 0xFF],
        Color::LightMagenta => [0xFF, 0x55, 0xFF],
        Color::LightCyan => [0x55, 0xFF, 0xFF],
        Color::White => [0xFF, 0xFF, 0xFF],
        Color::Indexed(i) => cga(*i),
    }
}

fn blend_mask(out: &mut [u8], px0: usize, py0: usize, m: &GlyphMask, fg: [u8; 3]) {
    for y in 0..m.h {
        if py0 + y >= H {
            break;
        }
        let row = (py0 + y) * W;
        for x in 0..m.w {
            if px0 + x >= W {
                break;
            }
            let a = u32::from(m.alpha[y * m.w + x]);
            if a == 0 {
                continue;
            }
            let i = (row + px0 + x) * 4;
            let ia = 255 - a;
            out[i] = ((u32::from(fg[0]) * a + u32::from(out[i]) * ia) / 255) as u8;
            out[i + 1] = ((u32::from(fg[1]) * a + u32::from(out[i + 1]) * ia) / 255) as u8;
            out[i + 2] = ((u32::from(fg[2]) * a + u32::from(out[i + 2]) * ia) / 255) as u8;
        }
    }
}

fn cga(i: u8) -> [u8; 3] {
    const P: [[u8; 3]; 16] = [
        [0x00, 0x00, 0x00],
        [0x00, 0x00, 0xAA],
        [0x00, 0xAA, 0x00],
        [0x00, 0xAA, 0xAA],
        [0xAA, 0x00, 0x00],
        [0xAA, 0x00, 0xAA],
        [0xAA, 0x55, 0x00],
        [0xAA, 0xAA, 0xAA],
        [0x55, 0x55, 0x55],
        [0x55, 0x55, 0xFF],
        [0x55, 0xFF, 0x55],
        [0x55, 0xFF, 0xFF],
        [0xFF, 0x55, 0x55],
        [0xFF, 0x55, 0xFF],
        [0xFF, 0xFF, 0x55],
        [0xFF, 0xFF, 0xFF],
    ];
    P[(i % 16) as usize]
}

// ---------- the studio: app + terminal + encoder pipe ----------

struct Studio {
    app: App,
    term: ratatui::Terminal<ratatui::backend::TestBackend>,
    painter: Painter,
    out: std::process::Child,
    stdin: std::process::ChildStdin,
    frame: u64,
}

macro_rules! key {
    ($code:expr) => {
        crossterm::event::KeyEvent::new($code, crossterm::event::KeyModifiers::NONE)
    };
    ($code:expr, ctrl) => {
        crossterm::event::KeyEvent::new($code, crossterm::event::KeyModifiers::CONTROL)
    };
    ($code:expr, shift) => {
        crossterm::event::KeyEvent::new($code, crossterm::event::KeyModifiers::SHIFT)
    };
}

impl Studio {
    fn emit(&mut self) -> Result<()> {
        use ratatui::backend::Backend as _;
        self.term
            .draw(|f| widgets::draw(&mut self.app, f))
            .context("draw")?;
        let cursor = self
            .term
            .backend_mut()
            .get_cursor_position()
            .ok()
            .map(|p| (p.x, p.y));
        let mut px = vec![0u8; W * H * 4];
        let blink_on = self.frame % 14 < 9;
        let buf = self.term.backend().buffer().clone();
        self.painter.paint(&buf, &mut px, cursor, blink_on);
        self.stdin.write_all(&px).context("ffmpeg pipe")?;
        self.frame += 1;
        Ok(())
    }

    /// hold the current state for `secs` (repeat the frame)
    fn hold(&mut self, secs: f64) -> Result<()> {
        let n = (secs * FPS as f64).round() as u64;
        for _ in 0..n.max(1) {
            self.emit()?;
        }
        Ok(())
    }

    fn beat(&mut self, note: &str) {
        eprintln!("[frame {:5}] {}", self.frame, note);
    }

    fn key(&mut self, k: crossterm::event::KeyEvent) {
        self.app.on_key(k);
    }

    fn ev(&mut self, e: BackendEvent) {
        self.app.on_backend_event(e);
    }

    fn mouse(&mut self, kind: crossterm::event::MouseEventKind, x: u16, y: u16) -> Result<()> {
        self.app.on_input(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        ));
        self.emit()
    }

    /// type text into whatever has focus, `total_secs` for the whole string
    fn type_str(&mut self, s: &str, total_secs: f64) -> Result<()> {
        let chars: Vec<char> = s.chars().collect();
        let per = (total_secs * FPS as f64 / chars.len() as f64).max(1.0) as u64;
        for c in chars {
            // real terminals deliver the UPPERCASE char + SHIFT for caps
            let code = crossterm::event::KeyCode::Char(c);
            let k = if c.is_uppercase() {
                key!(code, shift)
            } else {
                key!(code)
            };
            self.key(k);
            for _ in 0..per {
                self.emit()?;
            }
        }
        Ok(())
    }

    fn step_defrag(&mut self) {
        if let Some((_, _, _, an)) = self.app.defrag.as_mut() {
            an.step();
        }
    }

    fn toast_off(&mut self) {
        self.app.toast = None;
    }
}

// ---------- the scenario ----------

fn main() -> Result<()> {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ai-commander-demo.mp4".into());
    let out_path = std::env::current_dir()?.join(out_path);

    // ---- throwaway environment (BEFORE App::new: cwd + env isolate the
    // kanban store, policy, human token and worktrees from the real ones)
    let root = std::env::temp_dir().join(format!("ai-commander-demo-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    // SAFETY: single-threaded here (no runtime, no App yet) — the process
    // env is not concurrently read while we mutate it.
    unsafe {
        std::env::set_var("AI_COMMANDER_CONFIG_DIR", &root);
        std::env::set_var("AI_COMMANDER_WORKTREES_DIR", root.join("worktrees"));
    }
    std::env::set_current_dir(&root)?;

    // ---- ffmpeg rawvideo pipe
    std::fs::create_dir_all(out_path.parent().unwrap_or(std::path::Path::new(".")))?;
    let mut ff = std::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-s")
        .arg(format!("{}x{}", W, H))
        .arg("-r")
        .arg(FPS.to_string())
        .arg("-i")
        .arg("-")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("medium")
        .arg("-crf")
        .arg("18")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg(out_path.to_str().unwrap())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("ffmpeg is required on PATH for the demo video")?;
    let stdin = ff.stdin.take().context("ffmpeg stdin")?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run(stdin, ff, out_path))
}

async fn run(
    stdin: std::process::ChildStdin,
    ff: std::process::Child,
    out_path: std::path::PathBuf,
) -> Result<()> {
    // ---- config: one stub-kind harness (no real process), demo model
    let cfg = Config {
        lang: ai_commander::i18n::Lang::En,
        profile_name: "@tsingular".into(),
        trusted_projects: Vec::new(),
        harnesses: vec![HarnessCfg {
            id: HARNESS.into(),
            name: "Demo Agent".into(),
            kind: "stub".into(),
            enabled: true,
            server_url: String::new(),
            server_password: String::new(),
            workdir: String::new(),
            agent: String::new(),
            auto_approve: false,
            reuse_opencode_key: false,
            default_model: Some(ModelRef {
                provider_id: "demo".into(),
                model_id: "agent-1".into(),
            }),
        }],
    };

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(cfg, rx);
    app.backends.insert(
        HARNESS.into(),
        std::sync::Arc::new(DemoBackend {
            harness: HARNESS.into(),
        }),
    );
    app.active_harness = HARNESS.into();
    app.versions.insert(HARNESS.into(), "1.0".into());
    app.compaction_view = CompactionView::Defrag;

    // ---- seed the session list + the open chat
    let t0 = now_ms();
    app.sessions.insert(
        HARNESS.into(),
        vec![
            SessionInfo {
                native_id: "s1".into(),
                title: "Context compaction".into(),
                updated_ms: t0,
            },
            SessionInfo {
                native_id: "s2".into(),
                title: "Kanban worktree flow".into(),
                updated_ms: t0 - 3_600_000,
            },
            SessionInfo {
                native_id: "s3".into(),
                title: "Policy engine".into(),
                updated_ms: t0 - 7_200_000,
            },
        ],
    );
    app.sidebar_sel = Some((HARNESS.into(), "s1".into()));
    app.cur = CurrentChat {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        prev_native_id: None,
        title: "Context compaction".into(),
        loading: false,
        model_override: None,
        msgs: vec![
            ChatMsg {
                msg_id: "m1".into(),
                role: Role::User,
                parts: vec![RenderPart {
                    part_id: "p1".into(),
                    kind: PartKind::Text,
                    text: "What happens when the context window fills up?".into(),
                }],
                attachments: Vec::new(),
            },
            ChatMsg {
                msg_id: "m2".into(),
                role: Role::Assistant,
                parts: vec![
                    RenderPart {
                        part_id: "p2".into(),
                        kind: PartKind::Tool,
                        text: "read src/ui/defrag.rs".into(),
                    },
                    RenderPart {
                        part_id: "p3".into(),
                        kind: PartKind::Text,
                        text: "The DEFRAG view renders compaction as a disk defragmenter: \
every block is a message, pruned ones go dark, and the summarized middle \
is rewritten in place."
                            .into(),
                    },
                ],
                attachments: Vec::new(),
            },
        ],
    };
    // baseline usage for the status line
    app.on_backend_event(BackendEvent::Usage {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        used: 96_000,
        size: 200_000,
        cost_usd: Some(0.07),
        estimated: false,
    });
    app.models.insert(
        HARNESS.into(),
        vec![ModelInfo {
            provider_id: "demo".into(),
            model_id: "agent-1".into(),
            name: "Demo Agent 1".into(),
            context_limit: Some(200_000),
        }],
    );

    // ---- seed the kanban board
    {
        use ai_commander::kanban::store::BoardStore;
        fn tr(b: &mut BoardStore, id: &str, a: TaskAction) -> anyhow::Result<()> {
            b.transition(id, a).map_err(anyhow::Error::msg).map(|_| ())
        }
        let mut b = app.board.lock();
        let _ = b.add_task("release notes");
        let t = b.add_task("DEFRAG overlay");
        // pre-assign the harness so Start launches on the demo backend
        b.assign(&t.id, Some(HARNESS.to_string()))
            .map_err(anyhow::Error::msg)?;
        tr(&mut b, &t.id, TaskAction::Promote)?;
        let t = b.add_task("context map");
        tr(&mut b, &t.id, TaskAction::Promote)?;
        tr(&mut b, &t.id, TaskAction::Start)?;
        let t = b.add_task("merge guard");
        tr(&mut b, &t.id, TaskAction::Promote)?;
        tr(&mut b, &t.id, TaskAction::Start)?;
        tr(&mut b, &t.id, TaskAction::Finish)?;
        let t = b.add_task("IPC tokens");
        tr(&mut b, &t.id, TaskAction::Promote)?;
        tr(&mut b, &t.id, TaskAction::Start)?;
        tr(&mut b, &t.id, TaskAction::Finish)?;
        tr(&mut b, &t.id, TaskAction::Approve)?;
        let t = b.add_task("roles loader");
        tr(&mut b, &t.id, TaskAction::Promote)?;
        tr(&mut b, &t.id, TaskAction::Start)?;
        tr(&mut b, &t.id, TaskAction::Block)?;
    }

    let term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(COLS, ROWS))
        .context("terminal")?;
    let mut st = Studio {
        app,
        term,
        painter: Painter::new(),
        out: ff,
        stdin,
        frame: 0,
    };

    scenario(&mut st)?;

    // ---- flush + finish the encode
    drop(st.stdin);
    let status = st.out.wait().context("ffmpeg wait")?;
    eprintln!("demo video: {} frames -> {}", st.frame, out_path.display());
    if !status.success() {
        anyhow::bail!("ffmpeg exited with {status}");
    }
    Ok(())
}

fn scenario(st: &mut Studio) -> Result<()> {
    use crossterm::event::KeyCode;

    // ===== 1. opening frame
    st.beat("opening");
    st.hold(3.0)?;

    // ===== 2. typing a question (unicode editor, Cyrillic + latin)
    st.beat("typing the question");
    st.type_str("Привет! How does the DEFRAG view track compaction?", 2.2)?;
    st.hold(0.6)?;

    // Ctrl+Enter -> second line (multiline composer)
    st.beat("Ctrl+Enter newline");
    st.key(key!(KeyCode::Enter, ctrl));
    st.emit()?;
    st.type_str("And can I trigger it manually?", 1.0)?;
    st.hold(0.6)?;

    // ===== 3. Enter -> send, streamed reply with a tool line
    st.beat("send + stream");
    st.key(key!(KeyCode::Enter));
    st.emit()?;
    st.hold(0.5)?;
    let reply_words = "When the window fills up, the middle of the dialogue is \
compacted: old tool outputs are pruned and the rest is summarized into a \
short recap. The pass is rendered as a DOS-style defragmenter — watch the \
blocks move. /compact triggers it manually where the harness supports it."
        .split_whitespace()
        .collect::<Vec<_>>();
    // tool call first
    st.ev(part_updated_tool());
    st.emit()?;
    st.hold(0.3)?;
    for (i, w) in reply_words.iter().enumerate() {
        st.ev(BackendEvent::PartDelta {
            harness: HARNESS.into(),
            native_id: "s1".into(),
            msg_id: "m3".into(),
            part_id: "p5".into(),
            kind: PartKind::Text,
            delta: format!("{w} "),
        });
        st.emit()?;
        if i % 3 == 2 {
            st.emit()?;
        }
    }
    // usage ticks up while streaming
    st.ev(BackendEvent::Usage {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        used: 118_000,
        size: 200_000,
        cost_usd: Some(0.11),
        estimated: false,
    });
    st.ev(BackendEvent::StreamDone {
        harness: HARNESS.into(),
        native_id: "s1".into(),
    });
    st.beat("reply done (status line updated)");
    st.hold(3.0)?;
    st.toast_off();

    // ===== 4. Ctrl+F search
    st.beat("search");
    st.key(key!(KeyCode::Char('f'), ctrl));
    st.emit()?;
    st.type_str("compaction", 0.9)?;
    st.key(key!(KeyCode::Enter));
    st.beat("search applied");
    st.hold(2.0)?;
    st.key(key!(KeyCode::F(3)));
    st.emit()?;
    st.hold(1.5)?;
    st.toast_off();

    // ===== 5. selection + copy
    st.beat("select + copy");
    for _ in 0..4 {
        st.key(key!(KeyCode::Down, ctrl));
        st.hold(0.55)?;
    }
    st.key(key!(KeyCode::Char('c'), ctrl));
    st.beat("copied toast");
    st.hold(2.5)?;
    st.toast_off();

    // ===== 6. Ctrl+R rename
    st.beat("rename");
    st.key(key!(KeyCode::Char('r'), ctrl));
    st.emit()?;
    // clear the prefilled title from the end
    for _ in 0.."Context compaction".len() {
        st.key(key!(KeyCode::Backspace));
        st.emit()?;
    }
    st.type_str("DEFRAG deep dive", 1.0)?;
    st.key(key!(KeyCode::Enter));
    st.beat("renamed (sidebar)");
    st.hold(3.0)?;
    st.toast_off();

    // ===== 7. sidebar resize by mouse drag on the ┊ handle
    st.beat("sidebar drag");
    let handle_x = st.app.sidebar_width - 1; // ┊ column
    st.emit()?; // populate hit targets
    st.mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        handle_x,
        12,
    )?;
    for i in 1..=8u16 {
        st.mouse(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            handle_x + i,
            12,
        )?;
    }
    st.mouse(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        handle_x + 8,
        12,
    )?;
    st.beat("sidebar wider");
    st.hold(3.0)?;
    // back with Ctrl+= (keyboard resize)
    for _ in 0..4 {
        st.key(key!(KeyCode::Char('='), ctrl));
        st.hold(0.45)?;
    }

    // ===== 8. the kanban board
    st.beat("board");
    st.key(key!(KeyCode::Char('b'), ctrl));
    st.hold(3.0)?;

    // navigate: to Ready column, to the DEFRAG task
    st.key(key!(KeyCode::Right));
    st.hold(0.9)?;
    st.key(key!(KeyCode::Down));
    st.hold(0.9)?;

    // new task via the modal
    st.beat("new task modal");
    st.key(key!(KeyCode::Left)); // back to Backlog for the modal demo
    st.hold(0.6)?;
    st.key(key!(KeyCode::Char('n')));
    st.emit()?;
    st.type_str("policy engine docs", 1.0)?;
    st.key(key!(KeyCode::Enter));
    st.beat("task created in Backlog");
    st.hold(3.0)?;
    st.toast_off();

    // start the Ready task (moves to In progress, launches on the demo harness)
    st.beat("start task");
    st.key(key!(KeyCode::Right));
    st.hold(0.6)?;
    st.key(key!(KeyCode::Char('m')));
    st.emit()?;
    st.hold(3.0)?;
    st.toast_off();

    // a permission request arrives from the running task
    st.beat("permission ask");
    st.ev(BackendEvent::PermissionRequest {
        harness: HARNESS.into(),
        session_id: "s1".into(),
        request_id: "pr-1".into(),
        title: "Run bash: cargo test".into(),
        action: "bash".into(),
        resource: "cargo test".into(),
        resources: vec!["cargo test".into()],
    });
    st.hold(3.0)?;

    // approve (allow once) from the list modal
    st.key(key!(KeyCode::Enter));
    st.beat("approved");
    st.hold(3.0)?;
    st.toast_off();

    // ===== 9. back to chat for the DEFRAG showpiece
    st.beat("back to chat");
    st.key(key!(KeyCode::Char('b'), ctrl));
    st.hold(1.2)?;

    st.ev(BackendEvent::Usage {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        used: 171_000,
        size: 200_000,
        cost_usd: Some(0.19),
        estimated: false,
    });
    st.emit()?;
    st.hold(1.0)?;

    st.beat("DEFRAG animation");
    st.ev(BackendEvent::Compaction {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        compaction_id: "cd-0001".into(),
        status: CompactionStatus::InProgress,
        method: CompactionMethod::Summarize,
        summary_delta: None,
        error: None,
        source: CompactionSource::Reported,
    });
    let chunks = [
        "Summarizing the old context... ",
        "pruning stale tool outputs... ",
        "rewriting the middle blocks... ",
        "recap ready, defragmentation complete.",
    ];
    let total = (8.0 * FPS as f64) as usize;
    let mut ci = 0;
    for i in 0..total {
        if i % (total / chunks.len()) == 0 && ci < chunks.len() {
            st.ev(BackendEvent::Compaction {
                harness: HARNESS.into(),
                native_id: "s1".into(),
                compaction_id: "cd-0001".into(),
                status: CompactionStatus::InProgress,
                method: CompactionMethod::Summarize,
                summary_delta: Some(chunks[ci].into()),
                error: None,
                source: CompactionSource::Reported,
            });
            ci += 1;
        }
        st.step_defrag();
        st.emit()?;
    }
    st.ev(BackendEvent::Compaction {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        compaction_id: "cd-0001".into(),
        status: CompactionStatus::Completed,
        method: CompactionMethod::Summarize,
        summary_delta: None,
        error: None,
        source: CompactionSource::Reported,
    });
    st.ev(BackendEvent::Usage {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        used: 42_000,
        size: 200_000,
        cost_usd: Some(0.21),
        estimated: false,
    });
    st.beat("compaction done");
    st.hold(3.5)?;

    // ===== 10. F1 help, then hold the final frame
    st.beat("help");
    st.key(key!(KeyCode::F(1)));
    st.hold(3.0)?;
    st.key(key!(KeyCode::Esc));
    st.beat("final");
    st.hold(3.0)?;
    Ok(())
}

fn part_updated_tool() -> BackendEvent {
    BackendEvent::PartUpdated {
        harness: HARNESS.into(),
        native_id: "s1".into(),
        msg_id: "m3".into(),
        part: RenderPart {
            part_id: "p4".into(),
            kind: PartKind::Tool,
            text: "read docs/compaction.md".into(),
        },
    }
}
