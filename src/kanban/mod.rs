//! Kanban board for agent tasks: a finite-state machine per task, a single
//! source-of-truth store with atomic writes and an audit trail, and prompt /
//! reply-classification glue for executing tasks through any harness.
//!
//! v1 storage is an atomic JSON file + capped event log (the SQLite event
//! store from the roadmap needs a C toolchain this build avoids on purpose).
//! The invariants are what matter: one `Arc<Mutex<…>>` owner, every mutation
//! through `transition()`, every change recorded.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Lease duration: renewed on every agent progress event.
/// v4 Д1: tool calls can run silently for minutes (PartUpdated renews
/// the lease, but a single long call emits no updates) — 5 minutes before
/// the watchdog intervenes.
pub const LEASE_MS: i64 = 300_000;

pub fn default_max_attempts() -> u32 {
    3
}

pub fn default_max_turns() -> u32 {
    10
}

/// A turn is considered stuck after this long without a StreamDone.
pub const STUCK_AFTER_MS: i64 = 10 * 60 * 1000;

/// Max delegation chain depth (review §8: ограничить глубину делегирования).
pub const MAX_DELEGATION_DEPTH: usize = 3;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Ready,
    InProgress,
    Review,
    Done,
    Blocked,
    NeedsHuman,
    Cancelled,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl TaskStatus {
    pub const ALL: [TaskStatus; 8] = [
        TaskStatus::Backlog,
        TaskStatus::Ready,
        TaskStatus::InProgress,
        TaskStatus::Review,
        TaskStatus::Done,
        TaskStatus::Blocked,
        TaskStatus::NeedsHuman,
        TaskStatus::Cancelled,
    ];

    pub const COLUMN_ORDER: [TaskStatus; 5] = [
        TaskStatus::Backlog,
        TaskStatus::Ready,
        TaskStatus::InProgress,
        TaskStatus::Review,
        TaskStatus::Done,
    ];

    pub fn is_column(self) -> bool {
        Self::COLUMN_ORDER.contains(&self)
    }

    /// Actions are the only legal way between statuses.
    pub fn next(self, action: TaskAction) -> Option<TaskStatus> {
        use TaskAction::*;
        use TaskStatus::*;
        match (self, action) {
            (Backlog, Promote) | (Blocked, Promote) | (NeedsHuman, Promote) | (Review, Rework) => {
                Some(Ready)
            }
            (Ready, Start) | (Blocked, Start) | (NeedsHuman, Start) => Some(InProgress),
            (Ready, SendBack) => Some(Backlog),
            (InProgress, Finish) => Some(Review),
            // v4: Ready→NeedsHuman — a Start that cannot even begin
            // (broken worktree/branch) escalates instead of retry-looping
            (Ready, Ask) => Some(NeedsHuman),
            (InProgress, Interrupt) => Some(Ready),
            (InProgress, Block) => Some(Blocked),
            (InProgress, Ask) => Some(NeedsHuman),
            (Review, Approve) => Some(Done),
            (Done, Rework) => Some(Ready),
            (Backlog | Ready | InProgress | Review | Blocked | NeedsHuman, Cancel) => {
                Some(Cancelled)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    Promote,
    Start,
    Finish,
    Interrupt,
    Rework,
    Approve,
    SendBack,
    Cancel,
    /// the agent reported BLOCKED
    Block,
    /// the agent asked for a human decision
    Ask,
}

/// A thread entry on a task: agent/human comment, handoff contract or a
/// system note. This is the board's communication channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Comment {
    pub author: String,
    /// "comment" | "handoff" | "system"
    pub kind: String,
    pub text: String,
    pub ts_ms: i64,
}

/// Д8: a concrete output of a task turn — files written, the worktree branch,
/// a report link. Handoffs and DONE turns attach these so the next owner can
/// find the work without scraping the chat log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    /// "file" | "branch" | "link" | "text"
    pub kind: String,
    /// path / branch name / URL / short value
    pub value: String,
    /// role id that produced it
    pub by: String,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance: Vec<String>,
    pub status: TaskStatus,
    /// harness id the task runs on
    #[serde(default)]
    pub harness: Option<String>,
    /// role id (.ai-commander/agents/<id>.md) whose instructions prefix the prompt
    #[serde(default)]
    pub role: Option<String>,
    /// task thread: comments, handoffs, system notes (agent inbox)
    #[serde(default)]
    pub comments: Vec<Comment>,
    /// Д8: concrete outputs (files/branches/links) produced by task turns
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    /// v5 #2: the branch the task worktree was cut from — Approve merges
    /// into IT (via a temporary worktree), not the user's current checkout
    #[serde(default)]
    pub base_branch: String,
    /// v6: where the task's worktree actually lives (survives hash scheme
    /// and cache-location changes)
    #[serde(default)]
    pub worktree_path: String,
    /// DAG: task ids that must be Done/Cancelled before this one launches
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// linked chat session (native id; pending-* until the first turn)
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    /// wall-clock of the current Start (for the stuck-task watchdog)
    #[serde(default)]
    pub started_ms: Option<i64>,
    /// lease expiry: renewed by agent progress events (heartbeat); the
    /// watchdog only intervenes when this has passed (review v2 Б6)
    #[serde(default)]
    pub lease_until_ms: Option<i64>,
    /// Н1: don't launch this task before this timestamp (exponential backoff)
    #[serde(default)]
    pub not_before_ms: Option<i64>,
    /// max attempts before the task escalates to NeedsHuman
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// cron schedule (5-field) for recurring tasks; empty = one-off
    #[serde(default)]
    pub schedule: String,
    /// cost budget in USD for this task's subtree; 0 = unlimited
    #[serde(default)]
    pub budget_usd: f64,
    /// approximate cost of one attempt (from the role); attempts × this is
    /// checked against budget_usd at Start
    #[serde(default)]
    pub unit_cost_usd: f64,
    /// wall-clock budget for one turn in seconds; 0 = unlimited (watchdog
    /// escalates instead of waiting the full stuck window)
    #[serde(default)]
    pub run_budget_secs: i64,
    /// parent task (delegation chain; depth capped by MAX_DELEGATION_DEPTH)
    #[serde(default)]
    pub parent_id: Option<String>,
    /// max turns in the task thread (limiter); 0 = unlimited
    #[serde(default)]
    pub max_turns: u32,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Kanban {
    pub tasks: Vec<Task>,
    pub next_seq: u64,
}

impl Kanban {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("AI_COMMANDER_KANBAN_DB") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        crate::config::config_dir().join("kanban.redb")
    }

    pub fn by_status(&self, status: TaskStatus) -> Vec<Task> {
        self.tasks
            .iter()
            .filter(|t| t.status == status)
            .cloned()
            .collect()
    }

    /// Legacy atomic-JSON location, imported once on first redb open.
    pub fn legacy_path() -> PathBuf {
        crate::config::config_dir().join("kanban.json")
    }
}

pub mod ipc;
pub mod otel;
pub mod roles;
pub mod runner;
pub mod store;
pub mod worktree;
pub use store::{BoardStore, EventStore, RedbStore, SCHEMA_VERSION, StoredEvent};

/// The instruction sent to the agent when a task starts.
pub fn task_prompt(task: &Task) -> String {
    let mut p =
        String::from("You are working on a kanban task in a project repository.\n\n# Task\n");
    p.push_str(&task.title);
    p.push('\n');
    if !task.description.trim().is_empty() {
        p.push('\n');
        p.push_str(&task.description);
        p.push('\n');
    }
    if !task.acceptance.is_empty() {
        p.push_str("\n# Acceptance criteria\n");
        for a in &task.acceptance {
            p.push_str("- ");
            p.push_str(a);
            p.push('\n');
        }
    }
    p.push_str(
        "\nWork on it now, autonomously. When finished, reply with a short summary whose first line\n\
         starts with `DONE:`. If you cannot proceed, start the reply with `BLOCKED:` and explain.\n\
         If a human decision is required, start it with `ASK:` and state the question.",
    );
    p
}

/// What to do with the task when the agent's turn ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Finished,   // → Review
    Blocked,    // → Blocked
    NeedsHuman, // → NeedsHuman
    Failed,     // → retry (empty/short reply, review v2 Б5)
}

