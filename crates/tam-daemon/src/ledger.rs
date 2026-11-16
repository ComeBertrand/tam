//! Task ledger: append-only JSONL of task events, owned by the daemon.
//!
//! The daemon is the only writer. It holds an exclusive `flock` on a lock
//! file next to the ledger for its whole lifetime, so a second daemon (or a
//! stray old client) cannot interleave appends.
//!
//! A task is a name and one or more members: a worktree each, with at most
//! one agent session. Members have a stable id, independent of the task and
//! branch names, and agent runs are recorded against it — so moving a member
//! to another task only needs one event. Member short names are derived when
//! the ledger is read, never stored.
//!
//! On open the ledger is read, malformed lines are reported and set aside in
//! `<ledger>.rejected`, and the file is compacted: events of dropped tasks (and
//! events naming no live task or member) are removed, keeping only the history
//! of the active tasks. Task state is never stored — it is folded from the
//! events.
//!
//! Ledgers written before tasks had members are migrated on open (see
//! [`LegacyEvent`]): the old file is kept as `<ledger>.pre-members` and the
//! ledger is rewritten in the current format.
//!
//! Next to the folded tasks, the ledger keeps what reconciliation observed of
//! the members' worktrees (their health, their repository): in memory only,
//! never written, like any other derived state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tam_proto::{MemberHealth, MemberInfo, NewMember, RunInfo, TaskInfo};

/// A single event recorded in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEvent {
    TaskCreated {
        name: String,
        timestamp: u64,
    },
    /// A worktree joined the task as a member.
    MemberAdded {
        task: String,
        /// Stable member id.
        member: String,
        /// Repository name.
        repo: String,
        /// The repository's main checkout, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo_root: Option<PathBuf>,
        branch: String,
        dir: PathBuf,
        timestamp: u64,
    },
    AgentRunStarted {
        member: String,
        provider: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        timestamp: u64,
    },
    /// The session id of the member's current run became known after it
    /// started (a new session reported by the agent).
    AgentSessionIdentified {
        member: String,
        session_id: String,
        timestamp: u64,
    },
    AgentRunEnded {
        member: String,
        exit_code: i32,
        timestamp: u64,
    },
    WorktreeDeleted {
        task: String,
        timestamp: u64,
    },
    TaskDropped {
        task: String,
        timestamp: u64,
    },
    /// The agent of the member's current run moved to another session it
    /// had a turn in (e.g. after `/clear`): the run's session, and so the
    /// member's current one, is now that one.
    AgentSessionChanged {
        member: String,
        session_id: String,
        timestamp: u64,
    },
}

/// Ledger lines written before tasks had members (tam <= 0.3.6): a task had
/// one directory, owned (a worktree tam created) or borrowed, and runs were
/// keyed by task name. `worktree_deleted` and `task_dropped` kept their shape.
///
/// Every legacy line misses a field its current counterpart requires, or the
/// other way round, so a line is tried as legacy first: parsed as current, a
/// legacy `task_created` would silently lose its directory.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyEvent {
    TaskCreated {
        name: String,
        dir: PathBuf,
        owned: bool,
        timestamp: u64,
    },
    AgentRunStarted {
        task: String,
        provider: String,
        #[serde(default)]
        session_id: Option<String>,
        timestamp: u64,
    },
    AgentSessionIdentified {
        task: String,
        session_id: String,
        timestamp: u64,
    },
    AgentRunEnded {
        task: String,
        exit_code: i32,
        timestamp: u64,
    },
}

/// A line of the ledger file that could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedLine {
    /// 1-based line number in the file as it was read.
    pub line: usize,
    pub content: String,
    pub error: String,
}

/// What happened while opening the ledger.
#[derive(Debug, Default)]
pub struct LoadReport {
    pub malformed: Vec<MalformedLine>,
    /// Number of events removed by compaction.
    pub compacted: usize,
    /// Number of legacy lines converted to the current format.
    pub migrated: usize,
    /// Copy of the ledger taken before migrating it, when one was written.
    pub backup: Option<PathBuf>,
    /// Active borrowed tasks the migration dropped (tasks always own their
    /// worktrees now). Their directories are left alone.
    pub dropped_borrowed: Vec<String>,
}

/// An active task folded from its events.
#[derive(Debug, Clone)]
struct TaskRecord {
    name: String,
    created_at: u64,
    /// Member ids, in join order.
    members: Vec<String>,
    /// Task-level events, kept for compaction, tagged with their order.
    events: Vec<(usize, LedgerEvent)>,
}

/// An active member folded from its events.
#[derive(Debug, Clone)]
struct MemberRecord {
    id: String,
    task: String,
    repo: String,
    repo_root: Option<PathBuf>,
    branch: String,
    dir: PathBuf,
    runs: Vec<RunInfo>,
    /// Member events, kept for compaction, tagged with their order.
    events: Vec<(usize, LedgerEvent)>,
}

/// Active tasks and members.
#[derive(Debug, Default)]
struct State {
    tasks: BTreeMap<String, TaskRecord>,
    members: HashMap<String, MemberRecord>,
    /// What reconciliation last observed of the members' worktrees, by member
    /// id. In memory only.
    observed: HashMap<String, Observed>,
}

/// What reconciliation observed of a member's worktree.
#[derive(Debug, Clone, Default)]
struct Observed {
    health: Option<MemberHealth>,
    /// The worktree's repository, as found from its directory: lets a gone
    /// worktree be recreated when the ledger does not record its repository.
    repo_root: Option<PathBuf>,
}

/// The daemon's view of the ledger: active tasks, plus the open file.
pub struct Ledger {
    path: PathBuf,
    file: File,
    _lock: File,
    state: State,
    seq: usize,
}

/// Default ledger path: `$XDG_DATA_HOME/tam/ledger.jsonl`
/// (`~/.local/share/tam/ledger.jsonl`).
pub fn default_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("tam")
        .join("ledger.jsonl")
}

/// Current Unix timestamp in seconds.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

impl Ledger {
    /// Open (creating if needed), lock, load, migrate and compact the ledger
    /// at `path`.
    pub fn open(path: &Path) -> Result<(Self, LoadReport)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let lock = lock_exclusive(&sibling(path, ".lock"))?;

        let mut report = LoadReport::default();
        let mut state = State::default();
        let mut migration = Migration::default();
        let mut seq = 0;
        if path.exists() {
            let file = File::open(path)
                .with_context(|| format!("failed to open ledger at {}", path.display()))?;
            for (idx, line) in BufReader::new(file).lines().enumerate() {
                let line = line.with_context(|| format!("failed to read {}", path.display()))?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let events = if let Ok(legacy) = serde_json::from_str::<LegacyEvent>(trimmed) {
                    migration.convert(legacy, &state)
                } else {
                    match serde_json::from_str::<LedgerEvent>(trimmed) {
                        Ok(event) => {
                            migration.observe(&event);
                            vec![event]
                        }
                        Err(e) => {
                            report.malformed.push(MalformedLine {
                                line: idx + 1,
                                content: trimmed.to_string(),
                                error: e.to_string(),
                            });
                            continue;
                        }
                    }
                };
                for event in events {
                    state.apply(seq, event);
                    seq += 1;
                }
            }
        }

