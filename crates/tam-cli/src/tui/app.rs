use std::path::PathBuf;
use std::time::Instant;

use tam_proto::AgentInfo;

use crate::config::CustomCommand;
use crate::task::{Task, TaskStatus};

pub enum Mode {
    Normal,
    NewTaskPickProject(PickerState),
    NewTaskEnterName {
        project_dir: PathBuf,
        name: String,
        create_worktree: bool,
    },
    RunPickSession {
        task_name: String,
        picker: PickerState,
    },
    SpawnEnterPath(String),
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
}

impl PickerState {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self {
            title: title.into(),
            items,
            filter: String::new(),
            selected: 0,
        }
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
    pub peek: Option<String>,
    pub filter: String,
    pub filter_active: bool,
    pub commands: Vec<CustomCommand>,
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
        self.tasks
            .sort_by_key(|t| (t.status().sort_priority(), t.name.clone()));
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
                        || t.dir.to_string_lossy().to_lowercase().contains(&lower)
                        || t.agent_info
                            .as_ref()
                            .map(|a| a.provider.to_lowercase().contains(&lower))
                            .unwrap_or(false)
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

    /// Update agent info for a task by matching agent ID to task name.
    pub fn update_agent(&mut self, info: AgentInfo) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.name == info.id) {
            task.agent_info = Some(info);
            self.sort_tasks();
        }
    }

    pub fn update_state(&mut self, id: &str, new_state: tam_proto::AgentState) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.name == id) {
            if let Some(ref mut info) = task.agent_info {
                info.state = new_state;
            }
            self.sort_tasks();
        }
    }

    pub fn update_context(&mut self, id: &str, context_percent: u8) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.name == id) {
            if let Some(ref mut info) = task.agent_info {
                info.context_percent = Some(context_percent);
            }
        }
    }

    /// Remove agent info from a task (agent exited, task becomes idle).
    pub fn remove_agent(&mut self, id: &str) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.name == id) {
            task.agent_info = None;
            self.sort_tasks();
        }
        self.clamp_selection();
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

    /// Refresh git branch status for owned idle tasks and re-sort.
    pub fn refresh_git_status(&mut self) {
        for t in &mut self.tasks {
            if t.owned && t.agent_info.is_none() {
                t.git_branch_status = crate::task::check_git_branch_status(&t.name, &t.dir);
            }
        }
        self.sort_tasks();
        self.clamp_selection();
    }

    /// Count tasks needing attention (input or blocked).
    pub fn needs_attention_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| matches!(t.status(), TaskStatus::Input | TaskStatus::Block))
            .count()
    }
}
