use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::KeyEvent;

use tam_proto::{AgentInfo, ChatInfo, MessageInfo, RunInfo, SessionInfo, ShellInfo, TaskInfo};

use crate::chat::Chat;
use crate::config::CustomCommand;
use crate::task::{Member, Task, TaskStatus};

use ratatui::layout::Rect;

use super::answer::AnswerDialog;
use super::branch::{self, BranchState, Observation};
use super::fuzzy::fuzzy;
use super::input::TextInput;
use super::peek::Peek;

pub enum Mode {
    Normal,
    /// The repositories of a new task (several with tab), or the one of a
    /// new chat.
    PickProject(PickerState, Purpose),
    /// The task's name, for worktrees in these repositories (main
    /// checkouts). The name of an existing task adds them to it.
    NewTaskEnterName {
        project_dirs: Vec<PathBuf>,
        name: TextInput,
    },
    /// A repository's path, for a new task or chat.
    SpawnEnterPath(TextInput, Purpose),
    /// What a new chat starts from: a new session, or one of its
    /// repository's.
    PickSession(SessionPicker),
    ConfirmDropTask {
        name: String,
        /// The focused member, when the task has others: it can be dropped
        /// alone.
        member: Option<MemberChoice>,
    },
    /// The daemon refused the drop: `report` tells why (work it would lose).
    ConfirmForceDrop {
        /// What is dropped, as shown: the task name or a member label.
        name: String,
        /// What `DropTask` names: the task, or the member's id.
        target: String,
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
        /// The focused member, when the task has others: it can be dropped
        /// alone.
        member: Option<MemberChoice>,
    },
    /// The keys, custom commands included; any key closes it.
    Help,
    /// The request a member's agent waits on, to answer (`a`).
    Answer(AnswerDialog),
    /// The pane has the keyboard: what is typed goes to this agent, until
    /// ctrl-] (see `keys`). The dashboard is drawn as in `Normal`.
    Terminal {
        agent: String,
    },
    /// The pane scrolls through this agent's screen and history: `j`/`k`
    /// move it a line, until ctrl-] (back to the live screen). Keys go
    /// neither to the agent nor to the list. Drawn as in `Normal`.
    Scroll {
        agent: String,
    },
    /// Asks to close a chat.
    ConfirmCloseChat {
        id: String,
        label: String,
    },
    /// Asks to close a shell (`label`: `Shell #3 of <where it runs>`).
    ConfirmCloseShell {
        id: String,
        label: String,
    },
    /// A chat's name; empty: Claude's title (`title`) again.
    RenameChat {
        id: String,
        label: String,
        title: Option<String>,
        name: TextInput,
    },
}

impl Mode {
    /// The agent whose pane has the keyboard: typed into, or scrolled.
    pub fn pane_agent(&self) -> Option<&str> {
        match self {
            Mode::Terminal { agent } | Mode::Scroll { agent } => Some(agent),
            _ => None,
        }
    }
}

/// What the repository picker picks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A new task: one repository or several (tab).
    Task,
    /// A new chat: one repository.
    Chat,
    /// A new shell on a repository's main checkout: one repository.
    Shell,
}

/// The id of the session list's first item: a new session.
pub const NEW_SESSION: &str = "__new__";

/// What a new chat in the repository whose main checkout is `dir` starts
/// from: a new session (the first item), or one of `sessions` (the
/// repository's, newest first, those a chat holds left out: the daemon's
/// `ChatHistory`), picked with the picker's filter.
pub struct SessionPicker {
    pub dir: PathBuf,
    pub sessions: Vec<SessionInfo>,
    /// Its items' ids: [`NEW_SESSION`], then the sessions' ids; their
    /// display (what the filter matches) is the title, then the last
    /// prompt (see [`SessionPicker::TITLE_GAP`]).
    pub picker: PickerState,
}

impl SessionPicker {
    /// Between a session's title and its last prompt in an item's display.
    pub const TITLE_GAP: &str = "  ";

    pub fn new(dir: PathBuf, sessions: Vec<SessionInfo>) -> Self {
        let repo = dir.file_name().map_or_else(
            || dir.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let mut items = vec![PickerItem {
            display: "+ new chat".into(),
            id: NEW_SESSION.into(),
        }];
        items.extend(sessions.iter().map(|s| PickerItem {
            display: match (&s.title, &s.last_prompt) {
                (Some(title), Some(prompt)) => format!("{title}{}{prompt}", Self::TITLE_GAP),
                (Some(text), None) | (None, Some(text)) => text.clone(),
                (None, None) => s.id.clone(),
            },
            id: s.id.clone(),
        }));
        Self {
            picker: PickerState::new(format!("New chat in {repo}"), items),
            dir,
            sessions,
        }
    }
}

/// How the task list is ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Order {
    /// Under a header row per repository (the set of them for a task in
    /// several), the most urgent first in each.
    #[default]
    Repo,
    /// The most urgent first, all repositories together.
    Attention,
}

