//! К9: the DEFRAG animator — a deterministic state machine that turns
//! compaction events + a context map into animation frames, and the
//! ratatui widget that draws a frame. Pure data; `App` drives the clock.

use crate::ui::context_map::{Block, HistoryShape, build_after, build_before};

/// К12: how compaction is shown. `defrag` — full-screen animation,
/// `mini` — status-bar strip only, `off` — history line only.
/// `NO_COLOR` forces the monochrome palette; ASCII mode (old conhost)
/// replaces the map glyphs with plain ASCII.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionView {
    #[default]
    Defrag,
    Mini,
    Off,
}

impl CompactionView {
    /// `AI_COMMANDER_COMPACTION_VIEW=defrag|mini|off`
    pub fn from_env() -> Self {
        match std::env::var("AI_COMMANDER_COMPACTION_VIEW")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "mini" => CompactionView::Mini,
            "off" => CompactionView::Off,
            _ => CompactionView::Defrag,
        }
    }
}

/// The glyph + color palette of one block (CGA-named, as in MS-DOS DEFRAG).
pub fn block_cell(b: Block, ascii: bool) -> (&'static str, &'static str) {
    // (glyph, color name) — color names are resolved by the widget
    match b {
        Block::Pinned if ascii => ("X", "pinned"),
        Block::Pinned => ("X", "pinned"),
        Block::Turn if ascii => ("#", "turn"),
        Block::Turn => ("■", "turn"),
        Block::Pruned if ascii => ("B", "pruned"),
        Block::Pruned => ("B", "pruned"),
        Block::Summary if ascii => ("o", "summary"),
        Block::Summary => ("●", "summary"),
        Block::Recent if ascii => ("=", "recent"),
        Block::Recent => ("▓", "recent"),
        Block::Free if ascii => (".", "free"),
        Block::Free => ("░", "free"),
    }
}

/// One animation frame: the full map plus the status lines.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub blocks: Vec<Block>,
    /// reading cursor (cells), never ahead of the write cursor
    pub read_at: usize,
    /// writing cursor (cells), driven by real summary chunks
    pub write_at: usize,
    pub phase: Phase,
    /// tokens before compaction (real)
    pub used_before: u64,
    /// tokens after (estimate `~` until the real usage arrives)
    pub used_after_estimated: bool,
    pub used_after: u64,
    /// summary tokens written so far / the expected ceiling
    pub summary_tokens: u64,
    pub summary_ceiling: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Analyze,
    Prune,
    Summarize,
    Done,
    Failed,
}

/// Deterministic RNG (xorshift) seeded from the compaction id, so replays
/// and snapshot tests look identical run to run.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// DOS rhythm: mostly regular steps, occasional seek pauses.
    pub fn step_delay_ms(&mut self) -> u64 {
        let r = self.next() % 10;
        match r {
            0..=1 => 200, // "track seek" pause
            2..=3 => 50,  // fast burst
            _ => 80,      // regular
        }
    }
}

