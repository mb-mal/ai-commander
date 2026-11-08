//! Kanban board screen (Ctrl+B): NC-style status columns with task cards,
//! F-keys, mouse and the same silver-dialog language.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::theme::THEME as TH;
use super::{App, HitTarget};
use crate::i18n::{Lang, Str, t};
use crate::kanban::TaskStatus;

fn status_title(lang: Lang, s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Backlog => {
            if lang == Lang::Ru {
                "Бэклог"
            } else {
                "Backlog"
            }
        }
        TaskStatus::Ready => "Ready",
        TaskStatus::InProgress => {
            if lang == Lang::Ru {
                "В работе"
            } else {
                "In progress"
            }
        }
        TaskStatus::Review => "Review",
        TaskStatus::Done => {
            if lang == Lang::Ru {
                "Готово"
            } else {
                "Done"
            }
        }
        TaskStatus::Blocked => {
            if lang == Lang::Ru {
                "Блок"
            } else {
                "Blocked"
            }
        }
        TaskStatus::NeedsHuman => {
            if lang == Lang::Ru {
                "Человек"
            } else {
                "Needs human"
            }
        }
        TaskStatus::Cancelled => {
            if lang == Lang::Ru {
                "Отменено"
            } else {
                "Cancelled"
            }
        }
    }
}

fn status_color(s: TaskStatus) -> ratatui::style::Color {
    use TaskStatus::*;
    match s {
        Backlog => TH.dim,
        Ready => TH.accent,
        InProgress => TH.user, // NC yellow: active work
        Review => TH.border,
        Done => ratatui::style::Color::Rgb(0x7E, 0xE7, 0x87),
        Blocked => TH.err,
        NeedsHuman => TH.err,
        Cancelled => TH.dim,
    }
}

/// Render the board over the chat area. Cards register hit targets; the
/// footer hints live in the status bar (draw_statusbar switches on mode).
pub fn draw_board(app: &mut App, f: &mut Frame, area: Rect) {
    let lang = app.lang();
    let board = app.board.lock().unwrap();
    let kanban = board.projection();
    let cols = TaskStatus::COLUMN_ORDER;
    let n = cols.len() as u16;
    let widths = Layout::horizontal((0..n).map(|_| Constraint::Ratio(1, u32::from(n)))).split(area);

    for (ci, &status) in cols.iter().enumerate() {
        let rect = widths[ci];
        let col_selected = app.board_col == ci;
        let color = status_color(status);
        let title = format!(" {} ", status_title(lang, status));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(if col_selected {
                Style::new()
                    .fg(TH.accent)
                    .bg(TH.bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                TH.border_style()
            })
            .title(Span::styled(
                title,
                Style::new()
                    .fg(color)
                    .bg(TH.bg)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::new().bg(TH.bg));
        let inner = block.inner(rect);
        f.render_widget(block, rect);

        let tasks = kanban.by_status(status);
        // Blocked / NeedsHuman cards surface in their source column edge:
        // Blocked & NeedsHuman render inside "Ready" column as alert rows.
        let extra: Vec<crate::kanban::Task> = if status == TaskStatus::Ready {
            let mut v = kanban.by_status(TaskStatus::Blocked);
            v.extend(kanban.by_status(TaskStatus::NeedsHuman));
            v
        } else {
            Vec::new()
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut hits: Vec<(usize, String)> = Vec::new(); // (row, task id)
        let mut push_card = |idx: usize, task: &crate::kanban::Task, alert: bool| {
            let selected = col_selected && app.board_row == idx;
            let bg = if selected { TH.sel_bg } else { TH.bg };
            let fg = if selected {
                TH.sel_fg
            } else if alert {
                TH.err
            } else {
                TH.fg
            };
            let title: String = task
                .title
                .chars()
                .take(inner.width.saturating_sub(10) as usize)
                .collect();
            let badge = if task.attempts > 1 {
                format!("R{}", task.attempts)
            } else {
                String::new()
            };
            let assignee = task.harness.clone().unwrap_or_default();
            let right = format!("{badge}{assignee:>4}");
            let pad = (inner.width as usize)
                .saturating_sub(title.chars().count() + right.chars().count() + 3);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}{title}", " ".repeat(pad.min(1))),
                    Style::new().fg(fg).bg(bg),
                ),
                Span::styled(
                    " ".repeat(pad.saturating_sub(1)),
                    Style::new().fg(fg).bg(bg),
                ),
                Span::styled(
                    right,
                    if alert {
                        Style::new().fg(TH.err).bg(bg)
                    } else {
                        Style::new().fg(TH.dim).bg(bg)
                    },
                ),
            ]));
            hits.push((idx, task.id.clone()));
        };

        for (i, task) in tasks.iter().enumerate() {
            push_card(i, task, false);
        }
        for (i, task) in extra.iter().enumerate() {
            push_card(tasks.len() + i, task, true);
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {}", t(lang, Str::NoSessions)),
                TH.dim_style(),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner);

        // register hit targets (visible rows only)
        for (row, task_id) in hits {
            if row < inner.height as usize {
                app.hit.targets.push((
                    Rect {
                        x: rect.x,
                        y: inner.y + row as u16,
                        width: rect.width,
                        height: 1,
                    },
                    HitTarget::TaskCard(task_id),
                ));
            }
        }
    }

    // hint line at the bottom of the board area
    let hint = if lang == Lang::Ru {
        " F7 новая · F4 правка · F6 → статус · a харнесс · Enter открыть · d принять · r доработка · F8 отмена "
    } else {
        " F7 new · F4 edit · F6 status → · a harness · Enter open · d approve · r rework · F8 cancel "
    };
    if area.height > 0 {
        let h = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(Span::styled(hint, TH.dim_style())), h);
    }
}

/// F-key hints for the status bar in board mode.
pub fn board_status_keys(lang: Lang) -> Vec<(&'static str, &'static str)> {
    let ru = lang == Lang::Ru;
    vec![
        ("F7", if ru { "Новая" } else { "New" }),
        ("F6", if ru { "Статус→" } else { "Status→" }),
        ("a", "harness"),
        ("⏎", if ru { "Открыть" } else { "Open" }),
        ("d", if ru { "Принять" } else { "Approve" }),
        ("r", if ru { "Доработка" } else { "Rework" }),
        ("F8", if ru { "Отмена" } else { "Cancel" }),
        ("^B", if ru { "Чат" } else { "Chat" }),
    ]
}
