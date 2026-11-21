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
//! `<ledger>.rejected`, and the file is compacted: events of dropped tasks and
//! closed chats (and events naming no live task, member or chat) are removed,
//! keeping only the history of the active tasks and open chats. State is never
//! stored — it is folded from the events.
//!
//! Ledgers written before tasks had members are migrated on open (see
//! [`LegacyEvent`]): the old file is kept as `<ledger>.pre-members` and the
//! ledger is rewritten in the current format.
//!
//! Next to the folded tasks, the ledger keeps what reconciliation observed of
//! the members' worktrees (their health, their repository): in memory only,
//! never written, like any other derived state.
//!
//! Chats are the other kind of object: a Claude session on a repository's
//! main checkout, owning nothing on disk. A chat has a stable id (`c-…`) of
//! its own and keeps what a member keeps of its agent ([`History`]: runs,
//! sessions, messages), recorded with the same events, keyed by its id.
//! Closing a chat forgets it, with its history.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tam_proto::{
    AgentResponse, ChatInfo, Event, MemberHealth, MemberInfo, MessageInfo, NewMember, RunInfo,
    TaskInfo,
};

/// A single event recorded in the ledger.
///
/// The agent events (`AgentRun…`, `AgentSession…`) and the message events
/// name a task member or a chat in their `member` (`to`, `from`) field: the
/// id of what the agent runs for.
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
        /// The response that ended the run's last turn (`tam result`), when
        /// its agent had one at the end.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<AgentResponse>,
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
    /// A message to a member's agent. Kept with the recipient member.
    MessageSent {
        id: String,
        /// Sending member; absent when the user sent it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// Recipient member.
        to: String,
        text: String,
        #[serde(default)]
        hop: u32,
        /// The message this one answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        timestamp: u64,
    },
    /// The recipient's agent got the message.
    MessageDelivered {
        id: String,
        /// Recipient member.
        to: String,
        timestamp: u64,
    },
    /// A member left its task, which keeps its other members (`tam drop
    /// TASK/MEMBER`): its worktree is gone, or forgotten. Its history goes
    /// with it.
    MemberDropped {
        member: String,
        timestamp: u64,
    },
    /// The task's brief changed: a reference the user gave (a URL, a ticket
    /// id, a path), or `None` for the local file `brief.md` (the default).
    /// Only the latest is kept.
    BriefSet {
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brief: Option<String>,
        timestamp: u64,
    },
    /// A member moved to another task (`tam merge`, `tam split`) with its
    /// worktree, runs, sessions and messages. It joins that task last.
    MemberMoved {
        member: String,
        task: String,
        timestamp: u64,
    },
    /// A chat on the repository whose main checkout is `dir`.
    ChatCreated {
        /// Stable chat id (`c-…`), its agent's id.
        chat: String,
        dir: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The Claude session it was made from, resumed by its first agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        timestamp: u64,
    },
    /// The chat's name changed (`None`: unnamed). Only the latest is kept.
    ChatRenamed {
        chat: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        timestamp: u64,
    },
    /// The chat is kept (never closed for want of turns), or no longer.
    /// Only the latest is kept.
    ChatKept {
        chat: String,
        keep: bool,
        timestamp: u64,
    },
    /// The user prompted the chat's agent. Only the latest is kept.
    ChatTurn {
        chat: String,
        timestamp: u64,
    },
    /// The chat was closed: forgotten, with its history.
    ChatClosed {
        chat: String,
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
    history: History,
}

/// What a task member or a chat keeps of its agent, and its own events.
#[derive(Debug, Clone, Default)]
struct History {
    runs: Vec<RunInfo>,
    /// The response that ended the last turn of the latest run, once the
    /// run is over (see `Ledger::last_response`).
    response: Option<AgentResponse>,
    /// Its events, kept for compaction, tagged with their order.
    events: Vec<(usize, LedgerEvent)>,
    /// Messages to it, in the order they were sent.
    messages: Vec<MessageRecord>,
}

impl History {
    /// Keep `event` as the only one of its kind (the latest wins).
    fn replace(&mut self, seq: usize, event: LedgerEvent) {
        let kind = std::mem::discriminant(&event);
        self.events
            .retain(|(_, e)| std::mem::discriminant(e) != kind);
        self.events.push((seq, event));
    }
}

/// An open chat folded from its events.
#[derive(Debug, Clone)]
struct ChatRecord {
    id: String,
    dir: PathBuf,
    name: Option<String>,
    /// The session it was made from.
    session: Option<String>,
    created_at: u64,
    keep: bool,
    last_turn_at: Option<u64>,
    /// Order of its `ChatCreated`: chats are listed in creation order.
    order: usize,
    history: History,
}

/// A message folded from its events (kept by its recipient).
#[derive(Debug, Clone)]
struct MessageRecord {
    id: String,
    from: Option<String>,
    text: String,
    hop: u32,
    reply_to: Option<String>,
    sent_at: u64,
    delivered_at: Option<u64>,
    /// Order of its `MessageSent` event.
    seq: usize,
}

