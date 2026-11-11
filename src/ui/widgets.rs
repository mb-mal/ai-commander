//! Main screen rendering: header, sidebar (history + profile), chat, composer,
//! NC-style F-key status bar.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use super::theme::{THEME, wrap_spans, wrap_str};
use super::{App, Focus};
use crate::backend::{AttachmentMeta, PartKind, Role};
use crate::i18n::{Str, t};

pub const SIDEBAR_WIDTH: u16 = 32;

pub fn draw(app: &mut App, f: &mut Frame) {
    let th = &THEME;
    let size = f.area();
    f.render_widget(Block::new().style(Style::new().bg(th.bg).fg(th.fg)), size);
    app.hit.targets.clear();
    app.hit.modal_rect = None;

    // two bottom lines (v6 feedback): status info above the F-keys menu —
    // the menu must ALWAYS be visible (a layout regression hid it)
    let rows = if app.show_activity {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(6), // activity feed
            Constraint::Length(1), // status info
            Constraint::Length(1), // f-keys menu
        ])
        .split(size)
    } else {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1), // status info
            Constraint::Length(1), // f-keys menu
        ])
        .split(size)
    };
    let n = rows.len();
    let (header, body, info, status) = (rows[0], rows[1], rows[n - 2], rows[n - 1]);
    let feed = if app.show_activity {
        rows[2]
    } else {
        rows[n - 3]
    };

    draw_header(app, f, header);

    let cols = if app.show_sidebar {
        Layout::horizontal([Constraint::Length(app.sidebar_width), Constraint::Min(20)]).split(body)
    } else {
        Layout::horizontal([Constraint::Length(0), Constraint::Min(20)]).split(body)
    };
    if app.show_sidebar {
        draw_sidebar(app, f, cols[0]);
        // UX: the drag handle between sidebar and chat — a visible ┊ line
        // at the boundary. Layout puts cols adjacent (no gap), so the
        // handle sits ON the chat's left border column.
        let handle_x = cols[0].right().saturating_sub(1).max(cols[0].x);
        let handle = Rect {
            x: handle_x,
            y: cols[0].y,
            width: 1,
            height: cols[0].height,
        };
        // visual: a subtle ┊ in the LAST sidebar column (its right border)
        for y in handle.y..handle.y + handle.height {
            let c = &mut f.buffer_mut()[(handle.x, y)];
            c.set_symbol("┊")
                .set_style(Style::new().fg(th.border_dim).bg(th.bg));
        }
        // hit target is 2 columns wide for easier grabbing
        let grab = Rect {
            x: handle_x.saturating_sub(1),
            y: handle.y,
            width: 3,
            height: handle.height,
        };
        app.hit.targets.push((
            grab,
            super::HitTarget::SidebarResize {
                start_width: app.sidebar_width,
                start_x: handle_x,
            },
        ));
    }

    let chat_rows = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(composer_height(app, cols[1])),
    ])
    .split(cols[1]);
    draw_chat(app, f, chat_rows[0]);
    draw_composer(app, f, chat_rows[1]);

    draw_status_info(app, f, info);
    draw_statusbar(app, f, status);
    if app.show_activity {
        draw_activity_feed(app, f, feed);
    }

    if app.modal.is_some() {
        super::modals::draw_modal(app, f, size);
    }

    // К10: the DEFRAG compaction overlay over everything; Esc collapses
    // it into the mini-map (the animation keeps running in the status bar)
    if app.modal.is_none() {
        if let Some((_, _, _, an)) = app.defrag.as_ref() {
            if app.compaction_view == crate::ui::defrag::CompactionView::Defrag {
                crate::ui::defrag::draw_overlay(
                    f,
                    &an.frame,
                    crate::ui::defrag::ascii_mode(),
                    &crate::ui::defrag::DefragPalette::cga(crate::ui::defrag::mono_mode()),
                );
            }
        }
    }
}

fn composer_height(app: &App, area: Rect) -> u16 {
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let mut lines = 0usize;
    for l in app.editor.lines() {
        lines += wrap_str(l, inner_w).len();
    }
    // two visible input lines minimum (NC-style composer)
    lines = lines.clamp(2, 8);
    let chips = if app.attachments.is_empty() { 0 } else { 1 };
    ((lines + chips + 2) as u16)
        .min(area.height.saturating_sub(4))
        .max(4)
}