/// Classify an agent reply. An empty or trivially-short reply is a failure
/// (review v2 Б5: "done" was inferred from any text, including errors and
/// timeouts). `DONE:` prefix → Finished; `BLOCKED:` → Blocked; `ASK:` →
/// NeedsHuman; anything else ≥ 20 chars is treated as a finished summary.
pub fn classify_reply(reply: &str) -> TurnOutcome {
    let trimmed = reply.trim();
    if trimmed.len() < 20 {
        return TurnOutcome::Failed;
    }
    let head: String = trimmed.chars().take(16).collect::<String>().to_uppercase();
    if head.starts_with("BLOCKED") {
        TurnOutcome::Blocked
    } else if head.starts_with("ASK") {
        TurnOutcome::NeedsHuman
    } else if head.starts_with("DONE") {
        TurnOutcome::Finished
    } else {
        // Н2: no explicit DONE: marker → not a completion (error text,
        // timeouts, partial replies all land here)
        TurnOutcome::Failed
    }
}

/// Execute a board.* tool call (shared by the board-mcp binary and the IPC
/// gateway's /board proxy endpoint). Returns human-readable text or an error.
/// v6 #3: who is acting on the board. `Human` is the local user (the TUI
/// keyboard or the IPC human token); `Role` is an agent. Agents may never
/// un-park NeedsHuman tasks — that was a self-approval hole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Role(String),
    Human,
}