        // `seq` counts the events read (after conversion); all were applied.
        let kept = state.kept();
        if migration.lines > 0 || !report.malformed.is_empty() || kept < seq {
            if migration.lines > 0 {
                // Never overwrite an earlier migration's backup: it holds older
                // history (`.pre-members`, then `.pre-members.1`, …).
                let backup = (0..)
                    .map(|n| match n {
                        0 => sibling(path, ".pre-members"),
                        n => sibling(path, &format!(".pre-members.{n}")),
                    })
                    .find(|p| !p.exists())
                    .expect("a free backup name");
                fs::copy(path, &backup).with_context(|| {
                    format!("failed to back up the ledger to {}", backup.display())
                })?;
                report.backup = Some(backup);
            }
            if !report.malformed.is_empty() {
                set_aside(&sibling(path, ".rejected"), &report.malformed)?;
            }
            rewrite(path, &state)?;
        }
        report.compacted = seq - kept + migration.orphans;
        report.migrated = migration.lines;
        report.dropped_borrowed = migration.borrowed.into_iter().collect();

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open ledger at {}", path.display()))?;

        Ok((
            Self {
                path: path.to_path_buf(),
                file,
                _lock: lock,
                state,
                seq,
            },
            report,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Active tasks, sorted by name.
    pub fn tasks(&self) -> Vec<TaskInfo> {
        self.state
            .tasks
            .values()
            .map(|t| self.state.info(t))
            .collect()
    }

    pub fn task(&self, name: &str) -> Option<TaskInfo> {
        self.state.tasks.get(name).map(|t| self.state.info(t))
    }

    /// Runs of all the task's members, oldest first.
    pub fn runs(&self, name: &str) -> Vec<RunInfo> {
        let mut runs: Vec<RunInfo> = self
            .member_ids(name)
            .iter()
            .flat_map(|id| self.member_runs(id))
            .collect();
        runs.sort_by_key(|r| r.started_at);
        runs
    }

    /// Ids of the task's members, in join order.
    pub fn member_ids(&self, task: &str) -> Vec<String> {
        self.state
            .tasks
            .get(task)
            .map(|t| t.members.clone())
            .unwrap_or_default()
    }

    /// A member and its task.
    pub fn member(&self, id: &str) -> Option<(TaskInfo, MemberInfo)> {
        let task = self.task(self.member_task(id)?)?;
        let member = task.member(id)?.clone();
        Some((task, member))
    }

    /// Name of the task a member belongs to.
    pub fn member_task(&self, id: &str) -> Option<&str> {
        self.state.members.get(id).map(|m| m.task.as_str())
    }

    /// Runs of a member, oldest first.
    pub fn member_runs(&self, id: &str) -> Vec<RunInfo> {
        self.state
            .members
            .get(id)
            .map(|m| m.runs.clone())
            .unwrap_or_default()
    }

    /// Whether the member has a run with no end recorded.
    pub fn has_open_run(&self, member: &str) -> bool {
        self.state
            .members
            .get(member)
            .and_then(|m| m.runs.last())
            .is_some_and(|r| r.ended_at.is_none())
    }

    /// The member's current session for `provider`: the session of its
    /// latest run of that agent that has one. A run's session is the one it
    /// resumed, or the last one its agent reported having a turn in.
    pub fn current_session(&self, member: &str, provider: &str) -> Option<String> {
        self.state
            .members
            .get(member)?
            .runs
            .iter()
            .rev()
            .filter(|r| r.provider == provider)
            .find_map(|r| r.session_id.clone())
    }

    /// The agent of the member's latest run.
    pub fn last_provider(&self, member: &str) -> Option<String> {
        let runs = &self.state.members.get(member)?.runs;
        runs.last().map(|r| r.provider.clone())
    }

    /// Find one member from what a user typed: a task name (its only
    /// member), `<task>/<member>` (short name) or a member id.
    ///
    /// Task names are branch names and may contain `/`: the whole target is
    /// tried as a task first, then split on its last `/`.
    pub fn resolve(&self, target: &str) -> Result<(TaskInfo, MemberInfo)> {
        if let Some(task) = self.task(target) {
            return match task.members.as_slice() {
                [member] => {
                    let member = member.clone();
                    Ok((task, member))
                }
                [] => bail!("task '{}' has no member", target),
                members => {
                    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
                    bail!(
                        "task '{}' has several members ({}): name one as {}/<member>",
                        target,
                        names.join(", "),
                        target
                    )
                }
            };
        }
        if let Some((task_name, member_name)) = target.rsplit_once('/') {
            if let Some(task) = self.task(task_name) {
                let Some(member) = task.members.iter().find(|m| m.name == member_name) else {
                    bail!("task '{}' has no member '{}'", task_name, member_name);
                };
                let member = member.clone();
                return Ok((task, member));
            }
        }
        if let Some(found) = self.member(target) {
            return Ok(found);
        }
        bail!("task '{}' not found", target)
    }

    /// The member whose worktree contains `dir` (the innermost one).
    pub fn member_at(&self, dir: &Path) -> Option<(TaskInfo, MemberInfo)> {
        let id = self
            .state
            .members
            .values()
            .filter(|m| dir.starts_with(&m.dir))
            .max_by_key(|m| m.dir.components().count())?
            .id
            .clone();
        self.member(&id)
    }

    /// Record a new task with its members.
    pub fn create_task(&mut self, name: &str, members: &[NewMember]) -> Result<TaskInfo> {
        if name.trim().is_empty() {
            bail!("task name cannot be empty");
        }
        if self.state.tasks.contains_key(name) {
            bail!("task '{}' already exists", name);
        }
        if members.is_empty() {
            bail!("task '{}' needs at least one member", name);
        }
        let timestamp = now();
        let mut events = vec![LedgerEvent::TaskCreated {
            name: name.into(),
            timestamp,
        }];
        let mut ids: Vec<String> = Vec::new();
        for (i, m) in members.iter().enumerate() {
            if let Some(existing) = self.state.members.values().find(|e| e.dir == m.dir) {
                bail!(
                    "directory {} already has an active task: '{}'",
                    m.dir.display(),
                    existing.task
                );
            }
            if members[..i].iter().any(|o| o.dir == m.dir) {
                bail!("directory {} is given twice", m.dir.display());
            }
            let Some(repo) = m.repo_root.file_name() else {
                bail!("invalid repository root {}", m.repo_root.display());
            };
            let id = new_member_id(|id| {
                self.state.members.contains_key(id) || ids.iter().any(|i| i == id)
            });
            ids.push(id.clone());
            events.push(LedgerEvent::MemberAdded {
                task: name.into(),
                member: id,
                repo: repo.to_string_lossy().into_owned(),
                repo_root: Some(m.repo_root.clone()),
                branch: m.branch.clone(),
                dir: m.dir.clone(),
                timestamp,
            });
        }
        self.append_all(events)?;
        Ok(self.task(name).expect("task just created"))
    }

    /// Record the start of a member's agent run. Closes a still-open
    /// previous run first, since a member runs one agent at a time.
    pub fn run_started(
        &mut self,
        member: &str,
        provider: &str,
        session_id: Option<String>,
    ) -> Result<TaskInfo> {
        let task = self.require_member(member)?;
        self.run_ended(member, -1)?;
        self.append(LedgerEvent::AgentRunStarted {
            member: member.into(),
            provider: provider.into(),
            session_id,
            timestamp: now(),
        })?;
        Ok(self.task(&task).expect("member's task exists"))
    }

    /// Record the session id of the member's open run, if it has none yet.
    /// Returns whether anything was recorded.
    pub fn session_identified(&mut self, member: &str, session_id: &str) -> Result<bool> {
        let needed = self
            .state
            .members
            .get(member)
            .and_then(|m| m.runs.last())
            .is_some_and(|r| r.ended_at.is_none() && r.session_id.is_none());
        if !needed {
            return Ok(false);
        }
        self.append(LedgerEvent::AgentSessionIdentified {
            member: member.into(),
            session_id: session_id.into(),
            timestamp: now(),
        })?;
        Ok(true)
    }

    /// Record a session the agent of the member's open run had a turn in:
    /// it becomes the run's session (identified, or changed from another
    /// one). Returns whether anything was recorded.
    pub fn session_reported(&mut self, member: &str, session_id: &str) -> Result<bool> {
        let Some(run) = self
            .state
            .members
            .get(member)
            .and_then(|m| m.runs.last())
            .filter(|r| r.ended_at.is_none())
        else {
            return Ok(false);
        };
        let event = match run.session_id.as_deref() {
            None => LedgerEvent::AgentSessionIdentified {
                member: member.into(),
                session_id: session_id.into(),
                timestamp: now(),
            },
            Some(current) if current == session_id => return Ok(false),
            Some(_) => LedgerEvent::AgentSessionChanged {
                member: member.into(),
                session_id: session_id.into(),
                timestamp: now(),
            },
        };
        self.append(event)?;
        Ok(true)
    }

    /// Record the end of the member's open run. Returns the updated task, or
    /// `None` when there was no open run (nothing recorded).
    pub fn run_ended(&mut self, member: &str, exit_code: i32) -> Result<Option<TaskInfo>> {
        if !self.has_open_run(member) {
            return Ok(None);
        }
        self.append(LedgerEvent::AgentRunEnded {
            member: member.into(),
            exit_code,
            timestamp: now(),
        })?;
        Ok(self.member_task(member).and_then(|t| self.task(t)))
    }

    /// Forget a task and its members. Their open runs, if any, are ended
    /// first.
    pub fn drop_task(&mut self, name: &str, worktree_deleted: bool) -> Result<()> {
        self.require(name)?;
        for member in self.member_ids(name) {
            self.run_ended(&member, -1)?;
        }
        if worktree_deleted {
            self.append(LedgerEvent::WorktreeDeleted {
                task: name.into(),
                timestamp: now(),
            })?;
        }
        self.append(LedgerEvent::TaskDropped {
            task: name.into(),
            timestamp: now(),
        })
    }

    fn require(&self, name: &str) -> Result<()> {
        if !self.state.tasks.contains_key(name) {
            bail!("task '{}' not found", name);
        }
        Ok(())
    }

    /// The member's task name; errors if the member does not exist.
    fn require_member(&self, member: &str) -> Result<String> {
        match self.member_task(member) {
            Some(task) => Ok(task.to_string()),
            None => bail!("member '{}' not found", member),
        }
    }

    fn append(&mut self, event: LedgerEvent) -> Result<()> {
        self.append_all(vec![event])
    }

    /// Write events to disk in one go, then fold them into memory.
    fn append_all(&mut self, events: Vec<LedgerEvent>) -> Result<()> {
        let mut json = String::new();
        for event in &events {
            json.push_str(&serde_json::to_string(event)?);
            json.push('\n');
        }
        self.file
            .write_all(json.as_bytes())
            .and_then(|_| self.file.flush())
            .with_context(|| format!("failed to append to ledger {}", self.path.display()))?;
        for event in events {
            self.state.apply(self.seq, event);
            self.seq += 1;
        }
        Ok(())
    }
}

impl State {
    /// Fold one event into the active tasks and members.
    fn apply(&mut self, seq: usize, event: LedgerEvent) {
        match &event {
            LedgerEvent::TaskCreated { name, timestamp } => {
                // A re-created name starts a fresh task: earlier history is dropped.
                let name = name.clone();
                let created_at = *timestamp;
                self.remove_task(&name);
                self.tasks.insert(
                    name.clone(),
                    TaskRecord {
                        name,
                        created_at,
                        members: Vec::new(),
                        events: vec![(seq, event)],
                    },
                );
            }
            LedgerEvent::MemberAdded {
                task,
                member,
                repo,
                repo_root,
                branch,
                dir,
                ..
            } => {
                if self.members.contains_key(member) {
                    return; // ids are unique: a second add is bogus
                }
                let Some(record) = self.tasks.get_mut(task) else {
                    return; // member of an unknown or dropped task
                };
                record.members.push(member.clone());
                let member = MemberRecord {
                    id: member.clone(),
                    task: task.clone(),
                    repo: repo.clone(),
                    repo_root: repo_root.clone(),
                    branch: branch.clone(),
                    dir: dir.clone(),
                    runs: Vec::new(),
                    events: Vec::new(),
                };
                let id = member.id.clone();
                self.members.insert(id.clone(), member);
                self.members.get_mut(&id).unwrap().events.push((seq, event));
            }
            LedgerEvent::TaskDropped { task, .. } => {
                let task = task.clone();
                self.remove_task(&task);
            }
            LedgerEvent::WorktreeDeleted { task, .. } => {
                if let Some(record) = self.tasks.get_mut(task) {
                    record.events.push((seq, event));
                }
            }
            LedgerEvent::AgentRunStarted { member, .. }
            | LedgerEvent::AgentSessionIdentified { member, .. }
            | LedgerEvent::AgentRunEnded { member, .. } => {
                let Some(record) = self.members.get_mut(member) else {
                    return; // event for an unknown or dropped member
                };
                match &event {
                    LedgerEvent::AgentRunStarted {
                        member,
                        provider,
                        session_id,
                        timestamp,
                    } => record.runs.push(RunInfo {
                        member: member.clone(),
                        provider: provider.clone(),
                        session_id: session_id.clone(),
                        started_at: *timestamp,
                        ended_at: None,
                        exit_code: None,
                    }),
                    LedgerEvent::AgentSessionIdentified { session_id, .. } => {
                        if let Some(run) = record.runs.last_mut() {
                            run.session_id.get_or_insert_with(|| session_id.clone());
                        }
                    }
                    LedgerEvent::AgentRunEnded {
                        exit_code,
                        timestamp,
                        ..
                    } => {
                        if let Some(run) = record.runs.last_mut().filter(|r| r.ended_at.is_none()) {
                            run.ended_at = Some(*timestamp);
                            run.exit_code = Some(*exit_code);
                        }
                    }
                    _ => unreachable!(),
                }
                record.events.push((seq, event));
            }
            LedgerEvent::AgentSessionChanged {
                member, session_id, ..
            } => {
                let Some(record) = self.members.get_mut(member) else {
                    return; // event for an unknown or dropped member
                };
                if let Some(run) = record.runs.last_mut() {
                    run.session_id = Some(session_id.clone());
                }
                record.events.push((seq, event));
            }
        }
    }

    /// Forget a task and its members.
    fn remove_task(&mut self, name: &str) {
        if let Some(record) = self.tasks.remove(name) {
            for id in &record.members {
                self.members.remove(id);
                self.observed.remove(id);
            }
        }
    }

    /// The task's summary, with its members' short names.
    fn info(&self, task: &TaskRecord) -> TaskInfo {
        let members: Vec<&MemberRecord> = task
            .members
            .iter()
            .filter_map(|id| self.members.get(id))
            .collect();
        let names = member_names(&members);
        let mut run_count = 0;
        let mut last_activity = task.created_at;
        for run in members.iter().flat_map(|m| &m.runs) {
            run_count += 1;
            last_activity = last_activity.max(run.ended_at.unwrap_or(run.started_at));
        }
        TaskInfo {
            name: task.name.clone(),
            members: members
                .iter()
                .zip(names)
                .map(|(m, name)| MemberInfo {
                    id: m.id.clone(),
                    name,
                    repo: m.repo.clone(),
                    repo_root: m.repo_root.clone(),
                    branch: m.branch.clone(),
                    dir: m.dir.clone(),
                    health: self.observed.get(&m.id).and_then(|o| o.health.clone()),
                })
                .collect(),
            created_at: task.created_at,
            run_count,
            last_activity: Some(last_activity),
        }
    }

    /// Number of events kept by the active tasks and members.
    fn kept(&self) -> usize {
        self.tasks.values().map(|t| t.events.len()).sum::<usize>()
            + self.members.values().map(|m| m.events.len()).sum::<usize>()
    }

    /// Events of the active tasks and members, in their original order.
    fn events(&self) -> Vec<&(usize, LedgerEvent)> {
        let mut events: Vec<&(usize, LedgerEvent)> = self
            .tasks
            .values()
            .flat_map(|t| t.events.iter())
            .chain(self.members.values().flat_map(|m| m.events.iter()))
            .collect();
        events.sort_by_key(|(seq, _)| *seq);
        events
    }
}

/// Members' worktrees as reconciliation observes them, and members joining an
/// existing task.
impl Ledger {
    /// Record what was observed of a member's worktree: its health, and its
    /// repository when found. Never written to the file. Returns the member's
    /// task when its health changed (`None` too for an unknown member).
    pub fn observe(
        &mut self,
        member: &str,
        health: MemberHealth,
        repo_root: Option<PathBuf>,
    ) -> Option<String> {
        let task = self.state.members.get(member)?.task.clone();
        let observed = self.state.observed.entry(member.to_string()).or_default();
        if repo_root.is_some() {
            observed.repo_root = repo_root;
        }
        let changed = observed.health.as_ref() != Some(&health);
        observed.health = Some(health);
        changed.then_some(task)
    }

    /// What was last observed of a member's worktree.
    pub fn health(&self, member: &str) -> Option<&MemberHealth> {
        self.state.observed.get(member)?.health.as_ref()
    }

    /// A member's repository: recorded, else last observed.
    pub fn repo_root(&self, member: &str) -> Option<PathBuf> {
        let recorded = self.state.members.get(member)?.repo_root.clone();
        recorded.or_else(|| self.state.observed.get(member)?.repo_root.clone())
    }

    /// Add a worktree to an existing task as a new member (it joins last).
    /// Returns the updated task.
    pub fn add_member(&mut self, task: &str, member: &NewMember) -> Result<TaskInfo> {
        self.require(task)?;
        if let Some(existing) = self.state.members.values().find(|e| e.dir == member.dir) {
            bail!(
                "directory {} already has an active task: '{}'",
                member.dir.display(),
                existing.task
            );
        }
        let Some(repo) = member.repo_root.file_name() else {
            bail!("invalid repository root {}", member.repo_root.display());
        };
        let id = new_member_id(|id| self.state.members.contains_key(id));
        self.append(LedgerEvent::MemberAdded {
            task: task.into(),
            member: id,
            repo: repo.to_string_lossy().into_owned(),
            repo_root: Some(member.repo_root.clone()),
            branch: member.branch.clone(),
            dir: member.dir.clone(),
            timestamp: now(),
        })?;
        Ok(self.task(task).expect("task exists"))
    }
}

/// Short names of a task's members, given in join order: the repository
/// name, or `<repo>-<branch>` when an earlier member has it (then a numeric
/// suffix). A member joining never renames the members before it.
fn member_names(members: &[&MemberRecord]) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(members.len());
    for m in members {
        // `/` separates the task from the member in `<task>/<member>`.
        let repo = m.repo.replace('/', "-");
        let mut name = repo.clone();
        if names.contains(&name) {
            name = format!("{}-{}", repo, m.branch.replace('/', "-"));
        }
        let base = name.clone();
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        names.push(name);
    }
    names
}

/// A fresh member id (`m-` and 8 hex digits) for which `taken` is false.
fn new_member_id(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("m-{:08x}", random_u32());
        if !taken(&id) {
            return id;
        }
    }
}

fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        return u32::from_ne_bytes(buf);
    }
    // No /dev/urandom: mix the clock, the pid and a counter.
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    nanos ^ std::process::id().rotate_left(16) ^ count.wrapping_mul(0x9E37_79B9)
}

/// Converts legacy lines to current events while the ledger is read.
#[derive(Debug, Default)]
struct Migration {
    /// Member id given to each active legacy task, by task name.
    members: HashMap<String, String>,
    /// Active borrowed tasks (no longer supported), dropped by the migration.
    borrowed: BTreeSet<String>,
    /// Legacy lines read.
    lines: usize,
    /// Legacy lines dropped because they named no task.
    orphans: usize,
}

impl Migration {
    /// The current events for a legacy line. An owned task becomes a task
    /// with one member: its worktree, on the branch named after the task
    /// (tam created both from the task name). A borrowed task is dropped.
    fn convert(&mut self, legacy: LegacyEvent, state: &State) -> Vec<LedgerEvent> {
        self.lines += 1;
        let (task, event): (String, Box<dyn FnOnce(String) -> LedgerEvent>) = match legacy {
            LegacyEvent::TaskCreated {
                name,
                dir,
                owned,
                timestamp,
            } => {
                self.forget(&name);
                if !owned {
                    // A re-created name replaced the earlier task: end it.
                    let replaced =
                        state
                            .tasks
                            .contains_key(&name)
                            .then(|| LedgerEvent::TaskDropped {
                                task: name.clone(),
                                timestamp,
                            });
                    self.borrowed.insert(name);
                    return replaced.into_iter().collect();
                }
                let member = new_member_id(|id| {
                    state.members.contains_key(id) || self.members.values().any(|m| m == id)
                });
                self.members.insert(name.clone(), member.clone());
                return vec![
                    LedgerEvent::TaskCreated {
                        name: name.clone(),
                        timestamp,
                    },
                    LedgerEvent::MemberAdded {
                        task: name.clone(),
                        member,
                        repo: legacy_repo_name(&dir, &name),
                        repo_root: None,
                        branch: name,
                        dir,
                        timestamp,
                    },
                ];
            }
            LegacyEvent::AgentRunStarted {
                task,
                provider,
                session_id,
                timestamp,
            } => (
                task,
                Box::new(move |member| LedgerEvent::AgentRunStarted {
                    member,
                    provider,
                    session_id,
                    timestamp,
                }),
            ),
            LegacyEvent::AgentSessionIdentified {
                task,
                session_id,
                timestamp,
            } => (
                task,
                Box::new(move |member| LedgerEvent::AgentSessionIdentified {
                    member,
                    session_id,
                    timestamp,
                }),
            ),
            LegacyEvent::AgentRunEnded {
                task,
                exit_code,
                timestamp,
            } => (
                task,
                Box::new(move |member| LedgerEvent::AgentRunEnded {
                    member,
                    exit_code,
                    timestamp,
                }),
            ),
        };
        match self.members.get(&task) {
            Some(member) => vec![event(member.clone())],
            None => {
                // A borrowed task's run goes with it; anything else is an orphan.
                if !self.borrowed.contains(&task) {
                    self.orphans += 1;
                }
                Vec::new()
            }
        }
    }

