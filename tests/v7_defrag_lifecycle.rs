//! v7 #1: the DEFRAG dialog lifecycle — a second compaction in the same
//! session creates a FRESH animator, Esc closes the current dialog without
//! changing the user preference, and switching chats drops a foreign one.

use ai_commander::backend::{BackendEvent, CompactionMethod, CompactionSource, CompactionStatus};
use ai_commander::config::Config;
use ai_commander::ui::App;

fn app() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(Config::default(), rx)
}

fn compact(app: &mut App, harness: &str, sid: &str, id: &str, status: CompactionStatus) {
    app.on_backend_event(BackendEvent::Compaction {
        harness: harness.into(),
        native_id: sid.into(),
        compaction_id: id.into(),
        status,
        method: CompactionMethod::Summarize,
        summary_delta: None,
        error: None,
        source: CompactionSource::Reported,
    });
}

fn usage(app: &mut App, harness: &str, sid: &str, used: u64, size: u64) {
    app.on_backend_event(BackendEvent::Usage {
        harness: harness.into(),
        native_id: sid.into(),
        used,
        size,
        cost_usd: None,
        estimated: false,
    });
}

fn esc(app: &mut App) {
    app.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ));
}

/// v7 #1 acceptance: two compactions, Esc between them — the second
/// creates a NEW animator with compaction_id = c-2; `compaction_view`
/// stays Defrag (the user preference is not permanently changed).
#[tokio::test]
async fn second_compaction_creates_new_animator_after_esc() {
    let mut a = app();
    a.cur.harness = "oc".into();
    a.cur.native_id = "s1".into();
    a.compaction_view = ai_commander::ui::defrag::CompactionView::Defrag;
    usage(&mut a, "oc", "s1", 170_000, 200_000);

    // first compaction: dialog opens
    compact(&mut a, "oc", "s1", "c-1", CompactionStatus::InProgress);
    assert!(a.defrag.is_some(), "first compaction opens the dialog");
    assert!(
        a.defrag.as_ref().is_some_and(|(_, _, id, _)| id == "c-1"),
        "animator bound to c-1"
    );

    compact(&mut a, "oc", "s1", "c-1", CompactionStatus::Completed);

    // Esc closes the CURRENT dialog — but NOT the preference
    esc(&mut a);
    assert!(a.defrag.is_none(), "Esc dismissed the dialog");
    assert_eq!(
        a.compaction_view,
        ai_commander::ui::defrag::CompactionView::Defrag,
        "the user preference is NOT changed by Esc"
    );

    // second compaction: a FRESH animator with c-2
    usage(&mut a, "oc", "s1", 150_000, 200_000);
    compact(&mut a, "oc", "s1", "c-2", CompactionStatus::InProgress);
    assert!(
        a.defrag.is_some(),
        "second compaction opens a NEW dialog (view=Defrag)"
    );
    assert!(
        a.defrag.as_ref().is_some_and(|(_, _, id, _)| id == "c-2"),
        "the new animator is bound to c-2, not the old c-1"
    );
}

/// v7 #1: a finished animator is REPLACED by a new compaction_id even
/// without Esc (auto-replacement).
#[tokio::test]
async fn finished_animator_replaced_by_new_id() {
    let mut a = app();
    a.cur.harness = "oc".into();
    a.cur.native_id = "s1".into();
    a.compaction_view = ai_commander::ui::defrag::CompactionView::Defrag;
    usage(&mut a, "oc", "s1", 170_000, 200_000);

    compact(&mut a, "oc", "s1", "c-1", CompactionStatus::InProgress);
    compact(&mut a, "oc", "s1", "c-1", CompactionStatus::Completed);
    // the old animator is still there (done, waiting for Esc/any key)

    // a NEW compaction id replaces it
    compact(&mut a, "oc", "s1", "c-2", CompactionStatus::InProgress);
    assert!(
        a.defrag.as_ref().is_some_and(|(_, _, id, _)| id == "c-2"),
        "a new compaction_id replaces the finished animator"
    );
}

/// v7 #1: switching to another chat drops the dialog of the old session.
#[tokio::test]
async fn switching_chat_drops_foreign_dialog() {
    let mut a = app();
    a.cur.harness = "oc".into();
    a.cur.native_id = "s1".into();
    a.compaction_view = ai_commander::ui::defrag::CompactionView::Defrag;
    usage(&mut a, "oc", "s1", 170_000, 200_000);
    compact(&mut a, "oc", "s1", "c-1", CompactionStatus::InProgress);
    assert!(a.defrag.is_some());

    // switch to s2
    a.sessions.insert(
        "oc".into(),
        vec![ai_commander::backend::SessionInfo {
            native_id: "s2".into(),
            title: "other".into(),
            updated_ms: 0,
        }],
    );
    a.open_session("oc", "s2");
    assert!(
        a.defrag.is_none(),
        "switching to another chat drops the foreign DEFRAG dialog"
    );
}

/// v7 #1: any key dismisses a DONE dialog (not just Esc).
#[tokio::test]
async fn any_key_dismisses_done_dialog() {
    let mut a = app();
    a.cur.harness = "oc".into();
    a.cur.native_id = "s1".into();
    a.compaction_view = ai_commander::ui::defrag::CompactionView::Defrag;
    usage(&mut a, "oc", "s1", 170_000, 200_000);
    compact(&mut a, "oc", "s1", "c-1", CompactionStatus::InProgress);
    compact(&mut a, "oc", "s1", "c-1", CompactionStatus::Completed);

    // press a letter key — the DONE dialog goes away
    a.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(a.defrag.is_none(), "any key dismisses a DONE dialog");
}
