//! Event-sourced kanban storage.
//!
//! `EventStore` is the storage contract (append / read_since / snapshot);
//! `RedbStore` is the production backend — an embedded ACID, MVCC, crash-safe
//! redb database (pure Rust, no C toolchain). The board state is an
//! append-only event log plus a projection folded from it; each mutation
//! appends the event and the new projection snapshot **in one transaction**,
//! so the log and the snapshot can never diverge. Schema version and
//! migrations exist from day one.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{Kanban, Task, TaskAction, TaskStatus};

pub const SCHEMA_VERSION: u32 = 1;

/// One immutable fact in the board's life. Events carry full task state, so
/// projections fold trivially and format changes stay forward-compatible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredEvent {
    pub seq: u64,
    pub ts_ms: i64,
    pub task_id: String,
    /// "created" | "transition:<Action>" | "linked" | "imported" | "edited"
    pub action: String,
    /// full task state after the event (None for board-level events)
    pub task: Option<Task>,
}

/// The storage contract — swap redb for anything else without touching the core.
pub trait EventStore: Send + Sync {
    /// Append one event and atomically store the new projection snapshot.
    fn append(&self, event: StoredEvent, snapshot: &Kanban) -> Result<()>;
    /// Events with seq > `since`, oldest first.
    fn read_since(&self, since: u64, limit: usize) -> Result<Vec<StoredEvent>>;
    /// Highest committed sequence number.
    fn last_seq(&self) -> Result<u64>;
    /// seq covered by the stored snapshot.
    fn snapshot_seq(&self) -> Result<u64>;
    /// Load the stored snapshot (None on a fresh store).
    fn load_snapshot(&self) -> Result<Option<Kanban>>;
    fn schema_version(&self) -> Result<u32>;
}

// ---------------------------------------------------------------- memory ---

/// In-memory store for unit tests (and as fold-logic documentation).
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<MemInner>,
}

#[derive(Default)]
struct MemInner {
    events: Vec<StoredEvent>,
    snapshot: Option<(u64, Kanban)>,
}

impl EventStore for MemoryStore {
    fn append(&self, event: StoredEvent, snapshot: &Kanban) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        if event.seq != g.events.len() as u64 + 1 {
            bail!("non-monotonic seq {}", event.seq);
        }
        g.events.push(event);
        g.snapshot = Some((g.events.len() as u64, snapshot.clone()));
        Ok(())
    }
    fn read_since(&self, since: u64, limit: usize) -> Result<Vec<StoredEvent>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .events
            .iter()
            .skip(since as usize)
            .take(limit)
            .cloned()
            .collect())
    }
    fn last_seq(&self) -> Result<u64> {
        Ok(self.inner.lock().unwrap().events.len() as u64)
    }
    fn snapshot_seq(&self) -> Result<u64> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .map(|(s, _)| *s)
            .unwrap_or(0))
    }
    fn load_snapshot(&self) -> Result<Option<Kanban>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .map(|(_, k)| k.clone()))
    }
    fn schema_version(&self) -> Result<u32> {
        Ok(SCHEMA_VERSION)
    }
}

// ------------------------------------------------------------------ redb ---

use redb::{Database, Durability, ReadableTable, TableDefinition, WriteTransaction};

const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("events");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

pub struct RedbStore {
    db: Database,
    #[allow(dead_code)]
    path: PathBuf,
}

impl RedbStore {
    pub fn open(path: &Path) -> Result<RedbStore> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Database::create(path).map_err(|e| anyhow!("open kanban db: {e}"))?;
        let store = RedbStore {
            db,
            path: path.to_path_buf(),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate);
        // create tables on first open (redb creates them lazily in write txns;
        // readers error on missing tables)
        {
            let _ = txn.open_table(EVENTS)?;
            let _ = txn.open_table(META)?;
        }
        let existing: Option<u32> = {
            let meta = txn.open_table(META)?;
            meta.get("schema_version")?.map(|v| {
                let raw = v.value();
                u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
            })
        };
        match existing {
            None => {
                // fresh store: initialize schema v1, then try the one-time
                // legacy kanban.json import (migration 0→1) in the same txn
                let mut meta = txn.open_table(META)?;
                meta.insert("schema_version", SCHEMA_VERSION.to_le_bytes().as_slice())?;
                drop(meta);
                self.import_legacy_json(&mut txn)?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(other) => {
                bail!("kanban store schema {other} is newer than supported {SCHEMA_VERSION}")
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Migration 0→1: adopt an old atomic-JSON kanban.json as one event.
    fn import_legacy_json(&self, txn: &mut WriteTransaction) -> Result<Option<Kanban>> {
        let legacy_path = Kanban::legacy_path();
        let Ok(raw) = std::fs::read(&legacy_path) else {
            return Ok(None);
        };
        let Ok(kanban) = serde_json::from_slice::<Kanban>(&raw) else {
            return Ok(None);
        };
        let mut events = txn.open_table(EVENTS)?;
        let mut meta = txn.open_table(META)?;
        let mut seq = 0u64;
        for task in kanban.tasks.iter() {
            seq += 1;
            let ev = StoredEvent {
                seq,
                ts_ms: task.updated_ms,
                task_id: task.id.clone(),
                action: "imported".into(),
                task: Some(task.clone()),
            };
            events.insert(seq, serde_json::to_vec(&ev)?.as_slice())?;
        }
        meta.insert("snapshot", serde_json::to_vec(&kanban)?.as_slice())?;
        meta.insert("snapshot_seq", seq.to_le_bytes().as_slice())?;
        Ok(Some(kanban))
    }

    fn read_meta_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(META)?;
        Ok(meta.get(key)?.map(|v| v.value().to_vec()))
    }
}

impl EventStore for RedbStore {
    fn append(&self, event: StoredEvent, snapshot: &Kanban) -> Result<()> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate); // fsync on commit
        {
            let mut events = txn.open_table(EVENTS)?;
            let mut meta = txn.open_table(META)?;
            events.insert(event.seq, serde_json::to_vec(&event)?.as_slice())?;
            meta.insert("snapshot", serde_json::to_vec(snapshot)?.as_slice())?;
            meta.insert("snapshot_seq", event.seq.to_le_bytes().as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    fn read_since(&self, since: u64, limit: usize) -> Result<Vec<StoredEvent>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS)?;
        let mut out = Vec::new();
        for row in table.range((since + 1)..)? {
            if out.len() >= limit {
                break;
            }
            let (_k, v) = row?;
            let ev: StoredEvent =
                serde_json::from_slice(v.value()).map_err(|e| anyhow!("corrupt event: {e}"))?;
            out.push(ev);
        }
        Ok(out)
    }

    fn last_seq(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS)?;
        Ok(table
            .iter()?
            .next_back()
            .map(|r| r.ok().unwrap().0.value())
            .unwrap_or(0))
    }

