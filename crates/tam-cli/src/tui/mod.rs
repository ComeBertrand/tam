mod app;
mod branch;
mod peek;
mod table;
mod triage;
mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tam_proto::ServerMessage;

use crate::client::{self, Client, DropOutcome};
use crate::config::{self, Config};
use crate::repos;
use crate::task::{Member, Task};
use tam_proto::SessionChoice;

use self::app::{App, MemberChoice, Mode, PaneTab, PickerItem, PickerState, TaskDetail};
use self::peek::{Peek, Scroll};

/// Actions returned from the event loop.
enum Action {
    None,
    Quit,
    SelectNext,
    SelectPrev,
    /// Attach to a member's agent (`id` is the member id).
    Attach {
        id: String,
        label: String,
    },
    StopAgent {
        id: String,
        label: String,
    },
    /// Drop a task, or one member of it (`name`: the member's id).
    DropTask {
        name: String,
        /// How to name what is dropped: the task name, or the member label.
        label: String,
        force: bool,
        delete_branch: bool,
    },
    NewTask,
    RunAgent {
        member: String,
    },
    /// Worktrees on branch `name` in the repositories `dirs` (main
    /// checkouts): a new task, or more members of the task `name` if it
    /// exists.
    DoNewTask {
        name: String,
        dirs: Vec<PathBuf>,
    },
    CustomCommand {
        name: String,
        command: String,
        task: String,
        dir: PathBuf,
        provider: String,
    },
    TogglePeek,
    RefreshPeek,
    /// Start a member's agent in a new session.
    RunNewSession {
        member: String,
    },
    /// Recreate the task's gone worktrees that can be.
    RepairTask {
        name: String,
    },
    ScrollPeek(Scroll),
}

pub async fn run() -> Result<()> {
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "TUI requires an interactive terminal"
    );

    let config = config::load_config()?;
    let mut client = Client::connect().await?;
    // Subscribe before the snapshot: a change racing it is applied after.
    client.subscribe().await?;

    let tasks = build_task_list(&mut client).await?;

    let mut stdout = std::io::stdout();
    terminal::enable_raw_mode().context("failed to enable raw mode")?;
    stdout
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    app.commands = config.commands.clone();
    app.set_tasks(tasks);

    let result = run_loop(&mut terminal, &mut app, &mut client, &config).await;

    terminal::disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;

    result
}

enum ReaderCtl {
    /// Stop touching the tty; ack once the thread is out of poll/read.
    Pause(tokio::sync::oneshot::Sender<()>),
    Resume,
}

/// Crossterm event reader on a dedicated thread, pausable while attached.
///
/// Pausing is a handshake: a plain flag would only be checked between polls,
/// so the thread could still be inside crossterm's (blocking) tty read when
/// the attach relay starts reading too — and then swallow the user's keys.
struct InputReader {
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    ctl: std::sync::mpsc::Sender<ReaderCtl>,
}

impl InputReader {
    fn spawn() -> Self {
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let (ctl, ctl_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let mut paused = false;
            loop {
                // Block on the control channel while paused, never on the tty.
                let ctl = if paused {
                    ctl_rx
                        .recv()
                        .map_err(|_| std::sync::mpsc::TryRecvError::Disconnected)
                } else {
                    ctl_rx.try_recv()
                };
                match ctl {
                    Ok(ReaderCtl::Pause(ack)) => {
                        paused = true;
                        let _ = ack.send(());
                        continue;
                    }
                    Ok(ReaderCtl::Resume) => paused = false,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });

        Self { events, ctl }
    }

    /// Returns once the reader thread no longer reads the tty.
    async fn pause(&self) {
        let (ack, acked) = tokio::sync::oneshot::channel();
        if self.ctl.send(ReaderCtl::Pause(ack)).is_ok() {
            // Err means the thread exited — it isn't reading either.
            let _ = acked.await;
        }
    }

    /// Resume reading, dropping any events queued before the pause.
    fn resume(&mut self) {
        while self.events.try_recv().is_ok() {}
        let _ = self.ctl.send(ReaderCtl::Resume);
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    client: &mut Client,
    config: &Config,
) -> Result<()> {
    let mut input = InputReader::spawn();
    // Redraw at least every second: times in state move on.
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // What git said of a member's worktree (looked at off this thread).
    let (branch_tx, mut branch_rx) = tokio::sync::mpsc::unbounded_channel();

    loop {
        app.sync_focus();
        // Whatever moved the selection (a key, a re-sort, an agent starting
        // or exiting), the preview follows.
        if app.peek_stale() {
            refresh_peek(client, app).await;
        }
        if app.detail_stale() {
            refresh_detail(client, app).await;
        }
        if let Some((id, dir)) = app.branch_due() {
            let tx = branch_tx.clone();
            // A thread of its own: quitting never waits for a slow git (the
            // runtime would, for a blocking task).
            std::thread::spawn(move || {
                let _ = tx.send((id, branch::observe(&dir)));
            });
        }
        terminal.draw(|frame| ui::render(frame, app))?;

        let action = tokio::select! {
            Some(ev) = input.events.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        handle_key_event(key, app, config)
                    }
                    _ => Action::None,
                }
            }
            msg = client.read_message() => {
                match msg? {
                    ServerMessage::Event(tam_proto::Event::ScreenUpdated { id }) => {
                        let watched = app.peek.as_ref().and_then(|p| p.watching.as_ref());
                        if watched == Some(&id) {
                            Action::RefreshPeek
                        } else {
                            Action::None
                        }
                    }
                    ServerMessage::Event(event) => {
                        apply_event(app, event);
                        Action::None
                    }
                    ServerMessage::Response(_) => Action::None,
                }
            }
            Some((id, state)) = branch_rx.recv() => {
                app.set_branch(id, state);
                Action::None
            }
            _ = tick.tick() => Action::None,
        };

