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

pub fn default_max_attempts() -> u32 {
    3
}

/// A turn is considered stuck after this long without a StreamDone.
pub const STUCK_AFTER_MS: i64 = 10 * 60 * 1000;

/// Max delegation chain depth (review §8: ограничить глубину делегирования).
pub const MAX_DELEGATION_DEPTH: usize = 3;

fn now_ms() -> i64 {
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

pub mod a2a;
pub mod otel;
pub mod roles;
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
}

pub fn classify_reply(reply: &str) -> TurnOutcome {
    let head: String = reply
        .trim()
        .chars()
        .take(16)
        .collect::<String>()
        .to_uppercase();
    if head.starts_with("BLOCKED") {
        TurnOutcome::Blocked
    } else if head.starts_with("ASK") {
        TurnOutcome::NeedsHuman
    } else {
        TurnOutcome::Finished
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
            depends_on: Vec::new(),
            session_id: None,
            attempts: 0,
            started_ms: None,
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
        assert_eq!(classify_reply("DONE: ready"), TurnOutcome::Finished);
        assert_eq!(
            classify_reply("  blocked: no api key"),
            TurnOutcome::Blocked
        );
        assert_eq!(classify_reply("ASK: which model?"), TurnOutcome::NeedsHuman);
        assert_eq!(classify_reply("всё сделано"), TurnOutcome::Finished);
    }

    #[test]
    fn prompt_contains_task_and_criteria() {
        let p = task_prompt(&mk(TaskStatus::Ready));
        assert!(p.contains("demo"));
        assert!(p.contains("tests pass"));
        assert!(p.contains("DONE:"));
    }
}
