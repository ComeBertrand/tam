//! Task ledger: append-only JSONL of task events, owned by the daemon.
//!
//! The daemon is the only writer. It holds an exclusive `flock` on a lock
//! file next to the ledger for its whole lifetime, so a second daemon (or a
//! stray old client) cannot interleave appends.
//!
//! On open the ledger is read, malformed lines are reported and set aside in
//! `<ledger>.rejected`, and the file is compacted: events of dropped tasks (and
//! events naming no live task) are removed, keeping only the history of the
//! active tasks. Task state is never stored — it is folded from the events.

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tam_proto::{RunInfo, TaskInfo};

/// A single event recorded in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEvent {
    TaskCreated {
        name: String,
        dir: PathBuf,
        owned: bool,
        timestamp: u64,
    },
    AgentRunStarted {
        task: String,
        provider: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        timestamp: u64,
    },
    /// The session id of the task's current run became known after it
    /// started (a new session reported by the agent).
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
    WorktreeDeleted {
        task: String,
        timestamp: u64,
    },
    TaskDropped {
        task: String,
        timestamp: u64,
    },
}

impl LedgerEvent {
    fn task_name(&self) -> &str {
        match self {
            Self::TaskCreated { name, .. } => name,
            Self::AgentRunStarted { task, .. }
            | Self::AgentSessionIdentified { task, .. }
            | Self::AgentRunEnded { task, .. }
            | Self::WorktreeDeleted { task, .. }
            | Self::TaskDropped { task, .. } => task,
        }
    }
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
}

/// An active task folded from its events.
#[derive(Debug, Clone)]
struct TaskRecord {
    info: TaskInfo,
    runs: Vec<RunInfo>,
    /// Events of this task, kept for compaction, tagged with their order.
    events: Vec<(usize, LedgerEvent)>,
}

/// The daemon's view of the ledger: active tasks, plus the open file.
pub struct Ledger {
    path: PathBuf,
    file: File,
    _lock: File,
    tasks: BTreeMap<String, TaskRecord>,
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
    /// Open (creating if needed), lock, load and compact the ledger at `path`.
    pub fn open(path: &Path) -> Result<(Self, LoadReport)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let lock = lock_exclusive(&sibling(path, ".lock"))?;

        let mut report = LoadReport::default();
        let mut events = Vec::new();
        let mut lines = 0;
        if path.exists() {
            let file = File::open(path)
                .with_context(|| format!("failed to open ledger at {}", path.display()))?;
            for (idx, line) in BufReader::new(file).lines().enumerate() {
                let line = line.with_context(|| format!("failed to read {}", path.display()))?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                lines += 1;
                match serde_json::from_str::<LedgerEvent>(trimmed) {
                    Ok(event) => events.push(event),
                    Err(e) => report.malformed.push(MalformedLine {
                        line: idx + 1,
                        content: trimmed.to_string(),
                        error: e.to_string(),
                    }),
                }
            }
        }

        let mut tasks = BTreeMap::new();
        let mut seq = 0;
        for event in events {
            apply(&mut tasks, seq, event);
            seq += 1;
        }