fn draw_header(app: &App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let harness = app
        .cfg
        .harness(&app.cur.harness)
        .map(|h| format!("{} [{}]", h.name, h.id))
        .unwrap_or_else(|| app.cur.harness.clone());
    let model = app
        .current_model()
        .map(|m| m.display())
        .unwrap_or_else(|| t(app.lang(), Str::None_).to_string());
    let (state_sym, state_col) = if app.busy_now() {
        ("●", th.accent)
    } else if app.backend_err.contains_key(&app.cur.harness) {
        ("●", th.err)
    } else if app.connecting.contains(&app.cur.harness) {
        ("○", th.dim)
    } else {
        ("●", th.user)
    };
    let version = app
        .versions
        .get(&app.cur.harness)
        .filter(|v| v.as_str() != "?")
        .map(|v| format!(" v{v}"))
        .unwrap_or_default();

    let left = Line::from(vec![
        Span::styled(
            " ▦ ",
            Style::new()
                .fg(th.accent)
                .bg(th.header_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "ai-commander",
            Style::new()
                .fg(th.fg)
                .bg(th.header_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ▎ {harness}"),
            Style::new().fg(th.dim).bg(th.header_bg),
        ),
        Span::styled(
            format!("  ▎ {model}"),
            Style::new().fg(th.accent).bg(th.header_bg),
        ),
        Span::styled("  ", Style::new().bg(th.header_bg)),
        Span::styled(state_sym, Style::new().fg(state_col).bg(th.header_bg)),
        Span::styled(version, Style::new().fg(th.dim).bg(th.header_bg)),
    ]);
    let right = Line::from(vec![Span::styled(
        format!("👤 {} ", app.cfg.profile_name),
        Style::new().fg(th.dim).bg(th.header_bg),
    )]);
    let w_l = left.width() as u16;
    let w_r = right.width() as u16;
    let mut spans = left.spans.clone();
    if app.approvals_pending > 0 {
        spans.push(Span::styled(
            format!(
                "  ⚠ {}:{}",
                t(app.lang(), Str::Approvals),
                app.approvals_pending
            ),
            Style::new()
                .fg(th.err)
                .bg(th.header_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if w_l + w_r <= area.width {
        spans.push(Span::styled(
            " ".repeat((area.width - w_l - w_r) as usize),
            Style::new().bg(th.header_bg),
        ));
        spans.extend(right.spans.iter().cloned());
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(th.header_bg)),
        area,
    );
}

fn fmt_time(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let secs = ms / 1000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let dt_now = chrono_like(now);
    let dt = chrono_like(secs);
    if now - secs < 86400 && dt_now.0 == dt.0 {
        format!("{:02}:{:02}", dt.1, dt.2)
    } else {
        format!("{:02}.{:02}", dt.3, dt.4)
    }
}

/// minimal civil-date conversion (days→y/m/d + h/m), no chrono dependency
fn chrono_like(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    // days since 1970-01-01 → civil (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, h as u32, mi as u32, m as u32, d as u32)
}

fn draw_sidebar(app: &mut App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(th.border_style())
        .title(Span::styled(
            format!(" {} ", t(app.lang(), Str::History)),
            th.bold(th.accent),
        ))
        .style(Style::new().bg(th.bg));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // bottom: profile block (3 lines)
    let split = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
    let (list_area, bottom) = (split[0], split[1]);

    // bottom-left profile+settings — clickable
    let model_short = app.current_model().map(|m| m.model_id).unwrap_or_default();
    let model_short: String = model_short.chars().take(18).collect();
    let profile = vec![
        Line::from(vec![
            Span::styled(" 👤 ", th.dim_style()),
            Span::styled(
                app.cfg.profile_name.clone(),
                Style::new()
                    .fg(th.fg)
                    .bg(th.bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ⚙ ", th.tool_style()),
            Span::styled(
                t(app.lang(), Str::Settings).to_string(),
                Style::new()
                    .fg(th.fg)
                    .bg(th.bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  F2", th.dim_style()),
            Span::styled(format!("  {}", model_short), th.dim_style()),
        ]),
    ];
    f.render_widget(Paragraph::new(profile), bottom);
    app.hit.targets.push((
        Rect {
            x: bottom.x,
            y: bottom.y + 1,
            width: bottom.width,
            height: 1,
        },
        super::HitTarget::SettingsBtn,
    ));

    // session list with per-row hit zones
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<Option<super::HitTarget>> = Vec::new();
    let new_chat_label = format!("＋ {}", t(app.lang(), Str::NewChat));
    let is_draft = app.cur.native_id.is_empty();
    lines.push(Line::from(vec![
        Span::styled(
            new_chat_label,
            if is_draft {
                Style::new()
                    .fg(th.bg)
                    .bg(th.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(th.user).bg(th.bg)
            },
        ),
        Span::styled("  Ctrl+N", th.dim_style()),
    ]));
    hits.push(Some(super::HitTarget::NewChat));

    for h in &app.cfg.harnesses {
        if !h.enabled || !crate::backend::supported_kind(&h.kind) {
            continue;
        }
        let empty: Vec<crate::backend::SessionInfo> = Vec::new();
        let list = app.sessions.get(&h.id).unwrap_or(&empty);
        if list.is_empty() && !app.backends.contains_key(&h.id) {
            continue;
        }
        let active_h = app.active_harness == h.id;
        lines.push(Line::from(vec![
            Span::styled(
                format!("[{}] {}", h.id, h.name),
                if active_h {
                    Style::new()
                        .fg(th.accent)
                        .bg(th.bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    th.dim_style()
                },
            ),
            Span::styled(if active_h { " ▸" } else { "" }, th.accent_style()),
        ]));
        hits.push(None);
        if list.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("   {}", t(app.lang(), Str::NoSessions)),
                th.dim_style(),
            )));
            hits.push(None);
        }
        for s in list {
            let selected = app
                .sidebar_sel
                .as_ref()
                .map(|(sh, sid)| sh == &h.id && sid == &s.native_id)
                .unwrap_or(false);
            let title: String = s
                .title
                .chars()
                .take(list_area.width.saturating_sub(12) as usize)
                .collect();
            let time = fmt_time(s.updated_ms);
            let marker = if app.cur.harness == h.id && app.cur.native_id == s.native_id {
                "▸ "
            } else {
                "  "
            };
            let style = if selected {
                th.sel_style()
            } else {
                Style::new().fg(th.fg).bg(th.bg)
            };
            let time_style = if selected {
                Style::new().fg(th.dim).bg(th.sel_bg)
            } else {
                th.dim_style()
            };
            let pad = (list_area.width as usize)
                .saturating_sub(title.chars().count() + time.chars().count() + 5);
            lines.push(Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(title, style),
                Span::styled(" ".repeat(pad), style),
                Span::styled(time, time_style),
            ]));
            hits.push(Some(super::HitTarget::Session {
                harness: h.id.clone(),
                native_id: s.native_id.clone(),
            }));
        }
    }

    let visible = list_area.height as usize;
    // keep the highlighted row inside the window
    let sel_idx = hits
        .iter()
        .position(|h| matches!(h, Some(super::HitTarget::Session { .. })));
    let scroll = match sel_idx {
        Some(i) if i >= visible.saturating_sub(1) => (i + 1 - visible.max(1)) as u16,
        _ => 0,
    };
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), list_area);
    app.sidebar_scroll = scroll;

    for (i, h) in hits.iter().enumerate() {
        if let Some(target) = h.clone() {
            let row_y = list_area.y as i64 + i as i64 - scroll as i64;
            if row_y >= list_area.y as i64 && row_y < (list_area.y + list_area.height) as i64 {
                app.hit.targets.push((
                    Rect {
                        x: list_area.x,
                        y: row_y as u16,
                        width: list_area.width,
                        height: 1,
                    },
                    target,
                ));
            }
        }
    }
}

fn draw_chat(app: &mut App, f: &mut Frame, area: Rect) {
    app.hit.chat_rect = area;
    let th = &THEME;
    let lang = app.lang();
    let title = if app.cur.native_id.is_empty() {
        t(lang, Str::Draft).to_string()
    } else if app.cur.title.is_empty() {
        "—".to_string()
    } else {
        app.cur.title.clone()
    };
    let state = if app.cur.loading {
        t(lang, Str::Loading).to_string()
    } else if app.busy_now() {
        t(lang, Str::StateBusy).to_string()
    } else {
        String::new()
    };
    let title = if state.is_empty() {
        format!(" {title} ")
    } else {
        format!(" {title} — {state} ")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(th.border_style())
        .title(Span::styled(title, th.bold(th.accent)))
        .style(Style::new().bg(th.bg));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let width = inner.width.saturating_sub(1) as usize;
    let lines = build_chat_lines(app, width);
    let total = lines.len() as u16;
    let visible = inner.height;
    let scroll = if total > visible {
        total
            .saturating_sub(visible)
            .saturating_sub(app.bottom_offset.min(total.saturating_sub(visible)))
    } else {
        0
    };
    app.bottom_offset = app.bottom_offset.min(total.saturating_sub(visible));

    let para = Paragraph::new(lines)
        .scroll((scroll, 0))
        .style(Style::new().bg(th.bg));
    f.render_widget(para, inner);

    if total > visible {
        let mut sb_state =
            ScrollbarState::new(total.saturating_sub(visible) as usize).position(scroll as usize);
        f.render_stateful_widget(
            Scrollbar::default().orientation(ScrollbarOrientation::VerticalRight),
            area.inner(Margin::new(0, 1)),
            &mut sb_state,
        );
    }
}

fn attach_label(atts: &[AttachmentMeta]) -> String {
    let names: Vec<String> = atts.iter().map(|a| a.filename.clone()).collect();
    names.join(", ")
}

/// Search/copy view of the chat: the same lines the chat renders at its
/// CURRENT width (so indexes match what is on screen).
pub fn build_chat_lines_stable(app: &mut App) -> Vec<Line<'static>> {
    let w = app.hit.chat_rect.width.saturating_sub(3).max(20) as usize;
    build_chat_lines(app, w)
}

pub fn build_chat_lines(app: &mut App, width: usize) -> Vec<Line<'static>> {
    let th = &THEME;
    let lang = app.lang();
    let mut out: Vec<Line<'static>> = Vec::new();

    if app.cur.msgs.is_empty() {
        let hint = if app.cur.native_id.is_empty() {
            t(lang, Str::OpenDraft).to_string()
        } else if app.cur.loading {
            t(lang, Str::Loading).to_string()
        } else {
            t(lang, Str::NoHistory).to_string()
        };
        out.push(Line::from(Span::styled(hint, th.dim_style())));
        if app.busy_now() {
            out.push(Line::from(Span::styled(
                t(lang, Str::Generating),
                th.accent_style(),
            )));
        }
        return out;
    }

    for (i, msg) in app.cur.msgs.iter().enumerate() {
        if i > 0 {
            out.push(Line::from(Span::raw("")));
        }
        match msg.role {
            Role::User => {
                let mut header = vec![
                    Span::styled("▐ ", Style::new().fg(th.user).bg(th.bg)),
                    Span::styled(t(lang, Str::You).to_string(), th.bold(th.user)),
                ];
                if !msg.attachments.is_empty() {
                    header.push(Span::styled("  📎 ", th.dim_style()));
                    header.push(Span::styled(
                        attach_label(&msg.attachments),
                        Style::new().fg(th.tool).bg(th.bg),
                    ));
                }
                out.push(Line::from(header));
                for part in &msg.parts {
                    let key = (
                        app.cur.harness.clone(),
                        app.cur.native_id.clone(),
                        msg.msg_id.clone(),
                        part.part_id.clone(),
                    );
                    let entry = app.render_cache.entry(key).or_insert((0, 0, Vec::new()));
                    if entry.0 != width || entry.1 != part.text.len() {
                        *entry = (
                            width,
                            part.text.len(),
                            wrap_str(&part.text, width.saturating_sub(2).max(1))
                                .into_iter()
                                .map(|l| {
                                    Line::from(Span::styled(
                                        format!(" {l}"),
                                        Style::new().fg(th.fg).bg(th.bg),
                                    ))
                                })
                                .collect(),
                        );
                    }
                    out.extend(entry.2.iter().cloned());
                }
            }
            Role::Assistant => {
                out.push(Line::from(vec![
                    Span::styled("▐ ", Style::new().fg(th.accent).bg(th.bg)),
                    Span::styled(t(lang, Str::Assistant).to_string(), th.bold(th.accent)),
                ]));
                let mut any = false;
                for part in &msg.parts {
                    match part.kind {
                        PartKind::Text => {
                            any = true;
                            let key = (
                                app.cur.harness.clone(),
                                app.cur.native_id.clone(),
                                msg.msg_id.clone(),
                                part.part_id.clone(),
                            );
                            let entry = app.render_cache.entry(key).or_insert((0, 0, Vec::new()));
                            if entry.0 != width || entry.1 != part.text.len() {
                                let md = super::markdown::render_markdown(&part.text, th);
                                let mut lines = Vec::new();
                                for line in md {
                                    for wrapped in wrap_spans(
                                        line.spans.clone(),
                                        width.saturating_sub(2).max(1),
                                    ) {
                                        lines.push(Line::from(
                                            wrapped
                                                .spans
                                                .into_iter()
                                                .map(|mut s| {
                                                    s.style = s.style.bg(th.bg);
                                                    Span::styled(format!(" {}", s.content), s.style)
                                                })
                                                .collect::<Vec<_>>(),
                                        ));
                                    }
                                }
                                *entry = (width, part.text.len(), lines);
                            }
                            out.extend(entry.2.iter().cloned());
                        }
                        PartKind::Reasoning => {
                            any = true;
                            for l in wrap_str(&part.text, width.saturating_sub(4).max(1)) {
                                out.push(Line::from(Span::styled(
                                    format!(" · {l}"),
                                    th.reasoning_style(),
                                )));
                            }
                        }
                        PartKind::Tool => {
                            any = true;
                            // UX: tool lines carry their size — the user can
                            // see how much context a tool call eats
                            let tok = (part.text.len() as u64) / 4;
                            let meta = if tok >= 1000 {
                                format!(" [{:.1}k tok]", tok as f64 / 1000.0)
                            } else if tok > 0 {
                                format!(" [{tok} tok]")
                            } else {
                                String::new()
                            };
                            let lines_n = part.text.lines().count().max(1);
                            let meta = if lines_n > 1 {
                                format!("{meta} [{lines_n} ln]")
                            } else {
                                meta
                            };
                            out.push(Line::from(vec![
                                Span::styled(format!(" {}", part.text), th.tool_style()),
                                Span::styled(meta, th.dim_style()),
                            ]));
                        }
                    }
                }
                if !any && app.busy_now() && i == app.cur.msgs.len() - 1 {
                    out.push(Line::from(Span::styled(
                        t(lang, Str::Generating),
                        th.accent_style(),
                    )));
                }
            }
        }
    }
    out
}

fn draw_composer(app: &mut App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let lang = app.lang();
    let focus = app.focus == Focus::Composer && app.modal.is_none();

    let chips = if app.attachments.is_empty() {
        None
    } else {
        let mut spans = vec![Span::styled(
            "📎 ",
            Style::new().fg(th.tool).bg(th.composer_bg),
        )];
        for (i, a) in app.attachments.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(
                    " · ",
                    Style::new().fg(th.dim).bg(th.composer_bg),
                ));
            }
            spans.push(Span::styled(
                format!("{} ({})", a.filename, a.size_label()),
                Style::new().fg(th.tool).bg(th.composer_bg),
            ));
        }
        spans.push(Span::styled(
            "  Ctrl+D ✕",
            Style::new().fg(th.dim).bg(th.composer_bg),
        ));
        Some(Line::from(spans))
    };

    let title = chips.clone();
    let hint = if app.busy_now() {
        format!(
            " {} · {} ",
            t(lang, Str::StopEsc),
            t(lang, Str::NewlineHint)
        )
    } else {
        format!(
            " {} · {} ",
            t(lang, Str::SendEnter),
            t(lang, Str::NewlineHint)
        )
    };

    // composer sits on the deepest blue — visually apart from the chat screen
    let border_col = if focus { th.border } else { th.border_dim };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border_col).bg(th.composer_bg))
        .style(Style::new().bg(th.composer_bg))
        .title_bottom(Span::styled(
            hint,
            Style::new().fg(th.dim).bg(th.composer_bg),
        ));
    if let Some(chips) = title {
        block = block.title_top(chips);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hit
        .targets
        .push((area, super::HitTarget::ComposerFocus));

    let width = inner.width.max(1) as usize;
    // render editor with soft wrap (min two visible lines)
    let mut lines: Vec<Line<'static>> = Vec::new();
    let placeholder = app.editor.is_empty();
    if placeholder {
        lines.push(Line::from(Span::styled(
            format!(" {}", t(lang, Str::TypeMessage)),
            Style::new().fg(th.placeholder).bg(th.composer_bg),
        )));
        lines.push(Line::from(Span::styled(
            " ",
            Style::new().fg(th.placeholder).bg(th.composer_bg),
        )));
    } else {
        for l in app.editor.lines() {
            for w in wrap_str(l, width) {
                lines.push(Line::from(Span::styled(
                    format!(" {w}"),
                    Style::new().fg(th.fg).bg(th.composer_bg),
                )));
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::new().bg(th.composer_bg)),
        inner,
    );

    if focus {
        let (r, c) = app.editor.screen_cursor(width);
        let (r, c) = if placeholder { (0, 0) } else { (r, c) };
        let y = inner.y + r as u16;
        let x = inner.x + c as u16 + 1;
        if y < inner.bottom() {
            f.set_cursor_position((x, y));
        }
    }
}

fn draw_statusbar(app: &mut App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let lang = app.lang();
    let mut spans = vec![];

    let keys: Vec<(&str, &str)> = vec![
        ("F1", t(lang, Str::Help)),
        ("F2", t(lang, Str::Settings)),
        ("^N", t(lang, Str::NewChat)),
        ("^K", t(lang, Str::ModelPicker)),
        ("^P", t(lang, Str::FilePicker)),
        ("^H", t(lang, Str::HarnessPicker)),
        ("^O", t(lang, Str::History)),
        (
            "F10",
            if lang == crate::i18n::Lang::Ru {
                "Выход"
            } else {
                "Quit"
            },
        ),
    ];
    let mut x = area.x;
    for (i, (k, label)) in keys.iter().enumerate() {
        let seg_start = x;
        if i > 0 {
            spans.push(Span::styled(
                "│ ",
                Style::new().fg(th.border_dim).bg(th.header_bg),
            ));
            x += 2;
        }
        // NC bottom bar: yellow key labels on blue
        spans.push(Span::styled(
            format!(" {k} "),
            Style::new()
                .fg(th.user)
                .bg(th.header_bg)
                .add_modifier(Modifier::BOLD),
        ));
        x += 1 + k.len() as u16 + 1;
        spans.push(Span::styled(
            *label,
            Style::new().fg(th.fg).bg(th.header_bg),
        ));
        x += label.chars().count() as u16;
        app.hit.targets.push((
            Rect {
                x: seg_start,
                y: area.y,
                width: x - seg_start,
                height: 1,
            },
            super::HitTarget::StatusKey(i as u8),
        ));
    }
    if let Some((msg, is_err, _)) = &app.toast {
        let style = if *is_err {
            Style::new().fg(th.err).bg(th.header_bg)
        } else {
            Style::new().fg(th.user).bg(th.header_bg)
        };
        spans.push(Span::styled(" ".repeat(3), Style::new().bg(th.header_bg)));
        spans.push(Span::styled(format!("» {msg} "), style));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(th.header_bg)),
        area,
    );
}