    /// Track a current-format event that ends a task name's legacy life.
    fn observe(&mut self, event: &LedgerEvent) {
        match event {
            LedgerEvent::TaskCreated { name: task, .. } | LedgerEvent::TaskDropped { task, .. } => {
                self.forget(task)
            }
            _ => {}
        }
    }

    fn forget(&mut self, task: &str) {
        self.members.remove(task);
        self.borrowed.remove(task);
    }
}

/// Repository name of a legacy owned task's worktree, from its directory:
/// tam named it `<repo>--<task>` (a `/` in the task name nests it). Falls
/// back to the directory name.
fn legacy_repo_name(dir: &Path, task: &str) -> String {
    let full = dir.to_string_lossy();
    let base = match full.strip_suffix(&format!("--{task}")) {
        Some(prefix) => Path::new(prefix).file_name(),
        None => dir.file_name(),
    };
    base.map(|b| b.to_string_lossy().into_owned())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| task.to_string())
}

/// How long to wait for the ledger lock before giving up. A lock can outlive
/// its daemon briefly: a child forked just before exit holds the fd until it
/// execs, and a restarting daemon may race the old one's exit.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Take an exclusive `flock` on `path`, held while the returned file stays
/// open. Retries for up to [`LOCK_WAIT`].
fn lock_exclusive(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open ledger lock {}", path.display()))?;
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: flock on a valid, owned fd.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err).with_context(|| format!("failed to lock {}", path.display()));
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "ledger {} is locked by another tam daemon",
                path.with_extension("").display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Append malformed lines to the side file so compaction never loses them.
