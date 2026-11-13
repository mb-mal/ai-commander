//! Ctrl+B board screen rendering: the board replaces chat+composer, the
//! status bar switches to board key hints, and Chat mode is unchanged.

use ai_commander::config::Config;
use ai_commander::ui::App;

fn app() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(Config::default(), rx)
}

fn render(a: &mut App) -> String {
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 45)).expect("terminal");
    term.draw(|f| ai_commander::ui::widgets::draw(a, f))
        .expect("draw");
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area().height {
        for x in 0..buf.area().width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn ctrl_b(a: &mut App) {
    a.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('b'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
}

#[tokio::test]
async fn board_screen_replaces_chat_and_switches_hints() {
    let mut a = app();
    // a task in every interesting column
    {
        let mut b = a.board.lock();
        let t1 = b.add_task("DEFRAG overlay");
        b.transition(&t1.id, ai_commander::kanban::TaskAction::Promote)
            .unwrap();
        let t2 = b.add_task("merge guard");
        b.transition(&t2.id, ai_commander::kanban::TaskAction::Promote)
            .unwrap();
        b.transition(&t2.id, ai_commander::kanban::TaskAction::Start)
            .unwrap();
        b.transition(&t2.id, ai_commander::kanban::TaskAction::Finish)
            .unwrap();
    }

    let chat = render(&mut a);
    assert!(chat.contains("no sessions yet") || chat.contains("History"));

    ctrl_b(&mut a);
    assert!(matches!(a.mode, ai_commander::ui::UiMode::Board));
    let board = render(&mut a);
    assert!(board.contains("Backlog"), "Backlog column renders");
    assert!(board.contains("Ready"), "Ready column renders");
    assert!(board.contains("In progress"), "In progress column renders");
    assert!(board.contains("DEFRAG overlay"), "card renders");
    assert!(
        !board.contains("Enter to send"),
        "composer is hidden in Board mode"
    );
    assert!(board.contains("New task"), "status bar shows board hints");
    let last = board.lines().last().unwrap();
    assert!(last.contains("New task"), "status bar row: {last}");
    assert!(
        !last.contains("Settings"),
        "chat hints are gone from the bar"
    );

    // back to chat: composer and chat hints return
    ctrl_b(&mut a);
    let chat2 = render(&mut a);
    assert!(chat2.contains("Enter to send"), "composer is back");
    assert!(chat2.contains("F2 Settings"), "chat hints are back");
    assert!(!chat2.contains("New task"), "board hints are gone");
}
