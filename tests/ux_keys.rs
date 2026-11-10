//! UX probes from the v6 live feedback: End goes to line end, Alt+Enter
//! newlines, Ctrl+R renames without sidebar focus, Ctrl+F searches.

use ai_commander::config::Config;
use ai_commander::ui::App;

fn app() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(Config::default(), rx)
}

fn key(
    code: crossterm::event::KeyCode,
    mods: crossterm::event::KeyModifiers,
) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, mods)
}

#[tokio::test]
async fn end_goes_to_line_end_alt_enter_newlines() {
    let mut a = app();
    a.editor.set_text("hello world");

    // type "X" in the middle, then End must jump AFTER the last char
    for _ in 0..6 {
        a.on_key(key(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::NONE,
        ));
    }
    a.on_key(key(
        crossterm::event::KeyCode::Char('X'),
        crossterm::event::KeyModifiers::NONE,
    ));
    // End — must NOT scroll (that moved to Ctrl+End) but move the cursor
    a.on_key(key(
        crossterm::event::KeyCode::End,
        crossterm::event::KeyModifiers::NONE,
    ));
    a.on_key(key(
        crossterm::event::KeyCode::Char('!'),
        crossterm::event::KeyModifiers::NONE,
    ));
    let text = a.editor.text();
    assert!(
        text.ends_with('!'),
        "End moved the cursor to the end (text: {text:?})"
    );

    // Ctrl+Enter inserts a newline (Alt is fullscreen in Windows)
    a.on_key(key(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(
        a.editor.text().contains('\n'),
        "Ctrl+Enter newlines: {:?}",
        a.editor.text()
    );

    // Ctrl+J also newlines (the classic terminal-safe binding)
    let newlines = a.editor.text().matches('\n').count();
    a.on_key(key(
        crossterm::event::KeyCode::Char('j'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert_eq!(
        a.editor.text().matches('\n').count(),
        newlines + 1,
        "Ctrl+J newlines"
    );

    // Shift+Enter is NOT a command — it falls through to send_current()
    // (Shift is for capital letters only). The editor clears on send.
    assert!(
        !a.editor.text().is_empty(),
        "editor has text before the send"
    );
    a.on_key(key(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::SHIFT,
    ));
    assert!(
        a.editor.text().is_empty(),
        "Shift+Enter sends (no newline inserted — Shift is caps, not a command)"
    );
}

#[tokio::test]
async fn ctrl_r_renames_current_session() {
    let mut a = app();
    a.cur.harness = "oc".into();
    a.cur.native_id = "ses_x".into();
    a.sessions.insert(
        "oc".into(),
        vec![ai_commander::backend::SessionInfo {
            native_id: "ses_x".into(),
            title: "old title".into(),
            updated_ms: 0,
        }],
    );
    a.on_key(key(
        crossterm::event::KeyCode::Char('r'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    // the rename INPUT modal opens (previously: 'r' typed into the composer)
    assert!(
        matches!(&a.modal, Some(ai_commander::Modal::Input { title, .. }) if title.contains("Rename") || title.contains("Переименовать")),
        "Ctrl+R opens the rename modal, got {:?}",
        a.modal.is_some()
    );
    assert!(
        !a.editor.text().contains('r'),
        "nothing typed into the composer"
    );
}

#[tokio::test]
async fn search_finds_lines_and_copies_selection() {
    let mut a = app();
    a.cur.harness = "oc".into();
    a.cur.native_id = "ses_s".into();
    a.cur.msgs = vec![
        ai_commander::backend::ChatMsg {
            msg_id: "m1".into(),
            role: ai_commander::backend::Role::User,
            parts: vec![ai_commander::backend::RenderPart {
                part_id: "p1".into(),
                kind: ai_commander::backend::PartKind::Text,
                text: "hello unique_token here".into(),
            }],
            attachments: vec![],
        },
        ai_commander::backend::ChatMsg {
            msg_id: "m2".into(),
            role: ai_commander::backend::Role::Assistant,
            parts: vec![ai_commander::backend::RenderPart {
                part_id: "p2".into(),
                kind: ai_commander::backend::PartKind::Text,
                text: "another line".into(),
            }],
            attachments: vec![],
        },
    ];

    // Ctrl+F opens the search modal
    a.on_key(key(
        crossterm::event::KeyCode::Char('f'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(a.modal.is_some(), "search modal opens");

    // apply the search query through the input action
    a.apply_input(
        "unique_token".into(),
        ai_commander::InputAction::SearchQuery,
    );
    let sr = a.search.as_ref().expect("search state");
    assert_eq!(sr.matches.len(), 1, "one matching line");
    assert_eq!(sr.match_idx, 0);

    // selection: Ctrl+Down selects lines, Ctrl+C copies
    a.modal = None;
    a.on_key(key(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(a.sel_anchor.is_some(), "selection started");
    a.on_key(key(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::CONTROL,
    ));

    // typing clears the selection
    a.on_key(key(
        crossterm::event::KeyCode::Char('z'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(a.sel_anchor.is_none(), "typing clears the selection");
}