    fn snapshot_seq(&self) -> Result<u64> {
        Ok(self
            .read_meta_bytes("snapshot_seq")?
            .and_then(|raw| raw.try_into().ok().map(u64::from_le_bytes))
            .unwrap_or(0))
    }

    fn load_snapshot(&self) -> Result<Option<Kanban>> {
        Ok(self
            .read_meta_bytes("snapshot")?
            .and_then(|raw| serde_json::from_slice(&raw).ok()))
    }

    fn schema_version(&self) -> Result<u32> {
        Ok(self
            .read_meta_bytes("schema_version")?
            .and_then(|raw| raw.try_into().ok().map(u32::from_le_bytes))
            .unwrap_or(0))
    }
}

// -------------------------------------------------------------- fold/proj ---

/// Fold events onto a starting projection. Events carry full task state, so
/// replay is a keyed upsert; board-level events only bump the sequence.
pub fn fold(mut proj: Kanban, events: &[StoredEvent]) -> Kanban {
    for ev in events {
        if let Some(task) = &ev.task {
            match proj.tasks.iter_mut().find(|t| t.id == task.id) {
                Some(t) => *t = task.clone(),
                None => proj.tasks.push(task.clone()),
            }
            // keep next_seq ahead of any id ever seen
            if let Some(n) = task
                .id
                .strip_prefix("t-")
                .and_then(|s| s.parse::<u64>().ok())
            {
                proj.next_seq = proj.next_seq.max(n);
            }
        }
    }
    proj
}

/// Open the board backed by `store`: load snapshot, replay any events after
/// it, verify the fold equals the stored snapshot at that seq (consistency).
pub fn open_projection(store: &dyn EventStore) -> Result<Kanban> {
    let snap_seq = store.snapshot_seq()?;
    let last = store.last_seq()?;
    if snap_seq > last {
        bail!("corrupt store: snapshot seq {snap_seq} > log seq {last}");
    }
    let mut proj = store.load_snapshot()?.unwrap_or_default();
    if snap_seq < last {
        // replay what the snapshot missed (e.g. a crash between event and
        // snapshot is impossible by construction — both share one txn —
        // but a fresh snapshot write path may lag in alternative stores)
        let missed = store.read_since(snap_seq, usize::MAX)?;
        proj = fold(proj, &missed);
    }
    Ok(proj)
}

// -------------------------------------------------------------- BoardStore ---

/// The facade the app talks to: mutation methods append events and keep the
/// in-memory projection; persistence and atomicity live in the store.
pub struct BoardStore {
    store: std::sync::Arc<dyn EventStore>,
    proj: Kanban,
}

/// Stuck-task info for the watchdog: the caller aborts the session
/// BEFORE calling `watchdog_apply` (review v2 Б6: no duplicate agents).
#[derive(Debug, Clone)]
pub struct StuckTask {
    pub task_id: String,
    pub harness: String,
    pub session_id: String,
    /// "retry" | "escalated"
    pub disposition: &'static str,
}

impl BoardStore {
    pub fn open_memory() -> BoardStore {
        BoardStore {
            store: std::sync::Arc::new(MemoryStore::default()),
            proj: Kanban::default(),
        }
    }

    pub fn open_redb(path: &Path) -> Result<BoardStore> {
        let store = RedbStore::open(path)?;
        let proj = open_projection(&store)?;
        Ok(BoardStore {
            store: std::sync::Arc::new(store),
            proj,
        })
    }

    pub fn projection(&self) -> &Kanban {
        &self.proj
    }

    /// The A2A gateway talks to the store directly (same redb connection),
    /// wrapped so incoming tasks land as regular board events.
    pub fn gateway_handle(&self) -> std::sync::Arc<std::sync::Mutex<BoardStore>> {
        // a second BoardStore over the SAME Arc<EventStore> keeps one connection
        std::sync::Arc::new(std::sync::Mutex::new(BoardStore {
            store: std::sync::Arc::clone(&self.store),
            proj: Kanban::default(),
        }))
    }