        let kept: usize = tasks.values().map(|t: &TaskRecord| t.events.len()).sum();
        if kept < lines {
            if !report.malformed.is_empty() {
                set_aside(&sibling(path, ".rejected"), &report.malformed)?;
            }
            rewrite(path, &tasks)?;
            report.compacted = lines - kept - report.malformed.len();
        }

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
                tasks,
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
        self.tasks.values().map(|t| t.info.clone()).collect()
    }

    pub fn task(&self, name: &str) -> Option<&TaskInfo> {
        self.tasks.get(name).map(|t| &t.info)
    }

    /// Runs of a task, oldest first.
    pub fn runs(&self, name: &str) -> Vec<RunInfo> {
        self.tasks
            .get(name)
            .map(|t| t.runs.clone())
            .unwrap_or_default()
    }

    /// Whether the task has a run with no end recorded.
    pub fn has_open_run(&self, name: &str) -> bool {
        self.tasks
            .get(name)
            .and_then(|t| t.runs.last())
            .is_some_and(|r| r.ended_at.is_none())
    }

    /// Record a new task.
    pub fn create_task(&mut self, name: &str, dir: &Path, owned: bool) -> Result<TaskInfo> {
        if name.trim().is_empty() {
            bail!("task name cannot be empty");
        }
        if self.tasks.contains_key(name) {
            bail!("task '{}' already exists", name);
        }
        if let Some(existing) = self.tasks.values().find(|t| t.info.dir == dir) {
            bail!(
                "directory {} already has an active task: '{}'",
                dir.display(),
                existing.info.name
            );
        }
        self.append(LedgerEvent::TaskCreated {
            name: name.into(),
            dir: dir.into(),
            owned,
            timestamp: now(),
        })?;
        Ok(self.tasks[name].info.clone())
    }

    /// Record the start of an agent run. Closes a still-open previous run
    /// first, since a task runs one agent at a time.
    pub fn run_started(
        &mut self,
        task: &str,
        provider: &str,
        session_id: Option<String>,
    ) -> Result<TaskInfo> {
        self.require(task)?;
        self.run_ended(task, -1)?;
        self.append(LedgerEvent::AgentRunStarted {
            task: task.into(),
            provider: provider.into(),
            session_id,
            timestamp: now(),
        })?;
        Ok(self.tasks[task].info.clone())
    }

    /// Record the session id of the task's open run, if it has none yet.
    /// Returns whether anything was recorded.
    pub fn session_identified(&mut self, task: &str, session_id: &str) -> Result<bool> {
        let needed = self
            .tasks
            .get(task)
            .and_then(|t| t.runs.last())
            .is_some_and(|r| r.ended_at.is_none() && r.session_id.is_none());
        if !needed {
            return Ok(false);
        }
        self.append(LedgerEvent::AgentSessionIdentified {
            task: task.into(),
            session_id: session_id.into(),
            timestamp: now(),
        })?;
        Ok(true)
    }

    /// Record the end of the task's open run. Returns the updated task, or
    /// `None` when there was no open run (nothing recorded).
    pub fn run_ended(&mut self, task: &str, exit_code: i32) -> Result<Option<TaskInfo>> {
        if !self.has_open_run(task) {
            return Ok(None);
        }
        self.append(LedgerEvent::AgentRunEnded {
            task: task.into(),
            exit_code,
            timestamp: now(),
        })?;
        Ok(Some(self.tasks[task].info.clone()))
    }

    /// Forget a task. Its open run, if any, is ended first.
    pub fn drop_task(&mut self, name: &str, worktree_deleted: bool) -> Result<()> {
        self.require(name)?;
        self.run_ended(name, -1)?;
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
        if !self.tasks.contains_key(name) {
            bail!("task '{}' not found", name);
        }
        Ok(())
    }

    /// Write an event to disk, then fold it into memory.
    fn append(&mut self, event: LedgerEvent) -> Result<()> {
        let mut json = serde_json::to_string(&event)?;
        json.push('\n');
        self.file
            .write_all(json.as_bytes())
            .and_then(|_| self.file.flush())
            .with_context(|| format!("failed to append to ledger {}", self.path.display()))?;
        apply(&mut self.tasks, self.seq, event);
        self.seq += 1;
        Ok(())
    }
}

/// Fold one event into the active task map.
fn apply(tasks: &mut BTreeMap<String, TaskRecord>, seq: usize, event: LedgerEvent) {
    if let LedgerEvent::TaskCreated {
        name,
        dir,
        owned,
        timestamp,
    } = &event
    {
        // A re-created name starts a fresh task: earlier history is dropped.
        tasks.insert(
            name.clone(),
            TaskRecord {
                info: TaskInfo {
                    name: name.clone(),
                    dir: dir.clone(),
                    owned: *owned,
                    created_at: *timestamp,
                    run_count: 0,
                    last_activity: Some(*timestamp),
                },
                runs: Vec::new(),
                events: vec![(seq, event)],
            },
        );
        return;
    }

    if let LedgerEvent::TaskDropped { task, .. } = &event {
        tasks.remove(task);
        return;
    }

    let Some(record) = tasks.get_mut(event.task_name()) else {
        return; // event for an unknown or dropped task
    };
    match &event {
        LedgerEvent::AgentRunStarted {
            provider,
            session_id,
            timestamp,
            ..
        } => {
            record.info.run_count += 1;
            record.info.last_activity = Some(*timestamp);
            record.runs.push(RunInfo {
                provider: provider.clone(),
                session_id: session_id.clone(),
                started_at: *timestamp,
                ended_at: None,
                exit_code: None,
            });
        }
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
            record.info.last_activity = Some(*timestamp);
            if let Some(run) = record.runs.last_mut().filter(|r| r.ended_at.is_none()) {
                run.ended_at = Some(*timestamp);
                run.exit_code = Some(*exit_code);
            }
        }
        LedgerEvent::WorktreeDeleted { .. } => {}
        LedgerEvent::TaskCreated { .. } | LedgerEvent::TaskDropped { .. } => unreachable!(),
    }
    record.events.push((seq, event));
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

