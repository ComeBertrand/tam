use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use tam_proto::{AgentInfo, MessageInfo, RunInfo, TaskInfo};

use crate::config::CustomCommand;
use crate::task::{Member, Task};

use super::branch::{BranchState, Observation};
use super::peek::Peek;

pub enum Mode {
    Normal,
    NewTaskPickProject(PickerState),
    /// The task's name, for worktrees in these repositories (main
    /// checkouts). The name of an existing task adds them to it.
    NewTaskEnterName {
        project_dirs: Vec<PathBuf>,
        name: String,
    },
    SpawnEnterPath(String),
    ConfirmDropTask {
        name: String,
    },
    /// The daemon refused the drop: `report` tells why (work it would lose).
    ConfirmForceDrop {
        name: String,
        delete_branch: bool,
        report: Vec<String>,
    },
    /// A message too long for the status line (e.g. a failed drop); any key
    /// closes it.
    Notice {
        title: String,
        lines: Vec<String>,
    },
    /// A task with a worktree gone or unusable (`lines` say what): recreate
    /// the worktrees that can be (`repairable`), or drop the task. Deleting
    /// branches is offered only when some worktree is `in_place`: a drop keeps
    /// the branch of a gone one.
    ConfirmRepairOrDrop {
        name: String,
        lines: Vec<String>,
        repairable: bool,
        in_place: bool,
    },
}

/// Which view of the focused member the detail pane shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneTab {
    /// Why it waits and since when, its branch and messages; its screen
    /// below.
    #[default]
    Triage,
    /// Its screen, on the whole pane.
    Terminal,
}

/// The member of the selected task that the detail pane shows and member
/// actions (attach, stop, run) apply to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub task: String,
    pub member: String,
}

/// What the detail pane shows of the selected task beyond the task list,
/// fetched when the task is selected.
#[derive(Debug, Default)]
pub struct TaskDetail {
    pub task: String,
    /// The task's agent runs, oldest first.
    pub runs: Vec<RunInfo>,
    /// Messages to or from its members, oldest first.
    pub messages: Vec<MessageInfo>,
    /// To fetch again (e.g. a run started or ended).
    pub stale: bool,
}

pub struct PickerItem {
    pub display: String,
    pub id: String,
}

pub struct PickerState {
    pub title: String,
    pub items: Vec<PickerItem>,
    pub filter: String,
    pub selected: usize,
    /// Ids of the items picked with tab, in the order picked.
    pub marked: Vec<String>,
}

