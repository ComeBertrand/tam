use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tam_proto::{AgentInfo, AgentState, TaskInfo};

/// How long (in seconds) without activity before a task is considered stale.
const STALE_THRESHOLD_SECS: u64 = 30 * 24 * 3600; // 30 days

/// Git branch state for an owned task, populated by callers before status().
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitBranchStatus {
    #[default]
    Unknown,
    /// Branch exists
    Active,
    /// Local branch does not exist (deleted externally)
    Gone,
}

/// Computed task status — always derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Agent process is alive, producing output
    Run,
    /// Agent alive, waiting for user prompt
    Input,
    /// Agent alive, waiting for permission
    Block,
    /// Agent alive, asked the user a question
    Question,
    /// Agent alive, its last turn failed (e.g. API error)
    Error,
    /// No agent running, task exists
    Idle,
    /// No agent running, no activity for a long time
    Stale,
    /// Worktree or branch deleted externally
    Gone,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run => write!(f, "run"),
            Self::Input => write!(f, "input"),
            Self::Block => write!(f, "block"),
            Self::Question => write!(f, "question"),
            Self::Error => write!(f, "error"),
            Self::Idle => write!(f, "idle"),
            Self::Stale => write!(f, "stale"),
            Self::Gone => write!(f, "gone"),
        }
    }
}

impl TaskStatus {
    /// Sort priority: lower = more urgent (shown first in TUI).
    pub fn sort_priority(&self) -> u8 {
        match self {
            Self::Block | Self::Question => 0,
            Self::Error => 1,
            Self::Input => 2,
            Self::Run => 3,
            Self::Idle => 4,
            Self::Stale => 5,
            Self::Gone => 6,
        }
    }

    /// Whether the task waits on the user.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Self::Input | Self::Block | Self::Question | Self::Error
        )
    }

    /// Status indicator string for display.
    pub fn indicator(&self) -> &'static str {
        match self {
            Self::Run => "● run",
            Self::Input => "▲ input",
            Self::Block => "▲ block",
            Self::Question => "? question",
            Self::Error => "✗ error",
            Self::Idle => "○ idle",
            Self::Stale => "◌ stale",
            Self::Gone => "✗ gone",
        }
    }
}

/// A task with its computed status and optional running agent info.
#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    pub dir: PathBuf,
    pub owned: bool,
    pub repo_name: String,
    pub agent_info: Option<AgentInfo>,
    pub run_count: usize,
    pub last_activity: Option<u64>,
    pub git_branch_status: GitBranchStatus,
}

impl Task {
    /// Build a Task from the daemon's task info and optional agent info.
    pub fn from_info(info: TaskInfo, agent_info: Option<AgentInfo>) -> Self {
        let repo_name = compute_repo_name(&info.dir);
        Self {
            name: info.name,
            dir: info.dir,
            owned: info.owned,
            repo_name,
            agent_info,
            run_count: info.run_count,
            last_activity: info.last_activity,
            git_branch_status: GitBranchStatus::Unknown,
        }
    }

    /// Take the daemon's latest view of the task, keeping the derived state
    /// (agent, git status).
    pub fn update_info(&mut self, info: TaskInfo) {
        if info.dir != self.dir {
            self.repo_name = compute_repo_name(&info.dir);
        }
        self.dir = info.dir;
        self.owned = info.owned;
        self.run_count = info.run_count;
        self.last_activity = info.last_activity;
    }

    /// Refresh the git branch status (only meaningful for owned tasks
    /// without a running agent).
    pub fn refresh_git_status(&mut self) {
        if self.owned && self.agent_info.is_none() {
            self.git_branch_status = check_git_branch_status(&self.name, &self.dir);
        }
    }

    /// Compute the current status from daemon state + git branch state + activity.
    pub fn status(&self) -> TaskStatus {
        if let Some(ref info) = self.agent_info {
            return match info.state {
                AgentState::Working => TaskStatus::Run,
                AgentState::Input => TaskStatus::Input,
                AgentState::Blocked => TaskStatus::Block,
                AgentState::Idle => TaskStatus::Idle,
                AgentState::Question => TaskStatus::Question,
                AgentState::Error => TaskStatus::Error,
            };
        }

        // No agent running — check filesystem and git state for owned tasks
        if self.owned {
            if !self.dir.exists() {
                return TaskStatus::Gone;
            }
            if self.git_branch_status == GitBranchStatus::Gone {
                return TaskStatus::Gone;
            }
        }

        // Check staleness based on last activity
        if self.is_stale() {
            return TaskStatus::Stale;
        }

        TaskStatus::Idle
    }

    /// Whether the task has had no activity for longer than the stale threshold.
    fn is_stale(&self) -> bool {
        let Some(last) = self.last_activity else {
            return false;
        };
        now().saturating_sub(last) >= STALE_THRESHOLD_SECS
    }
}

