use std::path::{Path, PathBuf};

use tam_proto::{AgentInfo, AgentState};

use crate::ledger::TaskSnapshot;

/// Git branch state for an owned task, populated by callers before status().
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitBranchStatus {
    #[default]
    Unknown,
    /// Branch exists, not merged
    Active,
    /// Branch is merged into default branch
    Merged,
    /// Local branch does not exist (deleted externally)
    BranchGone,
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
    /// No agent running, task exists
    Idle,
    /// No agent running, branch is merged into default branch
    Merged,
    /// Worktree exists but branch was deleted
    Orphan,
    /// Worktree was deleted externally
    Gone,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Run => write!(f, "run"),
            Self::Input => write!(f, "input"),
            Self::Block => write!(f, "block"),
            Self::Idle => write!(f, "idle"),
            Self::Merged => write!(f, "merged"),
            Self::Orphan => write!(f, "orphan"),
            Self::Gone => write!(f, "gone"),
        }
    }
}

impl TaskStatus {
    /// Sort priority: lower = more urgent (shown first in TUI).
    pub fn sort_priority(&self) -> u8 {
        match self {
            Self::Block => 0,
            Self::Input => 1,
            Self::Run => 2,
            Self::Idle => 3,
            Self::Merged => 4,
            Self::Orphan => 5,
            Self::Gone => 6,
        }
    }

    /// Status indicator string for display.
    pub fn indicator(&self) -> &'static str {
        match self {
            Self::Run => "● run",
            Self::Input => "▲ input",
            Self::Block => "▲ block",
            Self::Idle => "○ idle",
            Self::Merged => "✓ merged",
            Self::Orphan => "? orphan",
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
    pub agent_info: Option<AgentInfo>,
    pub run_count: usize,
    pub last_activity: Option<u64>,
    pub git_branch_status: GitBranchStatus,
}

impl Task {
    /// Build a Task from a ledger snapshot and optional daemon agent info.
    pub fn from_snapshot(snapshot: TaskSnapshot, agent_info: Option<AgentInfo>) -> Self {
        Self {
            name: snapshot.name,
            dir: snapshot.dir,
            owned: snapshot.owned,
            agent_info,
            run_count: snapshot.run_count,
            last_activity: snapshot.last_activity,
            git_branch_status: GitBranchStatus::Unknown,
        }
    }

    /// Compute the current status from daemon state + git branch state.
    pub fn status(&self) -> TaskStatus {
        if let Some(ref info) = self.agent_info {
            return match info.state {
                AgentState::Working => TaskStatus::Run,
                AgentState::Input => TaskStatus::Input,
                AgentState::Blocked => TaskStatus::Block,
                AgentState::Idle => TaskStatus::Idle,
            };
        }

        // No agent running — check filesystem and git state for owned tasks
        if self.owned {
            if !self.dir.exists() {
                return TaskStatus::Gone;
            }
            match self.git_branch_status {
                GitBranchStatus::Merged => return TaskStatus::Merged,
                GitBranchStatus::BranchGone => return TaskStatus::Orphan,
                _ => {}
            }
        }

        TaskStatus::Idle
    }
}

/// Query git to determine branch status for an owned task.
/// Returns Unknown if git operations fail (graceful degradation).
pub fn check_git_branch_status(task_name: &str, task_dir: &Path) -> GitBranchStatus {
    let root = match tam_worktree::git::repo_root(task_dir) {
        Ok(r) => r,
        Err(_) => return GitBranchStatus::Unknown,
    };

    match tam_worktree::git::local_branch_exists(&root, task_name) {
        Ok(false) => return GitBranchStatus::BranchGone,
        Err(_) => return GitBranchStatus::Unknown,
        Ok(true) => {}
    }

    let default = match tam_worktree::git::default_branch(&root) {
        Ok(d) => d,
        Err(_) => return GitBranchStatus::Unknown,
    };

    match tam_worktree::git::is_branch_merged(&root, task_name, &default) {
        Ok(true) => GitBranchStatus::Merged,
        Ok(false) => GitBranchStatus::Active,
        Err(_) => GitBranchStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_snapshot() -> TaskSnapshot {
        TaskSnapshot {
            name: "feat".into(),
            dir: PathBuf::from("/tmp"), // use /tmp which always exists
            owned: true,
            run_count: 3,
            last_activity: Some(1000),
        }
    }

    #[test]
    fn idle_when_no_agent() {
        let task = Task::from_snapshot(test_snapshot(), None);
        assert_eq!(task.status(), TaskStatus::Idle);
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
        };
        let task = Task::from_snapshot(test_snapshot(), Some(info));
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
        };
        let task = Task::from_snapshot(test_snapshot(), Some(info));
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
        };
        let task = Task::from_snapshot(test_snapshot(), Some(info));
        assert_eq!(task.status(), TaskStatus::Block);
    }

    #[test]
    fn gone_when_dir_missing() {
        let mut snapshot = test_snapshot();
        snapshot.dir = PathBuf::from("/nonexistent/path/that/doesnt/exist");
        let task = Task::from_snapshot(snapshot, None);
        assert_eq!(task.status(), TaskStatus::Gone);
    }

    #[test]
    fn merged_when_branch_merged() {
        let mut task = Task::from_snapshot(test_snapshot(), None);
        task.git_branch_status = GitBranchStatus::Merged;
        assert_eq!(task.status(), TaskStatus::Merged);
    }

    #[test]
    fn orphan_when_branch_gone() {
        let mut task = Task::from_snapshot(test_snapshot(), None);
        task.git_branch_status = GitBranchStatus::BranchGone;
        assert_eq!(task.status(), TaskStatus::Orphan);
    }

    #[test]
    fn idle_when_branch_active() {
        let mut task = Task::from_snapshot(test_snapshot(), None);
        task.git_branch_status = GitBranchStatus::Active;
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn borrowed_task_ignores_git_status() {
        let mut snapshot = test_snapshot();
        snapshot.owned = false;
        let mut task = Task::from_snapshot(snapshot, None);
        task.git_branch_status = GitBranchStatus::Merged;
        // Borrowed tasks stay Idle regardless of git state
        assert_eq!(task.status(), TaskStatus::Idle);
    }

    #[test]
    fn agent_state_overrides_git_status() {
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
        };
        let mut task = Task::from_snapshot(test_snapshot(), Some(info));
        task.git_branch_status = GitBranchStatus::Merged;
        // Agent running takes priority over git state
        assert_eq!(task.status(), TaskStatus::Run);
    }

    #[test]
    fn sort_priority_ordering() {
        assert!(TaskStatus::Block.sort_priority() < TaskStatus::Input.sort_priority());
        assert!(TaskStatus::Input.sort_priority() < TaskStatus::Run.sort_priority());
        assert!(TaskStatus::Run.sort_priority() < TaskStatus::Idle.sort_priority());
        assert!(TaskStatus::Idle.sort_priority() < TaskStatus::Merged.sort_priority());
    }

    #[test]
    fn indicator_strings() {
        assert_eq!(TaskStatus::Run.indicator(), "● run");
        assert_eq!(TaskStatus::Input.indicator(), "▲ input");
        assert_eq!(TaskStatus::Merged.indicator(), "✓ merged");
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", TaskStatus::Run), "run");
        assert_eq!(format!("{}", TaskStatus::Idle), "idle");
    }
}