/// Active tasks and members, and open chats.
#[derive(Debug, Default)]
struct State {
    tasks: BTreeMap<String, TaskRecord>,
    members: HashMap<String, MemberRecord>,
    chats: HashMap<String, ChatRecord>,
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

/// Default ledger path: `$XDG_DATA_HOME/tam/ledger.jsonl`, else
/// `~/.local/share/tam/ledger.jsonl`, on every platform
/// ([`tam_worktree::xdg::data_home`]). The task directories are next to it.
pub fn default_path() -> PathBuf {
    tam_worktree::xdg::data_home()
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
            .flat_map(|id| self.runs_of(id))
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

    /// Runs of a member or a chat, oldest first.
    pub fn runs_of(&self, id: &str) -> Vec<RunInfo> {
        self.state
            .history(id)
            .map(|h| h.runs.clone())
            .unwrap_or_default()
    }

    /// Whether the member (or chat) has a run with no end recorded.
    pub fn has_open_run(&self, member: &str) -> bool {
        self.state
            .history(member)
            .and_then(|h| h.runs.last())
            .is_some_and(|r| r.ended_at.is_none())
    }

    /// The member's (or chat's) current session for `provider`: the session
    /// of its latest run of that agent that has one. A run's session is the
    /// one it resumed, or the last one its agent reported having a turn in.
    /// A chat without one has the session it was made from.
    pub fn current_session(&self, member: &str, provider: &str) -> Option<String> {
        self.state.current_session(member, provider)
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
        if self.state.chats.contains_key(target) {
            bail!("'{}' is a chat, not a task member", target);
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
        events.extend(self.member_events(name, members, timestamp)?);
        self.append_all(events)?;
        Ok(self.task(name).expect("task just created"))
    }

    /// `MemberAdded` events for new members of `task`, in order. Refused for
    /// a directory that is already a member's, or given twice.
    fn member_events(
        &self,
        task: &str,
        members: &[NewMember],
        timestamp: u64,
    ) -> Result<Vec<LedgerEvent>> {
        let mut events = Vec::with_capacity(members.len());
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
                task: task.into(),
                member: id,
                repo: repo.to_string_lossy().into_owned(),
                repo_root: Some(m.repo_root.clone()),
                branch: m.branch.clone(),
                dir: m.dir.clone(),
                timestamp,
            });
        }
        Ok(events)
    }

    /// Record the start of the agent run of a member (or a chat). Closes a
    /// still-open previous run first, since a member runs one agent at a
    /// time. Returns the member or chat, updated.
    pub fn run_started(
        &mut self,
        member: &str,
        provider: &str,
        session_id: Option<String>,
    ) -> Result<Owner> {
        self.require_owner(member)?;
        self.run_ended(member, -1)?;
        self.append(LedgerEvent::AgentRunStarted {
            member: member.into(),
            provider: provider.into(),
            session_id,
            timestamp: now(),
        })?;
        Ok(self.owner(member).expect("checked above"))
    }

    /// Record a session the agent of the member's open run had a turn in:
    /// it becomes the run's session (identified, or changed from another
    /// one). Returns whether anything was recorded.
    pub fn session_reported(&mut self, member: &str, session_id: &str) -> Result<bool> {
        let Some(run) = self
            .state
            .history(member)
            .and_then(|h| h.runs.last())
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

    /// Record the end of the member's (or chat's) open run. Returns it,
    /// updated, or `None` when there was no open run (nothing recorded).
    pub fn run_ended(&mut self, member: &str, exit_code: i32) -> Result<Option<Owner>> {
        self.run_ended_with(member, exit_code, None)
    }

    /// Record the end of the member's open run with the response that ended
    /// its last turn (see `run_ended`).
    pub fn run_ended_with(
        &mut self,
        member: &str,
        exit_code: i32,
        response: Option<AgentResponse>,
    ) -> Result<Option<Owner>> {
        if !self.has_open_run(member) {
            return Ok(None);
        }
        self.append(LedgerEvent::AgentRunEnded {
            member: member.into(),
            exit_code,
            timestamp: now(),
            response,
        })?;
        Ok(self.owner(member))
    }

    /// The response that ended the last turn of the member's latest run,
    /// once that run is over (while it runs, its agent has it). `None` when
    /// it ended before a turn did, or mid-turn.
    pub fn last_response(&self, member: &str) -> Option<AgentResponse> {
        let history = self.state.history(member)?;
        let ended = history.runs.last().is_some_and(|r| r.ended_at.is_some());
        history.response.clone().filter(|_| ended)
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

    /// Errors unless `id` is a member's or a chat's.
    fn require_owner(&self, id: &str) -> Result<()> {
        if self.state.history(id).is_none() {
            bail!("task member or chat '{}' not found", id);
        }
        Ok(())
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

    /// How to name a member to the user (see `TaskInfo::member_label`).
    pub fn member_label(&self, id: &str) -> Option<String> {
        let (task, member) = self.member(id)?;
        Some(task.member_label(&member))
    }

    /// How messages name a member (its label) or a chat (its id: it always
    /// resolves back to the chat, which a name may not).
    pub fn message_label(&self, id: &str) -> Option<String> {
        match self.member_label(id) {
            Some(label) => Some(label),
            None => self.state.chats.contains_key(id).then(|| id.to_string()),
        }
    }

    /// Record a message to member (or chat) `to`, from member (or chat)
    /// `from` (`None`: the user).
    pub fn send_message(
        &mut self,
        from: Option<&str>,
        to: &str,
        text: &str,
        hop: u32,
        reply_to: Option<&str>,
    ) -> Result<MessageInfo> {
        self.require_owner(to)?;
        let id = new_message_id(|id| self.message_record(id).is_some());
        self.append(LedgerEvent::MessageSent {
            id: id.clone(),
            from: from.map(str::to_string),
            to: to.into(),
            text: text.into(),
            hop,
            reply_to: reply_to.map(str::to_string),
            timestamp: now(),
        })?;
        Ok(self.message(&id).expect("message just recorded"))
    }

    /// Record that a message reached its recipient's agent. Returns the
    /// message, or `None` when it was delivered already or is unknown.
    pub fn message_delivered(&mut self, id: &str) -> Result<Option<MessageInfo>> {
        let to = match self.message_record(id) {
            Some((to, record)) if record.delivered_at.is_none() => to.to_string(),
            _ => return Ok(None),
        };
        self.append(LedgerEvent::MessageDelivered {
            id: id.into(),
            to,
            timestamp: now(),
        })?;
        Ok(self.message(id))
    }

    pub fn message(&self, id: &str) -> Option<MessageInfo> {
        let (to, record) = self.message_record(id)?;
        Some(self.message_info(to, record))
    }

    /// Messages to a member (or chat) that its agent has not got yet,
    /// oldest first.
    pub fn pending_messages(&self, member: &str) -> Vec<MessageInfo> {
        let Some(history) = self.state.history(member) else {
            return Vec::new();
        };
        history
            .messages
            .iter()
            .filter(|m| m.delivered_at.is_none())
            .map(|m| self.message_info(member, m))
            .collect()
    }

    /// Messages to or from the members of `task` (of every task and chat:
    /// `None`), oldest first.
    pub fn messages(&self, task: Option<&str>) -> Vec<MessageInfo> {
        let members = task.map(|t| self.member_ids(t));
        self.messages_where(|id| members.as_ref().is_none_or(|ms| ms.iter().any(|m| m == id)))
    }

    /// Messages to or from a chat, oldest first.
    pub fn chat_messages(&self, chat: &str) -> Vec<MessageInfo> {
        self.messages_where(|id| id == chat)
    }

    /// Messages to or from a member or chat `involved` takes, oldest first.
    fn messages_where(&self, involved: impl Fn(&str) -> bool) -> Vec<MessageInfo> {
        let mut found: Vec<(usize, MessageInfo)> = self
            .state
            .histories()
            .flat_map(|(to, h)| h.messages.iter().map(move |m| (to, m)))
            .filter(|(to, m)| involved(to) || m.from.as_deref().is_some_and(&involved))
            .map(|(to, m)| (m.seq, self.message_info(to, m)))
            .collect();
        found.sort_by_key(|(seq, _)| *seq);
        found.into_iter().map(|(_, m)| m).collect()
    }

    /// The latest message from member `from` to member `to` (either may be
    /// a chat), sent at or after `since`, that no message from `to` answers
    /// yet.
    pub fn unanswered_message(&self, from: &str, to: &str, since: u64) -> Option<String> {
        let sent = &self.state.history(to)?.messages;
        let answers = self
            .state
            .history(from)
            .map(|h| h.messages.as_slice())
            .unwrap_or_default();
        sent.iter()
            .rev()
            .filter(|m| m.from.as_deref() == Some(from) && m.sent_at >= since)
            .find(|m| {
                !answers
                    .iter()
                    .any(|a| a.reply_to.as_deref() == Some(m.id.as_str()))
            })
            .map(|m| m.id.clone())
    }

    /// A message and its recipient.
    fn message_record(&self, id: &str) -> Option<(&str, &MessageRecord)> {
        self.state
            .histories()
            .find_map(|(to, h)| h.messages.iter().find(|m| m.id == id).map(|m| (to, m)))
    }

    fn message_info(&self, to: &str, m: &MessageRecord) -> MessageInfo {
        let label = |id: &str| self.message_label(id).unwrap_or_else(|| id.to_string());
        MessageInfo {
            id: m.id.clone(),
            from: m.from.clone(),
            from_label: m
                .from
                .as_deref()
                .map(label)
                .unwrap_or_else(|| "user".into()),
            to: to.to_string(),
            to_label: label(to),
            text: m.text.clone(),
            hop: m.hop,
            reply_to: m.reply_to.clone(),
            sent_at: m.sent_at,
            delivered_at: m.delivered_at,
        }
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
                    history: History::default(),
                };
                let id = member.id.clone();
                self.members.insert(id.clone(), member);
                self.members
                    .get_mut(&id)
                    .unwrap()
                    .history
                    .events
                    .push((seq, event));
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
                let Some(record) = self.history_mut(member) else {
                    return; // event for an unknown or dropped member
                };
                match &event {
                    LedgerEvent::AgentRunStarted {
                        member,
                        provider,
                        session_id,
                        timestamp,
                    } => {
                        record.runs.push(RunInfo {
                            member: member.clone(),
                            provider: provider.clone(),
                            session_id: session_id.clone(),
                            started_at: *timestamp,
                            ended_at: None,
                            exit_code: None,
                        });
                        // The new run has no turn yet.
                        record.response = None;
                    }
                    LedgerEvent::AgentSessionIdentified { session_id, .. } => {
                        if let Some(run) = record.runs.last_mut() {
                            run.session_id.get_or_insert_with(|| session_id.clone());
                        }
                    }
                    LedgerEvent::AgentRunEnded {
                        exit_code,
                        timestamp,
                        response,
                        ..
                    } => {
                        if let Some(run) = record.runs.last_mut().filter(|r| r.ended_at.is_none()) {
                            run.ended_at = Some(*timestamp);
                            run.exit_code = Some(*exit_code);
                            record.response = response.clone();
                        }
                    }
                    _ => unreachable!(),
                }
                record.events.push((seq, event));
            }
            LedgerEvent::AgentSessionChanged {
                member, session_id, ..
            } => {
                let Some(record) = self.history_mut(member) else {
                    return; // event for an unknown or dropped member
                };
                if let Some(run) = record.runs.last_mut() {
                    run.session_id = Some(session_id.clone());
                }
                record.events.push((seq, event));
            }
            LedgerEvent::MessageSent {
                id,
                from,
                to,
                text,
                hop,
                reply_to,
                timestamp,
            } => {
                let Some(record) = self.history_mut(to) else {
                    return; // to an unknown or dropped member
                };
                if record.messages.iter().any(|m| m.id == *id) {
                    return;
                }
                record.messages.push(MessageRecord {
                    id: id.clone(),
                    from: from.clone(),
                    text: text.clone(),
                    hop: *hop,
                    reply_to: reply_to.clone(),
                    sent_at: *timestamp,
                    delivered_at: None,
                    seq,
                });
                record.events.push((seq, event));
            }
            LedgerEvent::MessageDelivered { id, to, timestamp } => {
                let Some(record) = self.history_mut(to) else {
                    return;
                };
                let Some(message) = record.messages.iter_mut().find(|m| m.id == *id) else {
                    return; // its message was compacted away or never recorded
                };
                if message.delivered_at.is_some() {
                    return;
                }
                message.delivered_at = Some(*timestamp);
                record.events.push((seq, event));
            }
            LedgerEvent::MemberDropped { member, .. } => {
                // Kept nowhere: compaction drops the member's events with it.
                if let Some(record) = self.members.remove(member) {
                    if let Some(task) = self.tasks.get_mut(&record.task) {
                        task.members.retain(|m| m != member);
                    }
                    self.observed.remove(member);
                }
            }
            LedgerEvent::BriefSet { task, .. } => {
                let Some(record) = self.tasks.get_mut(task) else {
                    return; // an unknown or dropped task
                };
                // The latest replaces the earlier ones (see `Ledger::brief`).
                record
                    .events
                    .retain(|(_, e)| !matches!(e, LedgerEvent::BriefSet { .. }));
                record.events.push((seq, event));
            }
            LedgerEvent::MemberMoved { member, task, .. } => {
                let (member, task) = (member.clone(), task.clone());
                self.move_member(seq, &member, &task, event);
            }
            LedgerEvent::ChatCreated {
                chat,
                dir,
                name,
                session,
                timestamp,
            } => {
                if self.chats.contains_key(chat) || self.members.contains_key(chat) {
                    return; // ids are unique: a second one is bogus
                }
                let record = ChatRecord {
                    id: chat.clone(),
                    dir: dir.clone(),
                    name: name.clone(),
                    session: session.clone(),
                    created_at: *timestamp,
                    keep: false,
                    last_turn_at: None,
                    order: seq,
                    history: History::default(),
                };
                let id = record.id.clone();
                self.chats.insert(id.clone(), record);
                self.chats
                    .get_mut(&id)
                    .unwrap()
                    .history
                    .events
                    .push((seq, event));
            }
            LedgerEvent::ChatRenamed { chat, name, .. } => {
                let Some(record) = self.chats.get_mut(chat) else {
                    return; // an unknown or closed chat
                };
                record.name = name.clone();
                record.history.replace(seq, event);
            }
            LedgerEvent::ChatKept { chat, keep, .. } => {
                let Some(record) = self.chats.get_mut(chat) else {
                    return;
                };
                record.keep = *keep;
                record.history.replace(seq, event);
            }
            LedgerEvent::ChatTurn { chat, timestamp } => {
                let Some(record) = self.chats.get_mut(chat) else {
                    return;
                };
                record.last_turn_at = Some(*timestamp);
                record.history.replace(seq, event);
            }
            LedgerEvent::ChatClosed { chat, .. } => {
                // Kept nowhere: compaction drops the chat's events with it.
                self.chats.remove(chat);
            }
        }
    }

    /// The history of a member or a chat.
    fn history(&self, id: &str) -> Option<&History> {
        match self.members.get(id) {
            Some(member) => Some(&member.history),
            None => self.chats.get(id).map(|c| &c.history),
        }
    }

    fn history_mut(&mut self, id: &str) -> Option<&mut History> {
        let State { members, chats, .. } = self;
        match members.get_mut(id) {
            Some(member) => Some(&mut member.history),
            None => chats.get_mut(id).map(|c| &mut c.history),
        }
    }

    /// Every member's and chat's history, with its id.
    fn histories(&self) -> impl Iterator<Item = (&str, &History)> {
        self.members
            .values()
            .map(|m| (m.id.as_str(), &m.history))
            .chain(self.chats.values().map(|c| (c.id.as_str(), &c.history)))
    }

    /// See [`Ledger::current_session`].
    fn current_session(&self, id: &str, provider: &str) -> Option<String> {
        let recorded = self
            .history(id)?
            .runs
            .iter()
            .rev()
            .filter(|r| r.provider == provider)
            .find_map(|r| r.session_id.clone());
        recorded.or_else(|| self.chats.get(id)?.session.clone())
    }

    /// The chat's summary.
    fn chat_info(&self, chat: &ChatRecord) -> ChatInfo {
        ChatInfo {
            id: chat.id.clone(),
            name: chat.name.clone(),
            repo: chat
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| chat.dir.display().to_string()),
            dir: chat.dir.clone(),
            created_at: chat.created_at,
            keep: chat.keep,
            session: self.current_session(&chat.id, CLAUDE),
            last_turn_at: chat.last_turn_at,
            run_count: chat.history.runs.len(),
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
        for run in members.iter().flat_map(|m| &m.history.runs) {
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

    /// Number of events kept by the active tasks, members and chats.
    fn kept(&self) -> usize {
        self.tasks.values().map(|t| t.events.len()).sum::<usize>()
            + self.histories().map(|(_, h)| h.events.len()).sum::<usize>()
    }

    /// Events of the active tasks, members and chats, in their original
    /// order.
    fn events(&self) -> Vec<&(usize, LedgerEvent)> {
        let mut events: Vec<&(usize, LedgerEvent)> = self
            .tasks
            .values()
            .flat_map(|t| t.events.iter())
            .chain(self.histories().flat_map(|(_, h)| h.events.iter()))
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
        self.add_members(task, std::slice::from_ref(member))
    }

    /// Forget one member of a task that has others (its worktree is gone),
    /// ending its open run. Returns the task without it.
    pub fn drop_member(&mut self, member: &str) -> Result<TaskInfo> {
        let task = self.require_member(member)?;
        if self.member_ids(&task).len() < 2 {
            bail!(
                "'{}' is the only member of task '{}': drop the task",
                member,
                task
            );
        }
        self.run_ended(member, -1)?;
        self.append(LedgerEvent::MemberDropped {
            member: member.into(),
            timestamp: now(),
        })?;
        Ok(self.task(&task).expect("the task keeps its other members"))
    }

    /// Add worktrees to an existing task as new members (they join last, in
    /// order), all or none. Returns the updated task.
    pub fn add_members(&mut self, task: &str, members: &[NewMember]) -> Result<TaskInfo> {
        self.require(task)?;
        if members.is_empty() {
            bail!("no member to add to task '{}'", task);
        }
        let events = self.member_events(task, members, now())?;
        self.append_all(events)?;
        Ok(self.task(task).expect("task exists"))
    }
}

/// Names as a member's agent uses them: the short names of its task's
/// members, besides what `resolve` takes.
impl Ledger {
    /// Like [`Ledger::resolve`], for a caller that is member `caller` (its
    /// agent): a target naming no task and no member id may also be the
    /// short name of a member of the caller's task. Task names come first,
    /// so every name `resolve` takes keeps its meaning (the reply hints of
    /// messages use labels).
    pub fn resolve_from(
        &self,
        caller: Option<&str>,
        target: &str,
    ) -> Result<(TaskInfo, MemberInfo)> {
        let found = self.resolve(target);
        // Short names never contain '/'.
        if found.is_ok() || target.contains('/') || self.task(target).is_some() {
            return found;
        }
        let Some((task, _)) = caller.and_then(|c| self.member(c)) else {
            return found;
        };
        match task.members.iter().find(|m| m.name == target) {
            Some(member) => {
                let member = member.clone();
                Ok((task, member))
            }
            None => {
                let names: Vec<&str> = task.members.iter().map(|m| m.name.as_str()).collect();
                bail!(
                    "'{}' is neither a task nor a member of task '{}' ({})",
                    target,
                    task.name,
                    names.join(", ")
                )
            }
        }
    }

    /// How member `caller`'s agent can name `member` (of `task`): its short
    /// name when that resolves to it from the caller, else its label.
    pub fn address(&self, caller: &str, task: &TaskInfo, member: &MemberInfo) -> String {
        match self.resolve_from(Some(caller), &member.name) {
            Ok((_, found)) if found.id == member.id => member.name.clone(),
            _ => task.member_label(member),
        }
    }
}

/// What an agent runs for: a task member (with its task), or a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Member(TaskInfo, MemberInfo),
    Chat(ChatInfo),
}

