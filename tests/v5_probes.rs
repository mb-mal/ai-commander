//! Probes from the v5 review follow-up. Each must fail on the pre-fix
//! code and pass after the fix.

use ai_commander::kanban::{TaskAction, TaskStatus, runner, store::BoardStore};

/// v5 М3: a failed Start escalates WITH the reason recorded in the task
/// thread — the human sees why the launch broke.
#[test]
fn launch_failure_reason_lands_in_the_task() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("cannot start");
    b.transition(&t.id, TaskAction::Promote).unwrap(); // → Ready

    runner::launch_failed(&mut b, &t.id, "worktree add failed: branch exists");

    let after = b.projection().tasks.iter().find(|x| x.id == t.id).unwrap();
    assert_eq!(after.status, TaskStatus::NeedsHuman);
    let note = after
        .comments
        .iter()
        .find(|c| c.text.contains("worktree add failed: branch exists"));
    assert!(
        note.is_some(),
        "the failure reason must be recorded in the task thread (М3)"
    );
}

/// v5: the policy engine said ASK while unattended (daemon) — the task
/// goes to NeedsHuman with the question in its thread, immediately,
/// instead of hanging until the lease expires.
#[test]
fn permission_ask_moves_task_to_needs_human_with_question() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("needs permission");
    b.assign(&t.id, Some("oc".into())).unwrap();
    b.link_session(&t.id, "ses_ask");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    let question = "agent asks permission: rm -rf /tmp (bash: /tmp/x)";
    let after = runner::on_permission_ask(&mut b, &t.id, question).unwrap();

    assert_eq!(
        after.status,
        TaskStatus::NeedsHuman,
        "ASK must escalate immediately, not wait for the lease"
    );
    let note = after.comments.iter().find(|c| c.text.contains(question));
    assert!(note.is_some(), "the question must land in the task thread");
    assert_eq!(note.unwrap().kind, "ask");

    // the task is no longer launchable and the watchdog leaves it alone
    assert!(!b.launchable().iter().any(|x| x.id == t.id));
    assert!(b.watchdog_scan(ai_commander::kanban::now_ms()).is_empty());
}

/// v5: a turn that finishes AFTER the task already went NeedsHuman
/// (permission ask mid-turn) must not re-classify it — mirrors the
/// daemon's InProgress guard.
#[test]
fn stream_done_after_ask_does_not_override_needs_human() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("ask then stream ends");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    runner::on_permission_ask(&mut b, &t.id, "agent asks: bash rm").unwrap();

    // the agent turn later completes with DONE: — but the task is already
    // waiting for a human; the daemon guard skips classification
    let still = b
        .transition(&t.id, TaskAction::Finish)
        .err()
        .expect("Finish must be illegal from NeedsHuman");
    assert!(still.contains("illegal"));
    assert_eq!(
        b.projection()
            .tasks
            .iter()
            .find(|x| x.id == t.id)
            .unwrap()
            .status,
        TaskStatus::NeedsHuman
    );
}