/// Atomically replace the ledger with the events of the active tasks, in
/// their original order.
fn rewrite(path: &Path, tasks: &BTreeMap<String, TaskRecord>) -> Result<()> {
    let mut events: Vec<&(usize, LedgerEvent)> =
        tasks.values().flat_map(|t| t.events.iter()).collect();
    events.sort_by_key(|(seq, _)| *seq);

    let tmp = sibling(path, ".tmp");
    {
        let mut file =
            File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
        for (_, event) in events {
            let json = serde_json::to_string(event)?;
            writeln!(file, "{json}")?;
        }
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to replace ledger {}", path.display()))?;
    Ok(())
}

/// Count events per task name — test helper for compaction checks.
#[cfg(test)]
fn count_by_task(path: &Path) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for line in fs::read_to_string(path).unwrap().lines() {
        let event: LedgerEvent = serde_json::from_str(line).unwrap();
        *counts.entry(event.task_name().to_string()).or_default() += 1;
    }
    counts
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

    fn created(name: &str, dir: &str, ts: u64) -> LedgerEvent {
        LedgerEvent::TaskCreated {
            name: name.into(),
            dir: dir.into(),
            owned: true,
            timestamp: ts,
        }
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
        {
            let mut ledger = open(&tmp);
            let info = ledger
                .create_task("feat", Path::new("/tmp/feat"), true)
                .unwrap();
            assert_eq!(info.name, "feat");
            assert!(info.owned);
            assert_eq!(info.run_count, 0);
        }
        let ledger = open(&tmp);
        assert_eq!(ledger.tasks().len(), 1);
        assert_eq!(ledger.task("feat").unwrap().dir, Path::new("/tmp/feat"));
    }

    #[test]
    fn create_rejects_duplicates() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        ledger.create_task("feat", Path::new("/a"), true).unwrap();
        let err = ledger.create_task("feat", Path::new("/b"), true);
        assert!(err.unwrap_err().to_string().contains("already exists"));
        let err = ledger.create_task("other", Path::new("/a"), false);
        assert!(err.unwrap_err().to_string().contains("already has"));
        assert!(ledger.create_task(" ", Path::new("/c"), false).is_err());
    }

    #[test]
    fn run_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        ledger.create_task("feat", Path::new("/a"), true).unwrap();

        let info = ledger.run_started("feat", "claude", None).unwrap();
        assert_eq!(info.run_count, 1);
        assert!(ledger.has_open_run("feat"));

        assert!(ledger.session_identified("feat", "s1").unwrap());
        // Already known: not recorded twice.
        assert!(!ledger.session_identified("feat", "s2").unwrap());

        assert!(ledger.run_ended("feat", 0).unwrap().is_some());
        // No open run left: nothing to record.
        assert!(ledger.run_ended("feat", 0).unwrap().is_none());
        assert!(!ledger.session_identified("feat", "s3").unwrap());

        let runs = ledger.runs("feat");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].session_id.as_deref(), Some("s1"));
        assert_eq!(runs[0].exit_code, Some(0));
        assert!(runs[0].ended_at.is_some());
    }

    #[test]
    fn run_started_closes_dangling_run() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        ledger.create_task("feat", Path::new("/a"), true).unwrap();
        ledger
            .run_started("feat", "claude", Some("s1".into()))
            .unwrap();
        ledger.run_started("feat", "codex", None).unwrap();
        let runs = ledger.runs("feat");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].exit_code, Some(-1));
        assert!(runs[1].ended_at.is_none());
    }

    #[test]
    fn run_requires_task() {
        let tmp = TempDir::new().unwrap();
        let mut ledger = open(&tmp);
        assert!(ledger.run_started("nope", "claude", None).is_err());
        assert!(ledger.drop_task("nope", false).is_err());
    }

    #[test]
    fn drop_ends_open_run_and_forgets_task() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        {
            let mut ledger = Ledger::open(&path).unwrap().0;
            ledger.create_task("feat", Path::new("/a"), true).unwrap();
            ledger.run_started("feat", "claude", None).unwrap();
            ledger.drop_task("feat", true).unwrap();
            assert!(ledger.tasks().is_empty());
            assert!(ledger.runs("feat").is_empty());
        }
        let raw = fs::read_to_string(&path).unwrap();
        let types: Vec<String> = raw
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            types,
            [
                "task_created",
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
        ledger.create_task("feat", Path::new("/a"), true).unwrap();
        ledger
            .run_started("feat", "claude", Some("old".into()))
            .unwrap();
        ledger.drop_task("feat", false).unwrap();
        let info = ledger.create_task("feat", Path::new("/b"), true).unwrap();
        assert_eq!(info.run_count, 0);
        assert!(ledger.runs("feat").is_empty());
    }

    #[test]
    fn compaction_removes_dropped_tasks_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        write_lines(
            &path,
            &[
                created("gone", "/g", 1),
                created("keep", "/k", 2),
                LedgerEvent::AgentRunStarted {
                    task: "gone".into(),
                    provider: "claude".into(),
                    session_id: None,
                    timestamp: 3,
                },
                LedgerEvent::AgentRunStarted {
                    task: "keep".into(),
                    provider: "claude".into(),
                    session_id: Some("s".into()),
                    timestamp: 4,
                },
                LedgerEvent::TaskDropped {
                    task: "gone".into(),
                    timestamp: 5,
                },
                // Orphan: never created.
                LedgerEvent::AgentRunEnded {
                    task: "ghost".into(),
                    exit_code: 0,
                    timestamp: 6,
                },
            ],
            &[],
        );

        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.compacted, 4);
        assert!(report.malformed.is_empty());
        assert_eq!(ledger.tasks().len(), 1);
        assert_eq!(ledger.task("keep").unwrap().run_count, 1);
        assert_eq!(ledger.task("keep").unwrap().last_activity, Some(4));
        drop(ledger);

        assert_eq!(
            count_by_task(&path),
            HashMap::from([("keep".to_string(), 2)])
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
            &[created("keep", "/k", 1)],
            &["not json", r#"{"type":"from_the_future","task":"keep"}"#],
        );

        let (ledger, report) = Ledger::open(&path).unwrap();
        assert_eq!(report.malformed.len(), 2);
        assert_eq!(report.malformed[0].line, 2);
        assert_eq!(report.malformed[0].content, "not json");
        assert_eq!(report.compacted, 0);
        assert_eq!(ledger.tasks().len(), 1);
        drop(ledger);

        let rejected = fs::read_to_string(tmp.path().join("ledger.jsonl.rejected")).unwrap();
        assert_eq!(
            rejected,
            "not json\n{\"type\":\"from_the_future\",\"task\":\"keep\"}\n"
        );
        assert_eq!(count_by_task(&path).len(), 1);
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

    #[test]
    fn reads_existing_ledger_format() {
        // Lines as written by tam <= 0.3.6 (the CLI-owned ledger).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ledger.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"task_created","name":"feat","dir":"/tmp/feat","owned":true,"timestamp":100}"#,
                "\n",
                r#"{"type":"agent_run_started","task":"feat","provider":"claude","timestamp":200}"#,
                "\n",
                r#"{"type":"agent_run_ended","task":"feat","exit_code":-1,"timestamp":300}"#,
                "\n",
            ),
        )
        .unwrap();
        let (ledger, report) = Ledger::open(&path).unwrap();
        assert!(report.malformed.is_empty());
        assert_eq!(report.compacted, 0);
        let info = ledger.task("feat").unwrap();
        assert_eq!(info.created_at, 100);
        assert_eq!(info.last_activity, Some(300));
        assert_eq!(ledger.runs("feat")[0].exit_code, Some(-1));
    }
}
