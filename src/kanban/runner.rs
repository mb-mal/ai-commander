//! TaskRunner: the single execution path for board tasks, used by both the
//! TUI and the daemon (review v2 Б8+Б9). Builds the launch context from
//! the task + role (instructions, model, MCP), tracks progress with
//! heartbeats, and finishes the task on StreamDone via classify_reply.

use std::sync::Arc;

use crate::backend::{Backend, SendOpts};
use crate::config::Config;
use crate::kanban::store::BoardStore;
use crate::kanban::{Task, TaskAction, TurnOutcome, classify_reply, roles::RoleSet};

pub struct LaunchCtx {
    pub prompt: String,
    pub model: Option<crate::config::ModelRef>,
    pub agent: Option<String>,
    pub mcp_role: Option<String>,
    /// Н3: the worktree path the agent should work in (set by launch_task)
    pub cwd: Option<std::path::PathBuf>,
}

/// Build the launch context for a task: role instructions prefix the prompt,
/// role model overrides the harness default, mcp_role enables the board MCP.
pub fn launch_ctx(task: &Task, cfg: &Config, roles: &RoleSet) -> LaunchCtx {
    let mut prompt = crate::kanban::task_prompt(task);
    if let Some(role_id) = &task.role {
        if let Some(prefix) = roles.role_prompt(role_id) {
            prompt = format!("{prefix}\n{prompt}");
        }
    }
    let model = task
        .role
        .as_ref()
        .and_then(|r| roles.roles.get(r))
        .and_then(|r| r.model.clone())
        .and_then(|m| {
            m.split_once('/').map(|(p, m)| crate::config::ModelRef {
                provider_id: p.to_string(),
                model_id: m.to_string(),
            })
        })
        .or_else(|| {
            task.harness
                .as_ref()
                .and_then(|h| cfg.harness(h))
                .and_then(|h| h.default_model.clone())
        });
    let agent = task
        .harness
        .as_ref()
        .and_then(|h| cfg.harness(h))
        .map(|h| h.agent.clone())
        .filter(|a| !a.is_empty());
    LaunchCtx {
        prompt,
        model,
        agent,
        mcp_role: task.role.clone(),
        cwd: None, // set by launch_task after the worktree is created
    }
}

/// Н3 sync phase: Start (which creates the worktree) and build the launch
/// context with `cwd` pointing at the task's worktree. Split out of
/// `launch_task` so the TUI can run it under its board lock, then do the
/// async session/send without holding the lock across `.await`.
pub fn prepare_launch(
    board: &mut BoardStore,
    task: &Task,
    cfg: &Config,
    roles: &RoleSet,
    repo: &std::path::Path,
) -> anyhow::Result<LaunchCtx> {
    // 1. Start first — this creates the worktree (Н3: agent must work there)
    board
        .transition_in(repo, &task.id, TaskAction::Start)
        .map_err(|e| anyhow::anyhow!("Start failed: {e}"))?;

    // 2. Find the worktree path (created by transition_in's Start hook)
    let worktree = crate::kanban::worktree::worktree_path(repo, task);
    let cwd = if worktree.exists() {
        worktree
    } else {
        // not a git repo or worktree failed — use the repo dir
        repo.to_path_buf()
    };

    // 3. Build the launch context with the worktree cwd
    let mut ctx = launch_ctx(task, cfg, roles);
    ctx.cwd = Some(cwd);
    Ok(ctx)
}

/// Н3: launch a task in its worktree — Start (creates worktree) →
/// create_session(cwd=worktree) → send. Returns the linked session id.
/// Takes the shared board (Б1: the owner serves it over IPC); the lock is
/// taken per sync segment only, never held across an `.await`.
pub async fn launch_task(
    backend: &Arc<dyn Backend>,
    board: &std::sync::Arc<parking_lot::Mutex<BoardStore>>,
    task: &Task,
    cfg: &Config,
    roles: &RoleSet,
) -> anyhow::Result<String> {
    let repo = std::env::current_dir().unwrap_or_default();
    let ctx = {
        let mut b = board.lock();
        prepare_launch(&mut b, task, cfg, roles, &repo)?
    };

    // Create the session (backends use ctx.cwd as the working dir)
    let session = backend
        .create_session(&task.title)
        .await
        .map_err(|e| anyhow::anyhow!("create_session: {e:#}"))?;
    board.lock().link_session(&task.id, &session.native_id);

    // Send with the worktree cwd
    send_task(backend, task, &ctx, &session.native_id).await?;
    Ok(session.native_id)
}

/// Handle a PartDelta for a task: renew the lease (heartbeat Б6) and
/// accumulate the reply for classification.
pub fn on_progress(board: &mut BoardStore, task_id: &str, delta: &str, reply: &mut String) {
    reply.push_str(delta);
    board.renew_lease(task_id);
}

/// Lease-only heartbeat (for callers that track replies separately, e.g. TUI).
pub fn on_progress_no_reply(board: &mut BoardStore, task_id: &str, delta: &str) {
    if !delta.is_empty() {
        board.renew_lease(task_id);
    }
}

