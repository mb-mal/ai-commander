//! Norton Commander palette on the classic blue base: cyan frames, yellow
//! highlights and selection markers, near-white text — the 1990 canon.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // palette kept complete for future themes
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub header_bg: Color,
    pub composer_bg: Color,
    pub border: Color,
    pub border_dim: Color,
    pub fg: Color,
    pub dim: Color,
    pub placeholder: Color,
    pub accent: Color, // NC cyan
    pub user: Color,   // NC yellow
    pub assistant: Color,
    pub reasoning: Color,
    pub tool: Color, // NC yellow (softer)
    pub err: Color,
    pub mark: Color,   // selection yellow bg
    pub sel_bg: Color, // NC cursor bar: cyan bg
    pub sel_fg: Color, // black text on the cyan bar
    pub code_bg: Color,
    // --- dialog (menu) palette: classic NC gray dialogs over the blue screen ---
    pub dlg_bg: Color,     // silver
    pub dlg_fg: Color,     // black
    pub dlg_dim: Color,    // dark gray
    pub dlg_accent: Color, // dark blue
    pub dlg_err: Color,    // dark red
    pub dlg_tool: Color,   // brown
    pub shadow: Color,     // near-black dialog shadow
}

pub const THEME: Theme = Theme {
    bg: Color::Rgb(0x00, 0x00, 0x8A), // NC blue
    panel: Color::Rgb(0x00, 0x00, 0x8A),
    header_bg: Color::Rgb(0x00, 0x00, 0x6E),   // deeper blue
    composer_bg: Color::Rgb(0x00, 0x00, 0x5A), // deepest blue — input zone
    border: Color::Rgb(0x4F, 0xBF, 0xBF),      // NC cyan (slightly softened)
    border_dim: Color::Rgb(0x2E, 0x74, 0x8F),
    fg: Color::Rgb(0xE8, 0xEC, 0xEC), // near-white
    dim: Color::Rgb(0x8E, 0x9C, 0xB8),
    placeholder: Color::Rgb(0xC4, 0xCB, 0xD8), // clearly lighter than the hints
    accent: Color::Rgb(0x55, 0xFF, 0xFF),      // NC bright cyan
    user: Color::Rgb(0xFF, 0xE1, 0x4D),        // NC yellow
    assistant: Color::Rgb(0xEC, 0xF2, 0xF2),
    reasoning: Color::Rgb(0x93, 0x9B, 0xC8),
    tool: Color::Rgb(0xF5, 0xD4, 0x4A),
    err: Color::Rgb(0xFF, 0x6B, 0x6B),
    mark: Color::Rgb(0xAA, 0xAA, 0x00),   // NC marked-file yellow
    sel_bg: Color::Rgb(0x00, 0xB3, 0xB3), // NC cursor: cyan bar
    sel_fg: Color::Rgb(0x00, 0x00, 0x00), // …with black text
    code_bg: Color::Rgb(0x00, 0x00, 0x64),
    dlg_bg: Color::Rgb(0xAA, 0xAA, 0xAA), // NC dialog silver
    dlg_fg: Color::Rgb(0x14, 0x14, 0x14), // black-ish text on silver
    dlg_dim: Color::Rgb(0x5A, 0x5A, 0x5A),
    dlg_accent: Color::Rgb(0x00, 0x00, 0xAA), // NC blue on silver
    dlg_err: Color::Rgb(0xAA, 0x00, 0x00),
    dlg_tool: Color::Rgb(0x7A, 0x5C, 0x00),
    shadow: Color::Rgb(0x10, 0x10, 0x10),
};

#[allow(dead_code)]
impl Theme {
    pub fn normal(&self) -> Style {
        Style::new().fg(self.fg).bg(self.bg)
    }
    pub fn dim_style(&self) -> Style {
        Style::new().fg(self.dim).bg(self.bg)
    }
    pub fn placeholder_style(&self) -> Style {
        Style::new().fg(self.placeholder).bg(self.bg)
    }
    pub fn accent_style(&self) -> Style {
        Style::new().fg(self.accent).bg(self.bg)
    }
    pub fn border_style(&self) -> Style {
        Style::new().fg(self.border).bg(self.bg)
    }
    pub fn err_style(&self) -> Style {
        Style::new().fg(self.err).bg(self.bg)
    }
    pub fn tool_style(&self) -> Style {
        Style::new().fg(self.tool).bg(self.bg)
    }
    pub fn user_style(&self) -> Style {
        Style::new().fg(self.user).bg(self.bg)
    }
    pub fn reasoning_style(&self) -> Style {
        Style::new()
            .fg(self.reasoning)
            .bg(self.bg)
            .add_modifier(Modifier::ITALIC)
    }
    pub fn bold(&self, fg: Color) -> Style {
        Style::new().fg(fg).bg(self.bg).add_modifier(Modifier::BOLD)
    }
    /// Selected row: NC-style black on cyan
    pub fn sel_style(&self) -> Style {
        Style::new().fg(self.sel_fg).bg(self.sel_bg)
    }

    // --- dialog (menu) styles: silver box, black text — distinct from the blue screen ---