        match action {
            Action::Quit => return Ok(()),
            Action::SelectNext => {
                app.select_next();
                refresh_peek(client, app).await;
            }
            Action::SelectPrev => {
                app.select_prev();
                refresh_peek(client, app).await;
            }
            Action::Attach { id, label } => {
                // The dashboard reads nothing while attached.
                unwatch_screen(client, app).await;
                input.pause().await;
                let provider = app
                    .find_member(&id)
                    .and_then(|(_, m)| m.agent_info.as_ref())
                    .map(|a| a.provider.clone())
                    .unwrap_or_default();
                let attached = attach_agent(terminal, app, &id, &label, &provider).await;
                input.resume();
                attached?;
                refresh_peek(client, app).await;
            }
            Action::StopAgent { id, label } => {
                stop_agent(client, app, &id, &label).await?;
                refresh_peek(client, app).await;
            }
            Action::DropTask {
                name,
                label,
                force,
                delete_branch,
            } => {
                drop_task(client, app, &name, &label, force, delete_branch).await;
                refresh_peek(client, app).await;
            }
            Action::NewTask => {
                start_new_task_flow(app, config);
            }
            Action::RunAgent { member } => {
                do_run(client, app, config, &member, SessionChoice::Current).await?;
            }
            Action::DoNewTask { name, dirs } => {
                do_new_task(client, app, &name, &dirs, config).await?;
            }
            Action::CustomCommand {
                name,
                command,
                task,
                dir,
                provider,
            } => match config::run_custom_command(&command, &task, &dir, &provider) {
                Ok(()) => app.set_status(format!("{name}: {task}"), Duration::from_secs(3)),
                Err(e) => app.set_status(format!("{name} failed: {e}"), Duration::from_secs(5)),
            },
            Action::TogglePeek => {
                if app.peek.is_some() {
                    unwatch_screen(client, app).await;
                    app.peek = None;
                } else {
                    app.peek = Some(Peek::default());
                    refresh_peek(client, app).await;
                }
            }
            Action::RefreshPeek => {
                refresh_peek(client, app).await;
            }
            Action::RunNewSession { member } => {
                do_run(client, app, config, &member, SessionChoice::New).await?;
            }
            Action::RepairTask { name } => {
                repair_task(client, app, &name).await;
            }
            Action::ScrollPeek(scroll) => {
                if let Some(peek) = app.peek.as_mut() {
                    peek.scroll(scroll);
                }
                refresh_peek(client, app).await;
            }
            Action::None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Key dispatch
// ---------------------------------------------------------------------------

fn handle_key_event(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    if app.filter_active {
        return handle_filter_key(key, app);
    }
    match &app.mode {
        Mode::Normal => handle_normal_key(key, app, config),
        Mode::NewTaskEnterName { .. } => handle_new_task_name_key(key, app),
        Mode::SpawnEnterPath(_) => handle_enter_path_key(key, app),
        Mode::ConfirmDropTask { .. } => handle_confirm_drop_key(key, app),
        Mode::ConfirmForceDrop { .. } => handle_confirm_force_drop_key(key, app),
        Mode::ConfirmRepairOrDrop { .. } => handle_repair_or_drop_key(key, app),
        Mode::Notice { .. } => {
            app.mode = Mode::Normal;
            Action::None
        }
        _ => handle_picker_key(key, app, config),
    }
}

fn handle_normal_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => Action::Quit,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::SelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::SelectPrev,
        (KeyCode::Enter, _) => match selected_member(app) {
            Some((task, member)) if member.agent_info.is_some() => Action::Attach {
                id: member.id().to_string(),
                label: task.member_label(member),
            },
            _ => Action::None,
        },
        (KeyCode::Char('/'), _) => {
            app.filter_active = true;
            Action::None
        }
        (KeyCode::Char('n'), _) => Action::NewTask,
        (KeyCode::Char('r'), _) => match selected_member(app) {
            Some((_, member)) if member.agent_info.is_none() => Action::RunAgent {
                member: member.id().to_string(),
            },
            _ => Action::None,
        },
        (KeyCode::Char('R'), _) => match selected_member(app) {
            Some((_, member)) if member.agent_info.is_none() => Action::RunNewSession {
                member: member.id().to_string(),
            },
            _ => Action::None,
        },
        (KeyCode::Char('s'), _) => match selected_member(app) {
            Some((task, member)) if member.agent_info.is_some() => Action::StopAgent {
                id: member.id().to_string(),
                label: task.member_label(member),
            },
            _ => Action::None,
        },
        (KeyCode::Char('p'), _) => Action::TogglePeek,
        (KeyCode::Char('g'), _) => {
            app.toggle_order();
            Action::None
        }
        (KeyCode::Tab, _) if app.peek.is_some() => {
            app.tab = match app.tab {
                PaneTab::Triage => PaneTab::Terminal,
                PaneTab::Terminal => PaneTab::Triage,
            };
            Action::None
        }
        // The pane (and the member keys) move to another member of the
        // task: only while shown, never out of sight.
        (KeyCode::Right, _) if app.peek.is_some() => {
            app.cycle_member(1);
            Action::None
        }
        (KeyCode::Left, _) if app.peek.is_some() => {
            app.cycle_member(-1);
            Action::None
        }
        // A gone worktree: recreate it, or drop the task.
        (KeyCode::Char('d'), _)
            if app
                .selected_task()
                .is_some_and(|t| t.unhealthy_members().next().is_some()) =>
        {
            let member = member_choice(app);
            app.mode = repair_or_drop_dialog(app.selected_task().expect("selected"), member);
            Action::None
        }
        (KeyCode::Char('d'), _) => {
            let member = member_choice(app);
            if let Some(task) = app.selected_task() {
                app.mode = Mode::ConfirmDropTask {
                    name: task.name.clone(),
                    member,
                };
            }
            Action::None
        }
        (KeyCode::Char(c), _) => {
            if let Some(cmd) = config.commands.iter().find(|cmd| cmd.key_char() == c) {
                if let Some((task, member)) = selected_member(app) {
                    let provider = member
                        .agent_info
                        .as_ref()
                        .map(|a| a.provider.clone())
                        .unwrap_or_default();
                    Action::CustomCommand {
                        name: cmd.name.clone(),
                        command: cmd.command.clone(),
                        task: task.name.clone(),
                        dir: member.info.dir.clone(),
                        provider,
                    }
                } else {
                    Action::None
                }
            } else {
                Action::None
            }
        }
        (KeyCode::PageUp, _) if app.peek.is_some() => Action::ScrollPeek(Scroll::PageUp),
        (KeyCode::PageDown, _) if app.peek.is_some() => Action::ScrollPeek(Scroll::PageDown),
        (KeyCode::Home, _) if app.peek.is_some() => Action::ScrollPeek(Scroll::Oldest),
        (KeyCode::End, _) if app.peek.is_some() => Action::ScrollPeek(Scroll::Live),
        _ => Action::None,
    }
}

fn handle_filter_key(key: KeyEvent, app: &mut App) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            if app.filter.is_empty() {
                app.filter_active = false;
            } else {
                app.filter.clear();
                app.selected = 0;
            }
            Action::None
        }
        (KeyCode::Enter, _) => {
            app.filter_active = false;
            Action::None
        }
        (KeyCode::Backspace, _) => {
            app.filter.pop();
            app.selected = 0;
            Action::None
        }
        (KeyCode::Down, _) => Action::SelectNext,
        (KeyCode::Up, _) => Action::SelectPrev,
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            app.filter.push(c);
            app.selected = 0;
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_picker_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.mode = Mode::Normal;
            Action::None
        }
        (KeyCode::Down, _) => {
            picker_mut(app, PickerState::select_next);
            Action::None
        }
        (KeyCode::Up, _) => {
            picker_mut(app, PickerState::select_prev);
            Action::None
        }
        (KeyCode::Enter, _) => handle_picker_enter(app, config),
        (KeyCode::Tab, _) => {
            picker_mut(app, PickerState::toggle_mark);
            Action::None
        }
        (KeyCode::Backspace, _) => {
            picker_mut(app, PickerState::backspace);
            Action::None
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            picker_mut(app, |p| p.type_char(c));
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_new_task_name_key(key: KeyEvent, app: &mut App) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.mode = Mode::Normal;
            Action::None
        }
        (KeyCode::Enter, _) => {
            let (name, dirs) = match &app.mode {
                Mode::NewTaskEnterName { name, project_dirs } => {
                    (name.clone(), project_dirs.clone())
                }
                _ => return Action::None,
            };
            if name.is_empty() {
                return Action::None;
            }
            app.mode = Mode::Normal;
            Action::DoNewTask { name, dirs }
        }
        (KeyCode::Backspace, _) => {
            if let Mode::NewTaskEnterName { ref mut name, .. } = app.mode {
                name.pop();
            }
            Action::None
        }
        (KeyCode::Char(c), _) => {
            if let Mode::NewTaskEnterName { ref mut name, .. } = app.mode {
                name.push(c);
            }
            Action::None
        }
        _ => Action::None,
    }
}

