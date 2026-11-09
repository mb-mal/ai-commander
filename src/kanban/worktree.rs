//! Git worktree isolation: every code task runs in its own worktree + branch.
//!
//! `start` → `ai-commander/t-0007` branch in a worktree OUTSIDE the user's
//! working tree (v5 #3: `<cache>/ai-commander/worktrees/<repo>/<task>` —
//! `git add -A` in the user's repo must never pick agent worktrees up as
//! embedded repositories; the legacy `<repo>/.aic-worktrees/` location is
//! still resolved for tasks created before the move).
//! `finish` (Review→Done) merges the task branch into the task's BASE
//! branch via a temporary worktree and moves the branch ref (v5 #2) — the
//! user's checkout is never touched. A base checked out with uncommitted
//! changes blocks the merge (task stays in Review for a human). Merge
//! conflicts abort the transition with an error. Cancel drops the worktree.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use super::{Task, TaskAction, TaskStatus};

/// Worktrees root, overridable for tests/portable setups.
fn worktrees_root() -> PathBuf {
    if let Some(p) = std::env::var_os("AI_COMMANDER_WORKTREES_DIR") {
        return PathBuf::from(p);
    }
    dirs::cache_dir()
        .map(|c| c.join("ai-commander").join("worktrees"))
        .unwrap_or_else(|| std::env::temp_dir().join("ai-commander-worktrees"))
}