impl Actor {
    pub fn as_str(&self) -> &str {
        match self {
            Actor::Role(r) => r,
            Actor::Human => "human",
        }
    }
}

pub fn board_tool_call(
    name: &str,
    args: &serde_json::Value,
    actor: &Actor,
    board: &mut store::BoardStore,
) -> Result<String, String> {
    use serde_json::Value;

    let role = actor.as_str();
    if role.is_empty() {
        return Err("no role token: set AI_COMMANDER_ROLE for this MCP server".into());
    }
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match name {
        "board_create_task" => {
            let title = str_arg("title");
            if title.trim().is_empty() {
                return Err("title is required".into());
            }
            let t = match args.get("parent_id").and_then(Value::as_str) {
                Some(parent) if !parent.is_empty() => board.add_subtask(parent, &title)?,
                _ => board.add_task(&title),
            };
            if let Some(deps) = args.get("depends_on").and_then(Value::as_array) {
                let ids: Vec<String> = deps
                    .iter()
                    .filter_map(|d| d.as_str().map(str::to_owned))
                    .collect();
                board.set_depends(&t.id, ids)?;
            }
            Ok(format!("created {} '{}' in Backlog", t.id, t.title))
        }
        "board_update_task" => {
            let id = str_arg("task_id");
            // v6 #3: leaving NeedsHuman is a HUMAN decision — an agent
            // promoting the task used to grant itself the parked permit
            if str_arg("action") == "promote" && !matches!(actor, Actor::Human) {
                if let Some(t) = board.projection().tasks.iter().find(|t| t.id == id) {
                    if t.status == TaskStatus::NeedsHuman {
                        return Err(
                            "NeedsHuman tasks are un-parked by a human —                              answer in ai-commander, not from the agent side"
                                .into(),
                        );
                    }
                }
            }
            let action = match str_arg("action").as_str() {
                "promote" => TaskAction::Promote,
                "start" => TaskAction::Start,
                "finish" => TaskAction::Finish,
                "approve" => TaskAction::Approve,
                "rework" => TaskAction::Rework,
                "cancel" => TaskAction::Cancel,
                other => return Err(format!("unknown action {other}")),
            };
            let t = board
                .projection()
                .tasks
                .iter()
                .find(|t| t.id == id)
                .cloned()
                .ok_or_else(|| format!("task {id} not found"))?;
            if matches!(action, TaskAction::Start | TaskAction::Finish)
                && t.role.as_deref().is_some_and(|r| r != role)
            {
                return Err(format!("task is assigned to {:?}, not to you", t.role));
            }
            if matches!(action, TaskAction::Approve | TaskAction::Rework)
                && t.role.as_deref() == Some(role)
            {
                return Err(
                    "you cannot approve your own work — a different role must review".into(),
                );
            }
            // v8 #3: transition_in so Approve merges the worktree back
            // and Cancel drops it — but ONLY for tasks that HAVE a worktree
            // (base_branch or worktree_path set); memory-store tasks
            // (tests, IPC without a git repo) use plain transition
            let has_worktree =
                board.projection().tasks.iter().any(|t| {
                    t.id == id && (!t.base_branch.is_empty() || !t.worktree_path.is_empty())
                });
            let t = if has_worktree {
                let repo = std::env::current_dir().unwrap_or_default();
                board.transition_in(&repo, &id, action)?
            } else {
                board.transition(&id, action)?
            };
            Ok(format!("{} → {:?}", t.id, t.status))
        }
        "board_comment" => {
            let id = str_arg("task_id");
            let text = str_arg("text");
            if text.trim().is_empty() {
                return Err("text is required".into());
            }
            board.comment(&id, role, "comment", &text)?;
            Ok("comment posted".into())
        }
        "board_handoff" => {
            let t = board.handoff(
                &str_arg("task_id"),
                role,
                &str_arg("to_role"),
                &str_arg("summary"),
                &str_arg("open_questions"),
            )?;
            Ok(format!("handed off to {:?}; status {:?}", t.role, t.status))
        }
        "board_artifact" => {
            let t = board.add_artifact(
                &str_arg("task_id"),
                &str_arg("kind"),
                &str_arg("value"),
                role,
            )?;
            Ok(format!(
                "artifact attached to {} ({} total)",
                t.id,
                t.artifacts.len()
            ))
        }
        "board_unpark" => {
            // v6 #3: the HUMAN answers a parked ask: NeedsHuman → Ready with
            // a journal record — the daemon grants the parked permit on it
            if !matches!(actor, Actor::Human) {
                return Err("board_unpark is human-only".into());
            }
            let id = str_arg("task_id");
            let t = board.transition(&id, TaskAction::Promote)?;
            board.comment(
                &id,
                "human",
                "permit",
                "human un-parked the task — the parked permission is granted once",
            )?;
            Ok(format!("{} → {:?} (permit granted)", t.id, t.status))
        }
        "board_read_inbox" => {
            let id = str_arg("task_id");
            let since = args.get("since_ms").and_then(Value::as_i64).unwrap_or(0);
            let inbox = board
                .read_inbox(&id, since)
                .ok_or_else(|| format!("task {id} not found"))?;
            if inbox.is_empty() {
                return Ok("inbox is empty".into());
            }
            Ok(inbox
                .iter()
                .map(|c| format!("[{}] {}: {}", c.kind, c.author, c.text.replace('\n', " ")))
                .collect::<Vec<_>>()
                .join(
                    "
",
                ))
        }
        "board_ask_human" => {
            let id = str_arg("task_id");
            board.comment(&id, role, "system", &str_arg("text"))?;
            board.transition(&id, TaskAction::Ask)?;
            Ok("escalated to NeedsHuman".into())
        }
        other => Err(format!("unknown tool {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::store::BoardStore;

    fn mk(status: TaskStatus) -> Task {
        Task {
            id: "t-0001".into(),
            title: "demo".into(),
            description: String::new(),
            acceptance: vec!["tests pass".into()],
            status,
            harness: Some("oc".into()),
            role: None,
            comments: Vec::new(),
            artifacts: Vec::new(),
            base_branch: String::new(),
            worktree_path: String::new(),
            depends_on: Vec::new(),
            session_id: None,
            attempts: 0,
            started_ms: None,
            lease_until_ms: None,
            not_before_ms: None,
            max_attempts: default_max_attempts(),
            schedule: String::new(),
            budget_usd: 0.0,
            unit_cost_usd: 0.0,
            run_budget_secs: 0,
            parent_id: None,
            max_turns: 0,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    #[test]
    fn happy_path_lifecycle() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("write docs");
        assert_eq!(t.status, TaskStatus::Backlog);
        for a in [
            TaskAction::Promote,
            TaskAction::Start,
            TaskAction::Finish,
            TaskAction::Approve,
        ] {
            b.transition(&t.id, a).unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(b.projection().tasks[0].status, TaskStatus::Done);
    }

    #[test]
    fn illegal_transitions_rejected() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("x");
        assert!(
            b.transition(&t.id, TaskAction::Approve).is_err(),
            "Backlog -> Done illegal"
        );
        assert!(
            b.transition("t-9999", TaskAction::Promote).is_err(),
            "unknown task"
        );
    }

    #[test]
    fn attempts_count_starts() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("x");
        b.transition(&t.id, TaskAction::Promote).unwrap();
        b.transition(&t.id, TaskAction::Start).unwrap();
        b.transition(&t.id, TaskAction::Interrupt).unwrap();
        b.transition(&t.id, TaskAction::Start).unwrap();
        assert_eq!(b.projection().tasks[0].attempts, 2);
        assert_eq!(b.projection().tasks[0].status, TaskStatus::InProgress);
    }

    #[test]
    fn classify() {
        assert_eq!(
            classify_reply("DONE: ready to ship the feature"),
            TurnOutcome::Finished
        );
        assert_eq!(
            classify_reply("  blocked: no api key available"),
            TurnOutcome::Blocked
        );
        assert_eq!(
            classify_reply("ASK: which model should I use?"),
            TurnOutcome::NeedsHuman
        );
        assert_eq!(
            classify_reply("всё сделано, тесты прошли успешно"),
            TurnOutcome::Failed,
            "no DONE: marker → not a completion (Н2)"
        );
        // review v2 Б5: short/empty replies are failures
        assert_eq!(classify_reply(""), TurnOutcome::Failed);
        assert_eq!(classify_reply("ok"), TurnOutcome::Failed);
    }

    #[test]
    fn prompt_contains_task_and_criteria() {
        let p = task_prompt(&mk(TaskStatus::Ready));
        assert!(p.contains("demo"));
        assert!(p.contains("tests pass"));
        assert!(p.contains("DONE:"));
    }
}