fn handle_enter_path_key(key: KeyEvent, app: &mut App) -> Action {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.mode = Mode::Normal;
            Action::None
        }
        (KeyCode::Enter, _) => {
            let path = match &app.mode {
                Mode::SpawnEnterPath(p) => p.clone(),
                _ => return Action::None,
            };
            let dir = PathBuf::from(&path);
            if !dir.is_dir() {
                app.set_status(format!("Not a directory: {path}"), Duration::from_secs(3));
                app.mode = Mode::Normal;
                return Action::None;
            }
            let root = match repository(&dir) {
                Ok(root) => root,
                Err(e) => {
                    app.set_status(e, Duration::from_secs(5));
                    app.mode = Mode::Normal;
                    return Action::None;
                }
            };
            app.mode = Mode::NewTaskEnterName {
                project_dirs: vec![root],
                name: String::new(),
            };
            Action::None
        }
        (KeyCode::Backspace, _) => {
            if let Mode::SpawnEnterPath(ref mut path) = app.mode {
                path.pop();
            }
            Action::None
        }
        (KeyCode::Char(c), _) => {
            if let Mode::SpawnEnterPath(ref mut path) = app.mode {
                path.push(c);
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// `y`/enter: drop, keeping the branch; `b`: drop and delete the branch;
/// anything else cancels.
fn handle_confirm_drop_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::ConfirmDropTask { name, member } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return Action::None;
    };
    let (target, label, delete_branch) = match (key.code, member) {
        (KeyCode::Char('y') | KeyCode::Enter, _) => (name.clone(), name, false),
        (KeyCode::Char('b'), _) => (name.clone(), name, true),
        // The focused member alone.
        (KeyCode::Char('m'), Some(m)) => (m.id, m.label, false),
        (KeyCode::Char('M'), Some(m)) => (m.id, m.label, true),
        _ => return Action::None,
    };
    Action::DropTask {
        name: target,
        label,
        force: false,
        delete_branch,
    }
}

/// The member a drop dialog offers to drop alone: the focused one (the one
/// the pane shows, else the one the task's status is about), when its task
/// has others.
fn member_choice(app: &App) -> Option<MemberChoice> {
    let (task, member) = app.focused_member()?;
    (task.members.len() > 1).then(|| MemberChoice {
        id: member.id().to_string(),
        label: task.member_label(member),
    })
}

/// `y`: drop anyway, losing the reported work; anything else cancels.
fn handle_confirm_force_drop_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::ConfirmForceDrop {
        name,
        target,
        delete_branch,
        ..
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return Action::None;
    };
    if key.code != KeyCode::Char('y') {
        app.set_status(format!("Kept '{name}'"), Duration::from_secs(3));
        return Action::None;
    }
    Action::DropTask {
        name: target,
        label: name,
        force: true,
        delete_branch,
    }
}

/// The dialog for a task with gone or unusable worktrees: what is wrong with
/// each, and whether any can be recreated.
fn repair_or_drop_dialog(task: &Task, member: Option<MemberChoice>) -> Mode {
    let lines = task
        .unhealthy_members()
        .filter_map(|m| {
            let problem = crate::health::problem(&m.info)?;
            Some(if task.members.len() == 1 {
                problem
            } else {
                format!("{}: {problem}", m.info.name)
            })
        })
        .collect();
    Mode::ConfirmRepairOrDrop {
        name: task.name.clone(),
        lines,
        repairable: task
            .unhealthy_members()
            .any(|m| crate::health::repairable(&m.info)),
        in_place: task.members.iter().any(|m| !m.is_unhealthy()),
        member,
    }
}

/// `r`: recreate the worktrees that can be; `y`: drop, keeping the branches;
/// `b` (with a worktree in place): drop and delete the branches; `m` (in a
/// task with several members): drop the focused member alone, keeping its
/// branch; anything else, enter included, cancels.
fn handle_repair_or_drop_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::ConfirmRepairOrDrop {
        name,
        repairable,
        in_place,
        member,
        ..
    } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return Action::None;
    };
    let (target, label, delete_branch) = match (key.code, member) {
        (KeyCode::Char('r'), _) if repairable => return Action::RepairTask { name },
        (KeyCode::Char('y'), _) => (name.clone(), name, false),
        (KeyCode::Char('b'), _) if in_place => (name.clone(), name, true),
        (KeyCode::Char('m'), Some(m)) => (m.id, m.label, false),
        _ => return Action::None,
    };
    Action::DropTask {
        name: target,
        label,
        force: false,
        delete_branch,
    }
}

/// The selected task and the member actions apply to: the one the detail
/// pane shows.
fn selected_member(app: &App) -> Option<(&Task, &Member)> {
    app.focused_member()
}

fn picker_mut(app: &mut App, f: impl FnOnce(&mut PickerState)) {
    if let Mode::NewTaskPickProject(p) = &mut app.mode {
        f(p)
    }
}

// ---------------------------------------------------------------------------
// New task flow
// ---------------------------------------------------------------------------

fn start_new_task_flow(app: &mut App, _config: &Config) {
    let cwd_display = std::env::current_dir()
        .ok()
        .map(|p| ui::shorten_home(&p.display().to_string()))
        .unwrap_or_else(|| ".".into());

    let mut items = vec![
        PickerItem {
            display: format!(". ({cwd_display})"),
            id: "__cwd__".into(),
        },
        PickerItem {
            display: "enter path...".into(),
            id: "__enter_path__".into(),
        },
    ];

    // Discover projects using tam-worktree
    if let Ok(wt_config) = tam_worktree::config::load_config() {
        if let Ok(ignore) = tam_worktree::discovery::build_ignore_set(&wt_config.ignore) {
            let root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            if let Ok(paths) =
                tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth)
            {
                let entries = tam_worktree::pretty::build_pretty_names(&paths);
                for entry in &entries {
                    items.push(PickerItem {
                        display: entry.display_name.clone(),
                        id: entry.path.to_string_lossy().into_owned(),
                    });
                }
            }
        }
    }

    app.mode = Mode::NewTaskPickProject(PickerState::new("Pick project", items));
}

/// Enter in the project picker: the projects picked with tab, else the
/// highlighted one (or the path entry).
fn handle_picker_enter(app: &mut App, _config: &Config) -> Action {
    let Mode::NewTaskPickProject(picker) = &app.mode else {
        return Action::None;
    };
    let ids = if picker.marked.is_empty() {
        match picker.selected_item() {
            Some(item) => vec![item.id.clone()],
            None => return Action::None,
        }
    } else {
        picker.marked.clone()
    };
    app.mode = Mode::Normal;
    if ids == ["__enter_path__"] {
        app.mode = Mode::SpawnEnterPath(String::new());
        return Action::None;
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    for id in ids {
        let dir = if id == "__cwd__" {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&id)
        };
        match repository(&dir) {
            // A repository and one of its worktrees: one member.
            Ok(root) if roots.contains(&root) => {}
            Ok(root) => roots.push(root),
            Err(e) => {
                app.set_status(e, Duration::from_secs(5));
                return Action::None;
            }
        }
    }
    app.mode = Mode::NewTaskEnterName {
        project_dirs: roots,
        name: String::new(),
    };
    Action::None
}

