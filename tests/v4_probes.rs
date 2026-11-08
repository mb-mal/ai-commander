//! Probes from the v4 review. Each must fail on the pre-fix code and pass
//! after the fix.

use ai_commander::kanban::{TaskAction, TaskStatus, runner, store::BoardStore};

/// Д1: a tool call that runs silently for 3 minutes must NOT be
/// watchdog-interrupted — the PartUpdated heartbeat (runner::on_activity)
/// covers the whole window (old lease was 60 s).
#[test]
fn tool_call_silence_does_not_interrupt() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("long tool call");
    b.assign(&t.id, Some("oc".into())).unwrap();
    b.link_session(&t.id, "ses_d1");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    // the tool call starts (PartUpdated) at T0 and emits NOTHING for 3 min
    let t0 = ai_commander::kanban::now_ms();
    runner::on_activity(&mut b, &t.id);

    // at T0+3min: the lease from the activity heartbeat must still hold
    let stuck = b.watchdog_scan(t0 + 3 * 60_000);
    assert!(
        stuck.is_empty(),
        "a 3-minute silent tool call must not be interrupted (Д1): {stuck:?}"
    );

    // well past the lease the watchdog still fires — the heartbeat is not
    // a blank check
    let lease_end = b.projection().tasks[0].lease_until_ms.unwrap();
    let stuck = b.watchdog_scan(lease_end + 1);
    assert_eq!(stuck.len(), 1, "an expired lease must still interrupt");
}

/// Н2 leftover: a stream that printed `DONE:` and then died must NOT land
/// in Review — the errored turn is Failed (and the retry cap applies).
#[test]
fn done_marker_after_stream_error_is_not_finished() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("broken stream");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    // sanity: without an error the marker still finishes the task
    let ok = runner::on_stream_done(&mut b, &t.id, "DONE: all good, tests pass").unwrap();
    assert_eq!(ok.status, TaskStatus::Review);
    b.transition(&t.id, TaskAction::Rework).unwrap(); // → Ready
    b.transition(&t.id, TaskAction::Start).unwrap();

    // the same marker AFTER a StreamError must be Failed → Ready (retry)
    let after =
        runner::on_stream_done_ex(&mut b, &t.id, "DONE: printed right before the crash", true)
            .unwrap();
    assert_ne!(
        after.status,
        TaskStatus::Review,
        "an errored stream must not reach Review (v4 Н2)"
    );
}

/// v4: a Start that cannot even begin (broken worktree/branch) escalates
/// to NeedsHuman instead of being retried every 30 seconds forever.
#[test]
fn failed_start_escalates_instead_of_retry_looping() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("cannot start");
    b.transition(&t.id, TaskAction::Promote).unwrap(); // → Ready

    runner::launch_failed(&mut b, &t.id);

    let after = b.projection().tasks.iter().find(|x| x.id == t.id).unwrap();
    assert_eq!(
        after.status,
        TaskStatus::NeedsHuman,
        "a failed Start must escalate to a human"
    );
    assert!(
        !b.launchable().iter().any(|x| x.id == t.id),
        "an escalated task must not be re-launched"
    );
}