impl Owner {
    /// The member's or chat's id, its agent's id.
    pub fn id(&self) -> &str {
        match self {
            Self::Member(_, member) => &member.id,
            Self::Chat(chat) => &chat.id,
        }
    }

    /// How to name it to the user: the member's label, the chat's.
    pub fn label(&self) -> String {
        match self {
            Self::Member(task, member) => task.member_label(member),
            Self::Chat(chat) => chat.label(),
        }
    }

    /// Where its agent runs: the member's worktree, the chat's checkout.
    pub fn dir(&self) -> &Path {
        match self {
            Self::Member(_, member) => &member.dir,
            Self::Chat(chat) => &chat.dir,
        }
    }

    /// The event telling clients it changed.
    pub fn updated(self) -> Event {
        match self {
            Self::Member(task, _) => Event::TaskUpdated { task },
            Self::Chat(chat) => Event::ChatUpdated { chat },
        }
    }
}

/// The agent a chat runs (and whose sessions it is made from).
const CLAUDE: &str = "claude";

/// Chats: Claude sessions on a repository's main checkout. Their agents'
/// runs, sessions and messages are recorded as a member's are, by id.
impl Ledger {
    /// Open chats, in creation order.
    pub fn chats(&self) -> Vec<ChatInfo> {
        let mut chats: Vec<&ChatRecord> = self.state.chats.values().collect();
        chats.sort_by_key(|c| c.order);
        chats.into_iter().map(|c| self.state.chat_info(c)).collect()
    }

    pub fn chat(&self, id: &str) -> Option<ChatInfo> {
        self.state.chats.get(id).map(|c| self.state.chat_info(c))
    }

    pub fn is_chat(&self, id: &str) -> bool {
        self.state.chats.contains_key(id)
    }

    /// The member (with its task) or chat with this id.
    pub fn owner(&self, id: &str) -> Option<Owner> {
        match self.member(id) {
            Some((task, member)) => Some(Owner::Member(task, member)),
            None => self.chat(id).map(Owner::Chat),
        }
    }

    /// Find a chat from what a user typed: its id, or its name.
    pub fn resolve_chat(&self, target: &str) -> Result<ChatInfo> {
        if let Some(chat) = self.chat(target) {
            return Ok(chat);
        }
        match self.chat_named(target) {
            Some(id) => Ok(self.chat(&id).expect("found by name")),
            None => bail!("chat '{}' not found", target),
        }
    }

    /// Id of the chat named `name`.
    fn chat_named(&self, name: &str) -> Option<String> {
        self.state
            .chats
            .values()
            .find(|c| c.name.as_deref() == Some(name))
            .map(|c| c.id.clone())
    }

    /// The recipient of a message from member (or chat) `caller`: what
    /// [`Ledger::resolve_from`] takes, else a chat's id or name. A target
    /// naming a task (or `<task>/…` of one) is never a chat's: no name that
    /// named something changes meaning.
    pub fn resolve_recipient(&self, caller: Option<&str>, target: &str) -> Result<Owner> {
        let found = self.resolve_from(caller, target);
        let names_task = self.task(target).is_some()
            || target
                .rsplit_once('/')
                .is_some_and(|(task, _)| self.task(task).is_some());
        match found {
            Ok((task, member)) => Ok(Owner::Member(task, member)),
            Err(e) if names_task => Err(e),
            Err(e) => self.resolve_chat(target).map(Owner::Chat).map_err(|_| e),
        }
    }