/// The main checkout of the repository a picked directory is in (a worktree
/// stands for its repository); `Err` says why there is none.
fn repository(dir: &std::path::Path) -> Result<PathBuf, String> {
    let dir = std::fs::canonicalize(dir)
        .map_err(|_| format!("Directory not found: {}", dir.display()))?;
    repos::main_checkout(&dir).map_err(|_| format!("Not in a git repository: {}", dir.display()))
}

// ---------------------------------------------------------------------------
// Peek
// ---------------------------------------------------------------------------

/// Show the selected task's agent screen in the preview pane (live, or the
/// lines scrolled back to), and have the daemon tell when it changes.
async fn refresh_peek(client: &mut Client, app: &mut App) {
    let wanted = app.peek_agent();
    let Some(peek) = app.peek.as_mut() else {
        return;
    };
    if peek.agent != wanted {
        // Another agent: from its live screen.
        *peek = Peek {
            agent: wanted.clone(),
            watching: peek.watching.take(),
            height: peek.height,
            ..Peek::default()
        };
    }
    if peek.watching != wanted {
        // An error is an agent already gone: its exit event follows.
        let _ = client.watch_screen(wanted.as_deref()).await;
        peek.watching = wanted.clone();
    }
    let Some(id) = wanted else {
        return;
    };
    match client.screen(&id, peek.from).await {
        Ok(screen) => peek.set_screen(&screen),
        Err(e) => {
            peek.screen = None;
            peek.error = Some(format!("{e:#}"));
        }
    }
}

/// Fetch the selected task's runs and messages for the detail pane (it
/// follows the daemon's events from then on).
async fn refresh_detail(client: &mut Client, app: &mut App) {
    let Some(name) = app.selected_task().map(|t| t.name.clone()) else {
        app.detail = None;
        return;
    };
    // Shown without them when they cannot be had: no retry until it changes.
    let runs = match client.get_task(&name).await {
        Ok((_, runs)) => runs,
        Err(_) => Vec::new(),
    };
    let messages = match client
        .send(tam_proto::Request::ListMessages {
            task: Some(name.clone()),
        })
        .await
    {
        Ok(tam_proto::Response::Messages { messages }) => messages,
        _ => Vec::new(),
    };
    app.detail = Some(TaskDetail {
        task: name,
        runs,
        messages,
        stale: false,
    });
}