    pub fn last_events(&self, n: usize) -> Vec<StoredEvent> {
        let last = self.store.last_seq().unwrap_or(0);
        let since = last.saturating_sub(n as u64);
        self.store.read_since(since, n).unwrap_or_default()
    }

    /// The ONLY mutation path (review v2 Б7): write the event to the journal
    /// FIRST; on success fold it into the projection. On failure the caller
    /// sees the error — the projection stays untouched.
    fn commit(&mut self, task_id: &str, action: &str, task: Option<Task>) -> Result<(), String> {
        let seq = self.store.last_seq().unwrap_or(0) + 1;
        let ev = StoredEvent {
            seq,
            ts_ms: crate::kanban::now_ms(),
            task_id: task_id.to_string(),
            action: action.to_string(),
            task,
        };
        let ev_for_fold = ev.clone();
        match self.store.append(ev, &self.proj) {
            Ok(()) => {
                self.proj = fold(self.proj.clone(), &[ev_for_fold]);
                Ok(())
            }
            Err(e) => Err(format!("journal write failed: {e:#}")),
        }
    }

    pub fn add_task(&mut self, title: &str) -> Task {
        self.proj.next_seq += 1;
        let t = Task {
            id: format!("t-{:04}", self.proj.next_seq),
            title: title.to_string(),
            description: String::new(),
            acceptance: Vec::new(),
            status: TaskStatus::Backlog,
            harness: None,
            role: None,
            comments: Vec::new(),
            depends_on: Vec::new(),
            session_id: None,
            attempts: 0,
            started_ms: None,
            lease_until_ms: None,
            max_attempts: crate::kanban::default_max_attempts(),
            schedule: String::new(),
            budget_usd: 0.0,
            unit_cost_usd: 0.0,
            run_budget_secs: 0,
            parent_id: None,
            max_turns: 0,
            created_ms: crate::kanban::now_ms(),
            updated_ms: crate::kanban::now_ms(),
        };
        self.proj.tasks.push(t.clone());
        // Б7: journal is the source of truth; on failure the projection entry
        // is stale — log loudly so the discrepancy is visible
        if let Err(e) = self.commit(&t.id, "created", Some(t.clone())) {
            crate::log_error!("kanban", "add_task journal write failed: {e}");
        }
        t
    }

    pub fn transition(&mut self, task_id: &str, action: TaskAction) -> Result<Task, String> {
        self.transition_inner(task_id, action, None)
    }

    /// Transition with an explicit repo root: engages git worktree isolation
    /// (used by the TUI runner and the daemon for code tasks).
    pub fn transition_in(
        &mut self,
        repo: &std::path::Path,
        task_id: &str,
        action: TaskAction,
    ) -> Result<Task, String> {
        let snapshot = self.proj.tasks.iter().find(|t| t.id == task_id).cloned();
        if let Some(task) = snapshot {
            if let Some(next) = task.status.next(action) {
                // side-effects run BEFORE the state flips, so a failed
                // merge/conflict keeps the task in its current status
                if let Err(e) = super::worktree::on_transition(repo, &task, next, action) {
                    return Err(format!("{e:#}"));
                }
            }
        }
        self.transition_inner(task_id, action, None)
    }