pub fn seed_of(compaction_id: &str) -> u64 {
    // FNV-1a over the id
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in compaction_id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The animator state for ONE compaction.
pub struct DefragAnimator {
    pub frame: Frame,
    shape: HistoryShape,
    map_cells: usize,
    rng: Rng,
    /// cells the writer still has to cover after `completed` arrived
    /// (catch-up, ≤1s total)
    pub finished: bool,
}

impl DefragAnimator {
    pub fn new(
        compaction_id: &str,
        used: u64,
        size: u64,
        shape: HistoryShape,
        map_cells: usize,
        prune: bool,
    ) -> Self {
        let blocks = build_before(used, size, &shape, map_cells, prune);
        // estimate: pinned + summary + recent (the reviewer's `~41k`)
        let est = shape.pinned_tokens + 4_096 + shape.recent_tokens;
        DefragAnimator {
            frame: Frame {
                blocks,
                used_before: used,
                used_after: est.min(used),
                used_after_estimated: true,
                summary_ceiling: 4_096,
                phase: if prune {
                    Phase::Prune
                } else {
                    Phase::Summarize
                },
                ..Default::default()
            },
            shape,
            map_cells,
            rng: Rng::new(seed_of(compaction_id)),
            finished: false,
        }
    }

    /// A real summary chunk arrived: the write cursor advances by its
    /// share of the summary region (a quarter of the map), NOT of the
    /// whole window — a summary is ~4k tokens against a 200k window.
    pub fn summary_chunk(&mut self, delta: &str) {
        let tokens = (delta.len() as u64) / 4;
        self.frame.summary_tokens += tokens;
        let writer_cells = (self.map_cells / 4).max(1);
        let ceiling = self.frame.summary_ceiling.max(1);
        let target = ((self.frame.summary_tokens * writer_cells as u64) / ceiling) as usize;
        self.frame.write_at = target.min(writer_cells).min(self.map_cells);
        // the read cursor tracks the writer, never ahead (theater rule)
        self.frame.read_at = (self.frame.write_at * 3 / 2).min(self.map_cells);
        // draw the written cells as Summary (●)
        for i in 0..self.frame.write_at.min(self.map_cells) {
            if self.frame.blocks[i] != Block::Pinned {
                self.frame.blocks[i] = Block::Summary;
            }
        }
    }

    /// Compaction finished (or failed): rebuild from real numbers.
    pub fn complete(&mut self, used_after: Option<u64>, error: Option<String>) {
        if let Some(u) = used_after {
            self.frame.used_after = u;
            self.frame.used_after_estimated = false;
        }
        match error {
            Some(e) => {
                self.frame.phase = Phase::Failed;
                self.frame.error = Some(e);
            }
            None => {
                self.frame.phase = Phase::Done;
                // the real "after" map: pinned → summary → recent → free
                self.frame.blocks = build_after(
                    self.frame.used_after,
                    self.frame.used_before.max(self.frame.used_after),
                    &self.shape,
                    self.frame.summary_tokens.max(64),
                    self.map_cells,
                );
                self.finished = true;
            }
        }
    }

    /// One animation step (called on the UI tick).
    pub fn step(&mut self) {
        if self.finished {
            return;
        }
        let _ = self.rng.step_delay_ms();
        // prune wave: sweep old Pruned candidates into Free
        if self.frame.phase == Phase::Prune {
            if let Some(pos) = self.frame.blocks.iter().position(|b| *b == Block::Pruned) {
                self.frame.blocks[pos] = Block::Free;
            } else {
                self.frame.phase = Phase::Summarize;
            }
            return;
        }
        // summarize: theater — the writer creeps forward between chunks
        if self.frame.phase == Phase::Summarize && self.frame.write_at < self.map_cells {
            self.frame.write_at += 1;
            if self.frame.blocks[self.frame.write_at - 1] != Block::Pinned {
                self.frame.blocks[self.frame.write_at - 1] = Block::Summary;
            }
        }
    }

    /// Progress percent: only with a summary stream, capped at 99 until
    /// completed — `None` means "unknown" (indeterminate bar).
    pub fn percent(&self) -> Option<u8> {
        if self.frame.summary_ceiling == 0 {
            return None;
        }
        let p = (self.frame.summary_tokens * 100 / self.frame.summary_ceiling) as u8;
        if self.frame.phase == Phase::Done {
            Some(100)
        } else {
            Some(p.min(99))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape() -> HistoryShape {
        HistoryShape {
            pinned_tokens: 2_000,
            messages: vec![
                (3_000, false),
                (2_000, true),
                (3_000, false),
                (2_500, false),
            ],
            recent_tokens: 2_500,
        }
    }

    #[test]
    fn deterministic_seed_and_rhythm() {
        let mut a = Rng::new(seed_of("c-1"));
        let mut b = Rng::new(seed_of("c-1"));
        for _ in 0..10 {
            assert_eq!(a.next(), b.next());
        }
        assert_ne!(seed_of("c-1"), seed_of("c-2"));
    }

    #[test]
    fn write_cursor_follows_chunks_and_read_never_ahead() {
        let mut an = DefragAnimator::new("c-1", 150_000, 200_000, shape(), 100, false);
        // realistic streamed chunks (~500 tokens each, as a real summary)
        let chunk = "word ".repeat(100);
        for _ in 0..5 {
            an.summary_chunk(&chunk);
        }
        assert!(an.frame.write_at > 0, "chunks advance the writer");
        assert!(an.frame.read_at >= an.frame.write_at);
        assert!(an.frame.read_at <= 100);
        // percent capped at 99 before completion
        assert!(an.percent().expect("streaming") <= 99);
    }

    #[test]
    fn completion_rebuilds_map_from_real_numbers() {
        let mut an = DefragAnimator::new("c-2", 150_000, 200_000, shape(), 100, false);
        an.summary_chunk("chunk");
        an.complete(Some(41_000), None);
        assert_eq!(an.frame.phase, Phase::Done);
        assert!(!an.frame.used_after_estimated);
        assert_eq!(an.percent(), Some(100));
        assert!(an.frame.blocks.iter().all(|b| *b != Block::Turn));
    }

    #[test]
    fn failure_keeps_error_and_phase() {
        let mut an = DefragAnimator::new("c-3", 150_000, 200_000, shape(), 100, false);
        an.complete(None, Some("model overloaded".into()));
        assert_eq!(an.frame.phase, Phase::Failed);
        assert_eq!(an.frame.error.as_deref(), Some("model overloaded"));
    }

    #[test]
    fn prune_wave_then_summarize() {
        let mut an = DefragAnimator::new("c-4", 150_000, 200_000, shape(), 60, true);
        assert_eq!(an.frame.phase, Phase::Prune);
        for _ in 0..30 {
            an.step();
        }
        assert_ne!(an.frame.phase, Phase::Prune, "prune is finite");
    }
}

// ---------------- widget rendering (К9/К10) ----------------

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

/// CGA-named palette as RGB (terminal themes override ANSI indexes).
pub struct DefragPalette {
    pub bg: Color,
    pub turn: Color,    // white blocks
    pub free: Color,    // blue background shows through
    pub summary: Color, // yellow
    pub pruned: Color,  // red on black
    pub recent: Color,  // cyan
    pub pinned: Color,  // white on blue
    pub chrome: Color,  // light gray menu/frames
    pub accent: Color,  // light red status line
    pub mono: bool,
}

impl DefragPalette {
    pub fn cga(mono: bool) -> Self {
        if mono {
            DefragPalette {
                bg: Color::Black,
                turn: Color::White,
                free: Color::DarkGray,
                summary: Color::White,
                pruned: Color::Gray,
                recent: Color::White,
                pinned: Color::White,
                chrome: Color::White,
                accent: Color::White,
                mono: true,
            }
        } else {
            DefragPalette {
                bg: Color::Rgb(0x00, 0x00, 0xAA),
                turn: Color::White,
                free: Color::Rgb(0x00, 0x00, 0xAA),
                summary: Color::Rgb(0xFF, 0xFF, 0x55),
                pruned: Color::Rgb(0xFF, 0x55, 0x55),
                recent: Color::Rgb(0x00, 0xAA, 0xAA),
                pinned: Color::White,
                chrome: Color::Rgb(0xAA, 0xAA, 0xAA),
                accent: Color::Rgb(0xFF, 0x55, 0x55),
                mono: false,
            }
        }
    }
}

/// Draw the compaction dialog: the app's STANDARD opaque dialog frame
/// (shadow + Clear + double border — same component as every modal), with
/// the DEFRAG content inside: a 3-row map strip, tokens before→after, a
/// progress bar, method and action lines. After completion it waits for Esc.
pub fn draw_overlay(f: &mut ratatui::Frame, data: &Frame, ascii: bool, palette: &DefragPalette) {
    let screen = f.area();
    // dialog size: ~70% width, fixed content height
    let w = (screen.width * 7 / 10).clamp(46, screen.width.saturating_sub(4));
    let h: u16 = 12;
    let h = h.min(screen.height.saturating_sub(2));
    if w < 40 || h < 8 {
        return;
    }
    let area = crate::ui::widgets::centered(screen, w, h);
    let inner = crate::ui::widgets::dialog_frame(f, area, "Context Defrag");
    if inner.width < 20 || inner.height < 5 {
        return;
    }
    let buf = f.buffer_mut();

    // --- map strip: 3 rows, evenly down-sampled so head AND tail show ---
    let map_rows: u16 = 3;
    let cells = (inner.width as usize) * map_rows as usize;
    let total = data.blocks.len().max(1);
    for i in 0..cells.min(total) {
        let b = data.blocks[i * total / cells.min(total)];
        let col = (i as u16) % inner.width;
        let row = (i as u16) / inner.width;
        let (glyph, color) = match block_cell(b, ascii) {
            (g, "turn") => (g, palette.turn),
            (g, "free") => (g, palette.free),
            (g, "summary") => (g, palette.summary),
            (g, "pruned") => (g, palette.pruned),
            (g, "recent") => (g, palette.recent),
            (g, _) => (g, palette.pinned),
        };
        let cell = &mut buf[(inner.x + col, inner.y + row)];
        cell.set_symbol(glyph).set_fg(color);
    }

    // --- tokens line ---
    let after_txt = if data.used_after_estimated {
        format!("~{}", data.used_after)
    } else {
        data.used_after.to_string()
    };
    Paragraph::new(Line::from(format!(
        "Tokens  {} -> {}",
        data.used_before, after_txt
    )))
    .render(
        Rect {
            x: inner.x,
            y: inner.y + map_rows + 1,
            width: inner.width,
            height: 1,
        },
        buf,
    );

    // --- progress bar (percent from the stream; indeterminate without) ---
    let bar_y = inner.y + map_rows + 2;
    let bar_w = inner.width.saturating_sub(2) as usize;
    let filled = if data.phase == Phase::Done {
        bar_w
    } else if data.summary_ceiling == 0 {
        0
    } else {
        let p = (data.summary_tokens as f64 / data.summary_ceiling as f64).clamp(0.0, 0.99);
        (p * bar_w as f64) as usize
    };
    let mut bar = String::from("[");
    for i in 0..bar_w {
        if data.phase == Phase::Done {
            bar.push('=');
        } else if data.summary_ceiling == 0 {
            bar.push(if i % 2 == 0 { '-' } else { ' ' });
        } else if i < filled {
            bar.push('=');
        } else {
            bar.push('.');
        }
    }
    bar.push(']');
    Paragraph::new(Line::from(Span::styled(
        bar,
        Style::default().fg(palette.summary),
    )))
    .render(
        Rect {
            x: inner.x,
            y: bar_y,
            width: (bar_w + 2) as u16,
            height: 1,
        },
        buf,
    );

    // --- method + action lines ---
    let method = match data.phase {
        Phase::Prune => "Method  Unfragment Files Only (prune)",
        _ => "Method  Full Optimization (summarize)",
    };
    Paragraph::new(Line::from(method)).render(
        Rect {
            x: inner.x,
            y: bar_y + 1,
            width: inner.width,
            height: 1,
        },
        buf,
    );

    let (action, color) = match data.phase {
        Phase::Analyze => ("Analyzing context...", palette.chrome),
        Phase::Prune => ("Pruning old tool outputs...", palette.chrome),
        Phase::Summarize => ("Writing summary...", palette.summary),
        Phase::Done => ("Compaction complete. Press Esc to close", palette.summary),
        Phase::Failed => ("Compaction FAILED", palette.pruned),
    };
    Paragraph::new(Line::from(Span::styled(
        format!(" {action}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )))
    .render(
        Rect {
            x: inner.x,
            y: bar_y + 2,
            width: inner.width,
            height: 1,
        },
        buf,
    );
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Draw the overlay over a background of X's, then dump the buffer.
    fn render_to_text(f: &Frame, ascii: bool, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        t.draw(|fr| {
            // noisy background: everything the dialog must OPAQUELY cover
            let bg_text = "@".repeat((w * h) as usize);
            let mut y = 0;
            let mut rest = bg_text.as_str();
            while rest.len() > w as usize {
                let (line, r) = rest.split_at(w as usize);
                Paragraph::new(Line::from(line)).render(
                    ratatui::layout::Rect {
                        x: 0,
                        y,
                        width: w,
                        height: 1,
                    },
                    fr.buffer_mut(),
                );
                rest = r;
                y += 1;
            }
            draw_overlay(fr, f, ascii, &DefragPalette::cga(false));
        })
        .expect("draw");
        let buf = t.backend().buffer();
        let mut rows: Vec<String> = Vec::new();
        for y in 0..h {
            let mut row = String::new();
            for x in 0..w {
                let c = buf[(x, y)].symbol().to_string();
                row.push_str(if c.is_empty() { " " } else { &c });
            }
            rows.push(row);
        }
        rows.join("\n")
    }

    fn frame() -> (DefragAnimator, Frame) {
        let sh = HistoryShape {
            pinned_tokens: 2_000,
            messages: vec![
                (20_000, false),
                (5_000, true),
                (20_000, false),
                (5_000, true),
                (20_000, false),
            ],
            recent_tokens: 8_000,
        };
        let mut an = DefragAnimator::new("c-render", 174_000, 200_000, sh, 80 * 8, true);
        for _ in 0..2000 {
            an.step();
            if an.frame.phase == Phase::Summarize {
                break;
            }
        }
        an.summary_chunk(&"word ".repeat(400));
        let f = an.frame.clone();
        (an, f)
    }

    /// The dialog is OPAQUE: no background X bleeds inside its border, and
    /// the strip is bounded (not a wall of map glyphs).
    #[test]
    fn dialog_is_opaque_and_bounded() {
        let (_, f) = frame();
        let text = render_to_text(&f, false, 80, 25);

        assert!(text.contains("Context Defrag"), "title on the frame");
        // the strip renders MAP GLYPHS (turns/recent/free); the writer's ●
        // needs a big streamed summary — checked separately below
        assert!(
            ['■', '▓', '░'].iter().any(|g| text.contains(*g)),
            "map strip glyphs visible"
        );
        assert!(text.contains("174000 -> ~"), "tokens line");
        assert!(text.contains("Writing summary..."), "action line");
        // opacity: find the border row; no 'X' between the border columns
        let lines: Vec<&str> = text.split('\n').collect();
        let border_row = lines
            .iter()
            .position(|l| l.contains("Context Defrag"))
            .expect("top border (the title row)");
        // opacity: NO background '@' inside the dialog's horizontal span
        // on ANY content row (the row below the title, map rows, and the
        // area between the border columns)
        let title_row = &lines[border_row];
        let left = title_row.find('@').unwrap_or(0) + 1;
        let right = title_row.rfind('@').unwrap_or(title_row.len());
        for row in lines.iter().take(border_row + 10).skip(border_row + 1) {
            if let Some(pos) = row.find('@') {
                if pos > left && pos < right {
                    panic!("the dialog leaks the background at col {pos}: {row:?}");
                }
            }
        }
        let blocks = text.matches('■').count();
        assert!(blocks < 300, "strip bounded ({blocks} blocks)");
    }

    /// A big streamed summary visibly advances the writer (● region).
    #[test]
    fn streamed_summary_shows_in_the_strip() {
        let (_, f) = frame();
        let mut an2 = DefragAnimator {
            frame: f.clone(),
            shape: HistoryShape {
                pinned_tokens: 2_000,
                messages: vec![(20_000, false), (5_000, true), (20_000, false)],
                recent_tokens: 8_000,
            },
            map_cells: 640,
            rng: Rng::new(seed_of("c-2")),
            finished: false,
        };
        // stream ~4000 tokens of summary (the whole ceiling)
        for _ in 0..8 {
            an2.summary_chunk(&"word ".repeat(400));
        }
        let text = render_to_text(&an2.frame, false, 80, 25);
        assert!(
            text.contains('●'),
            "writer cells visible: strip must show ●"
        );
    }

    #[test]
    fn ascii_mode_gifs() {
        let (_, f) = frame();
        let text = render_to_text(&f, true, 120, 40);
        assert!(text.contains('o'), "ASCII summary glyph");
        assert!(text.contains('#'), "ASCII used glyph");
    }

    #[test]
    fn done_frame_shows_complete_and_real_numbers() {
        let (mut an, _) = frame();
        an.complete(Some(41_000), None);
        let text = render_to_text(&an.frame, false, 80, 25);
        assert!(text.contains("41000"), "real after-tokens");
        assert!(text.contains("complete"), "final line");
        assert!(text.contains("Esc"), "waits for Esc");
    }
}

/// К12: ASCII glyphs (`/G0`) — env `AI_COMMANDER_ASCII=1`; monochrome
/// (`/BW`) — `NO_COLOR`.
pub fn ascii_mode() -> bool {
    std::env::var("AI_COMMANDER_ASCII")
        .map(|v| v == "1")
        .unwrap_or(false)
}

pub fn mono_mode() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}