/// v4 Д1: any liveness signal renews the lease — including `PartUpdated`
/// (tool-call status changes, ACP tool calls), which carries no delta text.
pub fn on_activity(board: &mut BoardStore, task_id: &str) {
    board.renew_lease(task_id);
}

/// v4: Start itself failed (worktree/branch broken, task still Ready) —
/// infrastructure is broken, escalate to a human instead of retrying the
/// same failing Start every 30 seconds forever. The reason lands in the
/// task thread so the human sees WHY (v5).
pub fn launch_failed(board: &mut BoardStore, task_id: &str, reason: &str) {
    let _ = board.transition(task_id, TaskAction::Ask);
    let _ = board.comment(
        task_id,
        "system",
        "system",
        &format!("start failed: {reason}"),
    );
}

/// v5: the policy engine said ASK while no human is watching (daemon).
/// The task goes to NeedsHuman with the question in its thread instead of
/// hanging on the lease until the watchdog fires; the agent's request is
/// answered with a reject so the turn ends deterministically.
pub fn on_permission_ask(
    board: &mut BoardStore,
    task_id: &str,
    question: &str,
) -> Result<Task, String> {
    board.comment(task_id, "agent", "ask", question)?;
    board.transition(task_id, TaskAction::Ask)
}

/// Classify the accumulated reply and transition the task (Б8: StreamDone
/// must finish the task, not just clear a busy flag).
/// Н1: Failed retries are capped by max_attempts and delayed exponentially.
pub fn on_stream_done(board: &mut BoardStore, task_id: &str, reply: &str) -> Result<Task, String> {
    on_stream_done_ex(board, task_id, reply, false)
}

/// v4 Н2: `errored` marks a turn whose stream died mid-reply — the
/// `DONE:` marker printed before the break must NOT count; the turn is
/// Failed regardless of the accumulated text.
pub fn on_stream_done_ex(
    board: &mut BoardStore,
    task_id: &str,
    reply: &str,
    errored: bool,
) -> Result<Task, String> {
    let outcome = if errored {
        TurnOutcome::Failed
    } else {
        classify_reply(reply)
    };
    let action = match outcome {
        TurnOutcome::Finished => TaskAction::Finish,
        TurnOutcome::Blocked => TaskAction::Block,
        TurnOutcome::NeedsHuman => TaskAction::Ask,
        TurnOutcome::Failed => {
            // Н1: cap retries — after max_attempts the task escalates
            let t = board
                .projection()
                .tasks
                .iter()
                .find(|t| t.id == task_id)
                .cloned();
            if let Some(t) = t {
                if t.attempts >= t.max_attempts {
                    TaskAction::Ask // escalate: exhausted retries
                } else {
                    // Н1: exponential backoff before the next launch
                    let delay_secs = backoff_secs(t.attempts);
                    let not_before = crate::kanban::now_ms() + delay_secs * 1000;
                    board.set_not_before(task_id, not_before);
                    TaskAction::Interrupt
                }
            } else {
                TaskAction::Interrupt
            }
        }
    };
    board.transition(task_id, action)
}

/// Exponential backoff: 30s, 2min, 8min… capped at 15min.
fn backoff_secs(attempts: u32) -> i64 {
    match attempts {
        0..=1 => 30,
        2 => 120,
        3 => 480,
        _ => 900,
    }
}

/// Д2: abort every stuck session and WAIT for all aborts to settle before
/// the caller applies transitions. Fire-and-forget aborts raced the apply:
/// the board re-launched a task while its agent process was still alive.
pub async fn abort_stuck(
    backends: &std::collections::HashMap<String, Arc<dyn Backend>>,
    stuck: &[crate::kanban::store::StuckTask],
) {
    let mut handles = Vec::new();
    for st in stuck {
        if let Some(b) = backends.get(&st.harness).cloned() {
            let sid = st.session_id.clone();
            handles.push(tokio::spawn(async move {
                // bounded: a wedged abort must not stall the watchdog
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(10), b.abort(&sid)).await;
            }));
        }
    }
    for h in handles {
        let _ = h.await;
    }
}