/// A row of the task list.
#[derive(Debug, Clone, Copy)]
pub enum Row<'a> {
    /// The header of the chats' section.
    Chats,
    /// A chat. Selected like a task.
    Chat(&'a Chat),
    /// The repositories of the tasks below it.
    Group(&'a Task),
    /// A task: the row of its only member, or the head of its members'
    /// rows. Selected, as chats and shells are.
    Task(&'a Task),
    /// A member of a task with several, below the task's row.
    Member {
        task: &'a Task,
        member: &'a Member,
        last: bool,
    },
    /// The header of the section of the shells on main checkouts.
    Shells,
    /// A shell: a row of that section (`tree` `None`), or a row on a branch
    /// below its task or member, for a shell in a worktree. Selected like a
    /// task.
    Shell {
        shell: &'a ShellInfo,
        tree: Option<Branch>,
    },
}

/// Where the row of a shell in a worktree hangs below its task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Branch {
    /// The last of its member's shells.
    pub last: bool,
    /// Below a member's row, in a task with several: whether that member is
    /// the task's last (the task's branch goes no further). `None`: below
    /// the task's own row.
    pub member_last: Option<bool>,
}

/// What the list shows and the selection lands on: a chat, a task or a
/// shell.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    Chat(&'a Chat),
    Task(&'a Task),
    Shell(&'a ShellInfo),
}

impl Entry<'_> {
    /// Which entry it is, to find it again (a chat or a shell by id, a task
    /// by name: never by label, which changes).
    pub fn pick(&self) -> Pick {
        match self {
            Entry::Chat(chat) => Pick::Chat(chat.info.id.clone()),
            Entry::Task(task) => Pick::Task(task.name.clone()),
            Entry::Shell(shell) => Pick::Shell(shell.id.clone()),
        }
    }

    /// Where attention order puts it: the most urgent first, then by name.
    /// Shells have no status: they are never sorted (see
    /// [`App::visible_entries`]).
    fn urgency(&self) -> (u8, String) {
        let (status, name) = match self {
            Entry::Chat(chat) => (chat.status(), chat.info.label()),
            Entry::Task(task) => (task.status(), task.name.clone()),
            Entry::Shell(shell) => (TaskStatus::Idle, shell.name()),
        };
        (status.sort_priority(), name.to_lowercase())
    }
}

/// An entry of the list, to find it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// A chat, by id.
    Chat(String),
    /// A task, by name.
    Task(String),
    /// A shell, by id.
    Shell(String),
}

/// What member actions (enter, attach, stop, answer, custom commands) apply
/// to, the one the pane shows: a member of the selected task, the selected
/// chat, or the selected shell.
#[derive(Debug, Clone, Copy)]
pub enum Focused<'a> {
    Member(&'a Task, &'a Member),
    Chat(&'a Chat),
    /// A shell, and the member whose worktree it runs in (`None` on a main
    /// checkout).
    Shell(&'a ShellInfo, Option<(&'a Task, &'a Member)>),
}

impl<'a> Focused<'a> {
    /// Its agent's id (a member's or chat's id), or the shell's id.
    pub fn id(&self) -> &'a str {
        match self {
            Focused::Member(_, member) => member.id(),
            Focused::Chat(chat) => &chat.info.id,
            Focused::Shell(shell, _) => &shell.id,
        }
    }

    /// How to name it to the user: a shell with where it runs (`Shell #3 ·
    /// fix-api`).
    pub fn label(&self) -> String {
        match self {
            Focused::Member(task, member) => task.member_label(member),
            Focused::Chat(chat) => chat.info.label(),
            Focused::Shell(shell, member) => {
                format!("{} · {}", shell.name(), shell_place(shell, *member))
            }
        }
    }

    /// Its running agent (a shell has none).
    pub fn agent(&self) -> Option<&'a AgentInfo> {
        match self {
            Focused::Member(_, member) => member.agent_info.as_ref(),
            Focused::Chat(chat) => chat.agent.as_ref(),
            Focused::Shell(..) => None,
        }
    }

    /// The terminal the pane shows, which `enter`, `f` and `v` act on, by
    /// id: its running agent's, or the shell.
    pub fn terminal(&self) -> Option<&'a str> {
        match self {
            Focused::Shell(shell, _) => Some(&shell.id),
            _ => self.agent().map(|agent| agent.id.as_str()),
        }
    }

    pub fn chat(&self) -> Option<&'a Chat> {
        match self {
            Focused::Chat(chat) => Some(chat),
            Focused::Member(..) | Focused::Shell(..) => None,
        }
    }

    /// What its custom commands' `{task}`, `{id}`, `{dir}` and
    /// `{provider}` stand for: its task's name, its id, its worktree and
    /// its agent's provider; a chat's label, id and main checkout. A shell
    /// in a worktree gives its member's; one on a main checkout its name,
    /// its id and the checkout.
    pub fn command_values(&self) -> (String, &'a str, &'a std::path::Path, String) {
        let provider = |agent: Option<&AgentInfo>| agent.map(|a| a.provider.clone());
        match *self {
            Focused::Member(task, member) => (
                task.name.clone(),
                member.id(),
                member.info.dir.as_path(),
                provider(member.agent_info.as_ref()).unwrap_or_default(),
            ),
            Focused::Chat(chat) => (
                chat.info.label(),
                &chat.info.id,
                chat.info.dir.as_path(),
                provider(chat.agent.as_ref()).unwrap_or_default(),
            ),
            Focused::Shell(_, Some((task, member))) => {
                Focused::Member(task, member).command_values()
            }
            Focused::Shell(shell, None) => {
                (shell.name(), &shell.id, shell.dir.as_path(), String::new())
            }
        }
    }
}

