//! v6 #3: an agent must not un-park NeedsHuman tasks (the review probe:
//! `board_update_task promote` used to return Ok and PermitBook granted
//! the parked ask). Only a HUMAN (the local-user IPC token / TUI) can.

use ai_commander::kanban::{Actor, TaskAction, TaskStatus, board_tool_call, store::BoardStore};
use serde_json::json;

fn needs_human_board() -> (BoardStore, String) {
    let mut b = BoardStore::open_memory();
    let t = b.add_task("parked");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition(&t.id, TaskAction::Start).unwrap();
    b.transition(&t.id, TaskAction::Ask).unwrap();
    let id = t.id.clone();
    (b, id)
}

/// The review probe: an agent promoting a NeedsHuman task is refused.
#[test]
fn agent_cannot_unpark_needs_human() {
    let (mut b, id) = needs_human_board();
    let err = board_tool_call(
        "board_update_task",
        &json!({"task_id": id, "action": "promote"}),
        &Actor::Role("coder".into()),
        &mut b,
    )
    .expect_err("agents must not un-park NeedsHuman tasks");
    assert!(err.contains("human"), "the error must say who may: {err}");
    assert_eq!(
        b.projection().tasks[0].status,
        TaskStatus::NeedsHuman,
        "the task stays parked"
    );
}

/// The human-only board_unpark tool: transition + `permit` journal record.
#[test]
fn human_unpark_grants_and_records() {
    let (mut b, id) = needs_human_board();

    // roles cannot call the tool at all
    let err = board_tool_call(
        "board_unpark",
        &json!({"task_id": id}),
        &Actor::Role("coder".into()),
        &mut b,
    )
    .expect_err("board_unpark is human-only");
    assert!(err.contains("human-only"), "{err}");

    // the human can
    let out = board_tool_call(
        "board_unpark",
        &json!({"task_id": id}),
        &Actor::Human,
        &mut b,
    )
    .expect("human un-park works");
    assert!(out.contains("permit granted"), "{out}");
    let t = &b.projection().tasks[0];
    assert_eq!(t.status, TaskStatus::Ready);
    assert!(
        t.comments
            .iter()
            .any(|c| c.kind == "permit" && c.author == "human"),
        "the PermissionGranted record is in the journal"
    );
}

/// End-to-end over IPC: the human token (3rd line of the port file)
/// authorizes board_unpark; a role token does not.
#[test]
fn ipc_human_token_authorizes_unpark() {
    use std::sync::Arc;
    let board = Arc::new(parking_lot::Mutex::new(needs_human_board().0));
    let b2 = board.clone();
    let handle: ai_commander::kanban::ipc::ToolHandler = Arc::new(move |tool, args, actor| {
        let mut b = b2.lock();
        ai_commander::kanban::board_tool_call(tool, args, &actor, &mut b)
    });
    let pf = std::env::temp_dir().join(format!("aic-human-{}", std::process::id()));
    let _ = std::fs::remove_file(&pf);
    let port = ai_commander::kanban::ipc::serve_at(&pf, handle).expect("serve");

    // v7: board.port has exactly 2 lines (port + transport); the human
    // token is NOT there — it is in the owner's memory / temp file
    let raw = std::fs::read_to_string(&pf).expect("port file");
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 2, "port file has port + transport only");
    assert_eq!(lines[0], port.to_string());
    // get the human token from the owner's internal state
    let human = ai_commander::kanban::ipc::human_token_internal()
        .expect("owner has a human token")
        .2;

    let call = |token: &str| {
        ai_commander::kanban::ipc::call_owner_at(
            &pf,
            "board_unpark",
            &json!({"task_id": "t-0001"}),
            token,
        )
    };
    // a ROLE token is refused
    let role_tok = ai_commander::kanban::ipc::issue_role_token("coder");
    assert!(
        call(&role_tok).expect("reachable").is_err(),
        "role token must not un-park"
    );
    // the HUMAN token works
    call(&human)
        .expect("reachable")
        .expect("human token un-parks");
    assert_eq!(board.lock().projection().tasks[0].status, TaskStatus::Ready);
    let _ = std::fs::remove_file(&pf);
}