    pub fn dlg_normal(&self) -> Style {
        Style::new().fg(self.dlg_fg).bg(self.dlg_bg)
    }
    pub fn dlg_dim_style(&self) -> Style {
        Style::new().fg(self.dlg_dim).bg(self.dlg_bg)
    }
    pub fn dlg_accent_style(&self) -> Style {
        Style::new().fg(self.dlg_accent).bg(self.dlg_bg)
    }
    pub fn dlg_bold(&self, fg: Color) -> Style {
        Style::new()
            .fg(fg)
            .bg(self.dlg_bg)
            .add_modifier(Modifier::BOLD)
    }
    pub fn dlg_border_style(&self) -> Style {
        Style::new().fg(self.dlg_accent).bg(self.dlg_bg)
    }
}

/// Word-aware wrap of a plain string into lines of `width` display columns.
pub fn wrap_str(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    // tabs measure 0 in unicode-width but render as columns: normalize first
    let normalized = s.replace('\t', " ");
    let s = normalized.as_str();
    let mut out: Vec<String> = Vec::new();
    for raw_line in s.split('\n') {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in raw_line.split(' ') {
            let ww = unicode_width::UnicodeWidthStr::width(word);
            if cur_w == 0 {
                // hard-break oversized words
                if ww > width {
                    let mut chunk = String::new();
                    let mut chunk_w = 0;
                    for ch in word.chars() {
                        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                        if chunk_w + cw > width {
                            out.push(std::mem::take(&mut chunk));
                            chunk_w = 0;
                        }
                        chunk.push(ch);
                        chunk_w += cw;
                    }
                    if !chunk.is_empty() {
                        cur = chunk;
                        cur_w = chunk_w;
                    }
                    continue;
                }
                cur = word.to_string();
                cur_w = ww;
            } else if cur_w + 1 + ww <= width {
                cur.push(' ');
                cur.push_str(word);
                cur_w += 1 + ww;
            } else {
                out.push(std::mem::take(&mut cur));
                if ww > width {
                    // hard-break oversized words on a new line too
                    let mut chunk = String::new();
                    let mut chunk_w = 0;
                    for ch in word.chars() {
                        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                        if chunk_w + cw > width {
                            out.push(std::mem::take(&mut chunk));
                            chunk_w = 0;
                        }
                        chunk.push(ch);
                        chunk_w += cw;
                    }
                    cur = chunk;
                    cur_w = chunk_w;
                } else {
                    cur = word.to_string();
                    cur_w = ww;
                }
            }
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Wrap styled spans into lines of `width` columns, preserving styles.
/// Word-aware; long words are hard-broken.
pub fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from(spans)];
    }
    // flatten to (style, char)
    let mut chars: Vec<(ratatui::style::Style, char)> = Vec::new();
    for sp in spans {
        for ch in sp.content.chars() {
            chars.push((sp.style, ch));
        }
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(ratatui::style::Style, char)> = Vec::new();
    let mut cur_w = 0usize;
    let mut pending_space: usize = 0;

    let flush = |lines: &mut Vec<Line<'static>>, cur: &mut Vec<(ratatui::style::Style, char)>| {
        lines.push(spans_line(std::mem::take(cur)));
    };

    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].1 == ' ' {
            pending_space += 1;
            i += 1;
            continue;
        }
        // gather a word
        let start = i;
        let mut word_w = 0usize;
        while i < chars.len() && chars[i].1 != ' ' {
            let cw = unicode_width::UnicodeWidthChar::width(chars[i].1).unwrap_or(0);
            if word_w + cw > width {
                break;
            }
            word_w += cw;
            i += 1;
        }
        let word = &chars[start..i];
        let space_w = if cur_w > 0 {
            pending_space.min(width)
        } else {
            0
        };

        if cur_w + space_w + word_w <= width {
            if cur_w > 0 {
                for _ in 0..pending_space {
                    cur.push((word[0].0, ' '));
                }
                cur_w += space_w;
            }
            cur.extend_from_slice(word);
            cur_w += word_w;
        } else {
            flush(&mut lines, &mut cur);
            cur_w = 0;
            // drop wrapping spaces, then hard-break oversized words
            let mut idx = start;
            while idx < word.len() + start && idx < chars.len() && chars[idx].1 != ' ' {
                let cw = unicode_width::UnicodeWidthChar::width(chars[idx].1).unwrap_or(0);
                if cur_w + cw > width {
                    flush(&mut lines, &mut cur);
                    cur_w = 0;
                }
                cur.push(chars[idx]);
                cur_w += cw;
                idx += 1;
            }
            i = idx;
        }
        pending_space = 0;
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(spans_line(cur));
    }
    lines
}

fn spans_line(chars: Vec<(ratatui::style::Style, char)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_style: Option<ratatui::style::Style> = None;
    let mut buf = String::new();
    for (st, ch) in chars {
        if cur_style != Some(st) {
            if let Some(prev) = cur_style.take() {
                spans.push(Span::styled(std::mem::take(&mut buf), prev));
            }
            cur_style = Some(st);
        }
        buf.push(ch);
    }
    if let Some(prev) = cur_style {
        if !buf.is_empty() {
            spans.push(Span::styled(buf, prev));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_long_lines() {
        let w = wrap_str("hello world this is a long line of text", 10);
        assert!(
            w.iter()
                .all(|l| unicode_width::UnicodeWidthStr::width(l.as_str()) <= 10)
        );
        assert!(w.len() >= 4);
    }

    #[test]
    fn keeps_short_lines() {
        assert_eq!(wrap_str("hi", 10), vec!["hi"]);
    }
}