/// Stable per-repository slug (hash of the canonical path).
fn repo_slug(repo: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let canon = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canon.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn worktrees_dir(repo: &Path) -> PathBuf {
    worktrees_root().join(repo_slug(repo))
}

/// Legacy in-repo location (v4 and earlier).
fn legacy_dir(repo: &Path) -> PathBuf {
    repo.join(".aic-worktrees")
}

pub fn worktree_path(repo: &Path, task: &Task) -> PathBuf {
    let modern = worktrees_dir(repo).join(sanitize(&task.id));
    if modern.exists() {
        return modern;
    }
    let legacy = legacy_dir(repo).join(sanitize(&task.id));
    if legacy.exists() {
        return legacy;
    }
    modern
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

fn repo_is_dirty(repo: &Path) -> bool {
    git(repo, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(true)
}

/// Create the worktree + branch for a task (Start). Returns the worktree
/// path and the base branch it was cut from (v5 #2: the base is recorded
/// on the task so Approve merges into IT, not whatever the user has
/// checked out by then). Idempotent: reuses an existing worktree.
pub fn start(repo: &Path, task: &Task) -> Result<(PathBuf, String)> {
    let wt = worktree_path(repo, task);
    let branch = format!("ai-commander/{}", sanitize(&task.id));
    if wt.exists() {
        // already isolated; recover the base from the branch config when
        // the task does not carry it
        let base = current_branch(repo).unwrap_or_default();
        return Ok((wt, base));
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
    Ok((wt, base))
}

/// Merge the task branch into the task's BASE branch via a temporary
/// worktree and fast-forward the branch ref (v5 #2). The user's checkout
/// is never touched; a base checked out dirty blocks the merge.
/// Commits any uncommitted agent work first (review v2 Б4).
pub fn finish(repo: &Path, task: &Task) -> Result<()> {
    let wt = worktree_path(repo, task);
    let branch = format!("ai-commander/{}", sanitize(&task.id));
    if !wt.exists() {
        return Ok(()); // nothing to merge (non-code task)
    }

    // commit the agent's uncommitted work (review v2 Б4: --force lost it)
    let _ = git(&wt, &["add", "-A"]);
    let author = format!(
        "{} <{}@ai-commander>",
        task.role.as_deref().unwrap_or("agent"),
        sanitize(&task.id)
    );
    let _ = git(
        &wt,
        &[
            "commit",
            "-m",
            &format!("task {}", task.id),
            "--author",
            &author,
        ],
    );

    // v5 #2: the target is the task's base branch (stored at Start), not
    // whatever branch the user happens to have checked out now
    let stored_base = task.base_branch.trim().to_string();
    let base = if stored_base.is_empty() {
        current_branch(repo).context("task base branch unknown")?
    } else {
        stored_base
    };

    // if the user IS on the base branch with uncommitted changes, merging
    // under their feet risks mixing work — a human decides
    let user_on_base = current_branch(repo).map(|b| b == base).unwrap_or(false);
    if user_on_base && repo_is_dirty(repo) {
        bail!(
            "base branch {base} is checked out with uncommitted changes — \
             merge task {} manually or commit first (files preserved in {})",
            task.id,
            wt.display()
        );
    }

    // merge inside a TEMPORARY worktree on the base tip, then move the
    // branch ref — the user's working tree is untouched
    let tmp = worktrees_dir(repo).join(format!("merge-{}", sanitize(&task.id)));
    let _ = std::fs::remove_dir_all(&tmp);
    let tmp_s = tmp.to_string_lossy().into_owned();
    git(repo, &["worktree", "add", "--detach", &tmp_s, &base])
        .context("open a temporary worktree on the base branch")?;
    let merge = git(&tmp, &["merge", "--no-ff", "--no-edit", &branch]);
    if let Err(e) = merge {
        let _ = git(&tmp, &["merge", "--abort"]);
        let _ = git(repo, &["worktree", "remove", "--force", &tmp_s]);
        return Err(anyhow!(
            "merge conflict for {} — task stays in Review. {e}",
            task.id
        ));
    }
    // fast-forward the base branch ref to the merge result
    let new_head = git(&tmp, &["rev-parse", "HEAD"])?.trim().to_string();
    git(
        &tmp,
        &["update-ref", &format!("refs/heads/{base}"), &new_head],
    )
    .with_context(|| format!("move refs/heads/{base} to the merge result"))?;
    let _ = git(repo, &["worktree", "remove", "--force", &tmp_s]);

    // the task worktree itself: refuse losing uncommitted leftovers
    let still_dirty = git(&wt, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(true);
    if still_dirty {
        bail!(
            "worktree for {} still has uncommitted changes — NeedsHuman, files preserved in {}",
            task.id,
            wt.display()
        );
    }
    let _ = git(repo, &["worktree", "remove", &wt.to_string_lossy()]);
    let _ = git(repo, &["branch", "-D", &branch]);
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
    // commit stray changes first so nothing is lost (review v2 Б4)
    let _ = git(&wt, &["add", "-A"]);
    let _ = git(&wt, &["commit", "-m", &format!("cleanup {}", task.id)]);
    // try without --force first; fall back only for Cancel (agent opted out)
    git(repo, &["worktree", "remove", &wt.to_string_lossy()]).or_else(|_| {
        git(
            repo,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
        )
    })?;
    let _ = git(repo, &["branch", "-D", &branch]);
    Ok(())
}

/// v5 #3: drop worktrees of tasks that no longer exist on the board
/// (crash leftovers). Returns the removed worktree ids.
pub fn prune_orphans(repo: &Path, live_task_ids: &[String]) -> Vec<String> {
    let _ = git(repo, &["worktree", "prune"]);
    let live: Vec<String> = live_task_ids.iter().map(|id| sanitize(id)).collect();
    let mut removed = Vec::new();
    let Ok(rd) = std::fs::read_dir(worktrees_dir(repo)) else {
        return removed;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("merge-") || live.contains(&name) {
            continue;
        }
        let p = e.path().to_string_lossy().into_owned();
        if git(repo, &["worktree", "remove", "--force", &p]).is_ok() {
            let _ = git(repo, &["branch", "-D", &format!("ai-commander/{name}")]);
            removed.push(name);
        }
    }
    removed
}

/// Hook the FSM: returns a human-readable note when isolation applies,
/// and the base branch to record on the task for Start (v5 #2).
pub fn on_transition(
    repo: &Path,
    task: &Task,
    next: TaskStatus,
    action: TaskAction,
) -> Result<HookNote> {
    match (task.status, next, action) {
        // Start → isolate (only for tasks that will touch code; heuristics:
        // any task — non-git repos just skip silently)
        (_, TaskStatus::InProgress, TaskAction::Start) => {
            if !is_git_repo(repo) {
                return Ok(HookNote::default());
            }
            let (p, base) = start(repo, task)?;
            Ok(HookNote {
                note: Some(format!("worktree {}", p.display())),
                base_branch: Some(base),
            })
        }
        // Review→Done → merge back into the task's base branch
        (TaskStatus::Review, TaskStatus::Done, TaskAction::Approve) => {
            if !is_git_repo(repo) {
                return Ok(HookNote::default());
            }
            finish(repo, task).map(|_| HookNote {
                note: Some("merged".into()),
                base_branch: None,
            })
        }
        // Cancel with an open worktree → drop it
        (_, TaskStatus::Cancelled, TaskAction::Cancel) => {
            if is_git_repo(repo) && worktree_path(repo, task).exists() {
                drop(repo, task).map(|_| HookNote {
                    note: Some("worktree dropped".into()),
                    base_branch: None,
                })
            } else {
                Ok(HookNote::default())
            }
        }
        _ => Ok(HookNote::default()),
    }
}

#[derive(Debug, Default)]
pub struct HookNote {
    pub note: Option<String>,
    /// Start: the base branch the task worktree was cut from (v5 #2)
    pub base_branch: Option<String>,
}
