//! Modal dialogs: help, settings, model picker, harness picker, NC-style file
//! picker, text input, confirm. Double borders + shadow — old-school.
//! List rows register hit zones so mouse clicks/wheel work inside modals.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::theme::{THEME as TH, wrap_str};
use super::{App, FilePicker, HitTarget, Modal};
use crate::i18n::{Lang, Str, t};

pub fn draw_modal(app: &mut App, f: &mut Frame, size: Rect) {
    match app.modal.clone() {
        Some(Modal::Help) => draw_help(app, f, size),
        Some(Modal::AgentsPanel) => draw_agents_panel(app, f, size),
        Some(Modal::About) => draw_about(app, f, size),
        Some(Modal::Settings { sel }) => draw_settings(app, f, size, sel),
        Some(Modal::ModelPicker { filter, sel, .. }) => {
            draw_model_picker(app, f, size, &filter.text(), sel)
        }
        Some(Modal::HarnessPicker { sel }) => draw_harness_picker(app, f, size, sel),
        Some(Modal::FilePicker(p)) => draw_file_picker(app, f, size, &p),
        Some(Modal::Input { title, editor, .. }) => {
            draw_input(app, f, size, &title, &editor.text())
        }
        Some(Modal::Confirm { title, message, .. }) => draw_confirm(app, f, size, &title, &message),
        Some(Modal::List {
            title, items, sel, ..
        }) => draw_list(app, f, size, &title, &items, sel),
        None => {}
    }
}