/// Ctrl+L collapsible activity feed: the board's event log tail.
fn draw_activity_feed(app: &App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(th.border_dim).bg(th.bg))
        .title(Span::styled(
            format!(" {} ", t(app.lang(), Str::Activity)),
            th.bold(th.accent),
        ))
        .style(Style::new().bg(th.bg));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let board = app.board.lock();
    let events = board.last_events(inner.height as usize + 2);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for ev in events.iter().rev().take(inner.height as usize) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", ev.task_id),
                Style::new().fg(th.accent).bg(th.bg),
            ),
            Span::styled(format!("{:<22}", ev.action), th.dim_style()),
            Span::styled(
                ev.task
                    .as_ref()
                    .map(|t2| format!("{:?}", t2.status))
                    .unwrap_or_default(),
                Style::new().fg(th.fg).bg(th.bg),
            ),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(" (no events yet)", th.dim_style())));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// v6 feedback: the second bottom line — active harness, model, context
/// fill relative to the model's window, session cost, local clock right.
fn draw_status_info(app: &App, f: &mut Frame, area: Rect) {
    let th = &THEME;
    let harness_name = app
        .cfg
        .harness(&app.cur.harness)
        .map(|h| h.name.clone())
        .unwrap_or_else(|| app.cur.harness.clone());
    let model = app
        .current_model()
        .map(|m| m.display())
        .unwrap_or_else(|| "-".into());

    // context fill: the reported window wins; opencode reports none (0) —
    // fall back to the ACTIVE model's context limit, else tokens only
    let key = (app.cur.harness.clone(), app.cur.native_id.clone());
    let usage = app.ctx_usage.get(&key).copied();
    // the ACTIVE model's window: the override/default ModelRef has no
    // limit — look it up in the harness's live model list
    let model_limit = app
        .current_model()
        .and_then(|m| {
            app.models
                .get(&app.cur.harness)
                .and_then(|list| {
                    list.iter()
                        .find(|x| x.provider_id == m.provider_id && x.model_id == m.model_id)
                })
                .map(|x| x.context_limit)
        })
        .flatten()
        .or_else(|| {
            app.models
                .get(&app.cur.harness)
                .and_then(|list| list.first())
                .and_then(|m| m.context_limit)
        });
    let ctx = match usage {
        Some((used, size, _)) if size > 0 => format!(
            "ctx {}/{} ({}%)",
            fmt_tokens(used),
            fmt_tokens(size),
            used.min(size) * 100 / size
        ),
        Some((used, _, _)) => match model_limit {
            Some(lim) if lim > 0 => format!(
                "ctx {}/{} ({}%)",
                fmt_tokens(used),
                fmt_tokens(lim),
                used.min(lim) * 100 / lim
            ),
            _ => format!("ctx {}", fmt_tokens(used)),
        },
        None => "ctx -".into(),
    };
    let cost = usage
        .and_then(|(_, _, c)| c)
        .map(|c| format!("${c:.2}"))
        .unwrap_or_else(|| "$-".into());

    let left = format!(" {harness_name} · {model} · {ctx} · {cost} ");
    let right = format!("{} ", chrono::Local::now().format("%H:%M"));

    let free_w = area.width as usize;
    let lw = left.chars().count();
    let rw = right.chars().count();
    let pad = if free_w > lw + rw {
        " ".repeat(free_w - lw - rw)
    } else {
        String::new()
    };
    let line = Line::from(vec![
        Span::styled(left, Style::new().fg(th.fg).bg(th.header_bg)),
        Span::styled(pad, Style::new().fg(th.dim).bg(th.header_bg)),
        Span::styled(right, Style::new().fg(th.accent).bg(th.header_bg)),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::new().bg(th.header_bg)),
        area,
    );
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ---------------- shared dialog component ----------------
// v6: THE one opaque dialog frame for every window in the app (modals,
// the DEFRAG overlay, …) — shadow, Clear (opaque background), double
// border, themed title. Caller-owned content goes into the returned inner.

/// Center a w×h rect inside `size`.
pub(crate) fn centered(size: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(size.width);
    let h = h.min(size.height);
    Rect {
        x: size.x + (size.width.saturating_sub(w)) / 2,
        y: size.y + (size.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Shadowed, OPAQUE, double-bordered dialog frame; returns the inner rect.
pub(crate) fn dialog_frame(f: &mut Frame, area: Rect, title: &str) -> Rect {
    use ratatui::symbols::border;
    use ratatui::widgets::{Block, Clear};
    let th = &THEME;
    // shadow
    if area.right() < f.area().right() || area.bottom() < f.area().bottom() {
        let shadow = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width,
            height: area.height,
        };
        let shadow_clipped = shadow.intersection(f.area());
        f.render_widget(
            Block::new().style(Style::new().bg(th.shadow)),
            shadow_clipped,
        );
    }
    // OPAQUE background — this is what makes it a real window
    f.render_widget(Clear, area);
    let block = Block::default()
        .border_set(border::DOUBLE)
        .border_style(th.dlg_border_style())
        .title(Span::styled(
            format!(" {title} "),
            th.dlg_bold(th.dlg_accent),
        ))
        .style(Style::new().bg(th.dlg_bg));
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

#[cfg(test)]
mod status_info_tests {
    use super::fmt_tokens;

    #[test]
    fn token_formatting() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(174_200), "174.2k");
        assert_eq!(fmt_tokens(1_250_000), "1.2M");
    }
}