/// Where a shell runs, as the user knows it: its member's label (the task's
/// name, or `<task>/<member>`), else its repository.
pub fn shell_place(shell: &ShellInfo, member: Option<(&Task, &Member)>) -> String {
    match member {
        Some((task, member)) => task.member_label(member),
        None => shell.repo.clone(),
    }
}

/// A member a drop dialog offers to drop alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberChoice {
    pub id: String,
    /// How to name it (`<task>/<member>`).
    pub label: String,
}

/// The member of the selected task that the detail pane shows and member
/// actions (attach, stop, run) apply to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub task: String,
    pub member: String,
}

/// What the detail pane shows of the selected task or chat beyond the
/// list, fetched when it is selected.
#[derive(Debug)]
pub struct TaskDetail {
    pub of: Pick,
    /// The task's (or chat's) agent runs, oldest first.
    pub runs: Vec<RunInfo>,
    /// Messages to or from its members (or the chat), oldest first.
    pub messages: Vec<MessageInfo>,
    /// To fetch again (e.g. a run started or ended).
    pub stale: bool,
}

impl TaskDetail {
    /// Of this task or chat, with no runs or messages.
    pub fn of(of: Pick) -> Self {
        Self {
            of,
            runs: Vec::new(),
            messages: Vec::new(),
            stale: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub display: String,
    pub id: String,
}

pub struct PickerState {
    pub title: String,
    pub items: Vec<PickerItem>,
    pub filter: TextInput,
    /// Index in the filtered items.
    pub selected: usize,
    /// Ids of the items picked with tab, in the order picked.
    pub marked: Vec<String>,
    /// More items are on their way (projects being discovered).
    pub loading: bool,
    /// The first item on screen (kept between draws).
    pub offset: usize,
}

impl PickerState {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        Self {
            title: title.into(),
            items,
            filter: TextInput::default(),
            selected: 0,
            marked: Vec::new(),
            loading: false,
            offset: 0,
        }
    }

    /// Replace the items, keeping the highlighted one (by id) when it is
    /// still there, and the filter and picks.
    pub fn set_items(&mut self, items: Vec<PickerItem>) {
        let selected = self.selected_item().map(|item| item.id.clone());
        self.items = items;
        self.selected = selected
            .and_then(|id| self.filtered_items().iter().position(|item| item.id == id))
            .unwrap_or(0);
    }

    /// Apply an editing key to the filter: `false` when it is not one.
    pub fn edit(&mut self, key: KeyEvent) -> bool {
        let before = self.filter.value().to_string();
        if !self.filter.handle(key) {
            return false;
        }
        if self.filter.value() != before {
            self.selected = 0;
        }
        true
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

    /// The items the filter matches, the best matches first (in their
    /// order when they match as well), with the characters matched.
    pub fn filtered(&self) -> Vec<(&PickerItem, Vec<usize>)> {
        if self.filter.is_empty() {
            return self.items.iter().map(|item| (item, Vec::new())).collect();
        }
        let mut matched: Vec<(&PickerItem, i32, Vec<usize>)> = self
            .items
            .iter()
            .filter_map(|item| {
                fuzzy(self.filter.value(), &item.display).map(|m| (item, m.score, m.positions))
            })
            .collect();
        // Stable: equal scores keep the items' order.
        matched.sort_by_key(|(_, score, _)| std::cmp::Reverse(*score));
        matched
            .into_iter()
            .map(|(item, _, positions)| (item, positions))
            .collect()
    }

    pub fn filtered_items(&self) -> Vec<&PickerItem> {
        self.filtered().into_iter().map(|(item, _)| item).collect()
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
}

/// Where the last draw put what the mouse can act on.
#[derive(Debug, Default)]
pub struct Hits {
    /// The task list's rows (below its column names).
    pub list: Rect,
    /// For each row of `list` from its top: the visible entry it belongs
    /// to (its index), and the member it is about.
    pub rows: Vec<Option<(usize, Option<String>)>>,
    /// The detail pane.
    pub pane: Option<Rect>,
    /// The project picker's items, and the index of the first one shown.
    pub picker: Option<(Rect, usize)>,
}

/// How long without a key before the dashboard takes keys again after the
/// pane lost the keyboard (see `App::stop_typing`).
const TYPING_PAUSE: std::time::Duration = std::time::Duration::from_millis(700);

pub struct App {
    pub tasks: Vec<Task>,
    /// In creation order.
    pub chats: Vec<Chat>,
    /// In opening order (the daemon's `ListShells`).
    pub shells: Vec<ShellInfo>,
    /// Index in [`App::visible_entries`].
    pub selected: usize,
    pub status: Option<(String, Instant)>,
    pub mode: Mode,
    /// The preview pane, when shown.
    pub peek: Peek,
    /// The running agents' spinner: its frame, `None` for a still mark
    /// (tests).
    pub spin: Option<usize>,
    /// The pane lost the keyboard while one was typing (its agent exited,
    /// say): keys are dropped until typing pauses, so that what was meant
    /// for the agent does not run as the dashboard's own keys.
    pub dropping_keys_until: Option<std::time::Instant>,
    pub filter: TextInput,
    pub filter_active: bool,
    pub commands: Vec<CustomCommand>,
    /// See [`App::focused_member`].
    pub focus: Option<Focus>,
    /// A first `g` was pressed: a second one goes to the first task.
    pub pending_g: bool,
    /// The selected task's runs and messages, while the pane is shown.
    pub detail: Option<TaskDetail>,
    /// Git's view of members' worktrees, by member id.
    pub branches: HashMap<String, Observation>,
    pub order: Order,
    /// The first row of the task list on screen (kept between draws, so
    /// the list scrolls only when the selection would leave it).
    pub table_offset: usize,
    /// The projects found under `$HOME` last time (for the picker, while
    /// they are looked for again).
    pub projects: Option<Vec<PickerItem>>,
    /// Projects are being looked for.
    pub discovering: bool,
    pub hits: Hits,
    /// The agent of this chat was just started for the pane to type into:
    /// the pane keeps the keyboard until it shows up (see
    /// [`App::check_typing`]).
    pub starting: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            chats: Vec::new(),
            shells: Vec::new(),
            selected: 0,
            status: None,
            mode: Mode::Normal,
            peek: Peek::default(),
            spin: None,
            dropping_keys_until: None,
            filter: TextInput::default(),
            filter_active: false,
            commands: Vec::new(),
            focus: None,
            pending_g: false,
            detail: None,
            branches: HashMap::new(),
            order: Order::default(),
            table_offset: 0,
            projects: None,
            discovering: false,
            hits: Hits::default(),
            starting: None,
        }
    }

    /// Replace the task list, preserving selection.
    pub fn set_tasks(&mut self, tasks: Vec<Task>) {
        let prev = self.pick();
        self.tasks = tasks;
        self.sort_tasks();
        self.reselect(prev);
    }

    /// Replace the chats (in creation order), preserving selection.
    pub fn set_chats(&mut self, chats: Vec<Chat>) {
        let prev = self.pick();
        self.chats = chats;
        self.reselect(prev);
    }

    /// Replace the shells (in opening order), preserving selection.
    pub fn set_shells(&mut self, shells: Vec<ShellInfo>) {
        let prev = self.pick();
        self.shells = shells;
        self.reselect(prev);
    }

    fn sort_tasks(&mut self) {
        match self.order {
            Order::Repo => self.tasks.sort_by_key(|t| {
                (
                    repo_group(t).to_lowercase(),
                    t.status().sort_priority(),
                    t.name.to_lowercase(),
                )
            }),
            Order::Attention => self
                .tasks
                .sort_by_key(|t| (t.status().sort_priority(), t.name.to_lowercase())),
        }
    }

    /// Group the list by repository, or show the most urgent first.
    pub fn toggle_order(&mut self) {
        let prev = self.pick();
        self.order = match self.order {
            Order::Repo => Order::Attention,
            Order::Attention => Order::Repo,
        };
        self.sort_tasks();
        self.reselect(prev);
    }

    /// The list's rows: grouped by repository, the chats' section (a header
    /// row, a row per chat), the section of the shells on main checkouts
    /// (the same), then a header row before each repository's tasks;
    /// attention first, chats and tasks in one list, then the shells on
    /// main checkouts. Below a task with several members, a row per member;
    /// below a member (or a task of one), a row per shell in its worktree.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let grouped = self.order == Order::Repo;
        let mut rows = Vec::new();
        let mut group: Option<String> = None;
        let mut shells_header = false;
        for entry in self.visible_entries() {
            let task = match entry {
                Entry::Chat(chat) => {
                    if grouped && rows.is_empty() {
                        rows.push(Row::Chats);
                    }
                    rows.push(Row::Chat(chat));
                    continue;
                }
                // On its task's branch (below).
                Entry::Shell(shell) if shell.member.is_some() => continue,
                Entry::Shell(shell) => {
                    if grouped && !std::mem::replace(&mut shells_header, true) {
                        rows.push(Row::Shells);
                    }
                    rows.push(Row::Shell { shell, tree: None });
                    continue;
                }
                Entry::Task(task) => task,
            };
            if grouped {
                let name = repo_group(task);
                if group.as_ref() != Some(&name) {
                    rows.push(Row::Group(task));
                    group = Some(name);
                }
            }
            rows.push(Row::Task(task));
            let count = task.members.len();
            let several = count > 1;
            for (i, member) in task.members.iter().enumerate() {
                let last = i + 1 == count;
                if several {
                    rows.push(Row::Member { task, member, last });
                }
                let shells = self.member_shells(member.id());
                let n = shells.len();
                rows.extend(shells.into_iter().enumerate().map(|(j, shell)| Row::Shell {
                    shell,
                    tree: Some(Branch {
                        last: j + 1 == n,
                        member_last: several.then_some(last),
                    }),
                }));
            }
        }
        rows
    }