/// Current Unix timestamp in seconds.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Derive the repository name from a task directory.
///
/// For worktrees, returns the main repo name (parsed from `.git` file).
/// Otherwise, returns the directory basename.
fn compute_repo_name(dir: &Path) -> String {
    if tam_worktree::pretty::is_worktree(dir) {
        if let Ok(name) = tam_worktree::pretty::worktree_main_repo_name(dir) {
            return name;
        }
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Query git to determine branch status for an owned task.
/// Returns Unknown if git operations fail (graceful degradation).
pub fn check_git_branch_status(task_name: &str, task_dir: &Path) -> GitBranchStatus {
    let root = match tam_worktree::git::repo_root(task_dir) {
        Ok(r) => r,
        Err(_) => return GitBranchStatus::Unknown,
    };

    match tam_worktree::git::local_branch_exists(&root, task_name) {
        Ok(true) => GitBranchStatus::Active,
        Ok(false) => GitBranchStatus::Gone,
        Err(_) => GitBranchStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn test_snapshot() -> TaskInfo {
        TaskInfo {
            name: "feat".into(),
            dir: PathBuf::from("/tmp"), // use /tmp which always exists
            owned: true,
            created_at: now_secs(),
            run_count: 3,
            last_activity: Some(now_secs()),
        }
    }

    #[test]
    fn idle_when_no_agent() {
        let task = Task::from_info(test_snapshot(), None);
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn stale_when_no_recent_activity() {
        let mut snapshot = test_snapshot();
        snapshot.last_activity = Some(1000); // epoch 1970 — very old
        let task = Task::from_info(snapshot, None);
        assert_eq!(task.status(), TaskStatus::Stale);
    }

    #[test]
    fn run_when_agent_working() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Working,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            detail: Default::default(),
        };
        let task = Task::from_info(test_snapshot(), Some(info));
        assert_eq!(task.status(), TaskStatus::Run);
    }

    #[test]
    fn input_when_agent_input() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Input,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            detail: Default::default(),
        };
        let task = Task::from_info(test_snapshot(), Some(info));
        assert_eq!(task.status(), TaskStatus::Input);
    }

    #[test]
    fn block_when_agent_blocked() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Blocked,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            detail: Default::default(),
        };
        let task = Task::from_info(test_snapshot(), Some(info));
        assert_eq!(task.status(), TaskStatus::Block);
    }

    #[test]
    fn gone_when_dir_missing() {
        let mut snapshot = test_snapshot();
        snapshot.dir = PathBuf::from("/nonexistent/path/that/doesnt/exist");
        let task = Task::from_info(snapshot, None);
        assert_eq!(task.status(), TaskStatus::Gone);
    }

    #[test]
    fn gone_when_branch_gone() {
        let mut task = Task::from_info(test_snapshot(), None);
        task.git_branch_status = GitBranchStatus::Gone;
        assert_eq!(task.status(), TaskStatus::Gone);
    }

    #[test]
    fn idle_when_branch_active() {
        let mut task = Task::from_info(test_snapshot(), None);
        task.git_branch_status = GitBranchStatus::Active;
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn borrowed_task_ignores_git_status() {
        let mut snapshot = test_snapshot();
        snapshot.owned = false;
        let mut task = Task::from_info(snapshot, None);
        task.git_branch_status = GitBranchStatus::Gone;
        // Borrowed tasks stay Idle regardless of git state
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn agent_state_overrides_staleness() {
        let info = AgentInfo {
            id: "feat".into(),
            provider: "claude".into(),
            dir: PathBuf::from("/tmp/feat"),
            state: AgentState::Working,
            pid: Some(1234),
            uptime_secs: 60,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            detail: Default::default(),
        };
        let mut snapshot = test_snapshot();
        snapshot.last_activity = Some(1000); // very old
        let task = Task::from_info(snapshot, Some(info));
        // Agent running takes priority over staleness
        assert_eq!(task.status(), TaskStatus::Run);
    }

    #[test]
    fn sort_priority_ordering() {
        assert!(TaskStatus::Block.sort_priority() < TaskStatus::Input.sort_priority());
        assert!(TaskStatus::Input.sort_priority() < TaskStatus::Run.sort_priority());
        assert!(TaskStatus::Run.sort_priority() < TaskStatus::Idle.sort_priority());
        assert!(TaskStatus::Idle.sort_priority() < TaskStatus::Stale.sort_priority());
    }

    #[test]
    fn indicator_strings() {
        assert_eq!(TaskStatus::Run.indicator(), "● run");
        assert_eq!(TaskStatus::Input.indicator(), "▲ input");
        assert_eq!(TaskStatus::Stale.indicator(), "◌ stale");
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", TaskStatus::Run), "run");
        assert_eq!(format!("{}", TaskStatus::Idle), "idle");
    }
}
