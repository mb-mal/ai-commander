//! v5 #10: a permission ask parked in NeedsHuman becomes a one-time permit
//! when the human un-parks the task — the relaunched turn does not hit the
//! same question again (and the permit is consumed after one use).

use ai_commander::kanban::TaskStatus;
use ai_commander::kanban::runner::PermitBook;

#[test]
fn human_approval_grants_the_ask_once() {
    let mut pb = PermitBook::new();

    // the daemon parked an ask for the task
    pb.record_ask("t-0001", "execute", "git push origin main");
    assert!(
        !pb.take_permit("t-0001", "execute", "git push origin main"),
        "no permit before the human acts"
    );

    // unrelated transitions do nothing
    pb.observe_transition("t-0001", TaskStatus::Backlog, TaskStatus::Ready);
    assert!(!pb.take_permit("t-0001", "execute", "git push origin main"));

    // the human un-parks the task: NeedsHuman → Ready grants it
    pb.observe_transition("t-0001", TaskStatus::NeedsHuman, TaskStatus::Ready);
    assert!(
        pb.take_permit("t-0001", "execute", "git push origin main"),
        "the exact approved ask passes once"
    );
    // …and only once, and nothing else passes
    assert!(
        !pb.take_permit("t-0001", "execute", "git push origin main"),
        "the permit is consumed"
    );
    assert!(
        !pb.take_permit("t-0001", "execute", "rm -rf target"),
        "a different resource does not inherit the permit"
    );
}

#[test]
fn permit_requires_the_parked_ask() {
    let mut pb = PermitBook::new();
    // a task that never asked gets nothing even after the same transition
    pb.observe_transition("t-0002", TaskStatus::NeedsHuman, TaskStatus::Ready);
    assert!(!pb.take_permit("t-0002", "edit", "src/lib.rs"));
}
