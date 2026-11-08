//! Git worktree isolation: every code task runs in its own worktree + branch.
//!
//! `start` → `ai-commander/t-0007` branch in `../.ai-commander-worktrees/t-0007`
//! `finish` (Review→Done) → merge the branch back to the base and remove the
//! worktree; Cancel drops it. Merge conflicts abort the transition with an
//! error (the task stays in Review for a human).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::{Task, TaskAction, TaskStatus};

/// Where worktrees live relative to the repo root: `<repo>/.aic-worktrees/`.
fn worktrees_dir(repo: &Path) -> PathBuf {
    repo.join(".aic-worktrees")
}

pub fn worktree_path(repo: &Path, task: &Task) -> PathBuf {
    worktrees_dir(repo).join(sanitize(&task.id))
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .map_err(|e| anyhow!("git: {e}"))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn is_git_repo(path: &Path) -> bool {
    git(path, &["rev-parse", "--git-dir"]).is_ok()
}

/// Current branch of the repo (worktrees report their own branch).
fn current_branch(repo: &Path) -> Result<String> {
    let b = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    if b.is_empty() || b == "HEAD" {
        bail!("detached HEAD, refusing to base a worktree here");
    }
    Ok(b)
}

/// Create the worktree + branch for a task (Start). Idempotent: reuses an
/// existing worktree of the same task.
pub fn start(repo: &Path, task: &Task) -> Result<PathBuf> {
    let wt = worktree_path(repo, task);
    let branch = format!("ai-commander/{}", sanitize(&task.id));
    if wt.exists() {
        // already isolated
        return Ok(wt);
    }
    let base = current_branch(repo).context("determine base branch")?;
    std::fs::create_dir_all(worktrees_dir(repo)).ok();
    git(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &wt.to_string_lossy(),
            &base,
        ],
    )
    .with_context(|| format!("create worktree for {} (base {base})", task.id))?;
    Ok(wt)
}

/// Merge the task branch back and drop the worktree (Review→Done).
/// Conflicts roll the merge back and leave the task in Review.
pub fn finish(repo: &Path, task: &Task) -> Result<()> {
    let wt = worktree_path(repo, task);
    let branch = format!("ai-commander/{}", sanitize(&task.id));
    if !wt.exists() {
        return Ok(()); // nothing to merge (non-code task)
    }
    // merge the branch into the base of the *main* repo checkout
    git(repo, &["merge", "--no-ff", "--no-edit", &branch]).map_err(|e| {
        // conflict: abort so the tree stays clean
        let _ = git(repo, &["merge", "--abort"]);
        anyhow!("merge conflict for {} — task stays in Review. {e}", task.id)
    })?;
    cleanup(repo, task).context("remove worktree after merge")?;
    Ok(())
}

/// Drop the worktree and branch without merging (Cancel/Rework cleanup).
pub fn drop(repo: &Path, task: &Task) -> Result<()> {
    let wt = worktree_path(repo, task);
    if wt.exists() {
        cleanup(repo, task)?;
    }
    Ok(())
}

fn cleanup(repo: &Path, task: &Task) -> Result<()> {
    let wt = worktree_path(repo, task);
    let branch = format!("ai-commander/{}", sanitize(&task.id));
    let _ = git(
        repo,
        &["worktree", "remove", "--force", &wt.to_string_lossy()],
    );
    let _ = git(repo, &["branch", "-D", &branch]);
    if !wt.exists() {
        Ok(())
    } else {
        bail!("worktree {} still present", wt.display())
    }
}

/// Hook the FSM: returns a human-readable note when isolation applies.
pub fn on_transition(
    repo: &Path,
    task: &Task,
    next: TaskStatus,
    action: TaskAction,
) -> Result<Option<String>> {
    match (task.status, next, action) {
        // Start → isolate (only for tasks that will touch code; heuristics:
        // any task — non-git repos just skip silently)
        (_, TaskStatus::InProgress, TaskAction::Start) => {
            if !is_git_repo(repo) {
                return Ok(None);
            }
            match start(repo, task) {
                Ok(p) => Ok(Some(format!("worktree {}", p.display()))),
                Err(e) => Err(e),
            }
        }
        // Review→Done → merge back
        (TaskStatus::Review, TaskStatus::Done, TaskAction::Approve) => {
            if !is_git_repo(repo) {
                return Ok(None);
            }
            finish(repo, task).map(|_| Some("merged".into()))
        }
        // Cancel with an open worktree → drop it
        (_, TaskStatus::Cancelled, TaskAction::Cancel) => {
            if is_git_repo(repo) && worktree_path(repo, task).exists() {
                drop(repo, task).map(|_| Some("worktree dropped".into()))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}