fn set_aside(path: &Path, malformed: &[MalformedLine]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    for m in malformed {
        writeln!(file, "{}", m.content)?;
    }
    file.sync_all()?;
    Ok(())
}

/// Atomically replace the ledger with the events of the active tasks and
/// members, in their original order.
fn rewrite(path: &Path, state: &State) -> Result<()> {
    let tmp = sibling(path, ".tmp");
    {
        let mut file =
            File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
        for (_, event) in state.events() {
            let json = serde_json::to_string(event)?;
            writeln!(file, "{json}")?;
        }
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to replace ledger {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open(tmp: &TempDir) -> Ledger {
        Ledger::open(&tmp.path().join("ledger.jsonl")).unwrap().0
    }

    fn write_lines(path: &Path, events: &[LedgerEvent], extra: &[&str]) {
        let mut out = String::new();
        for e in events {
            out.push_str(&serde_json::to_string(e).unwrap());
            out.push('\n');
        }
        for l in extra {
            out.push_str(l);
            out.push('\n');
        }
        fs::write(path, out).unwrap();
    }

    fn new_member(repo: &str, branch: &str, dir: &str) -> NewMember {
        NewMember {
            repo_root: PathBuf::from("/src").join(repo),
            branch: branch.into(),
            dir: dir.into(),
        }
    }

    /// Create a single-member task in repo `app`; returns its member id.
    fn create(ledger: &mut Ledger, name: &str, dir: &str) -> String {
        let task = ledger
            .create_task(name, &[new_member("app", name, dir)])
            .unwrap();
        task.members[0].id.clone()
    }

    fn created(name: &str, ts: u64) -> LedgerEvent {
        LedgerEvent::TaskCreated {
            name: name.into(),
            timestamp: ts,
        }
    }

    fn member_added(task: &str, member: &str, dir: &str, ts: u64) -> LedgerEvent {
        LedgerEvent::MemberAdded {
            task: task.into(),
            member: member.into(),
            repo: "app".into(),
            repo_root: None,
            branch: task.into(),
            dir: dir.into(),
            timestamp: ts,
        }
    }

    fn event_types(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn empty_ledger() {
        let tmp = TempDir::new().unwrap();
        let ledger = open(&tmp);
        assert!(ledger.tasks().is_empty());
    }

    #[test]
    fn create_and_reload() {
        let tmp = TempDir::new().unwrap();
        let id = {
            let mut ledger = open(&tmp);
            let info = ledger
                .create_task("feat", &[new_member("app", "feat", "/w/app--feat")])
                .unwrap();
            assert_eq!(info.name, "feat");
            assert_eq!(info.run_count, 0);
            let [member] = info.members.as_slice() else {
                panic!("one member expected: {info:?}");
            };
            assert!(member.id.starts_with("m-"), "{}", member.id);
            assert_eq!(member.name, "app");
            assert_eq!(member.repo, "app");
            assert_eq!(member.repo_root.as_deref(), Some(Path::new("/src/app")));
            assert_eq!(member.branch, "feat");
            assert_eq!(member.dir, Path::new("/w/app--feat"));
            member.id.clone()
        };
        let ledger = open(&tmp);
        assert_eq!(ledger.tasks().len(), 1);
        // The member id is stable across reloads.
        let (task, member) = ledger.member(&id).unwrap();
        assert_eq!(task.name, "feat");
        assert_eq!(member.dir, Path::new("/w/app--feat"));
        assert_eq!(ledger.member_task(&id), Some("feat"));
    }

    #[test]
    fn create_rejects_duplicates() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        create(&mut ledger, "feat", "/a");
        let err = ledger.create_task("feat", &[new_member("app", "x", "/b")]);
        assert!(err.unwrap_err().to_string().contains("already exists"));
        let err = ledger.create_task("other", &[new_member("app", "other", "/a")]);
        assert!(err.unwrap_err().to_string().contains("already has"));
        let err = ledger.create_task(
            "two",
            &[new_member("a", "two", "/c"), new_member("b", "two", "/c")],
        );
        assert!(err.unwrap_err().to_string().contains("twice"));
        assert!(ledger
            .create_task(" ", &[new_member("app", "x", "/d")])
            .is_err());
        let err = ledger.create_task("empty", &[]);
        assert!(err.unwrap_err().to_string().contains("at least one member"));
        assert_eq!(ledger.tasks().len(), 1);
    }

    #[test]
    fn member_names_use_repo_then_repo_and_branch() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let task = ledger
            .create_task(
                "feat",
                &[
                    new_member("backend", "feat", "/w/1"),
                    new_member("frontend", "feat", "/w/2"),
                    new_member("backend", "fix/auth", "/w/3"),
                    new_member("backend-fix-auth", "fix/auth", "/w/4"),
                ],
            )
            .unwrap();
        let names: Vec<&str> = task.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "backend",
                "frontend",
                "backend-fix-auth",
                "backend-fix-auth-fix-auth"
            ]
        );
        // Same name, same branch, in two different places: numbered.
        let one = MemberRecord {
            id: "m-1".into(),
            task: "t".into(),
            repo: "api".into(),
            repo_root: None,
            branch: "main".into(),
            dir: "/1".into(),
            runs: vec![],
            events: vec![],
        };
        let two = MemberRecord {
            id: "m-2".into(),
            dir: "/2".into(),
            ..one.clone()
        };
        let three = MemberRecord {
            id: "m-3".into(),
            dir: "/3".into(),
            ..one.clone()
        };
        assert_eq!(
            member_names(&[&one, &two, &three]),
            ["api", "api-main", "api-main-2"]
        );
    }

    #[test]
    fn resolve_targets() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let feat = create(&mut ledger, "feat", "/w/app--feat");
        let nested = create(&mut ledger, "fix/x", "/w/app--fix/x");
        let multi = ledger
            .create_task(
                "multi",
                &[
                    new_member("api", "multi", "/w/api--multi"),
                    new_member("web", "multi", "/w/web--multi"),
                ],
            )
            .unwrap();

        // A task name: its only member.
        assert_eq!(ledger.resolve("feat").unwrap().1.id, feat);
        // Task names may contain '/'.
        assert_eq!(ledger.resolve("fix/x").unwrap().1.id, nested);
        assert_eq!(ledger.resolve("fix/x/app").unwrap().1.id, nested);
        // Qualified name, and member id.
        let (task, web) = ledger.resolve("multi/web").unwrap();
        assert_eq!(task.name, "multi");
        assert_eq!(web.id, multi.members[1].id);
        assert_eq!(ledger.resolve(&web.id).unwrap().1.name, "web");
        assert_eq!(ledger.resolve("feat/app").unwrap().1.id, feat);

        let err = ledger.resolve("multi").unwrap_err().to_string();
        assert!(err.contains("several members (api, web)"), "{err}");
        let err = ledger.resolve("multi/db").unwrap_err().to_string();
        assert!(err.contains("no member 'db'"), "{err}");
        let err = ledger.resolve("nope").unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn member_at_finds_the_enclosing_worktree() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let feat = create(&mut ledger, "feat", "/w/app--feat");
        assert_eq!(
            ledger.member_at(Path::new("/w/app--feat")).unwrap().1.id,
            feat
        );
        assert_eq!(
            ledger
                .member_at(Path::new("/w/app--feat/src/lib"))
                .unwrap()
                .1
                .id,
            feat
        );
        // Component-wise: a sibling sharing a prefix does not match.
        assert!(ledger.member_at(Path::new("/w/app--feature")).is_none());
        assert!(ledger.member_at(Path::new("/w")).is_none());
    }

    #[test]
    fn run_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let m = create(&mut ledger, "feat", "/a");

        let info = ledger.run_started(&m, "claude", None).unwrap();
        assert_eq!(info.run_count, 1);
        assert!(ledger.has_open_run(&m));

        assert!(ledger.session_identified(&m, "s1").unwrap());
        // Already known: not recorded twice.
        assert!(!ledger.session_identified(&m, "s2").unwrap());

        assert!(ledger.run_ended(&m, 0).unwrap().is_some());
        // No open run left: nothing to record.
        assert!(ledger.run_ended(&m, 0).unwrap().is_none());
        assert!(!ledger.session_identified(&m, "s3").unwrap());

        let runs = ledger.runs("feat");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].member, m);
        assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
        assert_eq!(runs[0].exit_code, Some(0));
        assert!(runs[0].ended_at.is_some());
        assert_eq!(ledger.member_runs(&m), runs);
    }

    #[test]
    fn runs_are_kept_per_member() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let task = ledger
            .create_task(
                "multi",
                &[
                    new_member("api", "multi", "/w/1"),
                    new_member("web", "multi", "/w/2"),
                ],
            )
            .unwrap();
        let (api, web) = (&task.members[0].id, &task.members[1].id);
        ledger.run_started(api, "claude", None).unwrap();
        let info = ledger.run_started(web, "codex", None).unwrap();
        // Starting one member's run does not close the other's.
        assert!(ledger.has_open_run(api) && ledger.has_open_run(web));
        assert_eq!(info.run_count, 2);
        assert_eq!(ledger.member_runs(web)[0].provider, "codex");
        assert_eq!(ledger.runs("multi").len(), 2);

        ledger.drop_task("multi", false).unwrap();
        assert!(!ledger.has_open_run(api) && !ledger.has_open_run(web));
        assert!(ledger.member(api).is_none());
    }

    #[test]
    fn run_started_closes_dangling_run() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let m = create(&mut ledger, "feat", "/a");
        ledger.run_started(&m, "claude", Some("s1".into())).unwrap();
        ledger.run_started(&m, "codex", None).unwrap();
        let runs = ledger.runs("feat");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].exit_code, Some(-1));
        assert!(runs[1].ended_at.is_none());
    }

    #[test]
    fn run_requires_member() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        create(&mut ledger, "feat", "/a");
        // A task name is not a member id.
        assert!(ledger.run_started("feat", "claude", None).is_err());
        assert!(ledger.run_started("nope", "claude", None).is_err());
        assert!(ledger.drop_task("nope", false).is_err());
    }

    #[test]
    fn drop_ends_open_run_and_forgets_task() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        {
            let mut ledger = Ledger::open(&path).unwrap().0;
            let m = create(&mut ledger, "feat", "/a");
            ledger.run_started(&m, "claude", None).unwrap();
            ledger.drop_task("feat", true).unwrap();
            assert!(ledger.tasks().is_empty());
            assert!(ledger.runs("feat").is_empty());
            assert!(ledger.member(&m).is_none());
        }
        assert_eq!(
            event_types(&path),
            [
                "task_created",
                "member_added",
                "agent_run_started",
                "agent_run_ended",
                "worktree_deleted",
                "task_dropped"
            ]
        );
    }

    #[test]
    fn recreated_name_starts_fresh() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let old = create(&mut ledger, "feat", "/a");
        ledger
            .run_started(&old, "claude", Some("old".into()))
            .unwrap();
        ledger.drop_task("feat", false).unwrap();
        let info = ledger
            .create_task("feat", &[new_member("app", "feat", "/b")])
            .unwrap();
        assert_eq!(info.run_count, 0);
        assert_ne!(info.members[0].id, old);
        assert!(ledger.runs("feat").is_empty());
    }

    #[test]
    fn compaction_removes_dropped_tasks_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        write_lines(
            &path,
            &[
                created("gone", 1),
                member_added("gone", "m-g", "/g", 1),
                created("keep", 2),
                member_added("keep", "m-k", "/k", 2),
                LedgerEvent::AgentRunStarted {
                    member: "m-g".into(),
                    provider: "claude".into(),
                    session_id: None,
                    timestamp: 3,
                },
                LedgerEvent::AgentRunStarted {
                    member: "m-k".into(),
                    provider: "claude".into(),
                    session_id: Some("s".into()),
                    timestamp: 4,
                },
                LedgerEvent::TaskDropped {
                    task: "gone".into(),
                    timestamp: 5,
                },
                // Orphans: no such member, no such task.
                LedgerEvent::AgentRunEnded {
                    member: "m-ghost".into(),
                    exit_code: 0,
                    timestamp: 6,
                },
                member_added("ghost", "m-x", "/x", 7),
            ],
            &[],
        );

        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.compacted, 6);
        assert_eq!(report.migrated, 0);
        assert!(report.backup.is_none());
        assert!(report.malformed.is_empty());
        assert_eq!(ledger.tasks().len(), 1);
        assert_eq!(ledger.task("keep").unwrap().run_count, 1);
        assert_eq!(ledger.task("keep").unwrap().last_activity, Some(4));
        drop(ledger);

        assert_eq!(
            event_types(&path),
            ["task_created", "member_added", "agent_run_started"]
        );
        // A compacted ledger is left alone on the next open.
        let before = fs::read_to_string(&path).unwrap();
        let (_, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.compacted, 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn malformed_lines_are_reported_and_set_aside() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        write_lines(
            &path,
            &[created("keep", 1), member_added("keep", "m-k", "/k", 1)],
            &["not json", r#"{"type":"from_the_future","task":"keep"}"#],
        );

        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.malformed.len(), 2);
        assert_eq!(report.malformed[0].line, 3);
        assert_eq!(report.malformed[0].content, "not json");
        assert_eq!(report.compacted, 0);
        assert_eq!(ledger.tasks().len(), 1);
        drop(ledger);

        let rejected = fs::read_to_string(tmp.path().join("ledger.jsonl.rejected")).unwrap();
        assert_eq!(
            rejected,
            "not json\n{\"type\":\"from_the_future\",\"task\":\"keep\"}\n"
        );
        assert_eq!(event_types(&path), ["task_created", "member_added"]);
    }

    #[test]
    fn second_open_is_locked_out() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let first = Ledger::open(&path).unwrap();
        let err = Ledger::open(&path).err().unwrap();
        assert!(err.to_string().contains("locked"), "{err}");
        drop(first);
        assert!(Ledger::open(&path).is_ok());
    }

    /// Lines as written by tam <= 0.3.6 (the CLI-owned ledger) and by the
    /// first daemon-owned ledger: every legacy event type.
    const LEGACY: &str = concat!(
        r#"{"type":"task_created","name":"feat","dir":"/w/myapp--feat","owned":true,"timestamp":100}"#,
        "\n",
        r#"{"type":"agent_run_started","task":"feat","provider":"claude","timestamp":200}"#,
        "\n",
        r#"{"type":"agent_session_identified","task":"feat","session_id":"s1","timestamp":210}"#,
        "\n",
        r#"{"type":"agent_run_ended","task":"feat","exit_code":-1,"timestamp":300}"#,
        "\n",
        r#"{"type":"agent_run_started","task":"feat","provider":"codex","session_id":"s2","timestamp":400}"#,
        "\n",
        // Borrowed, still active: dropped by the migration.
        r#"{"type":"task_created","name":"here","dir":"/src/myapp","owned":false,"timestamp":500}"#,
        "\n",
        r#"{"type":"agent_run_started","task":"here","provider":"claude","timestamp":510}"#,
        "\n",
        // Borrowed, then dropped: nothing to report.
        r#"{"type":"task_created","name":"past","dir":"/src/other","owned":false,"timestamp":520}"#,
        "\n",
        r#"{"type":"task_dropped","task":"past","timestamp":530}"#,
        "\n",
        // Owned, worktree deleted and dropped.
        r#"{"type":"task_created","name":"done","dir":"/w/myapp--done","owned":true,"timestamp":600}"#,
        "\n",
        r#"{"type":"worktree_deleted","task":"done","timestamp":610}"#,
        "\n",
        r#"{"type":"task_dropped","task":"done","timestamp":620}"#,
        "\n",
        // Owned, then re-created borrowed without a drop: the borrowed task
        // replaced the owned one, so it goes too.
        r#"{"type":"task_created","name":"redo","dir":"/w/myapp--redo","owned":true,"timestamp":630}"#,
        "\n",
        r#"{"type":"agent_run_started","task":"redo","provider":"claude","timestamp":631}"#,
        "\n",
        r#"{"type":"task_created","name":"redo","dir":"/w/myapp--redo","owned":false,"timestamp":640}"#,
        "\n",
        // Owned, with a '/' in its name and the worktree gone: kept.
        r#"{"type":"task_created","name":"fix/x","dir":"/w/api--fix/x","owned":true,"timestamp":700}"#,
        "\n",
        // Orphan: never created.
        r#"{"type":"agent_run_ended","task":"ghost","exit_code":0,"timestamp":800}"#,
        "\n",
    );

    #[test]
    fn migrates_legacy_ledger() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        fs::write(&path, LEGACY).unwrap();

        let (ledger, report) = Ledger::open(&path).unwrap();
        assert!(report.malformed.is_empty(), "{:?}", report.malformed);
        // `task_dropped` and `worktree_deleted` kept their shape: not legacy.
        assert_eq!(report.migrated, 14);
        assert_eq!(report.dropped_borrowed, ["here", "redo"]);
        let backup = tmp.path().join("ledger.jsonl.pre-members");
        assert_eq!(report.backup.as_deref(), Some(backup.as_path()));
        assert_eq!(fs::read_to_string(&backup).unwrap(), LEGACY);

        let names: Vec<String> = ledger.tasks().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["feat", "fix/x"]);

        // Owned tasks load as single-member tasks.
        let feat = ledger.task("feat").unwrap();
        assert_eq!(feat.created_at, 100);
        assert_eq!(feat.run_count, 2);
        assert_eq!(feat.last_activity, Some(400));
        let [m] = feat.members.as_slice() else {
            panic!("{feat:?}");
        };
        assert_eq!(m.repo, "myapp");
        assert_eq!(m.name, "myapp");
        assert_eq!(m.branch, "feat");
        assert_eq!(m.dir, Path::new("/w/myapp--feat"));
        assert!(m.repo_root.is_none());
        let runs = ledger.member_runs(&m.id);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
        assert_eq!(runs[0].exit_code, Some(-1));
        assert_eq!(runs[1].provider, "codex");
        assert!(ledger.has_open_run(&m.id));

        let fix = ledger.task("fix/x").unwrap();
        assert_eq!(fix.members[0].repo, "api");
        assert_eq!(fix.members[0].branch, "fix/x");

        // Rewritten in the current format, with nothing legacy left.
        let feat_id = m.id.clone();
        drop(ledger);
        assert_eq!(
            event_types(&path),
            [
                "task_created",
                "member_added",
                "agent_run_started",
                "agent_session_identified",
                "agent_run_ended",
                "agent_run_started",
                "task_created",
                "member_added",
            ]
        );
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("\"owned\""), "{raw}");

        // Member ids are stable, and a migrated ledger is left alone.
        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.migrated, 0);
        assert_eq!(report.compacted, 0);
        assert!(report.backup.is_none());
        assert_eq!(ledger.task("feat").unwrap().members[0].id, feat_id);
        assert_eq!(fs::read_to_string(&path).unwrap(), raw);
    }

    #[test]
    fn legacy_lines_appended_later_are_migrated_too() {
        // E.g. an older tam ran against an already migrated ledger.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let backup = tmp.path().join("ledger.jsonl.pre-members");
        fs::write(&backup, "earlier backup\n").unwrap();
        let mut lines = String::new();
        for e in [
            created("new", 1),
            member_added("new", "m-n", "/w/a--new", 1),
        ] {
            lines.push_str(&serde_json::to_string(&e).unwrap());
            lines.push('\n');
        }
        lines.push_str(
            r#"{"type":"task_created","name":"old","dir":"/w/a--old","owned":true,"timestamp":2}"#,
        );
        lines.push('\n');
        lines.push_str(
            r#"{"type":"agent_run_started","task":"old","provider":"claude","timestamp":3}"#,
        );
        lines.push('\n');
        fs::write(&path, &lines).unwrap();

        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.migrated, 2);
        // The earlier backup is kept, this one goes next to it.
        assert_eq!(fs::read_to_string(&backup).unwrap(), "earlier backup\n");
        let second = tmp.path().join("ledger.jsonl.pre-members.1");
        assert_eq!(report.backup.as_deref(), Some(second.as_path()));
        assert_eq!(fs::read_to_string(&second).unwrap(), lines);
        assert_eq!(ledger.tasks().len(), 2);
        assert_eq!(ledger.task("old").unwrap().run_count, 1);
        assert_eq!(ledger.member("m-n").unwrap().0.name, "new");
    }

    fn ok(branch: &str) -> MemberHealth {
        MemberHealth::Ok {
            branch: Some(branch.into()),
        }
    }

    #[test]
    fn observed_health_shows_in_task_info_and_is_never_written() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let m = create(&mut ledger, "feat", "/w/app--feat");
        assert_eq!(ledger.task("feat").unwrap().members[0].health, None);

        // First observation, then a change: the task is returned.
        assert_eq!(
            ledger.observe(&m, ok("feat"), None).as_deref(),
            Some("feat")
        );
        assert_eq!(ledger.observe(&m, ok("feat"), None), None, "unchanged");
        let gone = MemberHealth::Gone {
            cannot_repair: None,
        };
        assert_eq!(
            ledger.observe(&m, gone.clone(), None).as_deref(),
            Some("feat")
        );
        assert_eq!(ledger.health(&m), Some(&gone));
        assert_eq!(ledger.task("feat").unwrap().members[0].health, Some(gone));
        assert_eq!(
            ledger.member(&m).unwrap().1.health,
            ledger.health(&m).cloned()
        );
        // Unknown members are ignored.
        assert_eq!(ledger.observe("m-nope", ok("x"), None), None);

        let before = fs::read_to_string(&path).unwrap();
        assert!(
            !before.contains("health") && !before.contains("gone"),
            "{before}"
        );
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap().0;
        assert_eq!(ledger.health(&m), None, "not persisted");
    }

    #[test]
    fn observed_repo_root_fills_in_for_unrecorded_ones() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        // A migrated member: no repository recorded.
        write_lines(
            &path,
            &[
                created("old", 1),
                member_added("old", "m-o", "/w/a--old", 1),
            ],
            &[],
        );
        let mut ledger = Ledger::open(&path).unwrap().0;
        assert_eq!(ledger.repo_root("m-o"), None);
        ledger.observe("m-o", ok("old"), Some(PathBuf::from("/src/a")));
        // A later observation that could not find it keeps the last one.
        ledger.observe(
            "m-o",
            MemberHealth::Gone {
                cannot_repair: None,
            },
            None,
        );
        assert_eq!(ledger.repo_root("m-o"), Some(PathBuf::from("/src/a")));
        // The member's info still shows what the ledger records.
        assert_eq!(ledger.member("m-o").unwrap().1.repo_root, None);

        let m = create(&mut ledger, "feat", "/w/app--feat");
        ledger.observe(&m, ok("feat"), Some(PathBuf::from("/elsewhere")));
        assert_eq!(
            ledger.repo_root(&m),
            Some(PathBuf::from("/src/app")),
            "recorded wins"
        );
    }

    #[test]
    fn dropping_a_task_forgets_its_observations() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let m = create(&mut ledger, "feat", "/a");
        ledger.observe(&m, ok("feat"), Some(PathBuf::from("/src/app")));
        ledger.drop_task("feat", false).unwrap();
        assert!(ledger.state.observed.is_empty());
        assert_eq!(ledger.repo_root(&m), None);
    }

    #[test]
    fn add_member_joins_an_existing_task() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let first = create(&mut ledger, "feat", "/w/app--feat");
        let task = ledger
            .add_member("feat", &new_member("web", "feat", "/w/web--feat"))
            .unwrap();
        assert_eq!(task.members.len(), 2);
        assert_eq!(task.members[0].id, first);
        let web = task.members[1].clone();
        assert_eq!(web.name, "web");
        assert_eq!(web.repo_root.as_deref(), Some(Path::new("/src/web")));
        assert_eq!(ledger.member(&web.id).unwrap().0.name, "feat");

        let err = ledger.add_member("feat", &new_member("web", "x", "/w/web--feat"));
        assert!(err.unwrap_err().to_string().contains("already has"));
        let err = ledger.add_member("nope", &new_member("api", "x", "/w/api--x"));
        assert!(err.unwrap_err().to_string().contains("not found"));

        drop(ledger);
        assert_eq!(
            event_types(&path),
            ["task_created", "member_added", "member_added"]
        );
        let ledger = Ledger::open(&path).unwrap().0;
        assert_eq!(ledger.task("feat").unwrap().members[1].id, web.id);
    }

    #[test]
    fn legacy_repo_names() {
        assert_eq!(
            legacy_repo_name(Path::new("/w/my-app--feat"), "feat"),
            "my-app"
        );
        assert_eq!(legacy_repo_name(Path::new("/w/api--fix/x"), "fix/x"), "api");
        assert_eq!(
            legacy_repo_name(Path::new("/w/elsewhere"), "feat"),
            "elsewhere"
        );
        assert_eq!(legacy_repo_name(Path::new("/"), "feat"), "feat");
    }

    #[test]
    fn reported_sessions_follow_the_open_run() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let m = create(&mut ledger, "feat", "/a");
        assert_eq!(ledger.current_session(&m, "claude"), None);

        ledger.run_started(&m, "claude", None).unwrap();
        assert!(ledger.session_reported(&m, "s1").unwrap());
        assert!(!ledger.session_reported(&m, "s1").unwrap());
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s1"));
        // e.g. `/clear`, then a turn in the new session
        assert!(ledger.session_reported(&m, "s2").unwrap());
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s2"));
        ledger.run_ended(&m, 0).unwrap();
        // No open run: nothing to record.
        assert!(!ledger.session_reported(&m, "s3").unwrap());

        let runs = ledger.member_runs(&m);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].session_id.as_deref(), Some("s2"));

        // Kept as is when the ledger is opened again.
        drop(ledger);
        assert_eq!(
            event_types(&path),
            [
                "task_created",
                "member_added",
                "agent_run_started",
                "agent_session_identified",
                "agent_session_changed",
                "agent_run_ended",
            ]
        );
        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.compacted, 0);
        assert!(report.malformed.is_empty(), "{:?}", report.malformed);
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s2"));
    }

    #[test]
    fn current_session_is_the_latest_one_of_the_agent() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let m = create(&mut ledger, "feat", "/a");

        ledger.run_started(&m, "claude", None).unwrap();
        ledger.session_reported(&m, "s1").unwrap();
        // A new session nobody had a turn in leaves the current one.
        ledger.run_started(&m, "claude", None).unwrap();
        ledger.run_ended(&m, 0).unwrap();
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s1"));

        // Another agent has its own.
        ledger.run_started(&m, "codex", Some("x1".into())).unwrap();
        ledger.run_ended(&m, 0).unwrap();
        assert_eq!(ledger.last_provider(&m).as_deref(), Some("codex"));
        assert_eq!(ledger.current_session(&m, "codex").as_deref(), Some("x1"));
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s1"));

        // A resumed session is the run's session from its start.
        ledger.run_started(&m, "claude", Some("s0".into())).unwrap();
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s0"));
        assert_eq!(ledger.current_session("m-nope", "claude"), None);
        assert_eq!(ledger.last_provider("m-nope"), None);
    }

    #[test]
    fn ledgers_from_before_current_sessions_resume_the_last_one() {
        // Written by a tam whose picker chose the session: a fresh run, then
        // one resuming an older session.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let run = |provider: &str, session: Option<&str>, ts| LedgerEvent::AgentRunStarted {
            member: "m-1".into(),
            provider: provider.into(),
            session_id: session.map(str::to_string),
            timestamp: ts,
        };
        let ended = |ts| LedgerEvent::AgentRunEnded {
            member: "m-1".into(),
            exit_code: 0,
            timestamp: ts,
        };
        write_lines(
            &path,
            &[
                created("feat", 100),
                member_added("feat", "m-1", "/a", 100),
                run("claude", None, 200),
                LedgerEvent::AgentSessionIdentified {
                    member: "m-1".into(),
                    session_id: "s1".into(),
                    timestamp: 201,
                },
                ended(300),
                run("claude", Some("s0"), 400),
                ended(500),
            ],
            &[],
        );
        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.migrated, 0);
        assert_eq!(report.compacted, 0);
        assert_eq!(
            ledger.current_session("m-1", "claude").as_deref(),
            Some("s0")
        );
    }
}
