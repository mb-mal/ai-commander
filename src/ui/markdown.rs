//! Markdown → ratatui lines for assistant messages (streaming-friendly:
//! re-parses the full text each frame, which is fine for chat sizes).

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

#[derive(Default)]
struct Ctx {
    lines: Vec<Line<'static>>,
    /// spans of the current paragraph
    buf: Vec<Span<'static>>,
    /// (style, bold, italic) inline state
    bold: u32,
    italic: u32,
    code: u32,
    /// code block accumulator
    in_code: bool,
    code_lang: String,
    code_lines: Vec<String>,
    code_cur: String,
    quote: bool,
    list_ordered: Option<u64>,
    list_depth_prefix: String,
    heading_style: Option<Style>,
}

impl Ctx {
    fn style(&self, base: Style) -> Style {
        let mut s = base;
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.code > 0 {
            s = s.fg(ratatui::style::Color::Rgb(255, 176, 98));
        }
        s
    }

    fn push_text(&mut self, t: &str, base: Style) {
        if self.in_code {
            self.code_cur.push_str(t);
            return;
        }
        // handle inline line breaks
        let parts: Vec<&str> = t.split('\n').collect();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                self.flush_para();
            }
            if part.is_empty() {
                continue;
            }
            let prefix = self.current_prefix();
            let text = if self.quote {
                format!("│ {part}")
            } else {
                format!("{prefix}{part}")
            };
            self.buf.push(Span::styled(text, self.style(base)));
            self.list_ordered = None;
        }
    }

    fn current_prefix(&self) -> String {
        self.list_depth_prefix.clone()
    }

    fn flush_para(&mut self) {
        if !self.buf.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.buf)));
        }
    }

    fn blank(&mut self) {
        self.flush_para();
        if let Some(last) = self.lines.last() {
            if !last.spans.is_empty() {
                self.lines.push(Line::from(Span::raw("")));
            }
        }
    }
}

/// Render markdown text into styled lines (each ≤ width via caller-side wrap).
pub fn render_markdown(md: &str, th: &Theme) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(md, opts);
    let mut ctx = Ctx::default();
    let base = Style::new().fg(th.assistant).bg(th.bg);

    let events: Vec<Event> = parser.into_iter().collect();

    for ev in events {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    ctx.flush_para();
                    let h = match level {
                        pulldown_cmark::HeadingLevel::H1 => {
                            Style::new().fg(th.accent).add_modifier(Modifier::BOLD)
                        }
                        _ => Style::new().fg(th.accent),
                    };
                    ctx.heading_style = Some(h);
                }
                Tag::BlockQuote(_) => {
                    ctx.quote = true;
                }
                Tag::CodeBlock(kind) => {
                    ctx.flush_para();
                    ctx.in_code = true;
                    ctx.code_lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    ctx.code_lines.clear();
                    ctx.code_cur.clear();
                }
                Tag::Emphasis => ctx.italic += 1,
                Tag::Strong => ctx.bold += 1,
                Tag::List(start) => {
                    ctx.list_ordered = start;
                }
                Tag::Item => {
                    ctx.flush_para();
                    match ctx.list_ordered {
                        Some(n) => {
                            ctx.list_depth_prefix = format!("{n}. ");
                            ctx.list_ordered = Some(n + 1);
                        }
                        None => ctx.list_depth_prefix = "• ".into(),
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    ctx.flush_para();
                    ctx.list_depth_prefix.clear();
                }
                TagEnd::Heading(_) => {
                    ctx.flush_para();
                    ctx.heading_style = None;
                    ctx.lines.push(Line::from(Span::raw("")));
                }
                TagEnd::BlockQuote(_) => {
                    ctx.flush_para();
                    ctx.quote = false;
                }
                TagEnd::CodeBlock => {
                    ctx.in_code = false;
                    let lang = std::mem::take(&mut ctx.code_lang);
                    if !lang.is_empty() {
                        ctx.lines.push(Line::from(Span::styled(
                            format!("╭─ {lang}"),
                            Style::new().fg(th.dim).bg(th.bg),
                        )));
                    } else {
                        ctx.lines.push(Line::from(Span::styled(
                            "╭─".to_string(),
                            Style::new().fg(th.dim).bg(th.bg),
                        )));
                    }
                    let code_style = Style::new()
                        .fg(ratatui::style::Color::Rgb(200, 212, 224))
                        .bg(th.code_bg);
                    for l in &ctx.code_lines {
                        ctx.lines
                            .push(Line::from(Span::styled(format!("│ {l}"), code_style)));
                    }
                    ctx.lines.push(Line::from(Span::styled(
                        "╰─".to_string(),
                        Style::new().fg(th.dim).bg(th.bg),
                    )));
                    ctx.code_lines.clear();
                    ctx.lines.push(Line::from(Span::raw("")));
                }
                TagEnd::Emphasis => ctx.italic -= 1,
                TagEnd::Strong => ctx.bold -= 1,
                TagEnd::List(_) => {
                    ctx.list_ordered = None;
                    ctx.list_depth_prefix.clear();
                }
                TagEnd::Item => {}
                _ => {}
            },
            Event::Text(t) => {
                if ctx.in_code {
                    for c in t.chars() {
                        if c == '\n' {
                            ctx.code_lines.push(std::mem::take(&mut ctx.code_cur));
                        } else {
                            ctx.code_cur.push(c);
                        }
                    }
                } else {
                    let st = ctx.heading_style.unwrap_or(base);
                    ctx.push_text(&t, st);
                }
            }
            Event::Code(t) => {
                let s = ctx.style(base);
                ctx.buf.push(Span::styled(format!("`{t}`"), s));
            }
            Event::SoftBreak | Event::HardBreak => {
                if ctx.in_code {
                    ctx.code_lines.push(std::mem::take(&mut ctx.code_cur));
                } else {
                    ctx.flush_para();
                }
            }
            Event::Rule => {
                ctx.blank();
                ctx.lines.push(Line::from(Span::styled(
                    "─".repeat(20),
                    Style::new().fg(th.border_dim).bg(th.bg),
                )));
            }
            Event::FootnoteReference(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
            Event::TaskListMarker(_) => {}
        }
    }
    ctx.flush_para();
    if ctx.in_code {
        // unterminated code block while streaming
        ctx.code_lines.push(std::mem::take(&mut ctx.code_cur));
        let code_style = Style::new()
            .fg(ratatui::style::Color::Rgb(200, 212, 224))
            .bg(th.code_bg);
        for l in &ctx.code_lines {
            ctx.lines
                .push(Line::from(Span::styled(format!("│ {l}"), code_style)));
        }
    }
    if ctx.lines.is_empty() {
        ctx.lines.push(Line::from(Span::raw("")));
    }
    ctx.lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::THEME;

    #[test]
    fn renders_code_block() {
        let md = "before\n```rust\nfn main() {}\n```\nafter";
        let lines = render_markdown(md, &THEME);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("╭─ rust")));
        assert!(text.iter().any(|l| l.contains("fn main() {}")));
    }

    #[test]
    fn streaming_unterminated_code() {
        let md = "text\n```py\nprint(1)";
        let lines = render_markdown(md, &THEME);
        assert!(lines.len() >= 3);
    }
}
