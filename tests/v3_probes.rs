//! Probes from the v3 review, turned into tests. Each must fail on the
//! pre-fix code and pass after the fix.

use ai_commander::kanban::{TaskAction, TaskStatus, runner, store::BoardStore};

/// Н1 probe: a task that always fails must NOT be retried indefinitely.
/// After max_attempts (3) it must land in NeedsHuman and not be launchable.
#[test]
fn no_infinite_retry_after_max_attempts() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("will always fail");
    b.transition(&t.id, TaskAction::Promote).unwrap();

    for i in 1..=3 {
        // simulate backoff expiry so the task is launchable again
        b.set_not_before(&t.id, ai_commander::kanban::now_ms() - 1);
        assert!(
            b.launchable().iter().any(|x| x.id == t.id),
            "not launchable at attempt {i} after backoff expiry"
        );
        b.transition(&t.id, TaskAction::Start).unwrap();
        let after = runner::on_stream_done(&mut b, &t.id, "").unwrap();
        if i < 3 {
            assert_eq!(after.status, TaskStatus::Ready, "attempt {i}: retry");
        } else {
            assert_eq!(
                after.status,
                TaskStatus::NeedsHuman,
                "attempt {i}: must escalate, not retry forever"
            );
        }
    }

    // Н1 acceptance: the task is NOT launchable after escalation
    assert!(
        !b.launchable().iter().any(|x| x.id == t.id),
        "escalated task must not be launchable"
    );
}

/// Н1: exponential backoff — a Failed task is not launchable immediately,
/// but becomes launchable after the backoff window.
#[test]
fn backoff_prevents_immediate_retry() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("backoff test");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    // fail → Interrupt → Ready with backoff
    let after = runner::on_stream_done(&mut b, &t.id, "").unwrap();
    assert_eq!(after.status, TaskStatus::Ready);

    // immediately after: not launchable (backoff 30s for first failure)
    assert!(
        !b.launchable().iter().any(|x| x.id == t.id),
        "backoff must prevent immediate retry"
    );

    // after the backoff window: launchable again
    b.set_not_before(&t.id, ai_commander::kanban::now_ms() - 1); // simulate time passage
    assert!(
        b.launchable().iter().any(|x| x.id == t.id),
        "task must be launchable after backoff expires"
    );
}

/// Н2 probe: error text longer than 20 chars must NOT count as done.
#[test]
fn error_text_is_not_done() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("x");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    let after =
        runner::on_stream_done(&mut b, &t.id, "Error: 401 Unauthorized - invalid API key").unwrap();

    assert_ne!(
        after.status,
        TaskStatus::Review,
        "error text must not transition to Review (Н2)"
    );
}

/// Н2: only an explicit DONE: prefix means Finished.
#[test]
fn done_marker_required() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("y");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    let after = runner::on_stream_done(&mut b, &t.id, "DONE: implemented and tested").unwrap();
    assert_eq!(after.status, TaskStatus::Review, "DONE: marker → Review");
}
