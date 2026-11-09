//! v5 #10 + v6 #3: a permission ask parked in NeedsHuman becomes a
//! one-time permit ONLY through an explicit human grant (the `permit`
//! journal comment from human-only board_unpark). Observing a
//! NeedsHuman-to-Ready transition grants nothing — agents used to trigger
//! it themselves with `promote`.

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

    // v6 #3: the explicit human grant is the ONLY path
    assert!(pb.grant("t-0001"), "the human grant works");
    assert!(
        pb.take_permit("t-0001", "execute", "git push origin main"),
        "the exact approved ask passes once"
    );
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
fn grant_requires_the_parked_ask() {
    let mut pb = PermitBook::new();
    assert!(!pb.grant("t-0002"));
    assert!(!pb.take_permit("t-0002", "edit", "src/lib.rs"));
}
