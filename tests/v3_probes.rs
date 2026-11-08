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

/// Н3: a launched task carries its worktree as the cwd so the agent works
/// in isolation, not in the shared repo checkout.
#[test]
fn launch_runs_in_worktree() {
    // real (throwaway) git repo — worktrees only exist in one
    let dir = std::env::temp_dir().join(format!("aic-n3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} in {:?}: {}{}",
            args.join(" "),
            dir,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("r.txt"), "base\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);

    let mut b = BoardStore::open_memory();
    let t = b.add_task("isolate me");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    let task = b.projection().tasks.last().unwrap().clone();

    let roles = ai_commander::kanban::roles::RoleSet::default();
    let cfg = ai_commander::config::Config::default();
    let ctx = runner::prepare_launch(&mut b, &task, &cfg, &roles, &dir).expect("prepare_launch");

    let expected = ai_commander::kanban::worktree::worktree_path(&dir, &task);
    assert_eq!(
        ctx.cwd.as_deref(),
        Some(expected.as_path()),
        "launch cwd must be the task worktree (Н3)"
    );
    assert!(expected.exists(), "worktree must exist after Start");
    assert!(
        expected.join(".git").exists(),
        "worktree must be a git worktree"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Д4: a manual Start of a task whose dependencies are not Done must be
/// refused — the DAG gates every Start, not just the daemon scheduler.
#[test]
fn manual_start_blocked_by_unfinished_dependency() {
    let mut b = BoardStore::open_memory();
    let a = b.add_task("dependency A");
    let c = b.add_task("child C");
    b.set_depends(&c.id, vec![a.id.clone()]).unwrap();
    b.transition(&c.id, TaskAction::Promote).unwrap(); // → Ready

    // A is still Backlog (not Done/Cancelled) → Start must fail
    let err = b.transition(&c.id, TaskAction::Start).unwrap_err();
    assert!(
        err.contains(&a.id),
        "error must name the blocking dependency: {err}"
    );
    assert_eq!(
        b.projection().tasks.iter().find(|t| t.id == c.id).unwrap().status,
        TaskStatus::Ready,
        "blocked task must stay Ready"
    );

    // finish A → Start goes through
    b.transition(&a.id, TaskAction::Promote).unwrap();
    b.transition(&a.id, TaskAction::Start).unwrap();
    b.transition(&a.id, TaskAction::Finish).unwrap(); // → Review
    b.transition(&a.id, TaskAction::Approve).unwrap(); // → Done
    b.transition(&c.id, TaskAction::Start).unwrap();
    assert_eq!(
        b.projection().tasks.iter().find(|t| t.id == c.id).unwrap().status,
        TaskStatus::InProgress
    );
}