    /// Record a new chat on the repository whose main checkout is `dir`,
    /// named `name`, made from Claude session `session` (`None`: its agent
    /// starts a new one). Refused for a name another chat has, and for a
    /// session another chat holds.
    pub fn create_chat(
        &mut self,
        dir: &Path,
        name: Option<&str>,
        session: Option<&str>,
    ) -> Result<ChatInfo> {
        let name = name.map(|n| self.check_chat_name(None, n)).transpose()?;
        if let Some(session) = session {
            if session.trim().is_empty() {
                bail!("a session id cannot be empty");
            }
            if let Some(holder) = self
                .chats()
                .into_iter()
                .find(|c| c.session.as_deref() == Some(session))
            {
                bail!(
                    "session {} is the session of chat '{}' already",
                    session,
                    holder.label()
                );
            }
        }
        let id = new_chat_id(|id| self.state.chats.contains_key(id));
        self.append(LedgerEvent::ChatCreated {
            chat: id.clone(),
            dir: dir.to_path_buf(),
            name,
            session: session.map(str::to_string),
            timestamp: now(),
        })?;
        Ok(self.chat(&id).expect("chat just created"))
    }

    /// Name a chat (`None`: unnamed). Returns whether its name changed.
    pub fn rename_chat(&mut self, id: &str, name: Option<&str>) -> Result<bool> {
        let chat = self.require_chat(id)?;
        let name = name
            .map(|n| self.check_chat_name(Some(id), n))
            .transpose()?;
        if chat.name == name {
            return Ok(false);
        }
        self.append(LedgerEvent::ChatRenamed {
            chat: id.into(),
            name,
            timestamp: now(),
        })?;
        Ok(true)
    }

    /// Keep a chat, or stop keeping it. Returns whether that changed.
    pub fn keep_chat(&mut self, id: &str, keep: bool) -> Result<bool> {
        if self.require_chat(id)?.keep == keep {
            return Ok(false);
        }
        self.append(LedgerEvent::ChatKept {
            chat: id.into(),
            keep,
            timestamp: now(),
        })?;
        Ok(true)
    }

    /// Record that the user prompted the chat's agent now.
    pub fn chat_turn(&mut self, id: &str) -> Result<()> {
        self.require_chat(id)?;
        self.append(LedgerEvent::ChatTurn {
            chat: id.into(),
            timestamp: now(),
        })
    }

    /// Forget a chat, ending its open run first. Its messages to others
    /// stay with them; those to it go with it.
    pub fn close_chat(&mut self, id: &str) -> Result<()> {
        self.require_chat(id)?;
        self.run_ended(id, -1)?;
        self.append(LedgerEvent::ChatClosed {
            chat: id.into(),
            timestamp: now(),
        })
    }

    fn require_chat(&self, id: &str) -> Result<ChatInfo> {
        match self.chat(id) {
            Some(chat) => Ok(chat),
            None => bail!("chat '{}' not found", id),
        }
    }

    /// `name` as a chat's name (`chat`'s, when renaming): trimmed, unique
    /// among chats, and not one that reads as another chat's id.
    fn check_chat_name(&self, chat: Option<&str>, name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a chat's name cannot be empty");
        }
        if name.chars().any(char::is_control) {
            bail!("a chat's name cannot hold control characters");
        }
        if is_chat_id(name) {
            bail!("'{}' reads as a chat id: give the chat another name", name);
        }
        if let Some(other) = self.chat_named(name).filter(|o| Some(o.as_str()) != chat) {
            bail!("chat {} is named '{}' already", other, name);
        }
        Ok(name.to_string())
    }
}

/// Tasks' briefs: what their members work from.
impl Ledger {
    /// The task's brief: the reference the user gave, `None` for the local
    /// file (no reference given, or changed back to it). Kept with the
    /// task's events, so a re-created name starts with none.
    pub fn brief(&self, task: &str) -> Option<String> {
        let record = self.state.tasks.get(task)?;
        record.events.iter().rev().find_map(|(_, e)| match e {
            LedgerEvent::BriefSet { brief, .. } => brief.clone(),
            _ => None,
        })
    }

    /// Change the task's brief (`None`: back to the local file). Returns
    /// whether it changed.
    pub fn set_brief(&mut self, task: &str, brief: Option<&str>) -> Result<bool> {
        self.require(task)?;
        if let Some(brief) = brief {
            if brief.trim().is_empty() {
                bail!("a brief cannot be empty");
            }
        }
        if self.brief(task).as_deref() == brief {
            return Ok(false);
        }
        self.append(LedgerEvent::BriefSet {
            task: task.into(),
            brief: brief.map(str::to_string),
            timestamp: now(),
        })?;
        Ok(true)
    }
}

/// Members moving between tasks (`tam merge`, `tam split`): one append each,
/// no worktree, branch or agent touched.
impl Ledger {
    /// Move the members of tasks `from` into task `into` (they join it last,
    /// task by task, in join order), then forget the `from` tasks. Returns
    /// the merged task.
    ///
    /// Briefs (#25): `into` keeps its brief; a `from` task's reference goes
    /// with its `TaskDropped`. Recording it with `into` instead would be
    /// one more event in this append, before that `TaskDropped`.
    pub fn merge_tasks(&mut self, into: &str, from: &[String]) -> Result<TaskInfo> {
        self.require(into)?;
        if from.is_empty() {
            bail!("no task to merge into '{}'", into);
        }
        let timestamp = now();
        let mut events = Vec::new();
        for (i, name) in from.iter().enumerate() {
            self.require(name)?;
            if name == into {
                bail!("cannot merge task '{}' into itself", into);
            }
            if from[..i].contains(name) {
                bail!("task '{}' is given twice", name);
            }
            for member in self.member_ids(name) {
                events.push(LedgerEvent::MemberMoved {
                    member,
                    task: into.into(),
                    timestamp,
                });
            }
            events.push(LedgerEvent::TaskDropped {
                task: name.clone(),
                timestamp,
            });
        }
        self.append_all(events)?;
        Ok(self.task(into).expect("task exists"))
    }

    /// Move a member out of its task into a new task `name`. Refused for a
    /// task's only member: that task would be left empty. Returns the new
    /// task.
    pub fn split_member(&mut self, member: &str, name: &str) -> Result<TaskInfo> {
        let task = self.require_member(member)?;
        if name.trim().is_empty() {
            bail!("task name cannot be empty");
        }
        if self.state.tasks.contains_key(name) {
            bail!("task '{}' already exists", name);
        }
        if self.member_ids(&task).len() < 2 {
            bail!("'{}' is the only member of task '{}'", member, task);
        }
        let timestamp = now();
        self.append_all(vec![
            LedgerEvent::TaskCreated {
                name: name.into(),
                timestamp,
            },
            LedgerEvent::MemberMoved {
                member: member.into(),
                task: name.into(),
                timestamp,
            },
        ])?;
        Ok(self.task(name).expect("task just created"))
    }
}

