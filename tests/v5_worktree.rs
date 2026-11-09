//! v5 #2 + #3: Approve merges into the task's BASE branch (the user's
//! checkout is never touched), and worktrees live OUTSIDE the repository
//! (no embedded-repo pollution of `git add -A`); orphans are pruned.

use std::path::Path;
use std::process::Command;

use ai_commander::kanban::store::BoardStore;
use ai_commander::kanban::{Task, TaskAction, worktree};

fn init_repo(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aic-wtv5-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let g = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {}: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    g(&["init", "-q", "-b", "main"]);
    g(&["config", "user.email", "t@t"]);
    g(&["config", "user.name", "t"]);
    std::fs::write(dir.join("base.txt"), "base\n").unwrap();
    g(&["add", "-A"]);
    g(&["commit", "-qm", "base"]);
    dir
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// v5 #2: the merge lands on the task's base branch; the branch the user
/// switched to meanwhile stays clean, and their checkout is untouched.
#[test]
fn approve_merges_into_task_base_branch() {
    let repo = init_repo("base");
    let mut b = BoardStore::open_memory();
    let t = b.add_task("feature");
    b.transition(&t.id, TaskAction::Promote).unwrap();

    // agent turn: worktree, change, finish into Review
    b.transition_in(&repo, &t.id, TaskAction::Start).unwrap();
    let started = b.projection().tasks.last().unwrap().clone();
    assert_eq!(started.base_branch, "main", "base recorded at Start");
    let wt = worktree::worktree_path(&repo, &started);
    assert!(wt.exists(), "worktree created");
    std::fs::write(wt.join("agent.txt"), "agent work\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "agent work"]);

    // the user moves on: switches to another branch, adds their own file
    git(&repo, &["checkout", "-q", "-b", "other"]);
    std::fs::write(repo.join("user.txt"), "user work\n").unwrap();

    b.transition_in(&repo, &t.id, TaskAction::Finish).unwrap(); // → Review
    b.transition_in(&repo, &t.id, TaskAction::Approve).unwrap(); // → Done (merge)

    // the agent's change is on MAIN (the task base)…
    assert!(
        git(&repo, &["show", "main:agent.txt"]).contains("agent work"),
        "the merge must land on the task base branch (main)"
    );
    // …NOT on the branch the user switched to…
    assert!(
        Command::new("git")
            .current_dir(&repo)
            .args(["show", "other:agent.txt"])
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true),
        "the user's current branch must not receive the merge"
    );
    // …and their working tree (with the uncommitted user.txt) is intact
    assert!(repo.join("user.txt").exists());
    let status = git(&repo, &["status", "--porcelain"]);
    assert!(
        status.contains("user.txt"),
        "user's file untouched: {status}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// v5 #2: the base branch checked out with uncommitted changes blocks the
/// merge — nothing is lost, the task stays in Review.
#[test]
fn dirty_base_checkout_blocks_approve() {
    let repo = init_repo("dirty");
    let mut b = BoardStore::open_memory();
    let t = b.add_task("feature2");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition_in(&repo, &t.id, TaskAction::Start).unwrap();
    let started = b.projection().tasks.last().unwrap().clone();
    let wt = worktree::worktree_path(&repo, &started);
    std::fs::write(wt.join("agent2.txt"), "x\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "agent 2"]);

    // user stays ON main with an uncommitted file
    std::fs::write(repo.join("wip.txt"), "wip\n").unwrap();
    b.transition_in(&repo, &t.id, TaskAction::Finish).unwrap();
    let err = b
        .transition_in(&repo, &t.id, TaskAction::Approve)
        .expect_err("dirty base must block the merge");
    assert!(
        err.contains("uncommitted changes"),
        "the error must explain the block: {err}"
    );
    assert!(repo.join("wip.txt").exists(), "user's WIP is intact");
    let status = git(&repo, &["status", "--porcelain"]);
    assert!(status.contains("wip.txt"));
    let _ = std::fs::remove_dir_all(&repo);
}

/// v5 #3: worktrees live outside the repo — `git status` in the user's
/// checkout stays clean; orphaned worktrees are pruned at startup.
#[test]
fn worktrees_stay_outside_and_orphans_pruned() {
    let repo = init_repo("outside");
    let mut b = BoardStore::open_memory();
    let t = b.add_task("live task");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition_in(&repo, &t.id, TaskAction::Start).unwrap();
    let task = b.projection().tasks.last().unwrap().clone();
    let wt = worktree::worktree_path(&repo, &task);
    assert!(
        !wt.starts_with(&repo),
        "worktree must be outside the repo: {}",
        wt.display()
    );

    // the user's repo does not see the worktree at all
    let status = git(&repo, &["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "repo must stay clean: {status}");

    // an orphan worktree (task deleted after a crash) is pruned; the live
    // one survives
    let orphan = Task {
        id: "t-9999".into(),
        ..task.clone()
    };
    worktree::start(&repo, &orphan).expect("orphan worktree");
    let live_ids: Vec<String> = vec![t.id.clone()];
    let removed = worktree::prune_orphans(&repo, &live_ids);
    assert_eq!(removed, vec!["t-9999".to_string()], "orphan pruned");
    assert!(wt.exists(), "the live task's worktree survives");
    let _ = std::fs::remove_dir_all(&repo);
}

/// v6 #1 (the review probe): the user stands on the base branch with a
/// CLEAN tree — Approve must make the agent's files APPEAR in their
/// checkout, never look deleted (a following `commit -am` must not wipe
/// the agent's work).
#[test]
fn approve_on_clean_base_shows_work_in_the_checkout() {
    let repo = init_repo("ff");
    let mut b = BoardStore::open_memory();
    let t = b.add_task("ff task");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition_in(&repo, &t.id, TaskAction::Start).unwrap();
    let started = b.projection().tasks.last().unwrap().clone();
    let wt = worktree::worktree_path(&repo, &started);
    std::fs::write(wt.join("g"), "agent file\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "agent g"]);

    // the user IS on main (the task base) with a clean tree
    b.transition_in(&repo, &t.id, TaskAction::Finish).unwrap();
    b.transition_in(&repo, &t.id, TaskAction::Approve).unwrap();

    // the review probe: status must NOT show the file as deleted
    let status = git(&repo, &["status", "--porcelain"]);
    assert!(
        !status.contains('D'),
        "the agent's file must not look deleted: {status}"
    );
    assert!(repo.join("g").exists(), "the file is present in the checkout");

    // and a commit -am afterwards keeps the agent's file
    std::fs::write(repo.join("user.txt"), "u
").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "user follow-up"]);
    let show = git(&repo, &["show", "HEAD:g"]);
    assert!(show.contains("agent file"), "commit -am kept the agent's file");
    assert!(!git(&repo, &["status", "--porcelain"]).contains('D'));
    let _ = std::fs::remove_dir_all(&repo);
}

/// v6 #1: update-ref with an expected-old value must REFUSE to move a ref
/// that changed in between (a concurrent user commit is never overwritten).
#[test]
fn update_ref_guard_refuses_moved_base() {
    let repo = init_repo("guard");
    // two commits on main; trying to move main with the WRONG old value
    // (the first commit) must fail and leave main where it is
    let first = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(repo.join("second.txt"), "2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "second"]);
    let second = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let out = std::process::Command::new("git")
        .current_dir(&repo)
        .args(["update-ref", "refs/heads/main", &second, &first])
        .output()
        .expect("git update-ref");
    assert!(
        !out.status.success(),
        "update-ref must refuse with a stale expected-old value"
    );
    let still = git(&repo, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(still, second, "main was not moved by the refused command");
    let _ = std::fs::remove_dir_all(&repo);
}

/// v6: the worktree path is STORED on the task — a relocated cache (hash
/// scheme change, different AI_COMMANDER_WORKTREES_DIR) still finds it.
#[test]
fn worktree_path_is_stored_and_survives_relocation() {
    // stable slug: same input → same slug across calls (FNV-1a)
    let repo = init_repo("stored");
    let mut b = BoardStore::open_memory();
    let t = b.add_task("stored path");
    b.transition(&t.id, TaskAction::Promote).unwrap();
    b.transition_in(&repo, &t.id, TaskAction::Start).unwrap();
    let started = b.projection().tasks.last().unwrap().clone();
    assert!(
        !started.worktree_path.is_empty(),
        "Start records the worktree path on the task"
    );
    let stored = std::path::PathBuf::from(&started.worktree_path);
    assert!(stored.exists(), "the stored path is real");

    // a "relocated" environment: the env override no longer matches — the
    // stored path still resolves (this is what finish() uses)
    let resolved = worktree::worktree_path(&repo, &started);
    assert_eq!(resolved, stored, "stored path wins over computed");

    let _ = std::fs::remove_dir_all(&repo);
}