impl PickerState {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self {
            title: title.into(),
            items,
            filter: String::new(),
            selected: 0,
            marked: Vec::new(),
        }
    }

    /// Pick the highlighted item (or unpick it), then move down. Items whose
    /// id starts with `__` other than `__cwd__` are actions: not picked.
    pub fn toggle_mark(&mut self) {
        let Some(id) = self.selected_item().map(|item| item.id.clone()) else {
            return;
        };
        if id.starts_with("__") && id != "__cwd__" {
            return;
        }
        match self.marked.iter().position(|m| *m == id) {
            Some(pos) => {
                self.marked.remove(pos);
            }
            None => self.marked.push(id),
        }
        self.select_next();
    }

    pub fn filtered_items(&self) -> Vec<&PickerItem> {
        if self.filter.is_empty() {
            self.items.iter().collect()
        } else {
            let lower = self.filter.to_lowercase();
            self.items
                .iter()
                .filter(|item| item.display.to_lowercase().contains(&lower))
                .collect()
        }
    }

    pub fn selected_item(&self) -> Option<&PickerItem> {
        let filtered = self.filtered_items();
        filtered.get(self.selected).copied()
    }

    pub fn select_next(&mut self) {
        let count = self.filtered_items().len();
        if count > 0 {
            self.selected = (self.selected + 1).min(count - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn type_char(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }
}

pub struct App {
    pub tasks: Vec<Task>,
    pub selected: usize,
    pub status: Option<(String, Instant)>,
    pub mode: Mode,
    /// The preview pane, when shown.
    pub peek: Option<Peek>,
    pub filter: String,
    pub filter_active: bool,
    pub commands: Vec<CustomCommand>,
    /// See [`App::focused_member`].
    pub focus: Option<Focus>,
    pub tab: PaneTab,
    /// The selected task's runs and messages, while the pane is shown.
    pub detail: Option<TaskDetail>,
    /// Git's view of members' worktrees, by member id.
    pub branches: HashMap<String, Observation>,
}

impl App {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            selected: 0,
            status: None,
            mode: Mode::Normal,
            peek: None,
            filter: String::new(),
            filter_active: false,
            commands: Vec::new(),
            focus: None,
            tab: PaneTab::default(),
            detail: None,
            branches: HashMap::new(),
        }
    }

    /// Replace the task list, preserving selection.
    pub fn set_tasks(&mut self, tasks: Vec<Task>) {
        let prev_name = self.selected_task().map(|t| t.name.clone());
        self.tasks = tasks;
        self.sort_tasks();
        if let Some(name) = prev_name {
            if let Some(pos) = self.tasks.iter().position(|t| t.name == name) {
                self.selected = pos;
                return;
            }
        }
        self.clamp_selection();
    }

    fn sort_tasks(&mut self) {
        self.tasks.sort_by_key(|t| {
            (
                t.repo_name().to_lowercase(),
                t.status().sort_priority(),
                t.name.to_lowercase(),
            )
        });
    }

    pub fn visible_tasks(&self) -> Vec<&Task> {
        if self.filter.is_empty() {
            self.tasks.iter().collect()
        } else {
            let lower = self.filter.to_lowercase();
            self.tasks
                .iter()
                .filter(|t| {
                    t.name.to_lowercase().contains(&lower)
                        || t.repo_name().to_lowercase().contains(&lower)
                        || t.members.iter().any(|m| {
                            m.info.dir.to_string_lossy().to_lowercase().contains(&lower)
                                || m.agent_info
                                    .as_ref()
                                    .is_some_and(|a| a.provider.to_lowercase().contains(&lower))
                        })
                })
                .collect()
        }
    }

    pub fn select_next(&mut self) {
        let count = self.visible_tasks().len();
        if count > 0 {
            self.selected = (self.selected + 1).min(count - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.visible_tasks().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        let count = self.visible_tasks().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(count - 1);
        }
    }

    /// Add a task, or replace the one with the same name.
    pub fn upsert_task(&mut self, task: Task) {
        let prev = self.selected_task().map(|t| t.name.clone());
        match self.tasks.iter_mut().find(|t| t.name == task.name) {
            Some(existing) => *existing = task,
            None => self.tasks.push(task),
        }
        self.sort_tasks();
        self.reselect(prev);
    }

    /// Apply the daemon's latest summary of a task.
    pub fn update_task_info(&mut self, info: TaskInfo) {
        let prev = self.selected_task().map(|t| t.name.clone());
        if let Some(detail) = self.detail.as_mut().filter(|d| d.task == info.name) {
            detail.stale = true;
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.name == info.name) {
            task.update_info(info);
            self.sort_tasks();
            self.reselect(prev);
        }
    }

    pub fn remove_task(&mut self, name: &str) {
        let prev = self.selected_task().map(|t| t.name.clone());
        self.tasks.retain(|t| t.name != name);
        self.reselect(prev);
    }

    /// Keep the selection on the named task if it is still visible.
    fn reselect(&mut self, name: Option<String>) {
        if let Some(name) = name {
            if let Some(pos) = self.visible_tasks().iter().position(|t| t.name == name) {
                self.selected = pos;
                return;
            }
        }
        self.clamp_selection();
    }

    /// The member an agent runs for (agent ids are member ids).
    fn member_mut(&mut self, id: &str) -> Option<&mut Member> {
        self.tasks.iter_mut().find_map(|t| t.member_mut(id))
    }

    /// A member and its task, by member id.
    pub fn find_member(&self, id: &str) -> Option<(&Task, &Member)> {
        self.tasks.iter().find_map(|t| t.member(id).map(|m| (t, m)))
    }

    /// Update the agent info of the member it runs for.
    pub fn update_agent(&mut self, info: AgentInfo) {
        if let Some(member) = self.member_mut(&info.id) {
            member.agent_info = Some(info);
            self.sort_tasks();
        }
    }

    pub fn update_state(&mut self, id: &str, new_state: tam_proto::AgentState) {
        if let Some(member) = self.member_mut(id) {
            if let Some(ref mut info) = member.agent_info {
                // Already taken from the daemon's info (with its time)
                // when a hook changed it.
                if info.state != new_state {
                    info.state = new_state;
                    info.state_since = Some(crate::task::now());
                }
            }
            self.sort_tasks();
        }
        // A turn ended or began: its changes are worth a new look.
        self.branch_changed(id);
    }

    pub fn update_context(&mut self, id: &str, context_percent: u8) {
        if let Some(member) = self.member_mut(id) {
            if let Some(ref mut info) = member.agent_info {
                info.context_percent = Some(context_percent);
            }
        }
    }

    /// Remove agent info from a member (agent exited, member becomes idle).
    pub fn remove_agent(&mut self, id: &str) {
        if let Some(member) = self.member_mut(id) {
            member.agent_info = None;
            self.sort_tasks();
        }
        self.clamp_selection();
        self.branch_changed(id);
    }

    pub fn set_status(&mut self, msg: String, duration: std::time::Duration) {
        self.status = Some((msg, Instant::now() + duration));
    }

    pub fn status_message(&mut self) -> Option<&str> {
        if let Some((_, expiry)) = &self.status {
            if Instant::now() >= *expiry {
                self.status = None;
                return None;
            }
        }
        self.status.as_ref().map(|(msg, _)| msg.as_str())
    }

    /// The agent the preview pane should show: the focused member's.
    pub fn peek_agent(&self) -> Option<String> {
        self.focused_member()
            .and_then(|(_, m)| m.agent_info.as_ref())
            .map(|a| a.id.clone())
    }

    /// The preview pane shows another agent than the one it should.
    pub fn peek_stale(&self) -> bool {
        self.peek
            .as_ref()
            .is_some_and(|p| p.agent != self.peek_agent())
    }

    /// Count tasks needing attention (input, blocked, question or error).
    pub fn needs_attention_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status().needs_attention())
            .count()
    }

    /// The member the detail pane shows and member actions apply to: the
    /// one focused in the selected task, else the one that most needs the
    /// user.
    pub fn focused_member(&self) -> Option<(&Task, &Member)> {
        let task = self.selected_task()?;
        let focused = self
            .focus
            .as_ref()
            .filter(|f| f.task == task.name)
            .and_then(|f| task.member(&f.member));
        Some((task, focused.or_else(|| most_urgent(task))?))
    }

    /// While the pane is shown, focus the selected task's most urgent
    /// member when the selection is on another task than the focus (or the
    /// focused member left it). The focus then stays while the task is
    /// selected, whatever becomes more urgent: a key never acts on a member
    /// the pane did not show. With the pane closed nothing is focused: keys
    /// act on the member the task's status in the list is about.
    pub fn sync_focus(&mut self) {
        let Some(task) = self.selected_task().filter(|_| self.peek.is_some()) else {
            self.focus = None;
            return;
        };
        let current = self
            .focus
            .as_ref()
            .is_some_and(|f| f.task == task.name && task.member(&f.member).is_some());
        if !current {
            self.focus = most_urgent(task).map(|m| Focus {
                task: task.name.clone(),
                member: m.id().to_string(),
            });
        }
    }

    /// Focus the next (`step` 1) or previous (-1) member of the selected
    /// task, wrapping around.
    pub fn cycle_member(&mut self, step: isize) {
        let Some((task, member)) = self.focused_member() else {
            return;
        };
        let count = task.members.len() as isize;
        let Some(pos) = task.members.iter().position(|m| m.id() == member.id()) else {
            return;
        };
        let next = (pos as isize + step).rem_euclid(count) as usize;
        self.focus = Some(Focus {
            task: task.name.clone(),
            member: task.members[next].id().to_string(),
        });
    }

    /// The pane shows another task's runs and messages than the selected
    /// one's, or they changed.
    pub fn detail_stale(&self) -> bool {
        if self.peek.is_none() {
            return false;
        }
        let selected = self.selected_task().map(|t| t.name.as_str());
        match &self.detail {
            Some(detail) => detail.stale || Some(detail.task.as_str()) != selected,
            None => selected.is_some(),
        }
    }

    /// A message was sent or delivered: keep it if it is to or from a
    /// member of the task shown.
    pub fn note_message(&mut self, message: MessageInfo) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let Some(task) = self.tasks.iter().find(|t| t.name == detail.task) else {
            return;
        };
        let ours = |id: &str| task.member(id).is_some();
        if !ours(&message.to) && !message.from.as_deref().is_some_and(ours) {
            return;
        }
        match detail.messages.iter_mut().find(|m| m.id == message.id) {
            Some(known) => *known = message,
            None => detail.messages.push(message),
        }
    }

    /// The focused member's worktree, when git should be asked about it
    /// again (marked as being looked at): `(member id, dir)`.
    pub fn branch_due(&mut self) -> Option<(String, PathBuf)> {
        self.peek.as_ref()?;
        let (_, member) = self.focused_member()?;
        if member.is_unhealthy() {
            return None;
        }
        let (id, dir) = (member.id().to_string(), member.info.dir.clone());
        let observation = self.branches.entry(id.clone()).or_default();
        if !observation.due() {
            return None;
        }
        observation.running = true;
        Some((id, dir))
    }

    /// What git said of a member's worktree.
    pub fn set_branch(&mut self, id: String, state: Result<BranchState, String>) {
        let observation = self.branches.entry(id).or_default();
        observation.state = Some(state);
        observation.at = Some(Instant::now());
        observation.running = false;
    }

    /// Look at a member's worktree again when it is next shown.
    fn branch_changed(&mut self, id: &str) {
        if let Some(observation) = self.branches.get_mut(id) {
            observation.at = None;
        }
    }
}

/// The member that most needs the user (the first one, in join order, of
/// the most urgent status).
fn most_urgent(task: &Task) -> Option<&Member> {
    task.members
        .iter()
        .min_by_key(|m| task.member_status(m).sort_priority())
}