impl State {
    /// Fold a `MemberMoved`: the member leaves its task and joins `task`
    /// last. Ignored for an unknown member or task.
    ///
    /// What compaction keeps must fold to the same, with the member's
    /// first task maybe gone: its `MemberAdded` is kept naming its current
    /// task, whose `TaskCreated` is kept before it (its order lowered to
    /// the member's when the task is younger), and only its latest move is
    /// kept: it puts the member at its place in the task's join order. A
    /// move into the member's own task, as such a ledger has, puts it last
    /// again.
    fn move_member(&mut self, seq: usize, member: &str, task: &str, event: LedgerEvent) {
        if !self.tasks.contains_key(task) {
            return;
        }
        let Some(record) = self.members.get_mut(member) else {
            return;
        };
        let from = std::mem::replace(&mut record.task, task.to_string());
        let events = &mut record.history.events;
        events.retain(|(_, e)| !matches!(e, LedgerEvent::MemberMoved { .. }));
        let mut added = None;
        for (at, e) in events.iter_mut() {
            if let LedgerEvent::MemberAdded { task: joined, .. } = e {
                *joined = task.to_string();
                added = Some(*at);
            }
        }
        events.push((seq, event));
        if let Some(old) = self.tasks.get_mut(&from) {
            old.members.retain(|m| m != member);
        }
        let target = self.tasks.get_mut(task).expect("checked above");
        target.members.retain(|m| m != member);
        target.members.push(member.to_string());
        if let (Some(added), Some((created, _))) = (added, target.events.first_mut()) {
            *created = (*created).min(added);
        }
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

/// A fresh chat id (`c-` and 8 hex digits) for which `taken` is false.
fn new_chat_id(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("c-{:08x}", random_u32());
        if !taken(&id) {
            return id;
        }
    }
}

/// Whether `s` has the shape of a chat id.
fn is_chat_id(s: &str) -> bool {
    s.strip_prefix("c-")
        .is_some_and(|hex| hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A fresh message id (`msg-` and 8 hex digits) for which `taken` is false.
fn new_message_id(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("msg-{:08x}", random_u32());
        if !taken(&id) {
            return id;
        }
    }
}

pub(crate) fn random_u32() -> u32 {
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
                    response: None,
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
            history: History::default(),
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
    fn short_names_resolve_from_a_member_of_the_task() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let feat = ledger
            .create_task(
                "feat",
                &[
                    new_member("api", "feat", "/w/api--feat"),
                    new_member("web", "feat", "/w/web--feat"),
                ],
            )
            .unwrap();
        let (api, web) = (&feat.members[0], &feat.members[1]);
        let other = create(&mut ledger, "other", "/w/app--other");

        // From api, its peer's short name.
        let (task, found) = ledger.resolve_from(Some(&api.id), "web").unwrap();
        assert_eq!(
            (task.name.as_str(), found.id.as_str()),
            ("feat", web.id.as_str())
        );
        assert_eq!(ledger.address(&api.id, &feat, web), "web");
        // Not from outside the task, nor for anyone else.
        assert!(ledger.resolve_from(None, "web").is_err());
        assert!(ledger.resolve_from(Some(&other), "web").is_err());
        assert!(ledger.resolve_from(Some("m-nobody"), "web").is_err());
        // What `resolve` takes keeps its meaning.
        assert_eq!(
            ledger.resolve_from(Some(&api.id), "other").unwrap().1.id,
            other
        );
        assert_eq!(
            ledger.resolve_from(Some(&api.id), "feat/web").unwrap().1.id,
            web.id
        );
        assert_eq!(
            ledger.resolve_from(Some(&api.id), &web.id).unwrap().1.id,
            web.id
        );
        let err = ledger
            .resolve_from(Some(&api.id), "db")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("neither a task nor a member of task 'feat' (api, web)"),
            "{err}"
        );

        // A task named like a peer takes the name: the peer's address is
        // its label then.
        let web_task = create(&mut ledger, "web", "/w/app--web");
        assert_eq!(
            ledger.resolve_from(Some(&api.id), "web").unwrap().1.id,
            web_task
        );
        assert_eq!(ledger.address(&api.id, &feat, web), "feat/web");
        let task = ledger.task("web").unwrap();
        assert_eq!(ledger.address(&api.id, &task, &task.members[0]), "web");
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

        let Owner::Member(info, _) = ledger.run_started(&m, "claude", None).unwrap() else {
            panic!("a member's run");
        };
        assert_eq!(info.run_count, 1);
        assert!(ledger.has_open_run(&m));

        assert!(ledger.session_reported(&m, "s1").unwrap());
        // Already known: not recorded twice.
        assert!(!ledger.session_reported(&m, "s1").unwrap());

        assert!(ledger.run_ended(&m, 0).unwrap().is_some());
        // No open run left: nothing to record.
        assert!(ledger.run_ended(&m, 0).unwrap().is_none());
        assert!(!ledger.session_reported(&m, "s3").unwrap());

        let runs = ledger.runs("feat");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].member, m);
        assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
        assert_eq!(runs[0].exit_code, Some(0));
        assert!(runs[0].ended_at.is_some());
        assert_eq!(ledger.runs_of(&m), runs);
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
        let Owner::Member(info, _) = ledger.run_started(web, "codex", None).unwrap() else {
            panic!("a member's run");
        };
        // Starting one member's run does not close the other's.
        assert!(ledger.has_open_run(api) && ledger.has_open_run(web));
        assert_eq!(info.run_count, 2);
        assert_eq!(ledger.runs_of(web)[0].provider, "codex");
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
                    response: None,
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
        let runs = ledger.runs_of(&m.id);
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
    fn drop_member_keeps_the_others() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let app = create(&mut ledger, "feat", "/w/app--feat");
        let task = ledger
            .add_members("feat", &[new_member("web", "feat", "/w/web--feat")])
            .unwrap();
        let web = task.members[1].id.clone();
        ledger.run_started(&web, "claude", None).unwrap();

        // The only member left cannot go alone: the task goes.
        let task = ledger.drop_member(&web).unwrap();
        assert_eq!(task.members.len(), 1);
        assert_eq!(task.members[0].id, app);
        assert!(ledger.member(&web).is_none());
        assert!(ledger.member_at(Path::new("/w/web--feat")).is_none());
        let err = ledger.drop_member(&app).unwrap_err().to_string();
        assert!(err.contains("only member"), "{err}");
        assert!(ledger.drop_member("m-nope").is_err());
        // Its run was ended before it went.
        assert_eq!(
            event_types(&path),
            [
                "task_created",
                "member_added",
                "member_added",
                "agent_run_started",
                "agent_run_ended",
                "member_dropped"
            ]
        );

        // Reopened (compacted): the dropped member's history is gone.
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap().0;
        assert_eq!(ledger.task("feat").unwrap().members.len(), 1);
        assert_eq!(event_types(&path), ["task_created", "member_added"]);
    }

    #[test]
    fn add_members_adds_all_or_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        create(&mut ledger, "feat", "/w/app--feat");

        // One directory already a member's: none is added.
        let err = ledger.add_members(
            "feat",
            &[
                new_member("web", "feat", "/w/web--feat"),
                new_member("app", "feat", "/w/app--feat"),
            ],
        );
        assert!(err.unwrap_err().to_string().contains("already has"));
        assert_eq!(ledger.task("feat").unwrap().members.len(), 1);
        assert!(ledger.add_members("feat", &[]).is_err());

        let task = ledger
            .add_members(
                "feat",
                &[
                    new_member("web", "feat", "/w/web--feat"),
                    new_member("docs", "feat", "/w/docs--feat"),
                ],
            )
            .unwrap();
        let names: Vec<&str> = task.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["app", "web", "docs"]);
        drop(ledger);
        assert_eq!(
            event_types(&path),
            [
                "task_created",
                "member_added",
                "member_added",
                "member_added"
            ]
        );
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

        let runs = ledger.runs_of(&m);
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
        assert_eq!(ledger.current_session(&m, "codex").as_deref(), Some("x1"));
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s1"));

        // A resumed session is the run's session from its start.
        ledger.run_started(&m, "claude", Some("s0".into())).unwrap();
        assert_eq!(ledger.current_session(&m, "claude").as_deref(), Some("s0"));
        assert_eq!(ledger.current_session("m-nope", "claude"), None);
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
            response: None,
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

    fn ids(messages: Vec<MessageInfo>) -> Vec<String> {
        messages.into_iter().map(|m| m.id).collect()
    }

    #[test]
    fn messages_survive_a_reopen_until_delivered() {
        let tmp = TempDir::new().unwrap();
        let (a, b, first, second) = {
            let mut ledger = open(&tmp);
            let a = create(&mut ledger, "a", "/w/a");
            let b = create(&mut ledger, "b", "/w/b");
            let first = ledger.send_message(None, &b, "hello", 0, None).unwrap();
            assert!(first.id.starts_with("msg-"));
            assert_eq!(
                (first.from_label.as_str(), first.to_label.as_str()),
                ("user", "b")
            );
            let second = ledger
                .send_message(Some(&b), &a, "hi back", 1, None)
                .unwrap();
            assert_eq!(second.from_label, "b");
            assert_eq!(second.hop, 1);
            assert_eq!(ids(ledger.pending_messages(&b)), [first.id.as_str()]);

            let delivered = ledger.message_delivered(&first.id).unwrap().unwrap();
            assert!(delivered.delivered_at.is_some());
            // Once only
            assert!(ledger.message_delivered(&first.id).unwrap().is_none());
            assert!(ledger.message_delivered("msg-nope").unwrap().is_none());
            assert!(ledger.pending_messages(&b).is_empty());
            assert!(ledger.send_message(None, "m-nobody", "x", 0, None).is_err());
            (a, b, first, second)
        };

        let (ledger, report) = Ledger::open(&tmp.path().join("ledger.jsonl")).unwrap();
        assert_eq!(report.compacted, 0);
        assert_eq!(ids(ledger.pending_messages(&a)), [second.id.as_str()]);
        assert!(ledger.pending_messages(&b).is_empty());
        assert!(ledger.message(&first.id).unwrap().delivered_at.is_some());
        // To or from the task's members, oldest first.
        assert_eq!(ids(ledger.messages(Some("a"))), [second.id.as_str()]);
        assert_eq!(
            ids(ledger.messages(Some("b"))),
            [first.id.clone(), second.id.clone()]
        );
        assert_eq!(ids(ledger.messages(None)), [first.id, second.id]);
    }

    #[test]
    fn messages_go_with_their_recipients_task() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = open(&tmp);
        let a = create(&mut ledger, "a", "/w/a");
        let b = create(&mut ledger, "b", "/w/b");
        let to_b = ledger.send_message(Some(&a), &b, "x", 1, None).unwrap();
        let to_a = ledger.send_message(Some(&b), &a, "y", 1, None).unwrap();
        ledger.message_delivered(&to_b.id).unwrap();
        ledger.drop_task("b", false).unwrap();
        assert!(ledger.message(&to_b.id).is_none());
        // A gone sender is named by its id.
        assert_eq!(ledger.message(&to_a.id).unwrap().from_label, b);
        drop(ledger);

        let (ledger, report) = Ledger::open(&path).unwrap();
        assert!(report.compacted > 0);
        assert!(ledger.message(&to_b.id).is_none());
        assert_eq!(ids(ledger.pending_messages(&a)), [to_a.id]);
        let types = event_types(&path);
        assert_eq!(types.iter().filter(|t| *t == "message_sent").count(), 1);
        assert!(!types.iter().any(|t| t == "message_delivered"));
    }

    #[test]
    fn a_message_answers_the_latest_unanswered_one() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let a = create(&mut ledger, "a", "/w/a");
        let b = create(&mut ledger, "b", "/w/b");
        assert_eq!(ledger.unanswered_message(&a, &b, 0), None);
        let question = ledger.send_message(Some(&a), &b, "?", 1, None).unwrap();
        // Only messages from a to b, sent in the window, count.
        ledger
            .send_message(None, &b, "from the user", 0, None)
            .unwrap();
        assert_eq!(
            ledger.unanswered_message(&a, &b, 0),
            Some(question.id.clone())
        );
        assert_eq!(ledger.unanswered_message(&b, &a, 0), None);
        assert_eq!(
            ledger.unanswered_message(&a, &b, question.sent_at + 1),
            None
        );
        let answer = ledger
            .send_message(Some(&b), &a, "!", 2, Some(&question.id))
            .unwrap();
        assert_eq!(answer.reply_to.as_deref(), Some(question.id.as_str()));
        assert_eq!(ledger.unanswered_message(&a, &b, 0), None);
    }

    #[test]
    fn last_response_outlives_the_run_and_a_reopen() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let response = AgentResponse {
            text: "All done.\nDetails…".into(),
            at: 42,
        };
        let member = {
            let mut ledger = open(&tmp);
            let member = create(&mut ledger, "feat", "/w/feat");
            ledger.run_started(&member, "claude", None).unwrap();
            // While the run goes on, its agent has the response.
            assert_eq!(ledger.last_response(&member), None);
            ledger
                .run_ended_with(&member, 0, Some(response.clone()))
                .unwrap();
            assert_eq!(ledger.last_response(&member), Some(response.clone()));
            member
        };
        // Read back as a current event, not mistaken for a legacy one.
        let ledger = open(&tmp);
        assert_eq!(ledger.last_response(&member), Some(response.clone()));
        assert_eq!(ledger.runs_of(&member)[0].exit_code, Some(0));
        assert_eq!(event_types(&path).last().unwrap(), "agent_run_ended");
        drop(ledger);

        // A new run starts with none, and keeps none if it ends before a
        // turn does.
        let mut ledger = open(&tmp);
        ledger.run_started(&member, "claude", None).unwrap();
        assert_eq!(ledger.last_response(&member), None);
        ledger.run_ended(&member, -1).unwrap();
        assert_eq!(ledger.last_response(&member), None);
        let ended = std::fs::read_to_string(&path).unwrap();
        let last = ended.lines().last().unwrap();
        assert!(!last.contains("response"), "{last}");
    }

    #[test]
    fn a_brief_is_kept_until_changed_and_goes_with_its_task() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        {
            let mut ledger = open(&tmp);
            create(&mut ledger, "feat", "/w/feat");
            // The local file until a reference is given.
            assert_eq!(ledger.brief("feat"), None);
            assert!(ledger.set_brief("feat", Some("CS-1")).unwrap());
            assert!(ledger.set_brief("feat", Some("https://t/CS-2")).unwrap());
            assert!(!ledger.set_brief("feat", Some("https://t/CS-2")).unwrap());
            assert_eq!(ledger.brief("feat").as_deref(), Some("https://t/CS-2"));
            assert!(ledger.set_brief("feat", Some("  ")).is_err());
            assert!(ledger.set_brief("nope", Some("CS-1")).is_err());
        }
        // Read back; compaction keeps the latest only.
        let mut ledger = open(&tmp);
        assert_eq!(ledger.brief("feat").as_deref(), Some("https://t/CS-2"));
        let types = event_types(&path);
        assert_eq!(types.iter().filter(|t| *t == "brief_set").count(), 1);

        // Back to the local file, also after a reopen.
        assert!(ledger.set_brief("feat", None).unwrap());
        assert_eq!(ledger.brief("feat"), None);
        drop(ledger);
        let mut ledger = open(&tmp);
        assert_eq!(ledger.brief("feat"), None);

        // A re-created name starts with the local file.
        ledger.set_brief("feat", Some("CS-3")).unwrap();
        ledger.drop_task("feat", true).unwrap();
        assert_eq!(ledger.brief("feat"), None);
        create(&mut ledger, "feat", "/w/feat");
        assert_eq!(ledger.brief("feat"), None);
    }

    /// What a member keeps across a move and a reopen.
    #[derive(Debug, PartialEq)]
    struct MemberFold {
        runs: Vec<RunInfo>,
        open: bool,
        session: Option<String>,
        pending: Vec<MessageInfo>,
        response: Option<AgentResponse>,
    }

    /// The folded tasks with their members' history, and every message.
    type Folded = (Vec<(TaskInfo, Vec<MemberFold>)>, Vec<MessageInfo>);

    fn folded(ledger: &Ledger) -> Folded {
        let tasks = ledger
            .tasks()
            .into_iter()
            .map(|t| {
                let members = t
                    .members
                    .iter()
                    .map(|m| MemberFold {
                        runs: ledger.runs_of(&m.id),
                        open: ledger.has_open_run(&m.id),
                        session: ledger.current_session(&m.id, "claude"),
                        pending: ledger.pending_messages(&m.id),
                        response: ledger.last_response(&m.id),
                    })
                    .collect();
                (t, members)
            })
            .collect();
        (tasks, ledger.messages(None))
    }

    /// Reopen twice: the first open compacts, the second finds nothing to
    /// compact and leaves the file as it is. Both fold to `expected`.
    fn reopen_keeps(path: &Path, expected: &Folded) -> Ledger {
        let ledger = Ledger::open(path).unwrap().0;
        assert_eq!(&folded(&ledger), expected);
        drop(ledger);
        let before = fs::read_to_string(path).unwrap();
        let (ledger, report) = Ledger::open(path).unwrap();
        assert_eq!(report.compacted, 0);
        assert_eq!(fs::read_to_string(path).unwrap(), before);
        assert_eq!(&folded(&ledger), expected);
        ledger
    }

    fn names(task: &TaskInfo) -> Vec<&str> {
        task.members.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn merged_members_keep_their_history_across_compaction() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        // The task merged away is the older one: its member joined before
        // the task it moves into was created.
        let ui = [new_member("frontend", "ui", "/w/frontend--ui")];
        let web = ledger.create_task("ui", &ui).unwrap().members[0].id.clone();
        ledger.run_started(&web, "claude", None).unwrap();
        ledger.session_reported(&web, "s-web").unwrap();
        let done = AgentResponse {
            text: "done".into(),
            at: 1,
        };
        ledger.run_ended_with(&web, 0, Some(done)).unwrap();
        let feat = [new_member("backend", "feat", "/w/backend--feat")];
        let api = ledger.create_task("feat", &feat).unwrap().members[0]
            .id
            .clone();
        ledger
            .run_started(&api, "claude", Some("s-api".into()))
            .unwrap();
        let pending = ledger.send_message(Some(&api), &web, "schema?", 1, None);
        let pending = pending.unwrap();
        let read = ledger.send_message(None, &web, "hi", 0, None).unwrap();
        ledger.message_delivered(&read.id).unwrap();

        let task = ledger.merge_tasks("feat", &["ui".into()]).unwrap();
        assert_eq!(names(&task), ["backend", "frontend"]);
        assert_eq!(task.run_count, 2);
        assert!(ledger.task("ui").is_none());
        assert_eq!(ledger.member_task(&web), Some("feat"));
        let session = ledger.current_session(&web, "claude");
        assert_eq!(session.as_deref(), Some("s-web"));
        assert_eq!(ledger.pending_messages(&web)[0].id, pending.id);
        assert_eq!(ledger.resolve("feat/frontend").unwrap().1.id, web);
        assert!(ledger.resolve("ui").is_err());
        assert_eq!(
            ledger.message(&pending.id).unwrap().to_label,
            "feat/frontend"
        );

        // Runs after the move are the member's too.
        ledger
            .run_started(&web, "claude", Some("s-web".into()))
            .unwrap();
        ledger.session_reported(&web, "s-web2").unwrap();
        ledger.run_ended(&api, 0).unwrap();
        let expected = folded(&ledger);
        assert_eq!(expected.0[0].0.run_count, 3);
        drop(ledger);

        let ledger = reopen_keeps(&path, &expected);
        let session = ledger.current_session(&web, "claude");
        assert_eq!(session.as_deref(), Some("s-web2"));
        assert!(ledger.has_open_run(&web));
        // The merged task's creation goes first, the member names it, and
        // its latest move keeps it after the member that was there first.
        assert_eq!(
            event_types(&path),
            [
                "task_created",
                "member_added",
                "agent_run_started",
                "agent_session_identified",
                "agent_run_ended",
                "member_added",
                "agent_run_started",
                "message_sent",
                "message_sent",
                "message_delivered",
                "member_moved",
                "agent_run_started",
                "agent_session_changed",
                "agent_run_ended",
            ]
        );
        // Nothing names the task merged away.
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(r#""task":"ui""#), "{raw}");
        assert!(!raw.contains(r#""name":"ui""#), "{raw}");
    }

    #[test]
    fn moves_keep_join_order_and_short_names() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        // Members of one repository in each task: the one that moves in
        // takes `<repo>-<branch>`, and no member already there is renamed.
        let a = create(&mut ledger, "a", "/w/app--a");
        let b = create(&mut ledger, "b", "/w/app--b");
        let task = ledger.merge_tasks("b", &["a".into()]).unwrap();
        assert_eq!(names(&task), ["app", "app-a"]);
        assert_eq!(task.members[0].id, b);
        let expected = folded(&ledger);
        drop(ledger);
        let mut ledger = reopen_keeps(&path, &expected);

        // Split back out under another name, then merged again the other
        // way round: a second move of the same member.
        let split = ledger.split_member(&a, "a2").unwrap();
        assert_eq!(split.members[0].id, a);
        assert_eq!(names(&split), ["app"]);
        assert_eq!(names(&ledger.task("b").unwrap()), ["app"]);
        let task = ledger.merge_tasks("a2", &["b".into()]).unwrap();
        assert_eq!(names(&task), ["app", "app-b"]);
        assert_eq!(task.members[1].id, b);
        let c = ledger
            .add_member("a2", &new_member("app", "c", "/w/app--c"))
            .unwrap()
            .members[2]
            .id
            .clone();
        let task = ledger.task("a2").unwrap();
        assert_eq!(names(&task), ["app", "app-b", "app-c"]);
        let expected = folded(&ledger);
        drop(ledger);
        let ledger = reopen_keeps(&path, &expected);
        assert_eq!(ledger.task("a2").unwrap().members[2].id, c);
        // One move kept per member that moved.
        let moves = event_types(&path)
            .into_iter()
            .filter(|t| t == "member_moved")
            .count();
        assert_eq!(moves, 2);
    }

    #[test]
    fn split_leaves_the_others_and_is_kept() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let task = ledger
            .create_task(
                "feat",
                &[
                    new_member("app", "feat", "/w/app--feat"),
                    new_member("app", "x", "/w/app--x"),
                    new_member("web", "feat", "/w/web--feat"),
                ],
            )
            .unwrap();
        let (first, second) = (task.members[0].id.clone(), task.members[1].id.clone());
        ledger
            .run_started(&first, "codex", Some("s1".into()))
            .unwrap();

        let split = ledger.split_member(&first, "feat-api").unwrap();
        assert_eq!(split.run_count, 1);
        assert!(ledger.has_open_run(&first));
        // The remaining members' names are derived again.
        let rest = ledger.task("feat").unwrap();
        assert_eq!(names(&rest), ["app", "web"]);
        assert_eq!(rest.members[0].id, second);
        assert_eq!(ledger.resolve("feat-api").unwrap().1.id, first);

        // Nothing dropped: the file is kept as written.
        let expected = folded(&ledger);
        drop(ledger);
        let before = fs::read_to_string(&path).unwrap();
        let mut ledger = reopen_keeps(&path, &expected);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);

        // Dropping the task it came from leaves it alone.
        ledger.drop_task("feat", true).unwrap();
        let expected = (
            vec![expected
                .0
                .into_iter()
                .find(|(t, _)| t.name == "feat-api")
                .unwrap()],
            Vec::new(),
        );
        assert_eq!(folded(&ledger), expected);
        drop(ledger);
        reopen_keeps(&path, &expected);
    }

    #[test]
    fn merge_and_split_refusals_record_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let a = create(&mut ledger, "a", "/w/app--a");
        create(&mut ledger, "b", "/w/app--b");
        let before = event_types(&path);
        let refused = |r: Result<TaskInfo>| r.unwrap_err().to_string();
        assert!(refused(ledger.merge_tasks("nope", &["a".into()])).contains("not found"));
        assert!(refused(ledger.merge_tasks("a", &["nope".into()])).contains("not found"));
        assert!(refused(ledger.merge_tasks("a", &[])).contains("no task"));
        assert!(refused(ledger.merge_tasks("a", &["a".into()])).contains("itself"));
        let twice = ledger.merge_tasks("a", &["b".into(), "b".into()]);
        assert!(refused(twice).contains("twice"));
        assert!(refused(ledger.split_member(&a, "c")).contains("only member"));
        assert!(refused(ledger.split_member("m-nope", "c")).contains("not found"));
        ledger
            .add_member("a", &new_member("web", "a", "/w/web--a"))
            .unwrap();
        assert!(refused(ledger.split_member(&a, "b")).contains("already exists"));
        assert!(refused(ledger.split_member(&a, " ")).contains("empty"));
        let mut after = before;
        after.push("member_added".into());
        assert_eq!(event_types(&path), after);
    }

    #[test]
    fn migrated_tasks_merge_like_any() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        fs::write(&path, LEGACY).unwrap();
        let mut ledger = Ledger::open(&path).unwrap().0;
        let myapp = ledger.task("feat").unwrap().members[0].id.clone();
        // `fix/x` was created after `feat`'s member, which has an open run.
        let task = ledger.merge_tasks("fix/x", &["feat".into()]).unwrap();
        assert_eq!(names(&task), ["api", "myapp"]);
        assert_eq!(task.run_count, 2);
        assert!(task.members.iter().all(|m| m.repo_root.is_none()));
        assert!(ledger.has_open_run(&myapp));
        let session = ledger.current_session(&myapp, "codex");
        assert_eq!(session.as_deref(), Some("s2"));
        ledger.run_ended(&myapp, 0).unwrap();
        let expected = folded(&ledger);
        drop(ledger);
        let ledger = reopen_keeps(&path, &expected);
        assert_eq!(ledger.tasks().len(), 1);
        assert_eq!(ledger.runs_of(&myapp)[1].exit_code, Some(0));
    }

    #[test]
    fn merge_then_split_back_under_the_old_name() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let back = [new_member("backend", "back", "/w/backend--back")];
        let api = ledger.create_task("back", &back).unwrap().members[0]
            .id
            .clone();
        let front = [new_member("frontend", "front", "/w/frontend--front")];
        let web = ledger.create_task("front", &front).unwrap().members[0]
            .id
            .clone();
        ledger
            .run_started(&web, "claude", Some("s-web".into()))
            .unwrap();
        ledger
            .send_message(Some(&web), &api, "done?", 1, None)
            .unwrap();

        ledger.merge_tasks("back", &["front".into()]).unwrap();
        // The default name `tam split` gives: the member's branch, free
        // again once its task was merged away.
        let split = ledger.split_member(&web, "front").unwrap();
        assert_eq!(split.members[0].id, web);
        assert_eq!(names(&split), ["frontend"]);
        assert_eq!(ledger.task("back").unwrap().members[0].id, api);
        assert!(ledger.has_open_run(&web));
        assert_eq!(ledger.messages(Some("back"))[0].from_label, "front");
        let expected = folded(&ledger);
        drop(ledger);
        reopen_keeps(&path, &expected);
    }

    #[test]
    fn a_merge_keeps_the_target_brief_and_a_split_starts_without() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let mut ledger = Ledger::open(&path).unwrap().0;
        let back = [new_member("backend", "back", "/w/backend--back")];
        ledger.create_task("back", &back).unwrap();
        let front = [new_member("frontend", "front", "/w/frontend--front")];
        let web = ledger.create_task("front", &front).unwrap().members[0]
            .id
            .clone();
        ledger.set_brief("back", Some("CS-1")).unwrap();
        ledger.set_brief("front", Some("CS-2")).unwrap();

        // The target's brief stays; the source's goes with the source.
        ledger.merge_tasks("back", &["front".into()]).unwrap();
        assert_eq!(ledger.brief("back").as_deref(), Some("CS-1"));
        assert_eq!(ledger.brief("front"), None);
        let expected = folded(&ledger);
        drop(ledger);
        let mut ledger = reopen_keeps(&path, &expected);
        assert_eq!(ledger.brief("back").as_deref(), Some("CS-1"));
        let types = event_types(&path);
        assert_eq!(types.iter().filter(|t| *t == "brief_set").count(), 1);

        // A task split off starts with the local brief, as a new task does.
        ledger.split_member(&web, "front").unwrap();
        assert_eq!(ledger.brief("front"), None);
        assert_eq!(ledger.brief("back").as_deref(), Some("CS-1"));
        let expected = folded(&ledger);
        drop(ledger);
        let ledger = reopen_keeps(&path, &expected);
        assert_eq!(ledger.brief("front"), None);
        assert_eq!(ledger.brief("back").as_deref(), Some("CS-1"));
    }

    fn chat_created(chat: &str, dir: &str, session: Option<&str>, ts: u64) -> LedgerEvent {
        LedgerEvent::ChatCreated {
            chat: chat.into(),
            dir: dir.into(),
            name: None,
            session: session.map(str::to_string),
            timestamp: ts,
        }
    }

    fn chat_ids(ledger: &Ledger) -> Vec<String> {
        ledger.chats().into_iter().map(|c| c.id).collect()
    }

    #[test]
    fn chats_keep_their_creation_order_and_state_across_reloads() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        // Ids in reverse order of creation, and the first chat changed last:
        // neither the ids nor the latest events give the order.
        let (c, b, a) = ("c-0000000c", "c-0000000b", "c-0000000a");
        write_lines(
            &path,
            &[
                chat_created(c, "/src/api", None, 10),
                chat_created(b, "/src/api", Some("s-b"), 11),
                chat_created(a, "/src/web", None, 12),
            ],
            &[],
        );
        {
            let mut ledger = Ledger::open(&path).unwrap().0;
            assert_eq!(chat_ids(&ledger), [c, b, a]);
            ledger.rename_chat(c, Some("debug")).unwrap();
            ledger.rename_chat(c, Some("deploy")).unwrap();
            ledger.keep_chat(c, true).unwrap();
            ledger.chat_turn(c).unwrap();
            ledger.chat_turn(c).unwrap();
            ledger.run_started(c, "claude", None).unwrap();
            assert!(ledger.session_reported(c, "s-c").unwrap());
            ledger.run_ended(c, 0).unwrap();
        }
        let (ledger, report) = Ledger::open(&path).unwrap();
        // Only the latest rename and turn are kept.
        assert_eq!(report.compacted, 2);
        assert_eq!(chat_ids(&ledger), [c, b, a]);
        let chat = ledger.chat(c).unwrap();
        assert_eq!(chat.name.as_deref(), Some("deploy"));
        assert_eq!(chat.label(), "deploy");
        assert!(chat.keep);
        assert!(chat.last_turn_at.is_some());
        assert_eq!(chat.session.as_deref(), Some("s-c"));
        assert_eq!(chat.run_count, 1);
        assert_eq!(chat.repo, "api");
        assert_eq!(chat.dir, Path::new("/src/api"));
        assert_eq!(chat.created_at, 10);
        let runs = ledger.runs_of(c);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].member, c);
        assert_eq!(runs[0].exit_code, Some(0));
        // Made from a session, never run: that session is its current one.
        let made = ledger.chat(b).unwrap();
        assert_eq!(made.session.as_deref(), Some("s-b"));
        assert_eq!(made.label(), b);
        assert!(!made.keep && made.last_turn_at.is_none());
        // Chats are no tasks.
        assert!(ledger.tasks().is_empty());
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        assert_eq!(ledger.1.compacted, 0);
        assert_eq!(chat_ids(&ledger.0), [c, b, a]);
    }

    #[test]
    fn chat_names_are_unique_and_sessions_held_once() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let dir = Path::new("/src/api");
        let one = ledger.create_chat(dir, Some("  debug "), None).unwrap();
        assert!(
            one.id.starts_with("c-") && is_chat_id(&one.id),
            "{}",
            one.id
        );
        assert_eq!(one.name.as_deref(), Some("debug"));
        // Several chats on one repository.
        let two = ledger.create_chat(dir, None, Some("s-1")).unwrap();
        assert_ne!(one.id, two.id);
        for (name, why) in [
            ("debug", "named 'debug' already"),
            (" ", "cannot be empty"),
            ("a\nb", "control characters"),
            ("c-12345678", "reads as a chat id"),
        ] {
            let err = ledger.create_chat(dir, Some(name), None).unwrap_err();
            assert!(err.to_string().contains(why), "{name:?}: {err}");
            let err = ledger.rename_chat(&two.id, Some(name)).unwrap_err();
            assert!(err.to_string().contains(why), "{name:?}: {err}");
        }
        // Its own name again: nothing to record.
        assert!(!ledger.rename_chat(&one.id, Some("debug")).unwrap());
        assert!(ledger.rename_chat(&one.id, None).unwrap());
        assert_eq!(ledger.chat(&one.id).unwrap().name, None);
        assert!(!ledger.keep_chat(&one.id, false).unwrap());
        assert!(ledger.keep_chat(&one.id, true).unwrap());
        // A session another chat holds.
        let err = ledger.create_chat(dir, None, Some("s-1")).unwrap_err();
        assert!(err.to_string().contains("already"), "{err}");
        assert!(ledger.create_chat(dir, None, Some(" ")).is_err());
        // Closed, it holds nothing: the chat can be made again from it.
        ledger.close_chat(&two.id).unwrap();
        let again = ledger.create_chat(dir, None, Some("s-1")).unwrap();
        assert_eq!(again.session.as_deref(), Some("s-1"));
        assert_ne!(again.id, two.id);
        assert!(ledger.chat(&two.id).is_none());
        for missing in [two.id.as_str(), "nope"] {
            assert!(ledger.rename_chat(missing, Some("x")).is_err());
            assert!(ledger.keep_chat(missing, true).is_err());
            assert!(ledger.chat_turn(missing).is_err());
            assert!(ledger.close_chat(missing).is_err());
        }
        // Found by id or name.
        ledger.rename_chat(&again.id, Some("deploy")).unwrap();
        assert_eq!(ledger.resolve_chat("deploy").unwrap().id, again.id);
        assert_eq!(ledger.resolve_chat(&again.id).unwrap().id, again.id);
        assert!(ledger.resolve_chat("debug").is_err());
    }

    #[test]
    fn a_chat_runs_and_messages_as_a_member_does() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let m = create(&mut ledger, "feat", "/w/app--feat");
        let chat = ledger
            .create_chat(Path::new("/src/app"), Some("debug"), Some("s-0"))
            .unwrap();
        let c = chat.id.as_str();
        assert_eq!(ledger.current_session(c, "claude").as_deref(), Some("s-0"));

        // Runs, sessions: recorded against the chat id.
        let Owner::Chat(info) = ledger.run_started(c, "claude", Some("s-0".into())).unwrap() else {
            panic!("a chat's run");
        };
        assert_eq!(info.run_count, 1);
        assert!(ledger.has_open_run(c));
        assert!(ledger.session_reported(c, "s-1").unwrap());
        assert_eq!(ledger.current_session(c, "claude").as_deref(), Some("s-1"));
        assert_eq!(ledger.chat(c).unwrap().session.as_deref(), Some("s-1"));
        assert!(matches!(ledger.owner(c), Some(Owner::Chat(_))));
        assert!(matches!(ledger.owner(&m), Some(Owner::Member(..))));
        assert!(ledger.member(c).is_none() && ledger.member_task(c).is_none());
        // The task's runs are its members' only.
        assert!(ledger.runs("feat").is_empty());

        // A chat is no task member: `resolve` says so.
        let err = ledger.resolve(c).unwrap_err();
        assert!(err.to_string().contains("is a chat"), "{err}");
        // Recipients: a chat by id or name, never by a task's name.
        let to = |caller: Option<&str>, target: &str| {
            ledger
                .resolve_recipient(caller, target)
                .map(|o| o.id().to_string())
        };
        assert_eq!(to(None, c).unwrap(), c);
        assert_eq!(to(Some(&m), "debug").unwrap(), c);
        assert_eq!(to(Some(c), "feat").unwrap(), m);
        assert!(to(None, "nope").is_err());

        // Messages both ways, labelled by the chat's id.
        let asked = ledger
            .send_message(Some(c), &m, "status?", 1, None)
            .unwrap();
        assert_eq!(asked.from_label, c);
        assert_eq!(asked.to_label, "feat");
        assert_eq!(ledger.unanswered_message(c, &m, 0), Some(asked.id.clone()));
        let reply = ledger
            .send_message(Some(&m), c, "done", 2, Some(&asked.id))
            .unwrap();
        assert_eq!(reply.to, c);
        assert_eq!(reply.to_label, c);
        assert_eq!(ledger.unanswered_message(c, &m, 0), None);
        assert_eq!(ledger.pending_messages(c).len(), 1);
        let user = ledger.send_message(None, c, "hi", 0, None).unwrap();
        assert!(ledger.message_delivered(&user.id).unwrap().is_some());
        let ids = |ms: Vec<MessageInfo>| ms.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(
            ids(ledger.chat_messages(c)),
            [asked.id.clone(), reply.id.clone(), user.id.clone()]
        );
        assert_eq!(
            ids(ledger.messages(Some("feat"))),
            [asked.id.clone(), reply.id.clone()]
        );
        assert_eq!(ledger.messages(None).len(), 3);

        // Closing ends its run and forgets it with what was sent to it; what
        // it sent stays with its recipient, under its id.
        ledger.close_chat(c).unwrap();
        assert!(ledger.chat(c).is_none() && ledger.owner(c).is_none());
        assert!(ledger.runs_of(c).is_empty());
        assert!(ledger.run_started(c, "claude", None).is_err());
        assert!(ledger.send_message(None, c, "hi", 0, None).is_err());
        let left = ledger.messages(None);
        assert_eq!(ids(left.clone()), std::slice::from_ref(&asked.id));
        assert_eq!(left[0].from_label, c);
        drop(ledger);
        let ledger = open(&tmp);
        assert!(ledger.chats().is_empty());
        assert_eq!(ids(ledger.messages(None)), [asked.id]);
        assert!(!event_types(&tmp.path().join("ledger.jsonl"))
            .iter()
            .any(|t| t.starts_with("chat_") || t == "agent_run_started"));
    }

    #[test]
    fn a_chat_named_like_a_task_never_takes_its_name() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        let task = ledger
            .create_task(
                "feat",
                &[
                    new_member("api", "feat", "/w/1"),
                    new_member("web", "feat", "/w/2"),
                ],
            )
            .unwrap();
        ledger
            .create_chat(Path::new("/src/api"), Some("feat"), None)
            .unwrap();
        ledger
            .create_chat(Path::new("/src/api"), Some("feat/api"), None)
            .unwrap();
        // The task has several members: an error, as before chats.
        let err = ledger.resolve_recipient(None, "feat").unwrap_err();
        assert!(err.to_string().contains("several members"), "{err}");
        let found = ledger.resolve_recipient(None, "feat/api").unwrap();
        assert_eq!(found.id(), task.members[0].id);
    }

    #[test]
    fn a_ledger_without_chats_loads_unchanged() {
        // As tam 0.4.1 wrote it: nothing to migrate or compact.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        let lines = [
            r#"{"type":"task_created","name":"feat","timestamp":1}"#,
            r#"{"type":"member_added","task":"feat","member":"m-00000001","repo":"app","repo_root":"/src/app","branch":"feat","dir":"/w/app--feat","timestamp":1}"#,
            r#"{"type":"agent_run_started","member":"m-00000001","provider":"claude","timestamp":2}"#,
            r#"{"type":"agent_session_identified","member":"m-00000001","session_id":"s-1","timestamp":3}"#,
            r#"{"type":"message_sent","id":"msg-00000001","to":"m-00000001","text":"hi","hop":0,"timestamp":4}"#,
            r#"{"type":"agent_run_ended","member":"m-00000001","exit_code":0,"timestamp":5}"#,
        ];
        write_lines(&path, &[], &lines);
        let before = fs::read_to_string(&path).unwrap();
        let (ledger, report) = Ledger::open(&path).unwrap();
        assert!(report.malformed.is_empty());
        assert_eq!((report.compacted, report.migrated), (0, 0));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(ledger.tasks().len(), 1);
        assert_eq!(ledger.runs_of("m-00000001").len(), 1);
        assert_eq!(
            ledger.current_session("m-00000001", "claude").as_deref(),
            Some("s-1")
        );
        assert_eq!(ledger.pending_messages("m-00000001").len(), 1);
        assert!(ledger.chats().is_empty());
    }
}
