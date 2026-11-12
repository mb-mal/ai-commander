//! v8 review fixes: task reply accumulation (#1), session rebinding (#2),
//! worktree merge on Approve (#3), leading ".." in normalize_path (#5).

use ai_commander::kanban::{TaskAction, TaskStatus, runner, store::BoardStore};

// ============ #1: task replies are classified correctly ============

#[test]
fn accumulated_reply_classifies_done() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("feature");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    // simulate what the daemon/TUI now does: accumulate text deltas
    let mut reply = String::new();
    runner::on_progress(&mut b, &t.id, "DONE: feature implemented", &mut reply);
    assert!(reply.contains("DONE:"));

    let after = runner::on_stream_done(&mut b, &t.id, &reply).unwrap();
    assert_eq!(
        after.status,
        TaskStatus::Review,
        "accumulated DONE: → Review"
    );
}

#[test]
fn empty_reply_is_failed_not_done() {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("empty");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();

    let after = runner::on_stream_done(&mut b, &t.id, "").unwrap();
    assert_ne!(
        after.status,
        TaskStatus::Review,
        "empty reply must not Finish"
    );
}

// ============ #5: normalize_path keeps leading ".." ============

#[test]
fn leading_dotdot_survives_normalization() {
    use ai_commander::policy::{Effect, Policy, Rule};
    let p = Policy {
        rules: vec![Rule {
            action: "edit".into(),
            pattern: "src/*".into(),
            effect: Effect::Allow,
        }],
        ..Default::default()
    };
    // inside the repo: allowed
    assert_eq!(p.check("edit", "src/lib.rs"), Effect::Allow);
    // OUTSIDE the repo: the leading ".." must not be dropped
    assert_ne!(
        p.check("edit", "../src/lib.rs"),
        Effect::Allow,
        "../src/lib.rs must not match src/* (leading .. dropped)"
    );
    assert_ne!(
        p.check("edit", "../../.ssh/id_rsa"),
        Effect::Allow,
        "../../ traversal must not match"
    );
}

// ============ #3: board_tool_call approve uses transition_in ============
// The code change is verified by the existing worktree tests
// (approve_merges_into_task_base_branch, dirty_base_checkout_blocks_approve).
// Here we just verify the task transitions correctly through board_tool_call
// in a NON-git environment (no worktree to merge, but the FSM still works).

#[test]
fn board_tool_call_approve_transitions_task() {
    use ai_commander::kanban::{Actor, board_tool_call};
    use serde_json::json;

    let mut b = BoardStore::open_memory();
    let t = b.add_task("v8 approve");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();
    b.transition(&t.id, TaskAction::Finish).unwrap();
    assert_eq!(b.projection().tasks[0].status, TaskStatus::Review);

    let result = board_tool_call(
        "board_update_task",
        &json!({"task_id": t.id, "action": "approve"}),
        &Actor::Role("reviewer".into()),
        &mut b,
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        b.projection().tasks[0].status,
        TaskStatus::Done,
        "board_tool_call approve transitions to Done"
    );
}