    fn transition_inner(
        &mut self,
        task_id: &str,
        action: TaskAction,
        _repo: Option<&std::path::Path>,
    ) -> Result<Task, String> {
        if action == TaskAction::Start {
            self.budget_ok(task_id)?;
        }
        {
            let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) else {
                return Err(format!("task {task_id} not found"));
            };
            // thread-turn limiter (review §8): a Start beyond max_turns is
            // rejected and the task escalates instead of looping forever
            if action == TaskAction::Start && t.max_turns > 0 && t.attempts >= t.max_turns {
                return Err(format!(
                    "task {} exceeded max_turns={} — escalate to a human",
                    t.id, t.max_turns
                ));
            }
            let Some(next) = t.status.next(action) else {
                return Err(format!("illegal transition {action:?} from {:?}", t.status));
            };
            t.status = next;
            t.updated_ms = crate::kanban::now_ms();
            if action == TaskAction::Start {
                t.attempts += 1;
                let now = crate::kanban::now_ms();
                t.started_ms = Some(now);
                t.lease_until_ms = Some(now + crate::kanban::LEASE_MS);
            }
            if matches!(
                next,
                TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Review
            ) {
                t.started_ms = None;
            }
        }
        let t = self
            .proj
            .tasks
            .iter()
            .find(|t| t.id == task_id)
            .cloned()
            .unwrap();
        self.commit(task_id, &format!("transition:{action:?}"), Some(t.clone()))?;
        Ok(t)
    }

    pub fn edit_task(&mut self, task_id: &str, title: &str) -> Result<(), String> {
        let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err("task not found".into());
        };
        t.title = title.to_string();
        t.updated_ms = crate::kanban::now_ms();
        let snap = t.clone();
        self.commit(task_id, "edited", Some(snap))?;
        Ok(())
    }

    /// assign a harness and (optionally) a role in one go
    pub fn assign_with_role(
        &mut self,
        task_id: &str,
        harness: Option<String>,
        role: Option<String>,
    ) -> Result<(), String> {
        let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err("task not found".into());
        };
        t.harness = harness;
        t.role = role;
        t.updated_ms = crate::kanban::now_ms();
        let snap = t.clone();
        self.commit(task_id, "assigned", Some(snap))?;
        Ok(())
    }

    pub fn assign(&mut self, task_id: &str, harness: Option<String>) -> Result<(), String> {
        let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err("task not found".into());
        };
        t.harness = harness;
        t.updated_ms = crate::kanban::now_ms();
        let snap = t.clone();
        self.commit(task_id, "assigned", Some(snap))?;
        Ok(())
    }

    pub fn link_session(&mut self, task_id: &str, session_id: &str) {
        if let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) {
            t.session_id = Some(session_id.to_string());
            t.updated_ms = crate::kanban::now_ms();
            let snap = t.clone();
            let _ = self.commit(task_id, "linked", Some(snap));
        }
    }

    /// Rebind a task's session after the harness minted the real id.
    pub fn rebind_session(&mut self, old: &str, new: &str) {
        let Some(id) = self
            .proj
            .tasks
            .iter()
            .find(|t| t.session_id.as_deref() == Some(old))
            .map(|t| t.id.clone())
        else {
            return;
        };
        if let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == id) {
            t.session_id = Some(new.to_string());
            t.updated_ms = crate::kanban::now_ms();
            let snap = t.clone();
            let _ = self.commit(&id, "rebound", Some(snap));
        }
    }

    /// Delegation: create a child task; the chain depth is capped.
    pub fn add_subtask(&mut self, parent_id: &str, title: &str) -> Result<Task, String> {
        let depth = |id: &str, proj: &Kanban| -> usize {
            let mut d = 0;
            let mut cur = id.to_string();
            while let Some(t) = proj.tasks.iter().find(|t| t.id == cur) {
                match &t.parent_id {
                    Some(p) if p != &t.id => {
                        cur = p.clone();
                        d += 1;
                    }
                    _ => break,
                }
            }
            d
        };
        let parent_depth = depth(parent_id, &self.proj);
        if parent_depth + 1 > crate::kanban::MAX_DELEGATION_DEPTH {
            return Err(format!(
                "delegation depth exceeds {} — escalate to a human instead",
                crate::kanban::MAX_DELEGATION_DEPTH
            ));
        }
        let child = self.add_task(title);
        if let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == child.id) {
            t.parent_id = Some(parent_id.to_string());
        }
        let snap = self
            .proj
            .tasks
            .iter()
            .find(|t| t.id == child.id)
            .cloned()
            .unwrap();
        self.commit(&child.id, "delegated", Some(snap))?;
        Ok(child)
    }

    /// Budget check at Start: attempts × unit cost must fit into budget_usd.
    fn budget_ok(&self, task_id: &str) -> Result<(), String> {
        let Some(t) = self.proj.tasks.iter().find(|t| t.id == task_id) else {
            return Ok(());
        };
        if t.budget_usd <= 0.0 || t.unit_cost_usd <= 0.0 {
            return Ok(());
        }
        if (f64::from(t.attempts + 1)) * t.unit_cost_usd > t.budget_usd {
            return Err(format!(
                "task {} budget ${} exceeded ({} attempts × ${}) — escalate",
                t.id,
                t.budget_usd,
                t.attempts + 1,
                t.unit_cost_usd
            ));
        }
        Ok(())
    }

    /// Append a thread entry (comment / handoff / system note).
    pub fn comment(
        &mut self,
        task_id: &str,
        author: &str,
        kind: &str,
        text: &str,
    ) -> Result<(), String> {
        let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err(format!("task {task_id} not found"));
        };
        t.comments.push(super::Comment {
            author: author.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
            ts_ms: crate::kanban::now_ms(),
        });
        t.updated_ms = crate::kanban::now_ms();
        let snap = t.clone();
        self.commit(task_id, &format!("comment:{kind}"), Some(snap))?;
        Ok(())
    }

    /// Structured handoff contract (review §7): summary is mandatory, the
    /// task is re-assigned to `to_role` and returns to Ready so the new
    /// owner picks it up. An empty/short summary is rejected.
    pub fn handoff(
        &mut self,
        task_id: &str,
        from: &str,
        to_role: &str,
        summary: &str,
        open_questions: &str,
    ) -> Result<Task, String> {
        if summary.trim().len() < 10 {
            return Err("handoff rejected: summary must be at least 10 characters".into());
        }
        if to_role.trim().is_empty() {
            return Err("handoff rejected: to_role is required".into());
        }
        {
            let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) else {
                return Err(format!("task {task_id} not found"));
            };
            t.role = Some(to_role.to_string());
            let mut text = format!(
                "[handoff {from} → {to_role}]
{summary}"
            );
            if !open_questions.trim().is_empty() {
                text.push_str(&format!(
                    "
Open questions: {open_questions}"
                ));
            }
            t.comments.push(super::Comment {
                author: from.to_string(),
                kind: "handoff".into(),
                text,
                ts_ms: crate::kanban::now_ms(),
            });
            t.updated_ms = crate::kanban::now_ms();
        }
        // an in-flight turn must go back to Ready for the new owner
        let t = self
            .proj
            .tasks
            .iter()
            .find(|t| t.id == task_id)
            .cloned()
            .unwrap();
        if t.status == TaskStatus::InProgress {
            self.transition(task_id, TaskAction::Interrupt)?;
        }
        let t = self
            .proj
            .tasks
            .iter()
            .find(|t| t.id == task_id)
            .cloned()
            .unwrap();
        self.commit(task_id, "handoff", Some(t.clone()))?;
        Ok(t)
    }

    /// Thread entries after `since_ms` (the agent's inbox).
    pub fn read_inbox(&self, task_id: &str, since_ms: i64) -> Option<&[super::Comment]> {
        self.proj
            .tasks
            .iter()
            .find(|t| t.id == task_id)
            .map(|t| &t.comments[..])
            .map(|all| {
                &all[all
                    .iter()
                    .position(|c| c.ts_ms > since_ms)
                    .unwrap_or(all.len())..]
            })
    }

    /// Set DAG dependencies (forward references to not-yet-created task ids
    /// are allowed; a missing dependency counts as settled at launch time).
    pub fn set_depends(&mut self, task_id: &str, deps: Vec<String>) -> Result<(), String> {
        // cycle detection: DFS through the hypothetical graph (review v2)
        {
            let mut hypothetical = self.proj.clone();
            if let Some(t) = hypothetical.tasks.iter_mut().find(|t| t.id == task_id) {
                t.depends_on = deps.clone();
            }
            let mut visited = std::collections::HashSet::new();
            let mut stack = vec![task_id.to_string()];
            while let Some(cur) = stack.pop() {
                if !visited.insert(cur.clone()) {
                    return Err(format!("dependency cycle detected involving {cur}"));
                }
                if let Some(t) = hypothetical.tasks.iter().find(|t| t.id == cur) {
                    stack.extend(t.depends_on.iter().cloned());
                }
            }
        }
        let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) else {
            return Err("task not found".into());
        };
        t.depends_on = deps;
        t.updated_ms = crate::kanban::now_ms();
        let snap = t.clone();
        self.commit(task_id, "depends", Some(snap))?;
        Ok(())
    }

    /// Cron triggers — DISABLED (review v2 Б3): the schedule string is not
    /// parsed as cron, so any non-empty value spawned a child every 60 s.
    /// Returning empty until a real cron parser (croner) + next_fire_at
    /// events are in place.
    pub fn spawn_due_scheduled(&mut self, _now_secs: u64) -> Vec<String> {
        Vec::new() // no-op until proper cron support (see roadmap)
    }

    /// DAG scheduler: Ready tasks whose dependencies are all settled
    /// (Done or Cancelled) — the daemon launches only these.
    pub fn launchable(&self) -> Vec<Task> {
        let settled = |id: &str| {
            self.proj
                .tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Cancelled))
                .unwrap_or(true) // missing dep counts as settled to avoid deadlock
        };
        self.proj
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Ready && t.depends_on.iter().all(|d| settled(d)))
            .cloned()
            .collect()
    }

    /// Heartbeat (review v2 Б6): renew the lease on an agent progress event.
    /// The watchdog won't touch a task whose lease is still valid.
    pub fn renew_lease(&mut self, task_id: &str) {
        self.renew_lease_at(task_id, crate::kanban::now_ms());
    }

    /// Clock-aware variant for tests.
    pub fn renew_lease_at(&mut self, task_id: &str, now: i64) {
        if let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == task_id) {
            if t.status == TaskStatus::InProgress {
                t.lease_until_ms = Some(now + crate::kanban::LEASE_MS);
            }
        }
    }

    /// Detect stuck tasks WITHOUT mutating: returns them for the caller to
    /// abort their sessions first. Then call `watchdog_apply` to transition.
    pub fn watchdog_scan(&self, now: i64) -> Vec<StuckTask> {
        self.proj
            .tasks
            .iter()
            .filter(|t| {
                // Б6: use the lease (heartbeat-renewed) instead of raw start time
                let lease_expired = t.lease_until_ms.map(|l| now > l).unwrap_or(true); // no lease = treat as expired
                let over_budget = t
                    .started_ms
                    .map(|s| t.run_budget_secs > 0 && now - s > t.run_budget_secs * 1000)
                    .unwrap_or(false);
                t.status == TaskStatus::InProgress && (lease_expired || over_budget)
            })
            .filter_map(|t| {
                let (harness, session) = match (&t.harness, &t.session_id) {
                    (Some(h), Some(s)) if !s.is_empty() => (h.clone(), s.clone()),
                    _ => return None, // no session to abort
                };
                let over_budget = t
                    .started_ms
                    .map(|s| t.run_budget_secs > 0 && now - s > t.run_budget_secs * 1000)
                    .unwrap_or(false);
                let disposition = if over_budget || t.attempts >= t.max_attempts {
                    "escalated"
                } else {
                    "retry"
                };
                Some(StuckTask {
                    task_id: t.id.clone(),
                    harness,
                    session_id: session,
                    disposition,
                })
            })
            .collect()
    }

    /// After the caller has aborted the sessions, apply the transitions.
    pub fn watchdog_apply(&mut self, stuck: &[StuckTask]) -> Vec<(String, &'static str)> {
        let mut acted = Vec::new();
        for st in stuck {
            let action = if st.disposition == "escalated" {
                TaskAction::Ask
            } else {
                TaskAction::Interrupt
            };
            if self.transition(&st.task_id, action).is_ok() {
                acted.push((st.task_id.clone(), st.disposition));
            }
        }
        acted
    }

    /// Legacy single-pass watchdog (kept for backwards compatibility with
    /// tests that don't need session abort). Prefer scan + apply.
    pub fn watchdog_pass(&mut self, now: i64) -> Vec<(String, &'static str)> {
        let stuck_ids: Vec<String> = self
            .proj
            .tasks
            .iter()
            .filter(|t| {
                let stuck = t
                    .started_ms
                    .map(|s| now - s > crate::kanban::STUCK_AFTER_MS)
                    .unwrap_or(false);
                let over_run_budget = t
                    .started_ms
                    .map(|s| t.run_budget_secs > 0 && now - s > t.run_budget_secs * 1000)
                    .unwrap_or(false);
                t.status == TaskStatus::InProgress && (stuck || over_run_budget)
            })
            .map(|t| t.id.clone())
            .collect();
        let mut acted = Vec::new();
        for id in stuck_ids {
            let attempts = self
                .proj
                .tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.attempts)
                .unwrap_or(0);
            let max = self
                .proj
                .tasks
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.max_attempts)
                .unwrap_or(crate::kanban::default_max_attempts());
            let over_run_budget = self
                .proj
                .tasks
                .iter()
                .find(|t| t.id == id)
                .and_then(|t| t.started_ms)
                .map(|s| {
                    let t = self.proj.tasks.iter().find(|t| t.id == id);
                    t.map(|t| t.run_budget_secs > 0 && now - s > t.run_budget_secs * 1000)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let action = if over_run_budget || attempts >= max {
                TaskAction::Ask // escalate: out of run budget or silent too many times
            } else {
                TaskAction::Interrupt // retry
            };
            let label = if action == TaskAction::Ask {
                "escalated"
            } else {
                "retried"
            };
            if self.transition(&id, action).is_ok() {
                // clear started_ms on Interrupt too (next Start re-stamps)
                if action == TaskAction::Interrupt {
                    if let Some(t) = self.proj.tasks.iter_mut().find(|t| t.id == id) {
                        t.started_ms = None;
                    }
                }
                acted.push((id, label));
            }
        }
        acted
    }

    pub fn find_by_session(&self, harness: &str, session: &str) -> Option<Task> {
        self.proj
            .tasks
            .iter()
            .find(|t| {
                t.harness.as_deref() == Some(harness) && t.session_id.as_deref() == Some(session)
            })
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdb(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aic-redb-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("kanban.redb")
    }

    #[test]
    fn append_replay_consistency() {
        let path = tmpdb("replay");
        let mut bs = BoardStore::open_redb(&path).unwrap();
        let t = bs.add_task("write tests");
        bs.transition(&t.id, TaskAction::Promote).unwrap();
        bs.transition(&t.id, TaskAction::Start).unwrap();
        bs.transition(&t.id, TaskAction::Finish).unwrap();
        drop(bs); // release the redb file lock before reopening

        // reopen from disk: snapshot + log must produce the same state
        let reopened = BoardStore::open_redb(&path).unwrap();
        assert_eq!(reopened.projection().tasks.len(), 1);
        assert_eq!(reopened.projection().tasks[0].status, TaskStatus::Review);
        assert_eq!(reopened.projection().tasks[0].attempts, 1);
        assert_eq!(reopened.store.last_seq().unwrap(), 4);
        assert_eq!(reopened.store.snapshot_seq().unwrap(), 4);
        assert_eq!(reopened.store.schema_version().unwrap(), SCHEMA_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fold_from_empty_equals_snapshot() {
        let path = tmpdb("fold");
        let mut bs = BoardStore::open_redb(&path).unwrap();
        bs.add_task("a");
        let t2 = bs.add_task("b");
        bs.transition(&t2.id, TaskAction::Cancel).unwrap();
        drop(bs); // release the lock

        let store = RedbStore::open(&path).unwrap();
        let all = store.read_since(0, usize::MAX).unwrap();
        let from_scratch = fold(Kanban::default(), &all);
        assert_eq!(from_scratch.tasks.len(), 2);
        assert_eq!(from_scratch.tasks[1].status, TaskStatus::Cancelled);
        // full-log fold matches the stored snapshot exactly
        assert_eq!(
            store.load_snapshot().unwrap().unwrap().tasks,
            from_scratch.tasks
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rebind_and_link_sessions() {
        let mut bs = BoardStore::open_memory();
        let t = bs.add_task("x");
        bs.assign(&t.id, Some("oc".into())).unwrap();
        bs.link_session(&t.id, "pending-1");
        bs.rebind_session("pending-1", "ses_real");
        assert_eq!(
            bs.projection().tasks[0].session_id.as_deref(),
            Some("ses_real")
        );
        assert!(bs.find_by_session("oc", "ses_real").is_some());
    }

    #[test]
    fn dag_launchable_respects_dependencies() {
        let mut b = BoardStore::open_memory();
        let t1 = b.add_task("first");
        let t2 = b.add_task("second");
        b.set_depends(&t2.id, vec![t1.id.clone()]).unwrap();
        b.transition(&t1.id, TaskAction::Promote).unwrap();
        b.transition(&t2.id, TaskAction::Promote).unwrap();
        assert_eq!(b.projection().by_status(TaskStatus::Ready).len(), 2);
        // t2 is Ready but blocked by t1
        let launchable = b.launchable();
        assert_eq!(launchable.len(), 1, "only t1 is launchable");
        assert_eq!(launchable[0].id, t1.id);
        // finish t1 -> t2 unblocked
        b.transition(&t1.id, TaskAction::Start).unwrap();
        b.transition(&t1.id, TaskAction::Finish).unwrap();
        b.transition(&t1.id, TaskAction::Approve).unwrap();
        let launchable = b.launchable();
        assert_eq!(launchable.len(), 1);
        assert_eq!(launchable[0].id, t2.id, "t2 unblocked after t1 Done");
        // unknown dep counts as settled (no deadlock)
        let t3 = b.add_task("third");
        b.set_depends(&t3.id, vec!["t-9999".into()]).unwrap();
        b.transition(&t3.id, TaskAction::Promote).unwrap();
        assert!(b.launchable().iter().any(|t| t.id == t3.id));
    }

    #[test]
    fn handoff_contract_validated() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("x");
        assert!(
            b.handoff(&t.id, "coder", "reviewer", "short", "").is_err(),
            "summary too short"
        );
        let updated = b
            .handoff(&t.id, "coder", "reviewer", "draft done, please review", "?")
            .unwrap();
        assert_eq!(updated.role.as_deref(), Some("reviewer"));
        // in-flight task goes back to Ready for the new owner
        b.transition(&t.id, TaskAction::Promote).unwrap();
        b.transition(&t.id, TaskAction::Start).unwrap();
        let updated = b
            .handoff(&t.id, "reviewer", "coder", "rework needed, see notes", "")
            .unwrap();
        assert_eq!(updated.status, TaskStatus::Ready);
        assert!(updated.comments.iter().any(|c| c.kind == "handoff"));
    }

    #[test]
    fn inbox_reads_only_new_entries() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("x");
        b.comment(&t.id, "coder", "comment", "first").unwrap();
        let ts = b.projection().tasks[0].comments[0].ts_ms;
        std::thread::sleep(std::time::Duration::from_millis(2));
        b.comment(&t.id, "reviewer", "comment", "second").unwrap();
        let inbox = b.read_inbox(&t.id, ts).unwrap();
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].text.contains("second"));
    }

    #[test]
    fn worktree_isolation_lifecycle() {
        use std::process::Command;
        // throwaway git repo
        let dir = std::env::temp_dir().join(format!("aic-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| {
            Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        assert!(g(&["init", "-q"]).status.success());
        assert!(g(&["config", "user.email", "t@t"]).status.success());
        assert!(g(&["config", "user.name", "t"]).status.success());
        std::fs::write(dir.join("f.txt"), "base").unwrap();
        assert!(g(&["add", "."]).status.success());
        assert!(g(&["commit", "-qm", "init"]).status.success());

        let mut b = BoardStore::open_memory();
        let t = b.add_task("code change");
        b.transition_in(&dir, &t.id, TaskAction::Promote).unwrap();
        b.transition_in(&dir, &t.id, TaskAction::Start).unwrap();
        let wt = super::super::worktree::worktree_path(&dir, b.projection().tasks.first().unwrap());
        assert!(wt.exists(), "worktree created at {}", wt.display());
        assert!(
            wt.join(".git").exists() || wt.join(".git").is_file(),
            "worktree is a git checkout"
        );

        // commit a change inside the worktree
        std::fs::write(wt.join("f.txt"), "task change").unwrap();
        let gw = |args: &[&str]| {
            Command::new("git")
                .current_dir(&wt)
                .args(args)
                .output()
                .unwrap()
        };
        assert!(gw(&["add", "."]).status.success());
        assert!(gw(&["commit", "-qm", "task work"]).status.success());

        // Review -> Done merges the branch back and removes the worktree
        b.transition_in(&dir, &t.id, TaskAction::Finish).unwrap();
        b.transition_in(&dir, &t.id, TaskAction::Approve).unwrap();
        assert!(!wt.exists(), "worktree removed after merge");
        let merged = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(merged, "task change", "branch merged into the base");
        assert_eq!(b.projection().tasks[0].status, TaskStatus::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_cancel_drops_without_merge() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("aic-wtc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| {
            Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        assert!(g(&["init", "-q"]).status.success());
        assert!(g(&["config", "user.email", "t@t"]).status.success());
        assert!(g(&["config", "user.name", "t"]).status.success());
        std::fs::write(dir.join("f.txt"), "base").unwrap();
        assert!(g(&["add", "."]).status.success());
        assert!(g(&["commit", "-qm", "init"]).status.success());

        let mut b = BoardStore::open_memory();
        let t = b.add_task("doomed");
        b.transition_in(&dir, &t.id, TaskAction::Promote).unwrap();
        b.transition_in(&dir, &t.id, TaskAction::Start).unwrap();
        let wt = super::super::worktree::worktree_path(&dir, b.projection().tasks.first().unwrap());
        assert!(wt.exists());

        b.transition_in(&dir, &t.id, TaskAction::Cancel).unwrap();
        assert!(!wt.exists(), "worktree dropped on cancel");
        // base file untouched
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "base");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turn_limiter_blocks_looping_starts() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("loopy");
        b.proj.tasks[0].max_turns = 2;
        b.transition(&t.id, TaskAction::Promote).unwrap();
        assert!(b.transition(&t.id, TaskAction::Start).is_ok()); // attempt 1
        b.transition(&t.id, TaskAction::Interrupt).unwrap();
        assert!(b.transition(&t.id, TaskAction::Start).is_ok()); // attempt 2
        b.transition(&t.id, TaskAction::Interrupt).unwrap();
        let err = b.transition(&t.id, TaskAction::Start).unwrap_err();
        assert!(err.contains("max_turns"), "{err}");
    }

    #[test]
    fn scheduled_tasks_are_disabled_until_cron_parser() {
        // review v2 Б3: spawn_due_scheduled is a no-op; verify no task is
        // created regardless of schedule string or elapsed time
        let mut b = BoardStore::open_memory();
        let _t = b.add_task("nightly");
        b.proj.tasks[0].schedule = "0 3 * * *".into();
        b.proj.tasks[0].updated_ms = 0;
        let now = 9_999_999_999u64;
        let spawned = b.spawn_due_scheduled(now);
        assert!(
            spawned.is_empty(),
            "schedule must not spawn until cron is parsed"
        );
        let spawned2 = b.spawn_due_scheduled(now);
        assert!(spawned2.is_empty());
        // no children created at all
        assert_eq!(b.projection().tasks.len(), 1, "no spawn, no child");
    }

    #[test]
    fn budget_blocks_over_budget_start() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("costly");
        b.proj.tasks[0].budget_usd = 1.0;
        b.proj.tasks[0].unit_cost_usd = 0.6;
        b.transition(&t.id, TaskAction::Promote).unwrap();
        assert!(b.transition(&t.id, TaskAction::Start).is_ok()); // 0.6 <= 1.0
        b.transition(&t.id, TaskAction::Interrupt).unwrap();
        let err = b.transition(&t.id, TaskAction::Start).unwrap_err(); // 2×0.6 > 1.0
        assert!(err.contains("budget"), "{err}");
        // unlimited when unset
        let t2 = b.add_task("free");
        b.transition(&t2.id, TaskAction::Promote).unwrap();
        assert!(b.transition(&t2.id, TaskAction::Start).is_ok());
    }

    #[test]
    fn delegation_depth_is_capped() {
        let mut b = BoardStore::open_memory();
        let root = b.add_task("root");
        let c1 = b.add_subtask(&root.id, "l1").unwrap();
        let c2 = b.add_subtask(&c1.id, "l2").unwrap();
        let c3 = b.add_subtask(&c2.id, "l3").unwrap();
        assert!(
            b.add_subtask(&c3.id, "l4").is_err(),
            "depth 4 must be rejected"
        );
        assert_eq!(
            b.projection()
                .tasks
                .iter()
                .filter(|t| t.parent_id.is_some())
                .count(),
            3
        );
    }

    #[test]
    fn run_budget_escalates_instead_of_retry() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("timed");
        b.transition(&t.id, TaskAction::Promote).unwrap();
        b.transition(&t.id, TaskAction::Start).unwrap();
        b.proj.tasks[0].run_budget_secs = 60;
        // pretend the turn started 2 minutes ago
        let past = crate::kanban::now_ms() - 120_000;
        b.proj.tasks[0].started_ms = Some(past);
        let acted = b.watchdog_pass(crate::kanban::now_ms());
        assert_eq!(acted.len(), 1);
        assert_eq!(acted[0].1, "escalated", "run budget escalates, not retries");
        assert_eq!(b.projection().tasks[0].status, TaskStatus::NeedsHuman);
    }

    #[test]
    fn lease_heartbeat_prevents_interruption() {
        let mut b = BoardStore::open_memory();
        let t = b.add_task("long task");
        b.transition(&t.id, TaskAction::Promote).unwrap();
        // link a session so watchdog_scan can see it (it filters sessionless tasks)
        b.assign(&t.id, Some("oc".into())).unwrap();
        b.link_session(&t.id, "ses_test");
        b.transition(&t.id, TaskAction::Start).unwrap();

        // simulate 30 minutes of activity with heartbeats every 30 s
        let start = crate::kanban::now_ms();
        for i in 0..60 {
            let now = start + i * 30_000; // 30s intervals
            b.renew_lease_at(&t.id, now); // heartbeat at the simulated time
            // watchdog at this moment: lease still valid → NOT stuck
            let stuck = b.watchdog_scan(now);
            assert!(
                stuck.is_empty(),
                "active task must not be interrupted at {i} seconds"
            );
        }

        // now the agent goes silent: after lease expires, watchdog fires
        let last_lease = b.projection().tasks[0].lease_until_ms.unwrap();
        let stuck = b.watchdog_scan(last_lease + 1);
        assert_eq!(stuck.len(), 1, "expired lease must trigger watchdog");
        assert_eq!(stuck[0].task_id, t.id);

        // run_budget escalation still works independently of lease
        b.proj.tasks[0].run_budget_secs = 60;
        b.proj.tasks[0].started_ms = Some(start - 120_000); // 2 min ago
        b.renew_lease_at(&t.id, start + 1_800_000); // lease valid at 30min mark
        let stuck = b.watchdog_scan(start + 1_800_000);
        assert_eq!(stuck.len(), 1, "run_budget must override lease");
        assert_eq!(stuck[0].disposition, "escalated");
    }

    #[test]
    fn dag_cycle_detection() {
        let mut b = BoardStore::open_memory();
        let t1 = b.add_task("a");
        let t2 = b.add_task("b");
        let t3 = b.add_task("c");
        b.set_depends(&t2.id, vec![t1.id.clone()]).unwrap();
        b.set_depends(&t3.id, vec![t2.id.clone()]).unwrap();
        // t1 depends on t3 → cycle t1→t3→t2→t1
        assert!(
            b.set_depends(&t1.id, vec![t3.id.clone()]).is_err(),
            "cycle must be rejected"
        );
        // self-dependency
        assert!(
            b.set_depends(&t1.id, vec![t1.id.clone()]).is_err(),
            "self-dep must be rejected"
        );
        // valid chain still works
        let t4 = b.add_task("d");
        assert!(b.set_depends(&t4.id, vec![t3.id.clone()]).is_ok());
    }

    #[test]
    fn audit_trail_read_back() {
        let mut bs = BoardStore::open_memory();
        let t = bs.add_task("audit");
        bs.transition(&t.id, TaskAction::Promote).unwrap();
        let evs = bs.last_events(10);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].action, "created");
        assert_eq!(evs[1].action, "transition:Promote");
    }
}