    /// What the list shows, in its order: grouped by repository, the chats
    /// (in creation order), the shells on main checkouts (in opening
    /// order), then the tasks; attention first, chats and tasks in one list,
    /// the most urgent first, then the shells on main checkouts. A shell in
    /// a worktree follows its task, wherever the task is: its members'
    /// shells, in join order, each one's by number. Shells have no status:
    /// never sorted.
    pub fn visible_entries(&self) -> Vec<Entry<'_>> {
        let chats = self.visible_chats().into_iter().map(Entry::Chat);
        let tasks = self.visible_tasks().into_iter().map(Entry::Task);
        let mut heads: Vec<Entry> = chats.chain(tasks).collect();
        if self.order == Order::Attention {
            // Stable: chats keep their order, tasks theirs.
            heads.sort_by_cached_key(Entry::urgency);
        }
        let mut checkouts = Some(self.visible_checkout_shells());
        let mut entries = Vec::with_capacity(heads.len() + self.shells.len());
        for head in heads {
            let Entry::Task(task) = head else {
                entries.push(head);
                continue;
            };
            // Grouped: the shells on main checkouts come before the tasks.
            if self.order == Order::Repo {
                entries.extend(checkouts.take().into_iter().flatten().map(Entry::Shell));
            }
            entries.push(head);
            for member in &task.members {
                entries.extend(
                    self.member_shells(member.id())
                        .into_iter()
                        .map(Entry::Shell),
                );
            }
        }
        entries.extend(checkouts.into_iter().flatten().map(Entry::Shell));
        entries
    }

    /// The shells in a member's worktree, by number.
    pub fn member_shells(&self, member: &str) -> Vec<&ShellInfo> {
        let mut shells: Vec<&ShellInfo> = self
            .shells
            .iter()
            .filter(|s| s.member.as_deref() == Some(member))
            .collect();
        shells.sort_by_key(|s| s.number);
        shells
    }

    /// The shells on main checkouts the filter matches (by repository), in
    /// opening order. (A shell in a worktree shows with its task.)
    fn visible_checkout_shells(&self) -> Vec<&ShellInfo> {
        let query = self.filter.value();
        self.shells
            .iter()
            .filter(|s| s.member.is_none())
            .filter(|s| query.is_empty() || fuzzy(query, &s.repo).is_some())
            .collect()
    }

    /// The tasks the filter matches (by name, or a member's name,
    /// repository or branch), in the list's order.
    pub fn visible_tasks(&self) -> Vec<&Task> {
        let query = self.filter.value();
        if query.is_empty() {
            return self.tasks.iter().collect();
        }
        let matches = |text: &str| fuzzy(query, text).is_some();
        self.tasks
            .iter()
            .filter(|t| {
                matches(&t.name)
                    || t.members.iter().any(|m| {
                        matches(&m.info.name) || matches(&m.info.repo) || matches(&m.info.branch)
                    })
            })
            .collect()
    }

    /// The chats the filter matches (by name, title, id or repository), in
    /// creation order.
    pub fn visible_chats(&self) -> Vec<&Chat> {
        let query = self.filter.value();
        if query.is_empty() {
            return self.chats.iter().collect();
        }
        let matches = |text: &str| fuzzy(query, text).is_some();
        self.chats
            .iter()
            .filter(|c| {
                let info = &c.info;
                info.name.as_deref().is_some_and(matches)
                    || info.title.as_deref().is_some_and(matches)
                    || matches(&info.id)
                    || matches(&info.repo)
            })
            .collect()
    }

    /// Empty the filter, the entry found with it still selected.
    pub fn clear_filter(&mut self) {
        let prev = self.pick();
        self.filter.clear();
        self.reselect(prev);
    }

    /// The project list was looked for: keep it, and show it in the picker
    /// if it is open.
    pub fn set_projects(&mut self, projects: Vec<PickerItem>) {
        self.discovering = false;
        if let Mode::PickProject(picker, _) = &mut self.mode {
            let mut items: Vec<PickerItem> = picker
                .items
                .iter()
                .filter(|item| item.id.starts_with("__"))
                .cloned()
                .collect();
            items.extend(projects.iter().cloned());
            picker.set_items(items);
            picker.loading = false;
        }
        self.projects = Some(projects);
    }

    /// Select the visible entry at `index` (e.g. clicked), focusing
    /// `member` (of a task) when the pane shows one.
    pub fn select_at(&mut self, index: usize, member: Option<String>) {
        if index >= self.visible_entries().len() {
            return;
        }
        self.selected = index;
        self.sync_focus();
        let Some(member) = member else {
            return;
        };
        if let Some(task) = self.selected_task().map(|t| t.name.clone()) {
            self.focus = Some(Focus { task, member });
        }
    }

    /// Select an entry (if visible).
    pub fn select(&mut self, pick: &Pick) {
        if let Some(pos) = self
            .visible_entries()
            .iter()
            .position(|e| e.pick() == *pick)
        {
            self.selected = pos;
            self.sync_focus();
        }
    }

    pub fn select_next(&mut self) {
        let count = self.visible_entries().len();
        if count > 0 {
            self.selected = (self.selected + 1).min(count - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.visible_entries().len().saturating_sub(1);
    }

    pub fn selected_entry(&self) -> Option<Entry<'_>> {
        self.visible_entries().get(self.selected).copied()
    }

    /// The selected task (`None` when a chat or a shell is selected).
    pub fn selected_task(&self) -> Option<&Task> {
        match self.selected_entry()? {
            Entry::Task(task) => Some(task),
            Entry::Chat(_) | Entry::Shell(_) => None,
        }
    }

    /// The selected chat (`None` when a task or a shell is selected).
    pub fn selected_chat(&self) -> Option<&Chat> {
        match self.selected_entry()? {
            Entry::Chat(chat) => Some(chat),
            Entry::Task(_) | Entry::Shell(_) => None,
        }
    }

    /// The selected shell (`None` when a task or a chat is selected).
    pub fn selected_shell(&self) -> Option<&ShellInfo> {
        match self.selected_entry()? {
            Entry::Shell(shell) => Some(shell),
            Entry::Task(_) | Entry::Chat(_) => None,
        }
    }

    /// The selected entry, to find it again.
    pub fn pick(&self) -> Option<Pick> {
        self.selected_entry().map(|e| e.pick())
    }

    fn clamp_selection(&mut self) {
        let count = self.visible_entries().len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(count - 1);
        }
    }

    /// Add a task, or replace the one with the same name.
    pub fn upsert_task(&mut self, task: Task) {
        let prev = self.pick();
        match self.tasks.iter_mut().find(|t| t.name == task.name) {
            Some(existing) => *existing = task,
            None => self.tasks.push(task),
        }
        self.sort_tasks();
        self.reselect(prev);
    }

    /// Apply the daemon's latest summary of a task.
    pub fn update_task_info(&mut self, info: TaskInfo) {
        let prev = self.pick();
        if let Some(detail) = self.detail.as_mut() {
            if detail.of == Pick::Task(info.name.clone()) {
                detail.stale = true;
            }
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.name == info.name) {
            task.update_info(info);
            self.sort_tasks();
            self.reselect(prev);
        }
    }

    pub fn remove_task(&mut self, name: &str) {
        let prev = self.pick();
        self.tasks.retain(|t| t.name != name);
        self.reselect(prev);
    }

    /// Add a chat (last: chats are in creation order), or take the daemon's
    /// latest view of it, keeping its running agent.
    pub fn upsert_chat(&mut self, info: ChatInfo) {
        let prev = self.pick();
        if let Some(detail) = self.detail.as_mut() {
            // A run started or ended.
            if detail.of == Pick::Chat(info.id.clone()) {
                detail.stale = true;
            }
        }
        match self.chats.iter_mut().find(|c| c.info.id == info.id) {
            Some(chat) => chat.info = info,
            None => self.chats.push(Chat::new(info, &[])),
        }
        self.reselect(prev);
    }

    pub fn remove_chat(&mut self, id: &str) {
        let prev = self.pick();
        self.chats.retain(|c| c.info.id != id);
        if self.starting.as_deref() == Some(id) {
            self.starting = None;
        }
        self.reselect(prev);
    }

    /// Add a shell (last: shells are in opening order), or take the
    /// daemon's latest view of it.
    pub fn upsert_shell(&mut self, shell: ShellInfo) {
        let prev = self.pick();
        match self.shells.iter_mut().find(|s| s.id == shell.id) {
            Some(known) => *known = shell,
            None => self.shells.push(shell),
        }
        self.reselect(prev);
    }

    /// A shell is gone (it exited, was closed, or went with its member).
    /// The pane gives back the keyboard it had for it (see
    /// [`App::stop_typing`]). Selected, the selection goes to its task, its
    /// member focused, for a shell in a worktree; to the entry now in its
    /// place for one on a main checkout, as for a closed chat.
    pub fn remove_shell(&mut self, id: &str) {
        let Some(pos) = self.shells.iter().position(|s| s.id == id) else {
            return;
        };
        let prev = self.pick();
        let shell = self.shells.remove(pos);
        if self.mode.pane_agent() == Some(id) {
            self.stop_typing("the shell closed");
        }
        let focus = (prev == Some(Pick::Shell(shell.id)))
            .then(|| shell.member.as_deref().and_then(|m| self.find_member(m)))
            .flatten()
            .map(|(task, member)| Focus {
                task: task.name.clone(),
                member: member.id().to_string(),
            });
        match focus {
            Some(focus) if self.visible_tasks().iter().any(|t| t.name == focus.task) => {
                self.select(&Pick::Task(focus.task.clone()));
                self.focus = Some(focus);
            }
            _ => self.reselect(prev),
        }
    }

    /// A shell just opened from the dashboard: listed, selected (the filter
    /// emptied if it would hide it), and given the keyboard.
    pub fn take_shell(&mut self, shell: ShellInfo) {
        let id = shell.id.clone();
        self.upsert_shell(shell);
        let pick = Pick::Shell(id.clone());
        if !self.visible_entries().iter().any(|e| e.pick() == pick) {
            self.filter.clear();
            self.filter_active = false;
        }
        self.select(&pick);
        // Not shown (its member is in no task we know of yet): the pane
        // would show something else.
        if self.pick() == Some(pick) {
            self.mode = Mode::Terminal { agent: id };
        }
    }

    /// Where a shell runs, as the user knows it (see [`shell_place`]).
    pub fn shell_place(&self, shell: &ShellInfo) -> String {
        let member = shell.member.as_deref().and_then(|m| self.find_member(m));
        shell_place(shell, member)
    }

    /// Keep the selection on this entry if it is still visible.
    fn reselect(&mut self, pick: Option<Pick>) {
        if let Some(pick) = pick {
            if let Some(pos) = self.visible_entries().iter().position(|e| e.pick() == pick) {
                self.selected = pos;
                return;
            }
        }
        self.clamp_selection();
    }

    /// The answer dialog's request is no longer the one its agent waits on
    /// (answered or changed meanwhile, or the agent is gone): it can take
    /// no answer.
    pub fn answer_stale(&self) -> bool {
        let Mode::Answer(dialog) = &self.mode else {
            return false;
        };
        let current = self
            .find_agent(&dialog.agent)
            .and_then(|a| a.detail.ask.as_ref());
        current.is_none_or(|ask| ask.id != dialog.ask.id)
    }

    /// Where an agent's info is kept: the member or chat it runs for (agent
    /// ids are member and chat ids).
    fn agent_slot(&mut self, id: &str) -> Option<&mut Option<AgentInfo>> {
        if let Some(member) = self.tasks.iter_mut().find_map(|t| t.member_mut(id)) {
            return Some(&mut member.agent_info);
        }
        self.chats
            .iter_mut()
            .find(|c| c.info.id == id)
            .map(|c| &mut c.agent)
    }

    /// A member and its task, by member id.
    pub fn find_member(&self, id: &str) -> Option<(&Task, &Member)> {
        self.tasks.iter().find_map(|t| t.member(id).map(|m| (t, m)))
    }

    /// A running agent, by id (a member's or a chat's).
    pub fn find_agent(&self, id: &str) -> Option<&AgentInfo> {
        match self.find_member(id) {
            Some((_, member)) => member.agent_info.as_ref(),
            None => self
                .chats
                .iter()
                .find(|c| c.info.id == id)
                .and_then(|c| c.agent.as_ref()),
        }
    }

    /// Update the agent info of the member or chat it runs for.
    pub fn update_agent(&mut self, info: AgentInfo) {
        let prev = self.pick();
        if self.starting.as_deref() == Some(info.id.as_str()) {
            self.starting = None;
        }
        if let Some(slot) = self.agent_slot(&info.id) {
            *slot = Some(info);
            self.sort_tasks();
            // The selection stays on its entry, wherever the order puts it.
            self.reselect(prev);
        }
    }

    pub fn update_state(&mut self, id: &str, new_state: tam_proto::AgentState) {
        let prev = self.pick();
        if let Some(slot) = self.agent_slot(id) {
            if let Some(info) = slot {
                // Already taken from the daemon's info (with its time)
                // when a hook changed it.
                if info.state != new_state {
                    info.state = new_state;
                    info.state_since = Some(crate::task::now());
                }
            }
            self.sort_tasks();
            self.reselect(prev);
        }
        // A turn ended or began: its changes are worth a new look.
        self.branch_changed(id);
    }

    pub fn update_context(&mut self, id: &str, context_percent: u8) {
        if let Some(Some(info)) = self.agent_slot(id) {
            info.context_percent = Some(context_percent);
        }
    }

    /// Remove agent info from a member or chat (agent exited: idle).
    pub fn remove_agent(&mut self, id: &str) {
        let prev = self.pick();
        if self.starting.as_deref() == Some(id) {
            self.starting = None;
        }
        if let Some(slot) = self.agent_slot(id) {
            *slot = None;
            self.sort_tasks();
            self.reselect(prev);
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

    /// The terminal the preview pane should show: the focused member's or
    /// chat's agent, or the selected shell (by its id).
    pub fn peek_agent(&self) -> Option<String> {
        self.focused()
            .and_then(|f| f.terminal())
            .map(str::to_string)
    }

    /// The preview pane shows another agent than the one it should.
    pub fn peek_stale(&self) -> bool {
        self.peek.agent != self.peek_agent()
    }

    /// The pane keeps the keyboard (typed into or scrolled) only for the
    /// agent it took it for: while the pane shows it, or while it starts
    /// for the chat the pane is on.
    pub fn check_typing(&mut self) {
        let Some(agent) = self.mode.pane_agent() else {
            return;
        };
        let starting = self.starting.as_deref() == Some(agent)
            && self.focused().is_some_and(|f| f.id() == agent);
        match self.peek_agent() {
            Some(shown) if shown == agent => {}
            Some(_) => self.stop_typing("the pane shows another agent"),
            None if starting => {}
            None => self.stop_typing("the agent exited"),
        }
    }

    /// The pane gives the keyboard back to the list, asked to (ctrl-], a
    /// click in the list): scrolled, it shows the live screen again (the
    /// event loop fetches it when `peek.from` changed).
    pub fn leave_pane(&mut self) {
        if matches!(self.mode, Mode::Scroll { .. }) {
            self.peek.from = None;
        }
        self.mode = Mode::Normal;
    }

    /// The pane gives the keyboard back without having been asked to
    /// (`why`): said in the status line, and the keys still coming are
    /// dropped (see `dropping_keys_until`): a held `j` scrolling the pane
    /// would move the selection otherwise.
    pub fn stop_typing(&mut self, why: &str) {
        if self.mode.pane_agent().is_some() {
            self.mode = Mode::Normal;
            self.dropping_keys_until = Some(std::time::Instant::now() + TYPING_PAUSE);
            self.set_status(
                format!("Back to the list: {why}"),
                std::time::Duration::from_secs(4),
            );
        }
    }

    /// Whether a key typed now is to be dropped (see `stop_typing`); each
    /// one pushes the pause back.
    pub fn drops_key(&mut self) -> bool {
        let now = std::time::Instant::now();
        match self.dropping_keys_until {
            Some(until) if now < until => {
                self.dropping_keys_until = Some(now + TYPING_PAUSE);
                true
            }
            _ => {
                self.dropping_keys_until = None;
                false
            }
        }
    }

    /// The custom command that took one of the dashboard's keys a custom
    /// command may still bind (`config::YIELDING_KEYS`): the key is the
    /// command's then.
    pub fn key_taken(&self, key: char) -> Option<&CustomCommand> {
        if !crate::config::YIELDING_KEYS.contains(&key) {
            return None;
        }
        self.commands.iter().find(|c| c.key_char() == key)
    }

    /// An agent is working: its spinner turns.
    pub fn any_running(&self) -> bool {
        let tasks = self.tasks.iter().any(|t| {
            t.members
                .iter()
                .any(|m| matches!(t.member_status(m), TaskStatus::Run))
        });
        tasks || self.chats.iter().any(|c| c.status() == TaskStatus::Run)
    }

    /// Count tasks and chats needing attention (ready, blocked, question or
    /// error).
    pub fn needs_attention_count(&self) -> usize {
        let tasks = self.tasks.iter().filter(|t| t.status().needs_attention());
        let chats = self.chats.iter().filter(|c| c.status().needs_attention());
        tasks.count() + chats.count()
    }

    /// What the detail pane shows and member actions apply to: the selected
    /// chat or shell, or the member focused in the selected task, else the
    /// one that most needs the user.
    pub fn focused(&self) -> Option<Focused<'_>> {
        let task = match self.selected_entry()? {
            Entry::Chat(chat) => return Some(Focused::Chat(chat)),
            Entry::Shell(shell) => {
                let member = shell.member.as_deref().and_then(|m| self.find_member(m));
                return Some(Focused::Shell(shell, member));
            }
            Entry::Task(task) => task,
        };
        let focused = self
            .focus
            .as_ref()
            .filter(|f| f.task == task.name)
            .and_then(|f| task.member(&f.member));
        Some(Focused::Member(
            task,
            focused.or_else(|| most_urgent(task))?,
        ))
    }

    /// The focused member of the selected task (`None` on a chat or a
    /// shell).
    pub fn focused_member(&self) -> Option<(&Task, &Member)> {
        match self.focused()? {
            Focused::Member(task, member) => Some((task, member)),
            Focused::Chat(_) | Focused::Shell(..) => None,
        }
    }

    /// Focus the selected task's most urgent
    /// member when the selection is on another task than the focus (or the
    /// focused member left it). The focus then stays while the task is
    /// selected, whatever becomes more urgent: a key never acts on a member
    /// the pane did not show.
    pub fn sync_focus(&mut self) {
        let Some(task) = self.selected_task() else {
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

    /// The pane shows another entry's runs and messages than the selected
    /// one's, or they changed.
    pub fn detail_stale(&self) -> bool {
        let selected = self.pick();
        match &self.detail {
            Some(detail) => detail.stale || Some(&detail.of) != selected.as_ref(),
            None => selected.is_some(),
        }
    }

    /// A message was sent or delivered: keep it if it is to or from a
    /// member of the task shown, or the chat shown.
    pub fn note_message(&mut self, message: MessageInfo) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let ours = |id: &str| match &detail.of {
            Pick::Chat(chat) => chat == id,
            // A shell gets no messages.
            Pick::Shell(_) => false,
            Pick::Task(name) => self
                .tasks
                .iter()
                .find(|t| t.name == *name)
                .is_some_and(|t| t.member(id).is_some()),
        };
        if !ours(&message.to) && !message.from.as_deref().is_some_and(ours) {
            return;
        }
        match detail.messages.iter_mut().find(|m| m.id == message.id) {
            Some(known) => *known = message,
            None => detail.messages.push(message),
        }
    }

    /// The next member worktree git should be asked about (marked as being
    /// looked at), one at a time: `(member id, dir)`. The focused member
    /// while the pane shows it, every [`branch::REFRESH`]; else the member
    /// looked at longest ago (never, or before its agent's state changed,
    /// first), every [`branch::BACKGROUND`]. Gone worktrees are left out.
    pub fn branch_due(&mut self) -> Option<(String, PathBuf)> {
        if self.branches.values().any(|o| o.running) {
            return None;
        }
        let due = |member: &Member, every| {
            !member.is_unhealthy() && self.branches.get(member.id()).is_none_or(|o| o.due(every))
        };
        let focused = self
            .focused_member()
            .map(|(_, m)| m)
            .filter(|m| due(m, branch::REFRESH));
        let member = focused.or_else(|| {
            self.tasks
                .iter()
                .flat_map(|t| &t.members)
                .filter(|m| due(m, branch::BACKGROUND))
                .min_by_key(|m| self.branches.get(m.id()).and_then(|o| o.at))
        })?;
        let (id, dir) = (member.id().to_string(), member.info.dir.clone());
        self.branches.entry(id.clone()).or_default().running = true;
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
/// the most urgent status): the one the task's status is about.
pub fn most_urgent(task: &Task) -> Option<&Member> {
    task.members
        .iter()
        .min_by_key(|m| task.member_status(m).sort_priority())
}

/// The repositories of a task's members, as its group in the task list
/// names them: `api`, or `api + web` for a task in both.
pub fn repo_group(task: &Task) -> String {
    let mut repos: Vec<&str> = task.members.iter().map(|m| m.info.repo.as_str()).collect();
    repos.sort_unstable();
    repos.dedup();
    repos.join(" + ")
}