fn centered(size: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(size.width);
    let h = h.min(size.height);
    Rect {
        x: size.x + (size.width.saturating_sub(w)) / 2,
        y: size.y + (size.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Draw a shadowed area then the double-bordered dialog frame.
/// Registers the modal rect as the wheel zone.
fn dialog_frame(app: &mut App, f: &mut Frame, area: Rect, title: &str) -> Rect {
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
            Block::new().style(Style::new().bg(TH.shadow)),
            shadow_clipped,
        );
    }
    f.render_widget(Clear, area);
    let block = Block::default()
        .border_set(border::DOUBLE)
        .border_style(TH.dlg_border_style())
        .title(Span::styled(
            format!(" {title} "),
            TH.dlg_bold(TH.dlg_accent),
        ))
        .style(Style::new().bg(TH.dlg_bg));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.hit.modal_rect = Some(area);
    inner
}

fn row_rect(inner: Rect, i: usize) -> Rect {
    Rect {
        x: inner.x,
        y: inner.y + i as u16,
        width: inner.width,
        height: 1,
    }
}

/// Agents panel: roles with harness/model, harness statuses.
fn draw_agents_panel(app: &mut App, f: &mut Frame, size: Rect) {
    let lang = app.lang();
    let inner = dialog_frame(
        app,
        f,
        centered(size, 64, 18),
        if lang == Lang::Ru {
            "Агенты и команды"
        } else {
            "Agents"
        },
    );
    let roles = crate::kanban::roles::RoleSet::cached(&std::env::current_dir().unwrap_or_default());
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(Span::styled(
        if lang == Lang::Ru {
            " Роли (.ai-commander/agents)"
        } else {
            " Roles (.ai-commander/agents)"
        },
        TH.dlg_bold(TH.dlg_accent),
    )));
    if roles.roles.is_empty() {
        lines.push(Line::from(Span::styled(
            if lang == Lang::Ru {
                "   нет — T создаст из шаблона"
            } else {
                "   none — T creates from a template"
            },
            TH.dlg_dim_style(),
        )));
    }
    for (id, r) in roles.roles.iter().take(8) {
        let busy = app
            .board
            .lock()
            .projection()
            .tasks
            .iter()
            .filter(|t| {
                t.role.as_deref() == Some(id) && t.status == crate::kanban::TaskStatus::InProgress
            })
            .count();
        let status = if busy > 0 {
            format!("▸ {busy} running")
        } else {
            "idle".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {id:<12}"), TH.dlg_bold(TH.dlg_fg)),
            Span::styled(format!("{:<18}", r.name), TH.dlg_normal()),
            Span::styled(format!("{:<14}", r.harness), TH.dlg_dim_style()),
            Span::styled(status, TH.dlg_accent_style()),
        ]));
    }
    // v5 #6: files that failed to load are VISIBLE here, not silently gone
    for e in roles.errors.iter().take(4) {
        let file = e
            .path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let one_line: String = e
            .message
            .lines()
            .next()
            .unwrap_or("load error")
            .chars()
            .take(58)
            .collect();
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {file}: {one_line}"),
            TH.err_style(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if lang == Lang::Ru {
            " Харнессы"
        } else {
            " Harnesses"
        },
        TH.dlg_bold(TH.dlg_accent),
    )));
    for h in app
        .cfg
        .harnesses
        .iter()
        .filter(|h| crate::backend::supported_kind(&h.kind))
    {
        let state = if !h.enabled {
            "off"
        } else if app.backends.contains_key(&h.id) {
            if app.busy.iter().any(|(hid, _)| hid == &h.id) {
                "busy"
            } else {
                "ready"
            }
        } else {
            "…"
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:<8}", h.id), TH.dlg_bold(TH.dlg_fg)),
            Span::styled(format!("{:<22}", h.name), TH.dlg_normal()),
            Span::styled(state.to_string(), TH.dlg_accent_style()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            " T: {} · Esc: {}",
            if lang == Lang::Ru {
                "новая роль из шаблона"
            } else {
                "new role from template"
            },
            if lang == Lang::Ru {
                "закрыть"
            } else {
                "close"
            }
        ),
        TH.dlg_dim_style(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_help(app: &mut App, f: &mut Frame, size: Rect) {
    let lang = app.lang();
    let inner = dialog_frame(app, f, centered(size, 72, 22), t(lang, Str::HelpTitle));
    let ru = lang == Lang::Ru;
    let keys: Vec<(&str, &str)> = vec![
        (
            "Enter",
            if ru {
                "отправить сообщение"
            } else {
                "send message"
            },
        ),
        (
            "Alt+Enter / Ctrl+J",
            if ru {
                "перенос строки"
            } else {
                "newline"
            },
        ),
        (
            "Esc",
            if ru {
                "стоп-следование / закрыть окно"
            } else {
                "stop following / close dialog"
            },
        ),
        ("Ctrl+N", if ru { "новый чат" } else { "new chat" }),
        (
            "Ctrl+K",
            if ru {
                "выбор модели (из доступных харнесса)"
            } else {
                "pick model (live list from harness)"
            },
        ),
        (
            "Ctrl+H",
            if ru {
                "переключить харнесс"
            } else {
                "switch harness"
            },
        ),
        (
            "Ctrl+P",
            if ru {
                "прикрепить файл/картинку"
            } else {
                "attach file/image"
            },
        ),
        (
            "Ctrl+O",
            if ru {
                "показать/скрыть панель истории"
            } else {
                "show/hide history panel"
            },
        ),
        (
            "Ctrl+D",
            if ru {
                "убрать последнее вложение"
            } else {
                "drop last attachment"
            },
        ),
        (
            "Tab",
            if ru {
                "переход: история ↔ ввод"
            } else {
                "focus: history ↔ input"
            },
        ),
        (
            "PgUp/PgDn",
            if ru {
                "прокрутка чата (PgDn/End — вниз)"
            } else {
                "scroll chat (PgDn/End — bottom)"
            },
        ),
        (
            "E (в истории)",
            if ru {
                "переименовать сессию"
            } else {
                "rename session"
            },
        ),
        (
            "D / Del (в истории)",
            if ru {
                "удалить сессию"
            } else {
                "delete session"
            },
        ),
        (
            "F1",
            if ru {
                "эта справка"
            } else {
                "this help"
            },
        ),
        (
            "F2",
            if ru {
                "настройки (язык, профиль, харнессы)"
            } else {
                "settings (language, profile, harnesses)"
            },
        ),
        ("F10 / Ctrl+Q", if ru { "выход" } else { "quit" }),
        ("", ""),
        (
            "Мышь / mouse",
            if ru {
                "клики по меню, сессиям, кнопкам; колесо — прокрутка"
            } else {
                "click menus, sessions, buttons; wheel scrolls"
            },
        ),
        (
            "Ins / Enter (в пикере)",
            if ru {
                "отметить / выбрать файлы"
            } else {
                "mark / pick files"
            },
        ),
        (
            "Tab (в пикере)",
            if ru {
                "список дисков ↔ каталог"
            } else {
                "drive list ↔ directory"
            },
        ),
        (
            "A",
            if ru {
                "об авторе (About)"
            } else {
                "about the author (About)"
            },
        ),
    ];
    let mut lines = Vec::new();
    for (k, d) in keys {
        if k.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{k:<22}"),
                Style::new()
                    .fg(TH.dlg_accent)
                    .bg(TH.dlg_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(d.to_string(), TH.dlg_normal()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Classic 90s About screen: name, version, author, copyright, "press any key".
fn draw_about(app: &mut App, f: &mut Frame, size: Rect) {
    let lang = app.lang();
    let title = if lang == Lang::Ru {
        "О программе"
    } else {
        "About"
    };
    let inner = dialog_frame(app, f, centered(size, 56, 16), title);
    let w = inner.width as usize;

    let center = |s: &str| -> String {
        let pad = (w.saturating_sub(unicode_width::UnicodeWidthStr::width(s))) / 2;
        format!("{}{}", " ".repeat(pad), s)
    };
    let sep = "─".repeat(w.saturating_sub(2));

    let version = env!("CARGO_PKG_VERSION");
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            center("════════════════════════"),
            TH.dlg_accent_style(),
        )),
        Line::from(Span::styled(
            center("ai-commander"),
            TH.dlg_bold(TH.dlg_accent),
        )),
        Line::from(Span::styled(
            center("════════════════════════"),
            TH.dlg_accent_style(),
        )),
        Line::from(Span::styled(
            center(&format!("version {version}")),
            TH.dlg_dim_style(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            center("ChatGPT-style TUI for AI agent harnesses"),
            TH.dlg_normal(),
        )),
        Line::from(Span::styled(
            center("with Norton-Commander DNA"),
            TH.dlg_normal(),
        )),
        Line::from(""),
        Line::from(Span::styled(sep.clone(), TH.dlg_dim_style())),
        Line::from(vec![
            Span::styled("  Developer:  ", TH.dlg_dim_style()),
            Span::styled("Mikhail Malyshev", TH.dlg_bold(TH.dlg_fg)),
        ]),
        Line::from(vec![
            Span::styled("  Telegram:   ", TH.dlg_dim_style()),
            Span::styled("@tsingular", TH.dlg_bold(TH.dlg_fg)),
        ]),
        Line::from(vec![
            Span::styled("  YouTube:    ", TH.dlg_dim_style()),
            Span::styled("@tsingular", TH.dlg_bold(TH.dlg_fg)),
        ]),
        Line::from(Span::styled(sep, TH.dlg_dim_style())),
        Line::from(""),
        Line::from(Span::styled(
            center("© 2026 Mikhail Malyshev"),
            TH.dlg_dim_style(),
        )),
        Line::from(Span::styled(
            center(if lang == Lang::Ru {
                "— нажмите любую клавишу —"
            } else {
                "— press any key —"
            }),
            TH.dlg_dim_style(),
        )),
    ];
    // drop lines that overflow the box height
    lines.truncate(inner.height as usize);
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_settings(app: &mut App, f: &mut Frame, size: Rect, sel: usize) {
    let lang = app.lang();
    let inner = dialog_frame(app, f, centered(size, 62, 14), t(lang, Str::SettingsTitle));
    let rows = app.settings_rows();
    let mut lines = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let selected = i == sel;
        let (fg, bg) = if selected {
            (TH.sel_fg, TH.sel_bg)
        } else {
            (TH.dlg_fg, TH.dlg_bg)
        };
        let mut spans = vec![Span::styled(
            format!(" {} ", if selected { "▸" } else { " " }),
            Style::new().fg(TH.accent).bg(bg),
        )];
        spans.push(Span::styled(
            format!("{:<28}", row.label),
            Style::new().fg(TH.dlg_dim).bg(bg),
        ));
        spans.push(Span::styled(
            format!(" {}", row.value),
            Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(spans));
        app.hit
            .targets
            .push((row_rect(inner, i), HitTarget::ModalSettingsRow(i)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            " Enter: {} · Esc: {}",
            t(lang, Str::Save),
            t(lang, Str::Close)
        ),
        TH.dlg_dim_style(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_model_picker(app: &mut App, f: &mut Frame, size: Rect, filter: &str, sel: usize) {
    let lang = app.lang();
    let inner = dialog_frame(app, f, centered(size, 58, 16), t(lang, Str::ModelPicker));
    let mut lines = vec![Line::from(vec![
        Span::styled(
            " /",
            Style::new()
                .fg(TH.dlg_accent)
                .bg(TH.dlg_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{filter}▏"), TH.dlg_normal()),
        Span::styled(format!("  {}", t(lang, Str::Search)), TH.dlg_dim_style()),
    ])];
    let items = app.filtered_models();
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", t(lang, Str::ModelPickerEmpty)),
            TH.dlg_dim_style(),
        )));
    }
    for (i, m) in items.iter().enumerate() {
        let selected = i == sel;
        let bg = if selected { TH.sel_bg } else { TH.dlg_bg };
        let fg = if selected { TH.sel_fg } else { TH.dlg_fg };
        let marker = if selected { "▸ " } else { "  " };
        lines.push(Line::from(vec![Span::styled(
            format!("{marker}{}", m.display()),
            Style::new().fg(fg).bg(bg),
        )]));
        app.hit
            .targets
            .push((row_rect(inner, i + 1), HitTarget::ModalModelRow(i)));
    }
    f.render_widget(Paragraph::new(lines), inner);
    let (x, y) = (inner.x + 2 + filter.chars().count() as u16, inner.y);
    f.set_cursor_position((x, y));
}

fn draw_harness_picker(app: &mut App, f: &mut Frame, size: Rect, sel: usize) {
    let lang = app.lang();
    let inner = dialog_frame(app, f, centered(size, 48, 10), t(lang, Str::HarnessPicker));
    let mut lines = Vec::new();
    for (i, h) in app.cfg.harnesses.iter().enumerate() {
        let selected = i == sel;
        let bg = if selected { TH.sel_bg } else { TH.dlg_bg };
        let status = if h.kind != "opencode" {
            t(lang, Str::SettingsDisabledKind).to_string()
        } else if h.enabled {
            t(lang, Str::SettingsEnabled).to_string()
        } else {
            "✗".to_string()
        };
        let marker = if selected { "▸ " } else { "  " };
        let active = app.active_harness == h.id;
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker}[{}] {} ", h.id, h.name),
                if active {
                    Style::new()
                        .fg(TH.dlg_accent)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                        .fg(if selected { TH.sel_fg } else { TH.dlg_fg })
                        .bg(bg)
                },
            ),
            Span::styled(
                format!("· {} · {}", h.kind, status),
                Style::new().fg(TH.dlg_dim).bg(bg),
            ),
        ]));
        app.hit
            .targets
            .push((row_rect(inner, i), HitTarget::ModalHarnessRow(i)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn fmt_size(size: u64) -> String {
    if size >= 1_048_576 {
        format!("{:.1}M", size as f64 / 1_048_576.0)
    } else if size >= 1024 {
        format!("{:.1}k", size as f64 / 1024.0)
    } else {
        format!("{}b", size)
    }
}

fn draw_file_picker(app: &mut App, f: &mut Frame, size: Rect, p: &FilePicker) {
    let lang = app.lang();
    let title = if p.dir_mode {
        t(lang, Str::SettingsWorkdir).to_string()
    } else {
        t(lang, Str::FilePicker).to_string()
    };
    let inner = dialog_frame(app, f, centered(size, 72, 22), &title);

    // path line
    let dir_label = match &p.dir {
        Some(d) => d.to_string_lossy().into_owned(),
        None => format!("[ {} ]", t(lang, Str::Drives)),
    };
    let dir_label: String = dir_label.chars().take(inner.width as usize).collect();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" ┌ ", TH.dlg_accent_style()),
            Span::styled(dir_label, TH.dlg_bold(TH.dlg_accent)),
            Span::styled(" ┐", TH.dlg_accent_style()),
        ]),
        Line::from(""),
    ];

    if let Some(err) = &p.err {
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {err}"),
            Style::new().fg(TH.dlg_err).bg(TH.dlg_bg),
        )));
    }

    let name_w = (inner.width as usize).saturating_sub(14).max(10);
    for (i, e) in p.entries.iter().enumerate() {
        let selected = i == p.sel;
        let marked = p.marked.contains(&e.path);
        let bg = if marked {
            TH.mark
        } else if selected {
            TH.sel_bg
        } else {
            TH.dlg_bg
        };
        let fg = if marked || selected {
            TH.sel_fg
        } else {
            TH.dlg_fg
        };
        let name: String = e.name.chars().take(name_w).collect();
        let right = if e.is_dir {
            "<DIR>".to_string()
        } else {
            fmt_size(e.size)
        };
        let pad = name_w.saturating_sub(name.chars().count());
        lines.push(Line::from(vec![
            Span::styled(
                if marked { " ■ " } else { "   " },
                Style::new().fg(TH.sel_fg).bg(TH.mark),
            ),
            Span::styled(
                format!("{name}{} ", " ".repeat(pad)),
                Style::new()
                    .fg(if marked {
                        TH.sel_fg
                    } else if e.is_dir {
                        TH.dlg_accent
                    } else {
                        fg
                    })
                    .bg(bg),
            ),
            Span::styled(format!("{right:>9}"), Style::new().fg(TH.dlg_dim).bg(bg)),
        ]));
        app.hit
            .targets
            .push((row_rect(inner, i + 2), HitTarget::ModalFileRow(i)));
    }

    lines.push(Line::from(""));
    let hint = if p.dir_mode {
        format!(
            " Enter: {} · F5: {} · Esc: {}",
            t(lang, Str::Close),
            t(lang, Str::PickAttach),
            t(lang, Str::Cancel)
        )
    } else {
        format!(
            " Enter: {} · Ins: mark · Tab: {} · Backspace: .. · Esc: {}",
            t(lang, Str::PickAttach),
            t(lang, Str::Drives),
            t(lang, Str::Cancel)
        )
    };
    lines.push(Line::from(Span::styled(
        format!(" {hint}"),
        TH.dlg_dim_style(),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_input(app: &mut App, f: &mut Frame, size: Rect, title: &str, value: &str) {
    let lang = app.lang();
    let inner = dialog_frame(app, f, centered(size, 54, 8), title);
    let mut lines = vec![Line::from(vec![
        Span::styled(" ", TH.dlg_normal()),
        Span::styled(format!("{value}▏"), TH.dlg_normal()),
    ])];
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            " Enter: {} · Esc: {}",
            t(lang, Str::Save),
            t(lang, Str::Cancel)
        ),
        TH.dlg_dim_style(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
    let (x, y) = (inner.x + 1 + value.chars().count() as u16, inner.y);
    f.set_cursor_position((x, y));
}

fn draw_confirm(app: &mut App, f: &mut Frame, size: Rect, title: &str, message: &str) {
    let lang = app.lang();
    let inner = dialog_frame(app, f, centered(size, 50, 8), title);
    let mut lines = Vec::new();
    for l in wrap_str(message, inner.width.saturating_sub(2).max(10) as usize) {
        lines.push(Line::from(Span::styled(format!(" {l}"), TH.dlg_normal())));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!(" Enter: {} · Esc: {} ", t(lang, Str::Yes), t(lang, Str::No)),
        TH.dlg_dim_style(),
    )]));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_list(
    app: &mut App,
    f: &mut Frame,
    size: Rect,
    title: &str,
    items: &[(String, String)],
    sel: usize,
) {
    let lang = app.lang();
    let inner = dialog_frame(app, f, centered(size, 44, 12), title);
    let mut lines = Vec::new();
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", t(lang, Str::ModelPickerEmpty)),
            TH.dlg_dim_style(),
        )));
    }
    for (i, (id, name)) in items.iter().enumerate() {
        let selected = i == sel;
        let bg = if selected { TH.sel_bg } else { TH.dlg_bg };
        let fg = if selected { TH.sel_fg } else { TH.dlg_fg };
        let marker = if selected { "▸ " } else { "  " };
        let label = if id == name {
            id.clone()
        } else {
            format!("{name} ({id})")
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{label}"),
            Style::new().fg(fg).bg(bg),
        )));
        app.hit
            .targets
            .push((row_rect(inner, i), HitTarget::ModalListRow(i)));
    }
    f.render_widget(Paragraph::new(lines), inner);
}