/// Send a task to its harness backend (shared by TUI and daemon).
pub async fn send_task(
    backend: &Arc<dyn Backend>,
    _task: &Task,
    ctx: &LaunchCtx,
    session_id: &str,
) -> anyhow::Result<()> {
    backend
        .send(
            session_id,
            &ctx.prompt,
            &[],
            SendOpts {
                model: ctx.model.as_ref(),
                agent: ctx.agent.as_deref(),
                mcp_role: ctx.mcp_role.as_deref(),
                cwd: ctx.cwd.as_deref(),
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::TaskAction;
    use crate::kanban::store::BoardStore;

    #[test]
    fn launch_ctx_includes_role_instructions_and_model() {
        let mut roles = RoleSet::default();
        roles.roles.insert(
            "coder".into(),
            crate::kanban::roles::Role {
                name: "Coder".into(),
                harness: "oc".into(),
                model: Some("deepseek/deepseek-flash".into()),
                autonomy: None,
                instructions: "You write clean code. Run tests before DONE.".into(),
            },
        );
        let cfg = crate::config::Config::default();

        let mut b = BoardStore::open_memory();
        let t = b.add_task("implement feature X");
        b.assign_with_role(&t.id, Some("oc".into()), Some("coder".into()))
            .unwrap();

        let task = b.projection().tasks.last().unwrap().clone();
        let ctx = launch_ctx(&task, &cfg, &roles);
        assert!(
            ctx.prompt.contains("You write clean code"),
            "role instructions in prompt"
        );
        assert!(
            ctx.prompt.contains("implement feature X"),
            "task title in prompt"
        );
        assert_eq!(
            ctx.model.as_ref().map(|m| m.display()),
            Some("deepseek/deepseek-flash".to_string()),
            "role model overrides harness default"
        );
        assert_eq!(
            ctx.mcp_role.as_deref(),
            Some("coder"),
            "mcp_role for board MCP injection"
        );
    }

    #[test]
    fn on_stream_done_transitions_correctly() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("x");
        b.transition(&t.id, TaskAction::Promote).unwrap();
        b.transition(&t.id, TaskAction::Start).unwrap();

        // done reply → Finish → Review
        let task =
            on_stream_done(&mut b, &t.id, "DONE: implemented and tested the feature").unwrap();
        assert_eq!(task.status, crate::kanban::TaskStatus::Review);

        // empty reply → Failed → Interrupt → Ready
        let t2 = b.add_task("y");
        b.transition(&t2.id, TaskAction::Promote).unwrap();
        b.transition(&t2.id, TaskAction::Start).unwrap();
        let task = on_stream_done(&mut b, &t2.id, "").unwrap();
        assert_eq!(task.status, crate::kanban::TaskStatus::Ready);
    }

    #[test]
    fn on_progress_renews_lease() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("active");
        b.transition(&t.id, TaskAction::Promote).unwrap();
        b.transition(&t.id, TaskAction::Start).unwrap();
        let before = b.projection().tasks[0].lease_until_ms.unwrap();

        let mut reply = String::new();
        on_progress(&mut b, &t.id, "writing code…", &mut reply);

        let after = b.projection().tasks[0].lease_until_ms.unwrap();
        assert!(after >= before, "lease must not shrink");
        assert!(reply.contains("writing code…"));
    }

    /// Д2: a slow abort — completion is recorded only after a delay.
    struct SlowAbortBackend {
        aborted: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl crate::backend::Backend for SlowAbortBackend {
        fn harness_id(&self) -> &str {
            "slow"
        }
        async fn connect(&self) -> anyhow::Result<String> {
            Ok("slow".into())
        }
        async fn refresh_sessions(&self) -> anyhow::Result<Vec<crate::backend::SessionInfo>> {
            Ok(vec![])
        }
        async fn create_session(&self, _: &str) -> anyhow::Result<crate::backend::SessionInfo> {
            Ok(crate::backend::SessionInfo {
                native_id: "s".into(),
                title: "t".into(),
                updated_ms: 0,
            })
        }
        async fn rename_session(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete_session(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn load_history(&self, _: &str) -> anyhow::Result<Vec<crate::backend::ChatMsg>> {
            Ok(vec![])
        }
        async fn send(
            &self,
            _: &str,
            _: &str,
            _: &[crate::backend::AttachmentMeta],
            _: crate::backend::SendOpts<'_>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn abort(&self, _: &str) -> anyhow::Result<()> {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            self.aborted
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn list_models(&self) -> anyhow::Result<Vec<crate::backend::ModelInfo>> {
            Ok(vec![])
        }
        async fn list_agents(&self) -> anyhow::Result<Vec<crate::backend::AgentInfo>> {
            Ok(vec![])
        }
        fn shutdown(&self) {}
        fn is_connected(&self) -> bool {
            true
        }
    }

    /// Д2: abort_stuck must WAIT for every abort to finish — a
    /// fire-and-forget spawn would return before the abort completed.
    #[tokio::test]
    async fn abort_stuck_waits_for_aborts_to_settle() {
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let b: Arc<dyn crate::backend::Backend> = Arc::new(SlowAbortBackend {
            aborted: aborted.clone(),
        });
        let mut backends = std::collections::HashMap::new();
        backends.insert("slow".to_string(), b);

        let stuck = vec![crate::kanban::store::StuckTask {
            task_id: "t-0001".into(),
            session_id: "ses_slow".into(),
            harness: "slow".into(),
            disposition: "retry",
        }];

        abort_stuck(&backends, &stuck).await;

        assert!(
            aborted.load(std::sync::atomic::Ordering::SeqCst),
            "abort_stuck returned before the abort finished (Д2)"
        );
    }
}