/// Stop the daemon's screen updates (the pane is closed, or the dashboard
/// stops reading for a while).
async fn unwatch_screen(client: &mut Client, app: &mut App) {
    if let Some(peek) = app.peek.as_mut() {
        if peek.watching.take().is_some() {
            let _ = client.watch_screen(None).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Task actions
// ---------------------------------------------------------------------------

/// Create worktrees on branch `name` in the repositories `dirs` (main
/// checkouts; the daemon does it, all or nothing): a new task, or more members of the task
/// `name` if it exists. Then start an agent in each new member.
async fn do_new_task(
    client: &mut Client,
    app: &mut App,
    name: &str,
    dirs: &[PathBuf],
    config: &Config,
) -> Result<()> {
    let join = app.tasks.iter().any(|t| t.name == name);
    // The picker gave main checkouts, each once.
    let roots = dirs;

    // The task shows up (or changes) through the daemon's events.
    let wt_config = tam_worktree::config::load_config()?;
    let created = match repos::new_members(roots, name, &wt_config) {
        Ok(members) => client.create_members(name, members, None, join).await,
        Err(e) => Err(e),
    };
    let task = match created {
        Ok(task) => task,
        // E.g. what a failed worktree left behind: the whole message.
        Err(e) => {
            app.mode = Mode::Notice {
                title: if join { "Add Failed" } else { "Create Failed" }.into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
            return Ok(());
        }
    };
    let new = &task.members[task.members.len().saturating_sub(roots.len())..];
    if wt_config.auto_init {
        for member in new {
            let _ = tam_worktree::init::run(&member.dir);
        }
    }

    let mut failed = Vec::new();
    for member in new {
        let started = client
            .run_agent(
                &member.id,
                None,
                &config.default_agent,
                SessionChoice::New,
                None,
                vec![],
            )
            .await;
        if let Err(e) = started {
            failed.push(format!("{}: {e:#}", task.member_label(member)));
        }
    }
    let what = match (join, new) {
        (false, [_]) => format!("Created+started '{name}'"),
        (false, _) => format!("Created+started '{name}' in {} repositories", new.len()),
        (true, _) => {
            let labels: Vec<String> = new.iter().map(|m| task.member_label(m)).collect();
            format!("Added+started {}", labels.join(", "))
        }
    };
    if failed.is_empty() {
        app.set_status(what, Duration::from_secs(3));
    } else {
        app.set_status(
            format!("{what}; spawn error: {}", failed.join("; ")),
            Duration::from_secs(5),
        );
    }
    Ok(())
}

/// Start a member's agent (its last agent, else the default one) in the
/// chosen session; the daemon resolves the member's current session.
async fn do_run(
    client: &mut Client,
    app: &mut App,
    config: &Config,
    member: &str,
    session: SessionChoice,
) -> Result<()> {
    let label = app
        .find_member(member)
        .map(|(t, m)| t.member_label(m))
        .unwrap_or_else(|| member.to_string());
    let started = client
        .run_agent(member, None, &config.default_agent, session, None, vec![])
        .await;
    match started {
        Ok((_, Some(_))) => {
            app.set_status(format!("Resumed '{label}'"), Duration::from_secs(3));
        }
        Ok((_, None)) => {
            app.set_status(
                format!("Started a new session in '{label}'"),
                Duration::from_secs(3),
            );
        }
        // E.g. a session that can no longer be resumed: the whole hint.
        Err(e) => {
            app.mode = Mode::Notice {
                title: "Run Failed".into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
        }
    }
    Ok(())
}

/// Attach to the agent `id`, named `name` in the status bar.
async fn attach_agent(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    id: &str,
    name: &str,
    provider: &str,
) -> Result<()> {
    terminal.backend_mut().execute(LeaveAlternateScreen)?;

    let (cols, rows) = client::terminal_size();

    if rows > 2 {
        draw_status_bar(name, provider, cols, rows);
    }

    let agent_rows = if rows > 2 { rows - 1 } else { rows };

    // On resize, redraw the status bar at the new bottom row (keeping the
    // agent's cursor) and give the agent everything above it.
    let mut bar_row = (rows > 2).then_some(rows);
    let on_resize = move || {
        let (cols, rows) = client::terminal_size();
        let new_bar_row = (rows > 2).then_some(rows);
        redraw_status_bar(name, provider, cols, bar_row, new_bar_row);
        bar_row = new_bar_row;
        match new_bar_row {
            Some(rows) => (cols, rows - 1),
            None => (cols, rows),
        }
    };

    let attach_client = Client::connect().await?;
    let result = attach_client
        .attach_relay(id, cols, agent_rows, on_resize)
        .await;

    client::reset_terminal_state();

    terminal.backend_mut().execute(EnterAlternateScreen)?;
    terminal.clear()?;

    if let Err(e) = result {
        app.set_status(format!("attach failed: {e:#}"), Duration::from_secs(5));
    }
    Ok(())
}

/// Draw the status bar on the bottom row and confine scrolling above it,
/// leaving the cursor at the top-left for the agent.
fn draw_status_bar(name: &str, provider: &str, cols: u16, rows: u16) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(status_bar_seq(name, provider, cols, rows).as_bytes());
    let _ = write!(out, "\x1b[1;1H");
    let _ = out.flush();
}

/// Move the status bar after a resize without moving the agent's cursor.
///
/// `old_row`/`new_row` are the bar's bottom row before and after the resize,
/// `None` when the terminal is too short to show it.
fn redraw_status_bar(
    name: &str,
    provider: &str,
    cols: u16,
    old_row: Option<u16>,
    new_row: Option<u16>,
) {
    use std::io::Write;
    let mut seq = String::new();
    // Growing leaves the old bar inside the agent's area; the agent has not
    // drawn there yet, so it is ours to clear. (Shrinking pushes it off-screen.)
    if let Some(old) = old_row {
        if new_row.is_none_or(|new| old < new) {
            seq.push_str(&format!("\x1b[{old};1H\x1b[2K"));
        }
    }
    match new_row {
        Some(rows) => seq.push_str(&status_bar_seq(name, provider, cols, rows)),
        // No room for a bar: give the agent the whole screen back.
        None => seq.push_str("\x1b[r"),
    }
    let mut out = std::io::stdout();
    // DECSC/DECRC around the redraw: setting the scroll region homes the cursor.
    let _ = write!(out, "\x1b7{seq}\x1b8");
    let _ = out.flush();
}

/// Scroll region above the bottom row, then the bar itself on the bottom row.
fn status_bar_seq(name: &str, provider: &str, cols: u16, rows: u16) -> String {
    let bar = format!(" tam > {name} ({provider})");
    let hint = "ctrl-]: detach ";
    let cols = cols as usize;
    let padding = cols.saturating_sub(bar.chars().count() + hint.len());
    // Cut to the row width so a narrow terminal doesn't wrap the bar.
    let line: String = format!("{bar}{:padding$}{hint}", "")
        .chars()
        .take(cols)
        .collect();
    format!(
        "\x1b[1;{}r\x1b[{rows};1H\x1b[2K\x1b[7m{line}\x1b[m",
        rows - 1
    )
}

async fn stop_agent(client: &mut Client, app: &mut App, id: &str, label: &str) -> Result<()> {
    // The daemon records the end of the run.
    let resp = client
        .send(tam_proto::Request::Kill { id: id.into() })
        .await?;
    match resp {
        tam_proto::Response::Ok => {
            app.set_status(format!("Stopped '{label}'"), Duration::from_secs(3));
        }
        tam_proto::Response::Error { message } => {
            app.set_status(format!("Error: {message}"), Duration::from_secs(5));
        }
        _ => {}
    }
    Ok(())
}

/// Ask the daemon to drop a task, or one member of it (`target`: the
/// member's id; `name` says what is dropped). A refusal opens the force
/// dialog with what would be lost, a failure or warnings a notice; the task
/// list follows the daemon's events.
async fn drop_task(
    client: &mut Client,
    app: &mut App,
    target: &str,
    name: &str,
    force: bool,
    delete_branch: bool,
) {
    match client.drop_task(target, force, delete_branch).await {
        Ok(DropOutcome::Dropped { warnings }) if warnings.is_empty() => {
            app.set_status(format!("Dropped '{name}'"), Duration::from_secs(3));
        }
        Ok(DropOutcome::Dropped { warnings }) => {
            let mut lines = vec![format!("Dropped '{name}', but:")];
            lines.extend(warnings.iter().map(|w| format!("- {w}")));
            app.mode = Mode::Notice {
                title: "Dropped".into(),
                lines,
            };
        }
        Ok(DropOutcome::Refused { worktrees }) => {
            app.mode = Mode::ConfirmForceDrop {
                name: name.into(),
                target: target.into(),
                delete_branch,
                report: crate::drop_report::lines(&worktrees),
            };
        }
        Err(e) => {
            app.mode = Mode::Notice {
                title: "Drop Failed".into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
        }
    }
}

fn apply_event(app: &mut App, event: tam_proto::Event) {
    match event {
        tam_proto::Event::AgentSpawned { info, .. }
        | tam_proto::Event::AgentUpdated { info, .. } => {
            app.update_agent(info);
        }
        tam_proto::Event::StateChange { id, new, .. } => {
            app.update_state(&id, new);
        }
        tam_proto::Event::AgentExited { id, .. } => {
            app.remove_agent(&id);
        }
        tam_proto::Event::ContextUpdate {
            id,
            context_percent,
        } => {
            app.update_context(&id, context_percent);
        }
        tam_proto::Event::TaskCreated { task } => {
            app.upsert_task(Task::from_info(task, &[]));
        }
        tam_proto::Event::TaskUpdated { task } => {
            app.update_task_info(task);
        }
        tam_proto::Event::TaskDropped { name } => {
            app.remove_task(&name);
        }
        // Followed by the event loop (see `refresh_peek`).
        tam_proto::Event::ScreenUpdated { .. } => {}
        tam_proto::Event::MessageSent { message } => {
            app.note_message(message);
        }
        tam_proto::Event::MessageDelivered { message } => {
            app.set_status(
                format!(
                    "Message {} from {} delivered to {}",
                    message.id, message.from_label, message.to_label
                ),
                Duration::from_secs(5),
            );
            app.note_message(message);
        }
    }
}

async fn build_task_list(client: &mut Client) -> Result<Vec<Task>> {
    // Fresh worktree health; the daemon pushes its changes from then on.
    let infos = client.reconcile().await?;
    let agents = client.list_agents().await.unwrap_or_default();

    Ok(infos
        .into_iter()
        .map(|info| Task::from_info(info, &agents))
        .collect())
}

/// Recreate the task's gone worktrees that can be (running the project's
/// worktree init when configured, as for a new task). The task list follows
/// the daemon's events.
async fn repair_task(client: &mut Client, app: &mut App, name: &str) {
    let Some(task) = app.tasks.iter().find(|t| t.name == name) else {
        return;
    };
    let members: Vec<(String, String)> = task
        .unhealthy_members()
        .filter(|m| crate::health::repairable(&m.info))
        .map(|m| (m.id().to_string(), task.member_label(m)))
        .collect();
    let auto_init = tam_worktree::config::load_config().is_ok_and(|c| c.auto_init);
    let mut failures = Vec::new();
    for (id, label) in &members {
        match client.repair_member(id).await {
            Ok((_, member)) => {
                if auto_init {
                    let _ = tam_worktree::init::run(&member.dir);
                }
            }
            Err(e) => failures.push(format!("{label}: {e:#}")),
        }
    }
    if failures.is_empty() {
        app.set_status(
            format!("Recreated the worktree of '{name}'"),
            Duration::from_secs(3),
        );
    } else {
        app.mode = Mode::Notice {
            title: "Repair Failed".into(),
            lines: failures,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task;

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn confirming(name: &str) -> App {
        let mut app = App::new();
        app.mode = Mode::ConfirmDropTask {
            name: name.into(),
            member: None,
        };
        app
    }

    #[test]
    fn drop_dialog_keys() {
        let mut app = confirming("feat");
        let action = handle_confirm_drop_key(press('y'), &mut app);
        assert!(matches!(
            action,
            Action::DropTask { ref name, force: false, delete_branch: false, .. } if name == "feat"
        ));
        assert!(matches!(app.mode, Mode::Normal));

        let mut app = confirming("feat");
        let action = handle_confirm_drop_key(press('b'), &mut app);
        assert!(matches!(
            action,
            Action::DropTask {
                force: false,
                delete_branch: true,
                ..
            }
        ));

        let mut app = confirming("feat");
        assert!(matches!(
            handle_confirm_drop_key(press('n'), &mut app),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn force_drop_dialog_keeps_the_branch_choice() {
        let force_dialog = || {
            let mut app = App::new();
            app.mode = Mode::ConfirmForceDrop {
                name: "feat".into(),
                target: "feat".into(),
                delete_branch: true,
                report: vec![],
            };
            app
        };
        let mut app = force_dialog();
        assert!(matches!(
            handle_confirm_force_drop_key(press('y'), &mut app),
            Action::DropTask {
                force: true,
                delete_branch: true,
                ..
            }
        ));

        // Enter does not force: only an explicit `y` loses work.
        let mut app = force_dialog();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            handle_confirm_force_drop_key(enter, &mut app),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.status_message(), Some("Kept 'feat'"));
    }

    #[test]
    fn r_resumes_and_shift_r_starts_a_new_session() {
        let info: tam_proto::TaskInfo = serde_json::from_value(serde_json::json!({
            "name": "feat",
            "members": [{
                "id": "m-1", "name": "app", "repo": "app",
                "branch": "feat", "dir": "/w/app--feat",
            }],
            "created_at": 0,
        }))
        .unwrap();
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &[])]);
        let config = Config::default();

        let action = handle_key_event(press('r'), &mut app, &config);
        assert!(matches!(action, Action::RunAgent { ref member } if member == "m-1"));
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        let action = handle_key_event(shift_r, &mut app, &config);
        assert!(matches!(action, Action::RunNewSession { ref member } if member == "m-1"));
        // No picker: still in the task list.
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// An app showing one task, `feat`, whose only member has `health`.
    fn app_with(health: Option<tam_proto::MemberHealth>) -> App {
        let info = tam_proto::TaskInfo {
            name: "feat".into(),
            members: vec![tam_proto::MemberInfo {
                id: "m-1".into(),
                name: "app".into(),
                repo: "app".into(),
                repo_root: None,
                branch: "feat".into(),
                dir: PathBuf::from("/w/app--feat"),
                health,
            }],
            created_at: 0,
            run_count: 0,
            last_activity: Some(task::now()),
        };
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &[])]);
        app
    }

    #[test]
    fn drop_key_on_a_gone_worktree_offers_to_recreate_it() {
        let config = Config::default();
        let gone = tam_proto::MemberHealth::Gone {
            cannot_repair: None,
        };
        let mut app = app_with(Some(gone));
        assert!(matches!(
            handle_normal_key(press('d'), &mut app, &config),
            Action::None
        ));
        match &app.mode {
            Mode::ConfirmRepairOrDrop {
                name,
                lines,
                repairable,
                in_place,
                member,
            } => {
                assert_eq!(name, "feat");
                // A single member: nothing to drop alone.
                assert!(member.is_none());
                assert!(*repairable);
                assert!(!*in_place);
                assert_eq!(
                    lines,
                    &["worktree /w/app--feat is gone, branch 'feat' is still there"]
                );
            }
            _ => panic!("expected the repair-or-drop dialog"),
        }
        assert!(matches!(
            handle_key_event(press('r'), &mut app, &config),
            Action::RepairTask { ref name } if name == "feat"
        ));
        assert!(matches!(app.mode, Mode::Normal));

        // It drops too, on an explicit `y` only. With every worktree gone
        // there is no branch to delete: `b` cancels, like enter.
        for key in [
            press('b'),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            app.mode = repair_or_drop_dialog(&app.tasks[0], None);
            assert!(matches!(
                handle_key_event(key, &mut app, &config),
                Action::None
            ));
        }
        app.mode = repair_or_drop_dialog(&app.tasks[0], None);
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::DropTask {
                force: false,
                delete_branch: false,
                ..
            }
        ));
    }

    #[test]
    fn repair_or_drop_offers_branch_deletion_with_a_worktree_in_place() {
        let config = Config::default();
        let mut app = app_with(Some(tam_proto::MemberHealth::Gone {
            cannot_repair: None,
        }));
        let mut info = tam_proto::TaskInfo {
            name: "feat".into(),
            members: vec![app.tasks[0].members[0].info.clone()],
            created_at: 0,
            run_count: 0,
            last_activity: Some(task::now()),
        };
        let mut web = info.members[0].clone();
        web.id = "m-2".into();
        web.name = "web".into();
        web.health = Some(tam_proto::MemberHealth::Ok {
            branch: Some("feat".into()),
        });
        info.members.push(web);
        app.set_tasks(vec![Task::from_info(info, &[])]);

        handle_normal_key(press('d'), &mut app, &config);
        match &app.mode {
            Mode::ConfirmRepairOrDrop {
                lines, in_place, ..
            } => {
                assert!(*in_place);
                // Only the gone member is described, by its name.
                assert_eq!(lines.len(), 1);
                assert!(lines[0].starts_with("app: worktree "), "{lines:?}");
            }
            _ => panic!("expected the repair-or-drop dialog"),
        }
        assert!(matches!(
            handle_key_event(press('b'), &mut app, &config),
            Action::DropTask {
                delete_branch: true,
                ..
            }
        ));
    }

    #[test]
    fn drop_key_on_a_lost_worktree_only_drops() {
        let config = Config::default();
        let lost = tam_proto::MemberHealth::Gone {
            cannot_repair: Some("branch 'feat' no longer exists".into()),
        };
        let mut app = app_with(Some(lost));
        handle_normal_key(press('d'), &mut app, &config);
        assert!(matches!(
            app.mode,
            Mode::ConfirmRepairOrDrop {
                repairable: false,
                ..
            }
        ));
        // No recreate: `r` cancels.
        assert!(matches!(
            handle_key_event(press('r'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));

        // A healthy (or unchecked) worktree: the plain drop dialog.
        for health in [None, Some(tam_proto::MemberHealth::Ok { branch: None })] {
            let mut app = app_with(health);
            handle_normal_key(press('d'), &mut app, &config);
            assert!(matches!(app.mode, Mode::ConfirmDropTask { .. }));
        }
    }

    #[test]
    fn page_keys_scroll_the_preview_only_when_shown() {
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = App::new();
        assert!(matches!(
            handle_normal_key(key(KeyCode::PageUp), &mut app, &config),
            Action::None
        ));
        app.peek = Some(Peek::default());
        for (code, scroll) in [
            (KeyCode::PageUp, Scroll::PageUp),
            (KeyCode::PageDown, Scroll::PageDown),
            (KeyCode::Home, Scroll::Oldest),
            (KeyCode::End, Scroll::Live),
        ] {
            assert!(matches!(
                handle_normal_key(key(code), &mut app, &config),
                Action::ScrollPeek(s) if s == scroll
            ));
        }
    }

    /// Tasks `feat` (members app and web) and `other`, with the agents
    /// `agents` (`(member id, state)`).
    fn two_member_app(agents: &[(&str, tam_proto::AgentState)]) -> App {
        let member = |id: &str, name: &str| tam_proto::MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: name.into(),
            repo_root: None,
            branch: "feat".into(),
            dir: PathBuf::from(format!("/w/{name}--feat")),
            health: None,
        };
        let task = |name: &str, members| tam_proto::TaskInfo {
            name: name.into(),
            members,
            created_at: 0,
            run_count: 0,
            last_activity: Some(task::now()),
        };
        let agents: Vec<tam_proto::AgentInfo> = agents
            .iter()
            .map(|(id, state)| {
                serde_json::from_value(serde_json::json!({
                    "id": id, "provider": "claude", "dir": "/w", "state": state,
                    "pid": 1, "uptime_secs": 1,
                }))
                .unwrap()
            })
            .collect();
        let mut app = App::new();
        app.set_tasks(vec![
            Task::from_info(
                task("feat", vec![member("m-1", "app"), member("m-2", "web")]),
                &agents,
            ),
            Task::from_info(task("other", vec![member("m-3", "lib")]), &agents),
        ]);
        app
    }

    fn attach_target(app: &mut App) -> Option<String> {
        match handle_normal_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            app,
            &Config::default(),
        ) {
            Action::Attach { id, .. } => Some(id),
            _ => None,
        }
    }

    #[test]
    fn member_keys_act_on_the_member_that_needs_the_user() {
        use tam_proto::AgentState::{Blocked, Input, Working};
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));

        // Pane closed: the one the task's status is about, as it changes.
        app.update_state("m-2", Working);
        app.update_state("m-1", Blocked);
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));

        // Pane open: the one it shows. It stays when the other one comes
        // to need the user more: a key never acts on a member the pane did
        // not show.
        app.peek = Some(Peek::default());
        app.sync_focus();
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));
        app.update_state("m-1", Working);
        app.update_state("m-2", Blocked);
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));

        // Back on the task after another one: the most urgent again.
        let feat = app.selected;
        app.selected = 1 - feat;
        app.sync_focus();
        app.selected = feat;
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));

        // ←/→ move it, only while the pane shows which member it is.
        let config = Config::default();
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_normal_key(right, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        handle_normal_key(right, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        handle_normal_key(left, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        app.peek = None;
        app.sync_focus();
        handle_normal_key(left, &mut app, &config);
        assert_eq!(
            attach_target(&mut app).as_deref(),
            Some("m-2"),
            "no pane, no ←"
        );
        app.peek = Some(Peek::default());
        app.sync_focus();
        handle_normal_key(left, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));

        // `s` and `r` too: here `r` runs the idle one.
        app.remove_agent("m-1");
        assert!(matches!(
            handle_normal_key(press('r'), &mut app, &config),
            Action::RunAgent { ref member } if member == "m-1"
        ));
        app.update_state("m-2", Input);
        app.cycle_member(1);
        assert!(matches!(
            handle_normal_key(press('s'), &mut app, &config),
            Action::StopAgent { ref id, ref label } if id == "m-2" && label == "feat/web"
        ));
    }

    #[test]
    fn tab_switches_the_pane_between_triage_and_terminal() {
        let config = Config::default();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let mut app = App::new();
        handle_normal_key(tab, &mut app, &config);
        assert_eq!(app.tab, PaneTab::Triage, "no pane, no tab");
        app.peek = Some(Peek::default());
        handle_normal_key(tab, &mut app, &config);
        assert_eq!(app.tab, PaneTab::Terminal);
        handle_normal_key(tab, &mut app, &config);
        assert_eq!(app.tab, PaneTab::Triage);
    }

    #[test]
    fn state_changes_note_when_they_happened() {
        let mut app = two_member_app(&[("m-1", tam_proto::AgentState::Working)]);
        let since = |app: &App| {
            app.find_member("m-1")
                .unwrap()
                .1
                .agent_info
                .as_ref()
                .unwrap()
                .state_since
        };
        assert_eq!(since(&app), None);
        app.update_state("m-1", tam_proto::AgentState::Blocked);
        let now = task::now();
        assert!(since(&app).is_some_and(|s| s + 1 >= now));
        // Already the daemon's (from a hook's AgentUpdated): kept.
        let agent = app.tasks.iter_mut().find_map(|t| t.member_mut("m-1"));
        agent.unwrap().agent_info.as_mut().unwrap().state_since = Some(5);
        app.update_state("m-1", tam_proto::AgentState::Blocked);
        assert_eq!(since(&app), Some(5));
    }

    #[test]
    fn the_pane_keeps_the_shown_task_runs_and_messages() {
        let mut app = two_member_app(&[]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        assert!(!app.detail_stale(), "pane closed: nothing to fetch");
        app.peek = Some(Peek::default());
        assert!(app.detail_stale());
        app.detail = Some(TaskDetail {
            task: "feat".into(),
            ..Default::default()
        });
        assert!(!app.detail_stale());

        let message = |id: &str, from: Option<&str>, to: &str| tam_proto::MessageInfo {
            id: id.into(),
            from: from.map(str::to_string),
            from_label: "user".into(),
            to: to.into(),
            to_label: to.into(),
            text: "hi".into(),
            hop: 0,
            reply_to: None,
            sent_at: 1,
            delivered_at: None,
        };
        app.note_message(message("msg-1", None, "m-2"));
        app.note_message(message("msg-2", Some("m-3"), "m-3"));
        app.note_message(message("msg-3", Some("m-1"), "m-3"));
        let mut delivered = message("msg-1", None, "m-2");
        delivered.delivered_at = Some(2);
        app.note_message(delivered);
        let detail = app.detail.as_ref().unwrap();
        let ids: Vec<&str> = detail.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["msg-1", "msg-3"], "only the task's members'");
        assert_eq!(detail.messages[0].delivered_at, Some(2));

        // A run started or ended: fetched again. Another task: too.
        let info = tam_proto::TaskInfo {
            name: "feat".into(),
            members: app.tasks[app.selected]
                .members
                .iter()
                .map(|m| m.info.clone())
                .collect(),
            created_at: 0,
            run_count: 1,
            last_activity: Some(task::now()),
        };
        app.update_task_info(info);
        assert!(app.detail_stale());
        app.detail.as_mut().unwrap().stale = false;
        app.selected = 1 - app.selected;
        assert!(app.detail_stale());
    }

    #[test]
    fn tab_picks_several_projects_in_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        let (api, web) = (tmp.path().join("api"), tmp.path().join("web"));
        for repo in [&api, &web] {
            std::fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "-q", "-b", "main"]);
            git(
                repo,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "i",
                ],
            );
        }
        // A worktree of web stands for web.
        let web_wt = tmp.path().join("web--x");
        git(
            &web,
            &["worktree", "add", "-q", "-b", "x", web_wt.to_str().unwrap()],
        );
        let item = |display: &str, id: &str| PickerItem {
            display: display.into(),
            id: id.into(),
        };
        let picker = PickerState::new(
            "Pick project",
            vec![
                item("enter path...", "__enter_path__"),
                item("api", api.to_str().unwrap()),
                item("web", web.to_str().unwrap()),
                item("x @web", web_wt.to_str().unwrap()),
            ],
        );
        let mut app = App::new();
        app.mode = Mode::NewTaskPickProject(picker);
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // The path entry is an action, not a project: tab skips it.
        handle_picker_key(key(KeyCode::Tab), &mut app, &config);
        let Mode::NewTaskPickProject(p) = &app.mode else {
            panic!("still picking");
        };
        assert!(p.marked.is_empty());
        // Items: enter path (0), api (1), web (2), x @web (3). Tab picks
        // and moves down. web, then api: picked in that order.
        let keys = |app: &mut App, codes: &[KeyCode]| {
            for code in codes {
                handle_picker_key(key(*code), app, &config);
            }
        };
        keys(&mut app, &[KeyCode::Down, KeyCode::Down, KeyCode::Tab]);
        keys(&mut app, &[KeyCode::Up, KeyCode::Up, KeyCode::Tab]);
        // Tab again unpicks.
        keys(&mut app, &[KeyCode::Up, KeyCode::Tab]);
        let Mode::NewTaskPickProject(p) = &app.mode else {
            panic!("still picking");
        };
        assert_eq!(p.marked, [web.to_str().unwrap()]);
        keys(&mut app, &[KeyCode::Up, KeyCode::Tab]);
        // web's worktree too: web once.
        keys(&mut app, &[KeyCode::Down, KeyCode::Tab]);
        handle_picker_key(key(KeyCode::Enter), &mut app, &config);
        let Mode::NewTaskEnterName { project_dirs, .. } = &app.mode else {
            panic!("expected the name input");
        };
        let expected = [web.canonicalize().unwrap(), api.canonicalize().unwrap()];
        assert_eq!(project_dirs, &expected);

        // Typed, then entered: one action with both.
        for c in "feat".chars() {
            handle_new_task_name_key(press(c), &mut app);
        }
        let action = handle_new_task_name_key(key(KeyCode::Enter), &mut app);
        assert!(
            matches!(action, Action::DoNewTask { ref name, ref dirs } if name == "feat" && *dirs == expected)
        );
    }

    /// An app showing task `feat` with members `api` (m-1) and `web` (m-2),
    /// web's worktree with `web_health`.
    fn app_with_two(web_health: Option<tam_proto::MemberHealth>) -> App {
        let info: tam_proto::TaskInfo = serde_json::from_value(serde_json::json!({
            "name": "feat",
            "members": [
                {"id": "m-1", "name": "api", "repo": "api", "branch": "feat", "dir": "/w/api--feat"},
                {"id": "m-2", "name": "web", "repo": "web", "branch": "feat", "dir": "/w/web--feat",
                 "health": web_health},
            ],
            "created_at": 0,
        }))
        .unwrap();
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &[])]);
        app
    }

    #[test]
    fn drop_dialog_offers_the_focused_member_alone() {
        let config = Config::default();
        let open = || {
            let mut app = app_with_two(None);
            handle_normal_key(press('d'), &mut app, &config);
            let Mode::ConfirmDropTask { name, member } = &app.mode else {
                panic!("expected the drop dialog");
            };
            assert_eq!(name, "feat");
            let member = member.clone().expect("a member to drop alone");
            assert!(["feat/api", "feat/web"].contains(&member.label.as_str()));
            (app, member)
        };

        // `m`: that member, by id, keeping its branch; `M`: and its branch.
        let (mut app, member) = open();
        let action = handle_key_event(press('m'), &mut app, &config);
        assert!(matches!(
            action,
            Action::DropTask { ref name, ref label, force: false, delete_branch: false }
                if *name == member.id && *label == member.label
        ));
        let (mut app, member) = open();
        let shift_m = KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT);
        assert!(matches!(
            handle_key_event(shift_m, &mut app, &config),
            Action::DropTask { ref name, delete_branch: true, .. } if *name == member.id
        ));
        // `y` still drops the task.
        let (mut app, _) = open();
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::DropTask { ref name, ref label, .. } if name == "feat" && label == "feat"
        ));

        // A single member: nothing to drop alone, `m` cancels.
        let mut app = app_with(None);
        handle_normal_key(press('d'), &mut app, &config);
        assert!(matches!(
            app.mode,
            Mode::ConfirmDropTask { member: None, .. }
        ));
        assert!(matches!(
            handle_key_event(press('m'), &mut app, &config),
            Action::None
        ));
    }

    #[test]
    fn a_gone_member_can_be_dropped_alone() {
        let config = Config::default();
        let gone = tam_proto::MemberHealth::Gone {
            cannot_repair: Some("branch 'feat' no longer exists".into()),
        };
        let mut app = app_with_two(Some(gone));
        handle_normal_key(press('d'), &mut app, &config);
        let Mode::ConfirmRepairOrDrop { member, .. } = &app.mode else {
            panic!("expected the repair-or-drop dialog");
        };
        let member = member.clone().expect("a member to drop alone");
        assert!(matches!(
            handle_key_event(press('m'), &mut app, &config),
            Action::DropTask { ref name, delete_branch: false, .. } if *name == member.id
        ));
    }

    #[test]
    fn a_forced_member_drop_names_the_member() {
        let mut app = App::new();
        app.mode = Mode::ConfirmForceDrop {
            name: "feat/web".into(),
            target: "m-2".into(),
            delete_branch: false,
            report: vec![],
        };
        assert!(matches!(
            handle_confirm_force_drop_key(press('y'), &mut app),
            Action::DropTask { ref name, ref label, force: true, .. }
                if name == "m-2" && label == "feat/web"
        ));
    }

    #[test]
    fn git_looks_at_one_worktree_at_a_time_the_focused_one_first() {
        use std::time::{Duration, Instant};
        let mut app = two_member_app(&[]);
        let looked = |app: &mut App, id: &str, ago: u64| {
            app.set_branch(id.into(), Ok(Default::default()));
            app.branches.get_mut(id).unwrap().at = Some(Instant::now() - Duration::from_secs(ago));
        };
        // Never looked at: each in turn, one at a time.
        let mut seen = Vec::new();
        while let Some((id, _)) = app.branch_due() {
            assert_eq!(app.branch_due(), None, "one at a time");
            looked(&mut app, &id, 0);
            seen.push(id);
        }
        seen.sort();
        assert_eq!(seen, ["m-1", "m-2", "m-3"]);

        // Looked at 30s ago: not due in the background (every 60s)...
        for id in ["m-1", "m-2", "m-3"] {
            looked(&mut app, id, 30);
        }
        assert_eq!(app.branch_due(), None);
        // ...but the member the pane shows is (every 10s).
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.peek = Some(Peek::default());
        app.sync_focus();
        let focused = app.focused_member().unwrap().1.id().to_string();
        assert_eq!(app.branch_due().map(|(id, _)| id), Some(focused.clone()));
        looked(&mut app, &focused, 0);

        // The oldest look goes first; a state change asks for a new one.
        looked(&mut app, "m-3", 90);
        app.update_state("m-1", tam_proto::AgentState::Input);
        assert_eq!(app.branch_due().map(|(id, _)| id).as_deref(), Some("m-1"));
        looked(&mut app, "m-1", 0);
        assert_eq!(app.branch_due().map(|(id, _)| id).as_deref(), Some("m-3"));
        looked(&mut app, "m-3", 0);

        // A gone worktree is not looked at.
        let mut gone = app_with(Some(tam_proto::MemberHealth::Gone {
            cannot_repair: None,
        }));
        assert_eq!(gone.branch_due(), None);
    }
}
