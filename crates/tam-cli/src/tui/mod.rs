mod answer;
mod app;
mod branch;
mod fuzzy;
mod input;
mod keys;
mod peek;
mod table;
mod theme;
mod triage;
mod ui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tam_proto::ServerMessage;

use crate::chat::{self as chats, Chat, Closes};
use crate::client::{self, Client, DropOutcome};
use crate::config::{self, CommandValues, Config, CustomCommand};
use crate::repos;
use crate::task::Task;
use tam_proto::SessionChoice;

use self::answer::{AnswerDialog, Outcome};
use self::app::{
    App, Focused, MemberChoice, Mode, Pick, PickerItem, PickerState, Purpose, SessionPicker,
    TaskDetail, NEW_SESSION,
};
use self::input::TextInput;
use self::peek::{Peek, Scroll};

/// Actions returned from the event loop.
enum Action {
    None,
    Quit,
    SelectNext,
    SelectPrev,
    /// Attach to a member's or chat's agent (`id`: the member's or chat's).
    Attach {
        id: String,
        label: String,
    },
    /// Start a chat's agent, in its session (resuming is never a step of
    /// its own for a chat: the key that types or attaches does it), then
    /// attach with `attach`, else type into it (the pane has the keyboard
    /// already: see `App::starting`).
    Resume {
        id: String,
        label: String,
        attach: bool,
    },
    /// Close a chat.
    CloseChat {
        id: String,
        label: String,
    },
    /// Keep a chat, or stop keeping it.
    KeepChat {
        id: String,
        label: String,
        keep: bool,
    },
    /// Name a chat (`None`: Claude's title again).
    RenameChat {
        id: String,
        label: String,
        name: Option<String>,
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
    /// Open the repository picker, for a new task or chat.
    PickProject(Purpose),
    /// The sessions a new chat in this repository (its main checkout) can
    /// start from.
    ChatHistory {
        dir: PathBuf,
    },
    /// Make a chat in this repository, resuming `session` (else a new
    /// one), start its agent and type into it.
    CreateChat {
        dir: PathBuf,
        session: Option<String>,
    },
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
    /// A custom command, on the focused member or chat: `task` and `dir`
    /// are its task's name and worktree, or the chat's label and main
    /// checkout; `id` its agent's id.
    CustomCommand {
        name: String,
        command: String,
        task: String,
        id: String,
        dir: PathBuf,
        provider: String,
    },
    /// Start a member's agent in a new session.
    RunNewSession {
        member: String,
    },
    /// Recreate the task's gone worktrees that can be.
    RepairTask {
        name: String,
    },
    ScrollPeek(Scroll),
    /// Bytes to type into the agent the pane shows (see `keys`).
    Keys(Vec<u8>),
    /// Switch to the next theme.
    NextTheme,
    /// Answer the request `ask` of agent `agent` (see `answer`).
    Answer {
        agent: String,
        ask: String,
        answer: tam_proto::AskAnswer,
        /// The member, and what the answer says: for the status line.
        label: String,
        what: String,
    },
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

    let (tasks, chats) = build_lists(&mut client).await?;

    let mut stdout = std::io::stdout();
    terminal::enable_raw_mode().context("failed to enable raw mode")?;
    stdout
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    mouse_capture(true);
    bracketed_paste(true);
    // Asked before the input reader starts: the answer comes on the tty.
    KEYBOARD_PROTOCOL.store(
        terminal::supports_keyboard_enhancement().unwrap_or(false),
        std::sync::atomic::Ordering::Relaxed,
    );
    keyboard_protocol(true);

    theme::set(config.theme.as_deref(), config.animate);
    let mut app = App::new();
    app.commands = config.commands.clone();
    // Chats first: the selection starts on the list's top entry.
    app.set_chats(chats);
    app.set_tasks(tasks);

    let result = run_loop(&mut terminal, &mut app, &mut client, &config).await;

    mouse_capture(false);
    bracketed_paste(false);
    keyboard_protocol(false);
    terminal::disable_raw_mode()?;
    std::io::stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Have the terminal send pastes between markers (crossterm then reports
/// them as one event), or stop. On while the dashboard has the terminal: a
/// paste is typed into the agent as one paste, goes into a prompt as text,
/// and is never read as the dashboard's keys.
fn bracketed_paste(on: bool) {
    use std::io::Write;
    let seq = if on { "\x1b[?2004h" } else { "\x1b[?2004l" };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// The terminal has the kitty keyboard protocol.
static KEYBOARD_PROTOCOL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Have the terminal tell keys apart that send the same bytes otherwise
/// (shift-enter from enter), where it can (the kitty keyboard protocol,
/// its first flag), or stop. Off while attached: the agent's terminal is
/// the user's then, with the agent's own modes.
fn keyboard_protocol(on: bool) {
    use std::io::Write;
    if !KEYBOARD_PROTOCOL.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let seq = if on { "\x1b[>1u" } else { "\x1b[<1u" };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Have the terminal report mouse clicks and the wheel (`?1000`: no
/// motion), with SGR coordinates (`?1006`), or stop. Off while attached:
/// the agent would get the reports as typed text.
fn mouse_capture(on: bool) {
    use std::io::Write;
    let seq = if on {
        "\x1b[?1000h\x1b[?1006h"
    } else {
        "\x1b[?1006l\x1b[?1000l"
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
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
    // Faster while an agent works: its spinner.
    let mut spin = tokio::time::interval(Duration::from_millis(120));
    spin.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    app.spin = Some(0);
    // What git said of a member's worktree (looked at off this thread).
    let (branch_tx, mut branch_rx) = tokio::sync::mpsc::unbounded_channel();
    // The projects found under $HOME (looked for off this thread).
    let (projects_tx, mut projects_rx) = tokio::sync::mpsc::unbounded_channel();
    // The agent's screen changed, or the pane scrolled: fetch it again.
    let mut screen_moved = true;

    loop {
        app.sync_focus();
        // The pane keeps the keyboard only for the agent it took it for.
        app.check_typing();
        // Typing into it or scrolling it is looking at it: the daemon is
        // told with the watch (see `refresh_peek`).
        if app.peek.viewing != app.mode.pane_agent().is_some() {
            screen_moved = true;
        }
        // Whatever moved the selection (a key, a re-sort, an agent starting
        // or exiting), the preview follows.
        if app.peek_stale() || std::mem::take(&mut screen_moved) {
            refresh_peek(client, app).await;
        }
        if app.detail_stale() {
            refresh_detail(client, app).await;
        }
        fit_agent(client, app).await;
        if let Some((id, dir)) = app.branch_due() {
            let tx = branch_tx.clone();
            // A thread of its own: quitting never waits for a slow git (the
            // runtime would, for a blocking task).
            std::thread::spawn(move || {
                let _ = tx.send((id, branch::observe(&dir)));
            });
        }
        terminal.draw(|frame| ui::render(frame, app))?;

        let events = tokio::select! {
            Some(ev) = input.events.recv() => {
                // And whatever came meanwhile (a wheel spun fast, a paste):
                // handled before the next draw.
                let mut events = vec![ev];
                while let Ok(ev) = input.events.try_recv() {
                    events.push(ev);
                }
                events
            }
            msg = client.read_message() => {
                match msg? {
                    ServerMessage::Event(tam_proto::Event::ScreenUpdated { id }) => {
                        let watched = app.peek.watching.as_ref();
                        screen_moved |= watched == Some(&id);
                    }
                    ServerMessage::Event(event) => apply_event(app, event),
                    ServerMessage::Response(_) => {}
                }
                Vec::new()
            }
            Some((id, state)) = branch_rx.recv() => {
                app.set_branch(id, state);
                Vec::new()
            }
            Some(projects) = projects_rx.recv() => {
                app.set_projects(projects);
                Vec::new()
            }
            _ = tick.tick() => Vec::new(),
            _ = spin.tick(), if app.any_running() || theme::animated() => {
                app.spin = app.spin.map(|n| n.wrapping_add(1));
                Vec::new()
            }
        };

        // Typed into the pane in this batch: one write, in order.
        let mut typed: Option<(String, Vec<u8>)> = None;
        // Whatever scrolled the pane (leaving scroll mode, too): fetched again.
        let from = app.peek.from;
        for ev in events {
            // As if drawn between them: keys act on the member the pane
            // would show.
            app.sync_focus();
            let typing = match &app.mode {
                Mode::Terminal { agent } => Some(agent.clone()),
                _ => None,
            };
            let action = match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key_event(key, app, config)
                }
                Event::Paste(text) if typing.is_some() => {
                    let bracketed = app
                        .peek
                        .screen
                        .as_ref()
                        .is_some_and(|s| s.bracketed_paste());
                    Action::Keys(keys::paste(&text, bracketed))
                }
                Event::Paste(text) => {
                    paste_into_prompt(&text, app, config);
                    Action::None
                }
                Event::Mouse(mouse) => handle_mouse(mouse, app),
                _ => Action::None,
            };
            // The pane just took the keyboard: what follows in this batch
            // was typed before any frame said so, and is dropped.
            let focused = typing.is_none() && matches!(app.mode, Mode::Terminal { .. });
            match action {
                Action::Quit => return Ok(()),
                Action::SelectNext => app.select_next(),
                Action::SelectPrev => app.select_prev(),
                Action::Attach { id, label } => {
                    attach(terminal, app, client, &mut input, &id, &label).await?;
                    screen_moved = true;
                    // What came before attaching is not for the dashboard.
                    break;
                }
                Action::Resume {
                    id,
                    label,
                    attach: full,
                } => {
                    if resume_chat(client, app, &id, &label).await && full {
                        attach(terminal, app, client, &mut input, &id, &label).await?;
                    }
                    screen_moved = true;
                    // The keys queued behind are not for the dashboard: for
                    // the agent, or typed before a notice said why not.
                    break;
                }
                Action::CloseChat { id, label } => {
                    close_chat(client, app, &id, &label).await;
                    screen_moved = true;
                }
                Action::KeepChat { id, label, keep } => {
                    keep_chat(client, app, &id, &label, keep).await;
                }
                Action::RenameChat { id, label, name } => {
                    rename_chat(client, app, &id, &label, name).await;
                }
                Action::StopAgent { id, label } => {
                    stop_agent(client, app, &id, &label).await?;
                    screen_moved = true;
                }
                Action::DropTask {
                    name,
                    label,
                    force,
                    delete_branch,
                } => {
                    drop_task(client, app, &name, &label, force, delete_branch).await;
                    screen_moved = true;
                }
                Action::PickProject(purpose) => {
                    start_pick_project(app, purpose);
                    if !app.discovering {
                        app.discovering = true;
                        let tx = projects_tx.clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(discover_projects());
                        });
                    }
                }
                Action::RunAgent { member } => {
                    do_run(client, app, &member, SessionChoice::Current).await?;
                }
                Action::ChatHistory { dir } => {
                    open_chat_history(client, app, dir).await;
                }
                Action::CreateChat { dir, session } => {
                    create_chat(client, app, &dir, session).await;
                    screen_moved = true;
                    // As for the enter that gives the pane the keyboard:
                    // the keys queued behind it are dropped.
                    break;
                }
                Action::DoNewTask { name, dirs } => {
                    do_new_task(client, app, &name, &dirs).await?;
                }
                Action::CustomCommand {
                    name,
                    command,
                    task,
                    id,
                    dir,
                    provider,
                } => match config::run_custom_command(
                    &command,
                    &CommandValues {
                        task: &task,
                        id: &id,
                        dir: &dir,
                        provider: &provider,
                    },
                ) {
                    Ok(()) => app.set_status(format!("{name}: {task}"), Duration::from_secs(3)),
                    Err(e) => app.set_status(format!("{name} failed: {e}"), Duration::from_secs(5)),
                },
                Action::RunNewSession { member } => {
                    do_run(client, app, &member, SessionChoice::New).await?;
                }
                Action::RepairTask { name } => {
                    repair_task(client, app, &name).await;
                }
                Action::ScrollPeek(scroll) => {
                    app.peek.scroll(scroll);
                    screen_moved = true;
                }
                Action::Keys(bytes) => {
                    if let Some(agent) = typing {
                        typed.get_or_insert((agent, Vec::new())).1.extend(bytes);
                    }
                    // Typing shows the live screen again.
                    if app.peek.from.take().is_some() {
                        screen_moved = true;
                    }
                }
                Action::NextTheme => {
                    let name = theme::next();
                    app.set_status(format!("theme: {name}"), Duration::from_secs(2));
                }
                Action::Answer {
                    agent,
                    ask,
                    answer,
                    label,
                    what,
                } => {
                    let request = tam_proto::Request::Answer { agent, ask, answer };
                    answer_request(client, app, request, &label, &what).await?;
                }
                Action::None => {}
            }
            if focused {
                break;
            }
        }
        screen_moved |= app.peek.from != from;
        if let Some((agent, bytes)) = typed {
            type_keys(client, app, agent, &bytes).await;
        }
    }
}

/// Type `bytes` into `agent`'s terminal; the pane gives the keyboard back
/// when it cannot (the agent is gone).
async fn type_keys(client: &mut Client, app: &mut App, id: String, bytes: &[u8]) {
    let data = String::from_utf8_lossy(bytes).into_owned();
    let failed = match client.send(tam_proto::Request::Keys { id, data }).await {
        Ok(tam_proto::Response::Error { message }) => Some(message),
        Ok(_) => None,
        Err(e) => Some(format!("{e:#}")),
    };
    if let Some(why) = failed {
        app.stop_typing(&format!("not typed ({why})"));
    }
}

// ---------------------------------------------------------------------------
// Key dispatch
// ---------------------------------------------------------------------------

fn handle_key_event(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    // Typed for an agent the pane no longer has the keyboard for.
    if !matches!(app.mode, Mode::Terminal { .. }) && app.drops_key() {
        return Action::None;
    }
    if app.filter_active {
        return handle_filter_key(key, app);
    }
    match &app.mode {
        Mode::Normal => handle_normal_key(key, app, config),
        Mode::NewTaskEnterName { .. } => handle_new_task_name_key(key, app),
        Mode::SpawnEnterPath(..) => handle_enter_path_key(key, app),
        Mode::ConfirmDropTask { .. } => handle_confirm_drop_key(key, app),
        Mode::ConfirmForceDrop { .. } => handle_confirm_force_drop_key(key, app),
        Mode::ConfirmRepairOrDrop { .. } => handle_repair_or_drop_key(key, app),
        Mode::Notice { .. } | Mode::Help => {
            app.mode = Mode::Normal;
            Action::None
        }
        Mode::PickProject(..) => handle_picker_key(key, app, config),
        Mode::PickSession(_) => handle_session_key(key, app),
        Mode::Answer(_) => handle_answer_key(key, app),
        Mode::Terminal { .. } => handle_terminal_key(key, app),
        Mode::Scroll { .. } => handle_scroll_key(key, app),
        Mode::ConfirmCloseChat { .. } => handle_close_chat_key(key, app),
        Mode::RenameChat { .. } => handle_rename_chat_key(key, app),
    }
}

/// A paste outside the pane: text for the prompt being typed in (the
/// filter, the project picker, a task or chat name, a path), its line ends and
/// other control characters left out. Anywhere else it is dropped: read as
/// keys, it would run the dashboard's commands.
fn paste_into_prompt(text: &str, app: &mut App, config: &Config) {
    let prompt = app.filter_active
        || matches!(
            app.mode,
            Mode::PickProject(..)
                | Mode::PickSession(_)
                | Mode::NewTaskEnterName { .. }
                | Mode::SpawnEnterPath(..)
                | Mode::RenameChat { .. }
        );
    if !prompt {
        return;
    }
    for c in text.chars().filter(|c| !c.is_control()) {
        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        handle_key_event(key, app, config);
    }
}

/// The pane has the keyboard: ctrl-] gives it back to the list, every
/// other key goes to the agent as its bytes.
fn handle_terminal_key(key: KeyEvent, app: &mut App) -> Action {
    if keys::is_leave(key) {
        app.leave_pane();
        return Action::None;
    }
    let app_cursor = app
        .peek
        .screen
        .as_ref()
        .is_some_and(|s| s.application_cursor());
    keys::encode(key, app_cursor).map_or(Action::None, Action::Keys)
}

/// The pane scrolls: `j`/`k` (`↓`/`↑`) a line, `g`/`G` (`home`/`end`) to
/// the oldest line kept or the live screen; ctrl-] gives the keyboard back
/// to the list, on the live screen. Any other key does nothing: neither the
/// agent's nor the list's.
fn handle_scroll_key(key: KeyEvent, app: &mut App) -> Action {
    if keys::is_leave(key) {
        app.leave_pane();
        return Action::None;
    }
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => Action::ScrollPeek(Scroll::Down(1)),
        KeyCode::Char('k') | KeyCode::Up => Action::ScrollPeek(Scroll::Up(1)),
        KeyCode::Char('g') | KeyCode::Home => Action::ScrollPeek(Scroll::Oldest),
        KeyCode::Char('G') | KeyCode::End => Action::ScrollPeek(Scroll::Live),
        _ => Action::None,
    }
}

fn handle_normal_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    // `gg`: only two `g` in a row.
    let after_g = std::mem::take(&mut app.pending_g);
    // A key of the dashboard's that a custom command took is the command's.
    if let (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) = (key.code, key.modifiers)
    {
        if let Some(cmd) = app.key_taken(c) {
            return custom_command(app, cmd);
        }
    }
    let chat = app
        .selected_chat()
        .map(|c| (c.info.clone(), c.agent.is_some()));
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => Action::Quit,
        // They scrolled the pane by pages once: never the letters' own keys
        // (ctrl-d is not drop, ctrl-f not full screen).
        (KeyCode::Char('u' | 'b' | 'd' | 'f'), KeyModifiers::CONTROL) => Action::None,
        (KeyCode::Char('j') | KeyCode::Down, _) => Action::SelectNext,
        (KeyCode::Char('k') | KeyCode::Up, _) => Action::SelectPrev,
        // The agent's terminal on the whole screen; a chat's agent started
        // first. (shift-enter where the terminal tells it from enter: see
        // `keyboard_protocol`.)
        (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) => full_screen(app),
        (KeyCode::Char('f'), _) => full_screen(app),
        (KeyCode::Enter, _) => match app.focused() {
            // The pane takes the keyboard.
            Some(focused) if focused.agent().is_some() => {
                app.mode = Mode::Terminal {
                    agent: focused.id().to_string(),
                };
                Action::None
            }
            // A chat's agent is started for it (see `App::starting`).
            Some(Focused::Chat(chat)) => {
                let (id, label) = (chat.info.id.clone(), chat.info.label());
                app.mode = Mode::Terminal { agent: id.clone() };
                if app.starting.as_ref() == Some(&id) {
                    return Action::None;
                }
                app.starting = Some(id.clone());
                Action::Resume {
                    id,
                    label,
                    attach: false,
                }
            }
            _ => Action::None,
        },
        (KeyCode::Char('/'), _) => {
            app.filter_active = true;
            Action::None
        }
        // The pane scrolls, a line at a time (see `handle_scroll_key`).
        (KeyCode::Char('v'), _) => {
            if let Some(agent) = app.peek_agent() {
                app.mode = Mode::Scroll { agent };
            }
            Action::None
        }
        (KeyCode::Char('n'), _) => Action::PickProject(Purpose::Task),
        (KeyCode::Char('c'), _) => Action::PickProject(Purpose::Chat),
        // A chat is its session: enter resumes it.
        (KeyCode::Char('r' | 'R'), _) if chat.as_ref().is_some_and(|(_, running)| !running) => {
            app.set_status(
                "A chat resumes when you type in it: enter".into(),
                Duration::from_secs(3),
            );
            Action::None
        }
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
        (KeyCode::Char('s'), _) => match app.focused() {
            Some(focused) if focused.agent().is_some() => Action::StopAgent {
                id: focused.id().to_string(),
                label: focused.label(),
            },
            _ => Action::None,
        },
        (KeyCode::Char('a'), _) => {
            open_answer(app);
            Action::None
        }
        (KeyCode::Char('?'), _) => {
            app.mode = Mode::Help;
            Action::None
        }
        (KeyCode::Char('g'), _) => {
            if after_g {
                app.select_first();
            } else {
                app.pending_g = true;
            }
            Action::None
        }
        (KeyCode::Char('G'), _) => {
            app.select_last();
            Action::None
        }
        (KeyCode::Char('T'), _) => Action::NextTheme,
        (KeyCode::Char('t'), _) => {
            app.toggle_order();
            Action::None
        }
        // The pane (and the member keys) move to another member of the task.
        (KeyCode::Right | KeyCode::Char('l'), _) => {
            app.cycle_member(1);
            Action::None
        }
        (KeyCode::Left | KeyCode::Char('h'), _) => {
            app.cycle_member(-1);
            Action::None
        }
        // A chat: close it, once confirmed.
        (KeyCode::Char('d'), _) if chat.is_some() => {
            let (info, _) = chat.expect("a chat");
            app.mode = Mode::ConfirmCloseChat {
                label: info.label(),
                id: info.id,
            };
            Action::None
        }
        (KeyCode::Char('p'), _) => match chat {
            Some((info, _)) => Action::KeepChat {
                label: info.label(),
                keep: !info.keep,
                id: info.id,
            },
            None => Action::None,
        },
        (KeyCode::Char('N'), _) => {
            if let Some((info, _)) = chat {
                app.mode = Mode::RenameChat {
                    label: info.label(),
                    name: TextInput::from(info.name.as_deref().unwrap_or("")),
                    title: info.title,
                    id: info.id,
                };
            }
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
        (KeyCode::Char(c), _) => match config.commands.iter().find(|cmd| cmd.key_char() == c) {
            Some(cmd) => custom_command(app, cmd),
            None => Action::None,
        },
        (KeyCode::Home, _) => Action::ScrollPeek(Scroll::Oldest),
        (KeyCode::End, _) => Action::ScrollPeek(Scroll::Live),
        _ => Action::None,
    }
}

/// `f` (or shift-enter): attach to the focused agent on the whole screen,
/// starting a chat's first.
fn full_screen(app: &App) -> Action {
    match app.focused() {
        Some(focused) if focused.agent().is_some() => Action::Attach {
            id: focused.id().to_string(),
            label: focused.label(),
        },
        Some(Focused::Chat(chat)) => Action::Resume {
            id: chat.info.id.clone(),
            label: chat.info.label(),
            attach: true,
        },
        _ => Action::None,
    }
}

/// A custom command on the focused member or chat (nothing without one).
fn custom_command(app: &App, cmd: &CustomCommand) -> Action {
    let Some(focused) = app.focused() else {
        return Action::None;
    };
    let (task, dir) = focused.task_and_dir();
    Action::CustomCommand {
        name: cmd.name.clone(),
        command: cmd.command.clone(),
        task,
        id: focused.id().to_string(),
        dir: dir.to_path_buf(),
        provider: focused
            .agent()
            .map(|a| a.provider.clone())
            .unwrap_or_default(),
    }
}

/// `y`/enter: close the chat; anything else cancels.
fn handle_close_chat_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::ConfirmCloseChat { id, label } = std::mem::replace(&mut app.mode, Mode::Normal)
    else {
        return Action::None;
    };
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => Action::CloseChat { id, label },
        _ => Action::None,
    }
}

/// The chat's name: enter names it (empty: Claude's title again), esc
/// cancels.
fn handle_rename_chat_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::RenameChat { name, .. } = &mut app.mode else {
        return Action::None;
    };
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            let typed = name.value().trim().to_string();
            let Mode::RenameChat { id, label, .. } = std::mem::replace(&mut app.mode, Mode::Normal)
            else {
                unreachable!("renaming a chat");
            };
            return Action::RenameChat {
                id,
                label,
                name: (!typed.is_empty()).then_some(typed),
            };
        }
        _ => {
            name.handle(key);
        }
    }
    Action::None
}

fn handle_filter_key(key: KeyEvent, app: &mut App) -> Action {
    match key.code {
        KeyCode::Esc if app.filter.is_empty() => app.filter_active = false,
        KeyCode::Esc => app.clear_filter(),
        KeyCode::Enter => app.filter_active = false,
        KeyCode::Down => return Action::SelectNext,
        KeyCode::Up => return Action::SelectPrev,
        _ => {
            let before = app.filter.value().to_string();
            if app.filter.handle(key) && app.filter.value() != before {
                app.selected = 0;
            }
        }
    }
    Action::None
}

fn handle_picker_key(key: KeyEvent, app: &mut App, config: &Config) -> Action {
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Down => picker_mut(app, PickerState::select_next),
        KeyCode::Up => picker_mut(app, PickerState::select_prev),
        // j/k type into the filter here: ctrl-n/ctrl-p move, as in vim's menus.
        KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
            picker_mut(app, PickerState::select_next)
        }
        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
            picker_mut(app, PickerState::select_prev)
        }
        KeyCode::Enter => return handle_picker_enter(app, config),
        // Several repositories make one task; a chat is in one.
        KeyCode::Tab if matches!(app.mode, Mode::PickProject(_, Purpose::Task)) => {
            picker_mut(app, PickerState::toggle_mark)
        }
        KeyCode::Tab => {}
        _ => picker_mut(app, |p| {
            p.edit(key);
        }),
    }
    Action::None
}

fn handle_new_task_name_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::NewTaskEnterName { name, project_dirs } = &mut app.mode else {
        return Action::None;
    };
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter if !name.is_empty() => {
            let action = Action::DoNewTask {
                name: name.value().to_string(),
                dirs: std::mem::take(project_dirs),
            };
            app.mode = Mode::Normal;
            return action;
        }
        _ => {
            name.handle(key);
        }
    }
    Action::None
}

fn handle_enter_path_key(key: KeyEvent, app: &mut App) -> Action {
    let Mode::SpawnEnterPath(input, purpose) = &mut app.mode else {
        return Action::None;
    };
    let purpose = *purpose;
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            let path = input.value().to_string();
            app.mode = Mode::Normal;
            let dir = PathBuf::from(&path);
            if !dir.is_dir() {
                app.set_status(format!("Not a directory: {path}"), Duration::from_secs(3));
                return Action::None;
            }
            match (repository(&dir), purpose) {
                (Ok(root), Purpose::Task) => {
                    app.mode = Mode::NewTaskEnterName {
                        project_dirs: vec![root],
                        name: TextInput::default(),
                    }
                }
                (Ok(root), Purpose::Chat) => return Action::ChatHistory { dir: root },
                (Err(e), _) => app.set_status(e, Duration::from_secs(5)),
            }
        }
        _ => {
            input.handle(key);
        }
    }
    Action::None
}

/// A click or the wheel: in the task list, select (a member's row focuses
/// that member) or move the selection; over the
/// pane, scroll the agent's screen; in the project picker, highlight or
/// move. A click closes the help.
fn handle_mouse(mouse: MouseEvent, app: &mut App) -> Action {
    let at = ratatui::layout::Position::new(mouse.column, mouse.row);
    let click = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
    match &mut app.mode {
        Mode::Normal => {}
        // A click in the list gives it the keyboard back; the wheel scrolls
        // the pane as in the list.
        Mode::Terminal { .. } | Mode::Scroll { .. } => {
            let in_list = app.hits.list.contains(at);
            if click && in_list {
                app.leave_pane();
            } else if in_list {
                // The wheel there would move the selection, and the
                // keyboard with it.
                return Action::None;
            }
        }
        Mode::Help if click => {
            app.mode = Mode::Normal;
            return Action::None;
        }
        Mode::PickProject(picker, _) | Mode::PickSession(SessionPicker { picker, .. }) => {
            match (mouse.kind, app.hits.picker) {
                (MouseEventKind::ScrollDown, _) => picker.select_next(),
                (MouseEventKind::ScrollUp, _) => picker.select_prev(),
                (MouseEventKind::Down(MouseButton::Left), Some((area, first)))
                    if area.contains(at) =>
                {
                    let index = first + usize::from(at.y - area.y);
                    if index < picker.filtered_items().len() {
                        picker.selected = index;
                    }
                }
                _ => {}
            }
            return Action::None;
        }
        _ => return Action::None,
    }
    let in_list = app.hits.list.contains(at);
    let in_pane = app.hits.pane.is_some_and(|pane| pane.contains(at));
    match mouse.kind {
        MouseEventKind::ScrollDown if in_list => Action::SelectNext,
        MouseEventKind::ScrollUp if in_list => Action::SelectPrev,
        MouseEventKind::ScrollDown if in_pane => Action::ScrollPeek(Scroll::Down(3)),
        MouseEventKind::ScrollUp if in_pane => Action::ScrollPeek(Scroll::Up(3)),
        MouseEventKind::Down(MouseButton::Left) if in_list => {
            let row = usize::from(at.y - app.hits.list.y);
            if let Some(Some((index, member))) = app.hits.rows.get(row).cloned() {
                app.select_at(index, member);
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
/// pane shows (`None` on a chat).
fn selected_member(app: &App) -> Option<(&Task, &crate::task::Member)> {
    app.focused_member()
}

/// `a`: the answer dialog, on the request the member's agent waits on as
/// shown now (the dialog keeps it: the one it answers), or why there is
/// none to answer from here.
fn open_answer(app: &mut App) {
    let Some(focused) = app.focused() else {
        return;
    };
    let label = focused.label();
    let why = match focused.agent() {
        Some(agent) => match &agent.detail.ask {
            Some(ask) => {
                let dialog = AnswerDialog::new(agent.id.clone(), label, ask.clone());
                app.mode = Mode::Answer(dialog);
                return;
            }
            None if matches!(
                agent.state,
                tam_proto::AgentState::Blocked | tam_proto::AgentState::Question
            ) =>
            {
                format!("'{label}' waits on what the dashboard cannot answer: enter attaches")
            }
            None => format!("'{label}' waits on no answer"),
        },
        None => format!("No agent runs for '{label}'"),
    };
    app.set_status(why, Duration::from_secs(4));
}

/// The answer dialog: the request's keys (see `answer::AnswerDialog::key`).
fn handle_answer_key(key: KeyEvent, app: &mut App) -> Action {
    let stale = app.answer_stale();
    let Mode::Answer(dialog) = &mut app.mode else {
        return Action::None;
    };
    match dialog.key(key, stale) {
        Outcome::Stay => Action::None,
        Outcome::Close => {
            app.mode = Mode::Normal;
            Action::None
        }
        Outcome::Send(answer) => {
            let Mode::Answer(dialog) = std::mem::replace(&mut app.mode, Mode::Normal) else {
                unreachable!("in the answer dialog");
            };
            Action::Answer {
                what: answer::describe(&dialog.ask, &answer),
                agent: dialog.agent,
                ask: dialog.ask.id,
                answer,
                label: dialog.label,
            }
        }
    }
}

/// Apply `f` to the open picker (the repositories', or a new chat's
/// sessions).
fn picker_mut(app: &mut App, f: impl FnOnce(&mut PickerState)) {
    if let Mode::PickProject(p, _) | Mode::PickSession(SessionPicker { picker: p, .. }) =
        &mut app.mode
    {
        f(p)
    }
}

/// The sessions a new chat can start from: enter starts it (a new session
/// for the first item, else resuming the one picked), esc cancels; the
/// filter and moves as in the repository picker (tab picks nothing).
fn handle_session_key(key: KeyEvent, app: &mut App) -> Action {
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Down => picker_mut(app, PickerState::select_next),
        KeyCode::Up => picker_mut(app, PickerState::select_prev),
        KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
            picker_mut(app, PickerState::select_next)
        }
        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
            picker_mut(app, PickerState::select_prev)
        }
        KeyCode::Tab => {}
        KeyCode::Enter => {
            let Mode::PickSession(sessions) = &app.mode else {
                return Action::None;
            };
            let Some(id) = sessions.picker.selected_item().map(|i| i.id.clone()) else {
                return Action::None;
            };
            let Mode::PickSession(sessions) = std::mem::replace(&mut app.mode, Mode::Normal) else {
                unreachable!("picking a session");
            };
            return Action::CreateChat {
                dir: sessions.dir,
                session: (id != NEW_SESSION).then_some(id),
            };
        }
        _ => picker_mut(app, |p| {
            p.edit(key);
        }),
    }
    Action::None
}

// ---------------------------------------------------------------------------
// New task flow
// ---------------------------------------------------------------------------

/// Open the repository picker at once (the projects found last time, while
/// they are looked for again), for a new task or chat.
fn start_pick_project(app: &mut App, purpose: Purpose) {
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
    // Those found last time, while they are looked for again.
    items.extend(app.projects.iter().flatten().cloned());
    let title = match purpose {
        Purpose::Task => "Pick project",
        Purpose::Chat => "New chat · pick a repository",
    };
    let mut picker = PickerState::new(title, items);
    picker.loading = true;
    app.mode = Mode::PickProject(picker, purpose);
}

/// The projects under `$HOME` (`tam ls`'s discovery): it walks the
/// directories, so not on the UI thread.
fn discover_projects() -> Vec<PickerItem> {
    let Ok(wt_config) = tam_worktree::config::load_config() else {
        return Vec::new();
    };
    let Ok(ignore) = tam_worktree::discovery::build_ignore_set(&wt_config.ignore) else {
        return Vec::new();
    };
    let root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let Ok(paths) = tam_worktree::discovery::discover(&root, &ignore, wt_config.max_depth) else {
        return Vec::new();
    };
    tam_worktree::pretty::build_pretty_names(&paths)
        .into_iter()
        .map(|entry| PickerItem {
            display: entry.display_name,
            id: entry.path.to_string_lossy().into_owned(),
        })
        .collect()
}

/// Enter in the project picker: the projects picked with tab, else the
/// highlighted one (or the path entry).
fn handle_picker_enter(app: &mut App, _config: &Config) -> Action {
    let Mode::PickProject(picker, purpose) = &app.mode else {
        return Action::None;
    };
    let purpose = *purpose;
    // A chat: the highlighted repository.
    let ids = if picker.marked.is_empty() || purpose == Purpose::Chat {
        match picker.selected_item() {
            Some(item) => vec![item.id.clone()],
            None => return Action::None,
        }
    } else {
        picker.marked.clone()
    };
    app.mode = Mode::Normal;
    if ids == ["__enter_path__"] {
        app.mode = Mode::SpawnEnterPath(TextInput::default(), purpose);
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
    if purpose == Purpose::Chat {
        return match roots.pop() {
            Some(dir) => Action::ChatHistory { dir },
            None => Action::None,
        };
    }
    app.mode = Mode::NewTaskEnterName {
        project_dirs: roots,
        name: TextInput::default(),
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
    let viewing = app.mode.pane_agent().is_some();
    let peek = &mut app.peek;
    if peek.agent != wanted {
        // Another agent: from its live screen.
        *peek = Peek {
            agent: wanted.clone(),
            watching: peek.watching.take(),
            viewing: peek.viewing,
            height: peek.height,
            room: peek.room,
            ..Peek::default()
        };
    }
    if peek.watching != wanted || peek.viewing != viewing {
        // An error is an agent already gone: its exit event follows.
        let _ = client.watch_screen(wanted.as_deref(), viewing).await;
        peek.watching = wanted.clone();
        peek.viewing = viewing;
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

/// Give the agent the pane shows a terminal of the pane's size, so that
/// its screen fills it: asked once per agent and size (see `Peek::asked`).
/// Its screen follows (`ScreenUpdated`).
async fn fit_agent(client: &mut Client, app: &mut App) {
    let (Some(id), Some((cols, rows))) = (app.peek.agent.clone(), app.peek.wanted_size()) else {
        return;
    };
    app.peek.asked = Some((cols, rows));
    // An error is an agent already gone: its exit event follows.
    let _ = client
        .send(tam_proto::Request::Resize { id, cols, rows })
        .await;
}

/// Fetch the selected task's (or chat's) runs and messages for the detail
/// pane (it follows the daemon's events from then on).
async fn refresh_detail(client: &mut Client, app: &mut App) {
    let Some(pick) = app.pick() else {
        app.detail = None;
        return;
    };
    // Shown without them when they cannot be had: no retry until it changes.
    let (runs, of) = match &pick {
        Pick::Task(name) => (client.get_task(name).await.map(|(_, runs)| runs), name),
        Pick::Chat(id) => (client.get_chat(id).await.map(|(_, runs)| runs), id),
    };
    let messages = match client
        .send(tam_proto::Request::ListMessages {
            task: Some(of.clone()),
        })
        .await
    {
        Ok(tam_proto::Response::Messages { messages }) => messages,
        _ => Vec::new(),
    };
    app.detail = Some(TaskDetail {
        runs: runs.unwrap_or_default(),
        messages,
        ..TaskDetail::of(pick)
    });
}

/// Stop the daemon's screen updates (the pane is closed, or the dashboard
/// stops reading for a while).
async fn unwatch_screen(client: &mut Client, app: &mut App) {
    if app.peek.watching.take().is_some() {
        let _ = client.watch_screen(None, false).await;
        app.peek.viewing = false;
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
            .run_agent(&member.id, SessionChoice::New, None, vec![])
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

/// Attach to the agent `id` on the whole screen (named `label` in its status
/// bar), the dashboard reading nothing meanwhile.
async fn attach(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    client: &mut Client,
    input: &mut InputReader,
    id: &str,
    label: &str,
) -> Result<()> {
    // Before the reader stops: no report may reach the agent.
    mouse_capture(false);
    bracketed_paste(false);
    keyboard_protocol(false);
    // The dashboard reads nothing while attached.
    unwatch_screen(client, app).await;
    input.pause().await;
    // An agent just started may not be in the list yet: it is Claude.
    let provider = app
        .find_agent(id)
        .map_or_else(|| "claude".to_string(), |a| a.provider.clone());
    let attached = attach_agent(terminal, app, id, label, &provider).await;
    mouse_capture(true);
    bracketed_paste(true);
    keyboard_protocol(true);
    input.resume();
    attached?;
    // Attached, the agent took the terminal's size.
    app.peek.asked = None;
    Ok(())
}

/// Start a chat's agent in its session. `false` when it did not start: a
/// notice says why (with the way out when its session is gone), and the
/// pane gives the keyboard back.
async fn resume_chat(client: &mut Client, app: &mut App, id: &str, label: &str) -> bool {
    match client
        .run_agent(id, SessionChoice::Current, None, vec![])
        .await
    {
        Ok((_, Some(_))) => {
            app.set_status(format!("Resumed '{label}'"), Duration::from_secs(3));
            true
        }
        Ok((_, None)) => {
            app.set_status(
                format!("Started a new session in '{label}'"),
                Duration::from_secs(3),
            );
            true
        }
        Err(e) => {
            if app.starting.as_deref() == Some(id) {
                app.starting = None;
            }
            let lines = match chats::session_gone(&e) {
                Some(why) => vec![
                    format!("{why}."),
                    "A chat is its session: d closes this one, c starts another.".into(),
                ],
                None => format!("{e:#}").lines().map(str::to_string).collect(),
            };
            app.mode = Mode::Notice {
                title: "Resume Failed".into(),
                lines,
            };
            false
        }
    }
}

/// The sessions a new chat in the repository `dir` (a main checkout) can
/// start from, to pick from (a notice when the daemon cannot list them).
async fn open_chat_history(client: &mut Client, app: &mut App, dir: PathBuf) {
    match client.chat_history(&dir).await {
        Ok(sessions) => app.mode = Mode::PickSession(SessionPicker::new(dir, sessions)),
        Err(e) => {
            app.mode = Mode::Notice {
                title: "No Chat".into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
        }
    }
}

/// Make a chat in the repository `dir`, resuming `session` (else a new
/// one), start its agent and give it the keyboard (see `App::starting`).
/// A chat whose agent cannot start is closed again: it never ran, and it
/// would hold the session. Shown without the filter when it would hide it.
async fn create_chat(client: &mut Client, app: &mut App, dir: &Path, session: Option<String>) {
    let failed = |app: &mut App, e: anyhow::Error| {
        app.mode = Mode::Notice {
            title: "Create Failed".into(),
            lines: format!("{e:#}").lines().map(str::to_string).collect(),
        };
    };
    let chat = match client.create_chat(dir, None, session.as_deref()).await {
        Ok(chat) => chat,
        Err(e) => return failed(app, e),
    };
    let id = chat.id.clone();
    app.upsert_chat(chat);
    let pick = Pick::Chat(id.clone());
    if !app.visible_entries().iter().any(|e| e.pick() == pick) {
        app.filter.clear();
        app.filter_active = false;
    }
    app.select(&pick);
    match client
        .run_agent(&id, SessionChoice::Current, None, vec![])
        .await
    {
        Ok(_) => {
            app.mode = Mode::Terminal { agent: id.clone() };
            app.starting = Some(id);
            let what = match &session {
                Some(_) => "Resumed a session in a new chat",
                None => "Started a new chat",
            };
            app.set_status(format!("{what}: typing into it"), Duration::from_secs(3));
        }
        Err(e) => {
            let closed = match client.close_chat(&id).await {
                Ok(_) => "the chat is closed again".to_string(),
                Err(close) => format!("closing the chat failed too: {close:#}"),
            };
            app.remove_chat(&id);
            failed(
                app,
                e.context(format!("its agent did not start ({closed})")),
            );
        }
    }
}

/// Close a chat (the daemon stops its agent); the list follows its events.
async fn close_chat(client: &mut Client, app: &mut App, id: &str, label: &str) {
    match client.close_chat(id).await {
        Ok(_) => app.set_status(
            format!("Closed '{label}': c makes it again from its repository's history"),
            Duration::from_secs(5),
        ),
        Err(e) => {
            app.mode = Mode::Notice {
                title: "Close Failed".into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
        }
    }
}

/// Keep a chat, or stop keeping it: the status line says when it closes
/// then.
async fn keep_chat(client: &mut Client, app: &mut App, id: &str, label: &str, keep: bool) {
    let chat = match client.keep_chat(id, keep).await {
        Ok(chat) => chat,
        Err(e) => {
            app.set_status(format!("Error: {e:#}"), Duration::from_secs(5));
            return;
        }
    };
    let what = match Closes::of(&chat, crate::task::now()) {
        Closes::Kept => format!("Kept '{label}': never closed for want of turns"),
        Closes::Off => format!("'{label}' is no longer kept (autoclean is off)"),
        Closes::Due => format!("'{label}' is no longer kept: closes when idle"),
        Closes::In(left) => format!(
            "'{label}' is no longer kept: closes in {}",
            chats::time_left(left)
        ),
    };
    app.upsert_chat(chat);
    app.set_status(what, Duration::from_secs(4));
}

/// Name a chat (`None`: Claude's title again); a refusal (a name taken by
/// another chat or a task) opens a notice.
async fn rename_chat(
    client: &mut Client,
    app: &mut App,
    id: &str,
    label: &str,
    name: Option<String>,
) {
    match client.rename_chat(id, name.as_deref()).await {
        Ok(chat) => {
            let what = match &name {
                Some(name) => format!("Renamed '{label}' to '{name}'"),
                None => format!("'{label}' is unnamed: it shows '{}'", chat.label()),
            };
            app.upsert_chat(chat);
            app.set_status(what, Duration::from_secs(4));
        }
        Err(e) => {
            app.mode = Mode::Notice {
                title: "Rename Refused".into(),
                lines: format!("{e:#}").lines().map(str::to_string).collect(),
            };
        }
    }
}

/// Start a member's agent in the chosen session; the daemon resolves the
/// member's current session.
async fn do_run(
    client: &mut Client,
    app: &mut App,
    member: &str,
    session: SessionChoice,
) -> Result<()> {
    let label = app
        .find_member(member)
        .map(|(t, m)| t.member_label(m))
        .unwrap_or_else(|| member.to_string());
    let started = client.run_agent(member, session, None, vec![]).await;
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

/// Send an answer: the status line says it went, a notice why it did not
/// (the request was answered or changed meanwhile: nothing was sent).
async fn answer_request(
    client: &mut Client,
    app: &mut App,
    request: tam_proto::Request,
    label: &str,
    what: &str,
) -> Result<()> {
    match client.send(request).await? {
        tam_proto::Response::Ok => {
            app.set_status(format!("Sent to '{label}': {what}"), Duration::from_secs(4));
        }
        tam_proto::Response::Error { message } => {
            app.mode = Mode::Notice {
                title: "Not Sent".into(),
                lines: vec![format!("Your answer to '{label}': {message}.")],
            };
        }
        _ => {}
    }
    Ok(())
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
        tam_proto::Event::ChatCreated { chat } | tam_proto::Event::ChatUpdated { chat } => {
            app.upsert_chat(chat);
        }
        tam_proto::Event::ChatClosed { id } => {
            app.remove_chat(&id);
        }
    }
}

/// The tasks and the chats, with their running agents.
async fn build_lists(client: &mut Client) -> Result<(Vec<Task>, Vec<Chat>)> {
    // Fresh worktree health; the daemon pushes its changes from then on.
    let infos = client.reconcile().await?;
    let chats = client.list_chats().await?;
    let agents = client.list_agents().await.unwrap_or_default();

    let tasks = infos
        .into_iter()
        .map(|info| Task::from_info(info, &agents))
        .collect();
    let chats = chats
        .into_iter()
        .map(|info| Chat::new(info, &agents))
        .collect();
    Ok((tasks, chats))
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
    fn home_and_end_scroll_the_preview_page_keys_no_more() {
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = App::new();
        for (code, scroll) in [
            (KeyCode::Home, Scroll::Oldest),
            (KeyCode::End, Scroll::Live),
        ] {
            assert!(matches!(
                handle_normal_key(key(code), &mut app, &config),
                Action::ScrollPeek(s) if s == scroll
            ));
        }
        for code in [KeyCode::PageUp, KeyCode::PageDown] {
            assert!(matches!(
                handle_normal_key(key(code), &mut app, &config),
                Action::None
            ));
        }
    }

    #[test]
    fn v_scrolls_the_pane_a_line_at_a_time_until_ctrl_bracket() {
        use tam_proto::AgentState::{Blocked, Ready, Working};
        let config = Config::default();
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        handle_key_event(press('v'), &mut app, &config);
        assert!(matches!(&app.mode, Mode::Scroll { agent } if agent == "m-2"));

        let scrolls = |app: &mut App, code, modifiers| match handle_key_event(
            key(code, modifiers),
            app,
            &Config::default(),
        ) {
            Action::ScrollPeek(scroll) => Some(scroll),
            _ => None,
        };
        let none = KeyModifiers::NONE;
        for (code, scroll) in [
            (KeyCode::Char('j'), Scroll::Down(1)),
            (KeyCode::Down, Scroll::Down(1)),
            (KeyCode::Char('k'), Scroll::Up(1)),
            (KeyCode::Up, Scroll::Up(1)),
            (KeyCode::Char('g'), Scroll::Oldest),
            (KeyCode::Home, Scroll::Oldest),
            (KeyCode::End, Scroll::Live),
        ] {
            assert_eq!(scrolls(&mut app, code, none), Some(scroll), "{code:?}");
        }
        let shift = KeyModifiers::SHIFT;
        assert_eq!(
            scrolls(&mut app, KeyCode::Char('G'), shift),
            Some(Scroll::Live)
        );
        // Every other key does nothing: neither the agent's (nothing typed)
        // nor the list's (`d` would drop, `q` quit, enter type).
        for (code, modifiers) in [
            (KeyCode::Char('q'), none),
            (KeyCode::Char('d'), none),
            (KeyCode::Char('s'), none),
            (KeyCode::Char('l'), none),
            (KeyCode::Char('v'), none),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (KeyCode::Enter, none),
            (KeyCode::Esc, none),
        ] {
            assert!(
                matches!(
                    handle_key_event(key(code, modifiers), &mut app, &config),
                    Action::None
                ),
                "{code:?}"
            );
            assert!(matches!(&app.mode, Mode::Scroll { agent } if agent == "m-2"));
        }
        // A re-sort keeps it on its agent.
        app.update_state("m-1", Blocked);
        app.update_state("m-2", Ready);
        app.sync_focus();
        app.check_typing();
        assert!(matches!(&app.mode, Mode::Scroll { agent } if agent == "m-2"));

        // The wheel scrolls the pane, never the list (the selection, and
        // the pane with it, would move).
        draw(&mut app, 80, 24);
        let pane = app.hits.pane.unwrap();
        assert!(matches!(
            handle_mouse(
                mouse(MouseEventKind::ScrollUp, pane.x + 2, pane.y + 2),
                &mut app
            ),
            Action::ScrollPeek(Scroll::Up(3))
        ));
        let list = app.hits.list;
        assert!(matches!(
            handle_mouse(mouse(MouseEventKind::ScrollDown, 3, list.y + 1), &mut app),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Scroll { .. }));

        // ctrl-] (read as ctrl-5 without the kitty protocol): back to the
        // list, on the live screen.
        app.peek.from = Some(4);
        let leave = key(KeyCode::Char('5'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key_event(leave, &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.peek.from, None);
        assert!(matches!(
            handle_key_event(press('j'), &mut app, &config),
            Action::SelectNext
        ));

        // A click in the list leaves too.
        handle_key_event(press('v'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Scroll { .. }));
        app.peek.from = Some(4);
        let click = MouseEventKind::Down(MouseButton::Left);
        handle_mouse(mouse(click, 3, list.y + 1), &mut app);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.peek.from, None);

        // Its agent exits: back to the list, and the keys still coming
        // (a held `j`) are dropped, not run.
        handle_key_event(press('v'), &mut app, &config);
        let Mode::Scroll { agent } = &app.mode else {
            panic!("not scrolling");
        };
        let agent = agent.clone();
        app.remove_agent(&agent);
        app.sync_focus();
        app.check_typing();
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status_message().unwrap().contains("the agent exited"));
        let selected = app.selected;
        assert!(matches!(
            handle_key_event(press('j'), &mut app, &config),
            Action::None
        ));
        assert_eq!(app.selected, selected);

        // No agent: nothing to scroll.
        app.dropping_keys_until = None;
        app.remove_agent("m-1");
        app.remove_agent("m-2");
        app.sync_focus();
        handle_key_event(press('v'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn v_scrolls_a_chat_s_pane_and_never_starts_its_agent() {
        let config = Config::default();
        let mut app = chat_app();
        select(&mut app, Pick::Chat("c-1".into()));
        handle_key_event(press('v'), &mut app, &config);
        assert!(matches!(&app.mode, Mode::Scroll { agent } if agent == "c-1"));
        let leave = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL);
        handle_key_event(leave, &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
        // Without an agent: nothing to scroll, and nothing resumed (as
        // enter does).
        select(&mut app, Pick::Chat("c-2".into()));
        assert!(matches!(
            handle_key_event(press('v'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.starting, None);
    }

    #[test]
    fn v_is_a_custom_command_s_when_one_took_it() {
        use tam_proto::AgentState::Working;
        let config = Config {
            commands: vec![CustomCommand {
                name: "view".into(),
                key: "v".into(),
                command: "true".into(),
            }],
            ..Config::default()
        };
        let mut app = two_member_app(&[("m-1", Working)]);
        app.commands = config.commands.clone();
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        assert!(matches!(
            handle_key_event(press('v'), &mut app, &config),
            Action::CustomCommand { ref name, .. } if name == "view"
        ));
        assert!(matches!(app.mode, Mode::Normal));
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

    /// The member `f` (and so every member key) acts on.
    fn attach_target(app: &mut App) -> Option<String> {
        match handle_normal_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            app,
            &Config::default(),
        ) {
            Action::Attach { id, .. } => Some(id),
            _ => None,
        }
    }

    #[test]
    fn keys_for_the_agent_never_run_as_the_dashboard_s() {
        use tam_proto::AgentState::{Blocked, Ready, Working};
        let config = Config::default();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let mut app = two_member_app(&[("m-1", Working), ("m-3", Working)]);
        // Most urgent first: both tasks in one list, ordered by status.
        app.toggle_order();
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        handle_key_event(enter, &mut app, &config);
        assert!(matches!(&app.mode, Mode::Terminal { agent } if agent == "m-1"));

        // The other task comes to need the user, then this one ends its
        // turn: the list is ordered again each time, the selection stays on
        // its task and the pane on its agent.
        app.update_state("m-3", Blocked);
        app.update_state("m-1", Ready);
        app.sync_focus();
        assert_eq!(app.selected_task().unwrap().name, "feat");
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));

        // The wheel over the list would move the selection: not while typing.
        app.hits.list = ratatui::layout::Rect::new(0, 1, 40, 10);
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 3,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(handle_mouse(wheel, &mut app), Action::None));
        assert!(matches!(app.mode, Mode::Terminal { .. }));

        // Its agent exits mid-word: the keys still coming are dropped, not
        // run (`d` then `y` would drop the task).
        app.remove_agent("m-1");
        app.stop_typing("the agent exited");
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status_message().unwrap().contains("the agent exited"));
        for c in ['d', 'y', 'q'] {
            assert!(matches!(
                handle_key_event(key(c), &mut app, &config),
                Action::None
            ));
            assert!(matches!(app.mode, Mode::Normal), "{c}");
        }
        // Once typing paused, keys are the dashboard's again.
        app.dropping_keys_until = Some(std::time::Instant::now());
        assert!(matches!(
            handle_key_event(key('q'), &mut app, &config),
            Action::Quit
        ));

        // The user's own ctrl-] drops nothing.
        app.update_agent(
            serde_json::from_value(serde_json::json!({
                "id": "m-1", "provider": "claude", "dir": "/w", "state": "working",
                "pid": 1, "uptime_secs": 1,
            }))
            .unwrap(),
        );
        app.sync_focus();
        handle_key_event(enter, &mut app, &config);
        assert!(matches!(app.mode, Mode::Terminal { .. }));
        let leave = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL);
        handle_key_event(leave, &mut app, &config);
        assert!(matches!(
            handle_key_event(key('q'), &mut app, &config),
            Action::Quit
        ));
    }

    #[test]
    fn a_paste_goes_into_a_prompt_or_nowhere() {
        let config = Config::default();
        let mut app = two_member_app(&[]);
        // On the list: dropped (as keys, `d` would open the drop dialog).
        paste_into_prompt("d\ny", &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
        // In the filter: its text, on one line.
        app.filter_active = true;
        paste_into_prompt("fe\nat", &mut app, &config);
        assert_eq!(app.filter.value(), "feat");
        assert!(app.filter_active);
    }

    #[test]
    fn enter_gives_the_pane_the_keyboard_until_ctrl_bracket() {
        use tam_proto::AgentState::{Blocked, Working};
        let config = Config::default();
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            handle_key_event(enter, &mut app, &config),
            Action::None
        ));
        assert!(matches!(&app.mode, Mode::Terminal { agent } if agent == "m-2"));

        // Every key is the agent's, the dashboard's own keys too.
        let typed = |app: &mut App, code, modifiers| match handle_key_event(
            key(code, modifiers),
            app,
            &Config::default(),
        ) {
            Action::Keys(bytes) => bytes,
            _ => panic!("not typed"),
        };
        assert_eq!(
            typed(&mut app, KeyCode::Char('q'), KeyModifiers::NONE),
            b"q"
        );
        assert_eq!(
            typed(&mut app, KeyCode::Char('d'), KeyModifiers::NONE),
            b"d"
        );
        assert_eq!(typed(&mut app, KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(
            typed(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
            [3]
        );
        assert_eq!(typed(&mut app, KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert!(matches!(app.mode, Mode::Terminal { .. }));

        // ctrl-] (read as ctrl-5 without the kitty protocol): back to the list.
        let leave = key(KeyCode::Char('5'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key_event(leave, &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(
            handle_key_event(
                key(KeyCode::Char('q'), KeyModifiers::NONE),
                &mut app,
                &config
            ),
            Action::Quit
        ));

        // shift-enter (told from enter by some terminals): full screen, as `f`.
        let shift_enter = key(KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(matches!(
            handle_key_event(shift_enter, &mut app, &config),
            Action::Attach { id, .. } if id == "m-2"
        ));
        assert!(matches!(app.mode, Mode::Normal));

        // A click in the list gives it the keyboard back too.
        handle_key_event(enter, &mut app, &config);
        app.hits.list = ratatui::layout::Rect::new(0, 1, 40, 10);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(click, &mut app);
        assert!(matches!(app.mode, Mode::Normal));

        // No agent: nothing to type into.
        app.remove_agent("m-1");
        app.remove_agent("m-2");
        handle_key_event(enter, &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn member_keys_act_on_the_member_that_needs_the_user() {
        use tam_proto::AgentState::{Blocked, Ready, Working};
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));

        // The one the pane shows. It stays when the other one comes to
        // need the user more: a key never acts on a member the pane did
        // not show.
        app.update_state("m-2", Working);
        app.update_state("m-1", Blocked);
        app.sync_focus();
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));
        assert_eq!(app.peek_agent().as_deref(), Some("m-2"));
        // Back on the task after another one: the most urgent again.
        let feat = app.selected;
        app.selected = 1 - feat;
        app.sync_focus();
        app.selected = feat;
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

        // ←/→ move it.
        let config = Config::default();
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        handle_normal_key(right, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        handle_normal_key(right, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        handle_normal_key(left, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        // gg and G: the first and the last task; one g alone does nothing.
        let press = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let before = app.selected;
        let last = app.visible_tasks().len() - 1;
        handle_normal_key(press('G'), &mut app, &config);
        assert_eq!(app.selected, last);
        handle_normal_key(press('g'), &mut app, &config);
        assert_eq!(app.selected, last, "one g");
        handle_normal_key(press('j'), &mut app, &config);
        handle_normal_key(press('g'), &mut app, &config);
        assert_eq!(app.selected, last, "g, another key, g: not gg");
        handle_normal_key(press('g'), &mut app, &config);
        assert_eq!(app.selected, 0);
        app.selected = before;
        app.sync_focus();
        let order = app.order;
        handle_normal_key(press('t'), &mut app, &config);
        assert_ne!(app.order, order, "t switches the order");
        assert!(matches!(
            handle_normal_key(press('T'), &mut app, &config),
            Action::NextTheme
        ));
        handle_normal_key(press('t'), &mut app, &config);
        app.sync_focus();
        handle_normal_key(right, &mut app, &config);
        handle_normal_key(left, &mut app, &config);
        // h/l are the same keys.
        let l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        handle_normal_key(l, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-2"));
        let h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        handle_normal_key(h, &mut app, &config);
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));
        // ctrl-d, ctrl-u, ctrl-f and ctrl-b scrolled the pane by pages
        // once: never the drop dialog nor full screen.
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        for c in ['d', 'u', 'f', 'b'] {
            assert!(matches!(
                handle_normal_key(ctrl(c), &mut app, &config),
                Action::None
            ));
        }
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(attach_target(&mut app).as_deref(), Some("m-1"));

        // `s` and `r` too: here `r` runs the idle one.
        app.remove_agent("m-1");
        assert!(matches!(
            handle_normal_key(press('r'), &mut app, &config),
            Action::RunAgent { ref member } if member == "m-1"
        ));
        app.update_state("m-2", Ready);
        app.cycle_member(1);
        assert!(matches!(
            handle_normal_key(press('s'), &mut app, &config),
            Action::StopAgent { ref id, ref label } if id == "m-2" && label == "feat/web"
        ));
    }

    fn set_ask(app: &mut App, id: &str, ask: Option<tam_proto::Ask>) {
        let mut info = app.find_member(id).unwrap().1.agent_info.clone().unwrap();
        info.detail.ask = ask;
        app.update_agent(info);
    }

    fn bash_ask(id: &str) -> tam_proto::Ask {
        tam_proto::Ask {
            id: id.into(),
            tool: "Bash".into(),
            kind: tam_proto::AskKind::Permission {
                input: "command: ls".into(),
                cut: false,
            },
        }
    }

    // -- Chats --------------------------------------------------------------

    fn chat_info(id: &str, title: &str) -> tam_proto::ChatInfo {
        tam_proto::ChatInfo {
            id: id.into(),
            name: None,
            repo: "api".into(),
            dir: PathBuf::from("/src/api"),
            created_at: task::now() - 3600,
            keep: false,
            session: Some(format!("s-{id}")),
            last_turn_at: None,
            closes_at: Some(task::now() + 3 * 86400),
            run_count: 1,
            title: Some(title.into()),
            last_prompt: None,
        }
    }

    fn agent_info(id: &str, state: tam_proto::AgentState) -> tam_proto::AgentInfo {
        serde_json::from_value(serde_json::json!({
            "id": id, "provider": "claude", "dir": "/src/api", "state": state,
            "pid": 1, "uptime_secs": 1,
        }))
        .unwrap()
    }

    /// Tasks `feat` (two members, `m-1` working) and `other`, and chats
    /// `c-1` (its agent ready) and `c-2` (none), the first chat selected.
    fn chat_app() -> App {
        use tam_proto::AgentState::{Ready, Working};
        let mut app = two_member_app(&[("m-1", Working)]);
        app.set_chats(vec![
            Chat::new(
                chat_info("c-1", "Running chat"),
                &[agent_info("c-1", Ready)],
            ),
            Chat::new(chat_info("c-2", "Idle chat"), &[]),
        ]);
        app.selected = 0;
        app.sync_focus();
        app
    }

    fn selected(app: &App) -> Option<Pick> {
        app.pick()
    }

    fn select(app: &mut App, pick: Pick) {
        app.selected = app
            .visible_entries()
            .iter()
            .position(|e| e.pick() == pick)
            .unwrap();
        app.sync_focus();
    }

    #[test]
    fn the_selection_goes_from_the_chats_to_the_tasks() {
        let config = Config::default();
        let mut app = chat_app();
        assert_eq!(selected(&app), Some(Pick::Chat("c-1".into())));
        app.select_next();
        assert_eq!(selected(&app), Some(Pick::Chat("c-2".into())));
        app.select_next();
        assert_eq!(selected(&app), Some(Pick::Task("feat".into())));
        assert_eq!(app.focused().map(|f| f.id()), Some("m-1"));
        app.select_prev();
        assert!(app.focused_member().is_none(), "a chat has no member");
        handle_key_event(press('G'), &mut app, &config);
        assert_eq!(selected(&app), Some(Pick::Task("other".into())));
        handle_key_event(press('g'), &mut app, &config);
        handle_key_event(press('g'), &mut app, &config);
        assert_eq!(selected(&app), Some(Pick::Chat("c-1".into())));

        // A click on a chat's row.
        app.hits.list = ratatui::layout::Rect::new(0, 1, 40, 10);
        app.hits.rows = vec![
            None,
            Some((0, None)),
            Some((1, None)),
            None,
            Some((2, None)),
        ];
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(click(3), &mut app);
        assert_eq!(selected(&app), Some(Pick::Chat("c-2".into())));
        handle_mouse(click(5), &mut app);
        assert_eq!(selected(&app), Some(Pick::Task("feat".into())));

        // A new dashboard starts on the top entry: the first chat.
        let mut fresh = App::new();
        fresh.set_chats(app.chats.clone());
        fresh.set_tasks(app.tasks.clone());
        assert_eq!(selected(&fresh), Some(Pick::Chat("c-1".into())));
    }

    #[test]
    fn enter_types_into_a_chat_resuming_it_first() {
        use tam_proto::AgentState::Working;
        let config = Config::default();
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);
        let leave = key(KeyCode::Char('5'), KeyModifiers::CONTROL);
        let mut app = chat_app();

        // Its agent runs: the pane takes the keyboard.
        assert!(matches!(
            handle_key_event(enter, &mut app, &config),
            Action::None
        ));
        assert!(matches!(&app.mode, Mode::Terminal { agent } if agent == "c-1"));
        app.check_typing();
        assert!(matches!(app.mode, Mode::Terminal { .. }));
        handle_key_event(leave, &mut app, &config);

        // None runs: started, the pane taking the keyboard at once, kept
        // while its agent is on its way.
        app.select_next();
        match handle_key_event(enter, &mut app, &config) {
            Action::Resume { id, label, attach } => {
                assert_eq!((id.as_str(), label.as_str()), ("c-2", "Idle chat"));
                assert!(!attach);
            }
            _ => panic!("not resumed"),
        }
        assert!(matches!(&app.mode, Mode::Terminal { agent } if agent == "c-2"));
        assert_eq!(app.starting.as_deref(), Some("c-2"));
        app.check_typing();
        assert!(matches!(app.mode, Mode::Terminal { .. }));
        // It shows up: typing goes on.
        apply_event(
            &mut app,
            tam_proto::Event::AgentSpawned {
                id: "c-2".into(),
                info: agent_info("c-2", Working),
            },
        );
        assert_eq!(app.starting, None);
        app.check_typing();
        assert!(matches!(&app.mode, Mode::Terminal { agent } if agent == "c-2"));
        // It exits: back to the list.
        apply_event(
            &mut app,
            tam_proto::Event::AgentExited {
                id: "c-2".into(),
                exit_code: 0,
            },
        );
        app.check_typing();
        assert!(matches!(app.mode, Mode::Normal));

        // Not starting, or starting for a chat the pane no longer shows:
        // the keyboard goes back.
        app.dropping_keys_until = None;
        app.mode = Mode::Terminal {
            agent: "c-2".into(),
        };
        app.check_typing();
        assert!(matches!(app.mode, Mode::Normal));
        app.mode = Mode::Terminal {
            agent: "c-2".into(),
        };
        app.starting = Some("c-2".into());
        app.select_next();
        app.sync_focus();
        app.check_typing();
        assert!(matches!(app.mode, Mode::Normal));
        app.select_prev();
        app.starting = None;
        app.dropping_keys_until = None;

        // `f` and shift-enter start it too, then attach; on a running one
        // they attach.
        for key in [press('f'), key(KeyCode::Enter, KeyModifiers::SHIFT)] {
            assert!(matches!(
                handle_key_event(key, &mut app, &config),
                Action::Resume { id, attach: true, .. } if id == "c-2"
            ));
        }
        app.select_prev();
        assert!(matches!(
            handle_key_event(press('f'), &mut app, &config),
            Action::Attach { id, .. } if id == "c-1"
        ));
        app.select_next();

        // A chat is its session: `r` and `R` say enter resumes it, and
        // never start another session.
        for c in ['r', 'R'] {
            assert!(matches!(
                handle_key_event(press(c), &mut app, &config),
                Action::None
            ));
            let status = app.status_message().unwrap().to_string();
            assert!(status.contains("enter"), "{status}");
        }
    }

    #[test]
    fn d_closes_a_chat_once_confirmed_p_keeps_it_and_n_names_it() {
        let config = Config::default();
        let mut app = chat_app();
        app.select_next();

        handle_key_event(press('d'), &mut app, &config);
        assert!(matches!(
            &app.mode,
            Mode::ConfirmCloseChat { id, label } if id == "c-2" && label == "Idle chat"
        ));
        assert!(matches!(
            handle_key_event(press('n'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        for confirm in [
            press('y'),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            handle_key_event(press('d'), &mut app, &config);
            assert!(matches!(
                handle_key_event(confirm, &mut app, &config),
                Action::CloseChat { id, .. } if id == "c-2"
            ));
            assert!(matches!(app.mode, Mode::Normal));
        }

        // Keep, then stop keeping.
        assert!(matches!(
            handle_key_event(press('p'), &mut app, &config),
            Action::KeepChat { id, keep: true, .. } if id == "c-2"
        ));
        let mut kept = chat_info("c-2", "Idle chat");
        kept.keep = true;
        app.upsert_chat(kept);
        assert!(matches!(
            handle_key_event(press('p'), &mut app, &config),
            Action::KeepChat { keep: false, .. }
        ));

        // Name it; empty, Claude's title again.
        handle_key_event(press('N'), &mut app, &config);
        let Mode::RenameChat { name, title, .. } = &app.mode else {
            panic!("no rename");
        };
        assert!(name.is_empty());
        assert_eq!(title.as_deref(), Some("Idle chat"));
        for c in "  deploy ".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            handle_key_event(enter, &mut app, &config),
            Action::RenameChat { id, name: Some(name), .. } if id == "c-2" && name == "deploy"
        ));
        let mut named = chat_info("c-2", "Idle chat");
        named.name = Some("deploy".into());
        app.upsert_chat(named);
        handle_key_event(press('N'), &mut app, &config);
        assert!(matches!(&app.mode, Mode::RenameChat { name, .. } if name.value() == "deploy"));
        let erase = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        handle_key_event(erase, &mut app, &config);
        assert!(matches!(
            handle_key_event(enter, &mut app, &config),
            Action::RenameChat { name: None, .. }
        ));
        handle_key_event(press('N'), &mut app, &config);
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &config,
        );
        assert!(matches!(app.mode, Mode::Normal));
        // A paste goes into the name.
        handle_key_event(press('N'), &mut app, &config);
        paste_into_prompt("-v2\n", &mut app, &config);
        assert!(matches!(&app.mode, Mode::RenameChat { name, .. } if name.value() == "deploy-v2"));
        app.mode = Mode::Normal;

        // On a task: `p` and `N` do nothing, `d` drops.
        app.select_next();
        assert!(matches!(
            handle_key_event(press('p'), &mut app, &config),
            Action::None
        ));
        handle_key_event(press('N'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
        handle_key_event(press('d'), &mut app, &config);
        assert!(matches!(app.mode, Mode::ConfirmDropTask { .. }));
    }

    #[test]
    fn custom_commands_on_a_chat_and_the_keys_they_took() {
        let mut app = chat_app();
        let command = |key: &str| CustomCommand {
            name: format!("cmd {key}"),
            key: key.into(),
            command: "true".into(),
        };
        let config = Config {
            commands: vec![command("o"), command("p")],
            ..Config::default()
        };
        app.commands = config.commands.clone();
        let run = |app: &mut App, c| match handle_key_event(press(c), app, &config) {
            Action::CustomCommand {
                name,
                task,
                id,
                dir,
                ..
            } => (name, task, id, dir),
            _ => panic!("no command"),
        };
        // `{task}` is the chat's label, `{id}` its id, `{dir}` its main
        // checkout.
        assert_eq!(
            run(&mut app, 'o'),
            (
                "cmd o".into(),
                "Running chat".into(),
                "c-1".into(),
                PathBuf::from("/src/api")
            )
        );
        // `p` is the custom command's, also on a chat.
        assert_eq!(run(&mut app, 'p').0, "cmd p");
        // On a task: its name, the member's id and worktree.
        select(&mut app, Pick::Task("feat".into()));
        assert_eq!(
            run(&mut app, 'o'),
            (
                "cmd o".into(),
                "feat".into(),
                "m-1".into(),
                PathBuf::from("/w/app--feat")
            )
        );
    }

    #[test]
    fn a_chat_s_request_is_answered_and_its_events_followed() {
        use tam_proto::AgentState::{Blocked, Question, Ready};
        let config = Config::default();
        let mut app = chat_app();
        let mut info = agent_info("c-1", Blocked);
        info.detail.ask = Some(bash_ask("ask-9"));
        apply_event(
            &mut app,
            tam_proto::Event::AgentUpdated {
                id: "c-1".into(),
                info,
            },
        );
        handle_key_event(press('a'), &mut app, &config);
        assert!(matches!(&app.mode, Mode::Answer(d) if d.agent == "c-1"));
        assert!(!app.answer_stale());
        draw(&mut app, 100, 30);
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::Answer { agent, ask, label, .. }
                if agent == "c-1" && ask == "ask-9" && label == "Running chat"
        ));
        assert!(matches!(
            handle_key_event(press('s'), &mut app, &config),
            Action::StopAgent { id, .. } if id == "c-1"
        ));

        // The header's count and the spinner take chats.
        assert_eq!(app.needs_attention_count(), 1);
        apply_event(
            &mut app,
            tam_proto::Event::StateChange {
                id: "c-1".into(),
                old: Blocked,
                new: Ready,
            },
        );
        apply_event(
            &mut app,
            tam_proto::Event::ContextUpdate {
                id: "c-1".into(),
                context_percent: 42,
            },
        );
        let agent = app.chats[0].agent.as_ref().unwrap();
        assert_eq!((agent.state, agent.context_percent), (Ready, Some(42)));

        // Attention first, the selection follows its chat as it moves.
        select(&mut app, Pick::Chat("c-2".into()));
        app.toggle_order();
        assert_eq!(selected(&app), Some(Pick::Chat("c-2".into())));
        apply_event(
            &mut app,
            tam_proto::Event::AgentSpawned {
                id: "c-2".into(),
                info: agent_info("c-2", Question),
            },
        );
        assert_eq!(app.selected, 0, "the most urgent: {:?}", app.pick());
        assert_eq!(selected(&app), Some(Pick::Chat("c-2".into())));
        app.toggle_order();

        // Made, renamed (its agent kept), closed.
        let mut made = chat_info("c-3", "Made elsewhere");
        apply_event(
            &mut app,
            tam_proto::Event::ChatCreated { chat: made.clone() },
        );
        let ids: Vec<&str> = app.chats.iter().map(|c| c.info.id.as_str()).collect();
        assert_eq!(ids, ["c-1", "c-2", "c-3"], "creation order");
        let mut renamed = chat_info("c-1", "Running chat");
        renamed.name = Some("release".into());
        apply_event(&mut app, tam_proto::Event::ChatUpdated { chat: renamed });
        assert_eq!(app.chats[0].info.label(), "release");
        assert!(app.chats[0].agent.is_some());
        made.keep = true;
        apply_event(&mut app, tam_proto::Event::ChatUpdated { chat: made });
        assert!(app.chats[2].info.keep);
        assert_eq!(selected(&app), Some(Pick::Chat("c-2".into())));
        apply_event(&mut app, tam_proto::Event::ChatClosed { id: "c-2".into() });
        assert_eq!(app.chats.len(), 2);
        assert_eq!(selected(&app), Some(Pick::Chat("c-3".into())));
    }

    #[test]
    fn the_pane_keeps_a_chat_s_runs_and_messages() {
        let mut app = chat_app();
        assert!(app.detail_stale());
        app.detail = Some(TaskDetail::of(Pick::Chat("c-1".into())));
        assert!(!app.detail_stale());
        let message = |id: &str, to: &str| tam_proto::MessageInfo {
            id: id.into(),
            from: Some("m-1".into()),
            from_label: "feat/app".into(),
            to: to.into(),
            to_label: to.into(),
            text: "hello".into(),
            hop: 0,
            reply_to: None,
            sent_at: task::now(),
            delivered_at: None,
        };
        app.note_message(message("msg-1", "c-1"));
        app.note_message(message("msg-2", "c-2"));
        let ids: Vec<&str> = app
            .detail
            .as_ref()
            .unwrap()
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, ["msg-1"]);
        // A run started or ended: fetched again.
        app.upsert_chat(chat_info("c-1", "Running chat"));
        assert!(app.detail_stale());
        app.select_next();
        app.detail = Some(TaskDetail::of(Pick::Chat("c-1".into())));
        assert!(app.detail_stale(), "another chat shown");
    }

    #[test]
    fn a_answers_the_request_it_opened_on_and_nothing_else() {
        use tam_proto::AgentState::{Blocked, Working};
        let mut app = two_member_app(&[("m-1", Working), ("m-2", Blocked)]);
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        let config = Config::default();

        // Blocked on what the dashboard cannot answer (its hook is gone).
        handle_key_event(press('a'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
        let status = app.status_message().unwrap().to_string();
        assert!(status.contains("enter attaches"), "{status}");

        // The other member asks: the dialog is about that one.
        set_ask(&mut app, "m-2", Some(bash_ask("ask-1")));
        handle_key_event(press('a'), &mut app, &config);
        let Mode::Answer(dialog) = &app.mode else {
            panic!("no answer dialog");
        };
        assert_eq!(
            (dialog.agent.as_str(), dialog.ask.id.as_str()),
            ("m-2", "ask-1")
        );
        // Typed with `a`, before the request was drawn: nothing.
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::None
        ));
        draw(&mut app, 100, 30);
        // Another member comes to need the user more: the dialog keeps to
        // its own.
        app.update_state("m-1", Blocked);
        app.sync_focus();
        match handle_key_event(press('y'), &mut app, &config) {
            Action::Answer {
                agent,
                ask,
                answer,
                label,
                what,
            } => {
                assert_eq!((agent.as_str(), ask.as_str()), ("m-2", "ask-1"));
                assert_eq!(answer, tam_proto::AskAnswer::Allow);
                assert_eq!(
                    (label.as_str(), what.as_str()),
                    ("feat/web", "Bash allowed once")
                );
            }
            _ => panic!("no answer sent"),
        }
        assert!(matches!(app.mode, Mode::Normal));

        // The request changes while the dialog is open: any key closes it,
        // nothing sent.
        app.update_state("m-1", Working);
        app.sync_focus();
        handle_key_event(press('a'), &mut app, &config);
        draw(&mut app, 100, 30);
        set_ask(&mut app, "m-2", Some(bash_ask("ask-2")));
        assert!(app.answer_stale());
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
        // Answered in the terminal: the same.
        handle_key_event(press('a'), &mut app, &config);
        draw(&mut app, 100, 30);
        set_ask(&mut app, "m-2", None);
        assert!(app.answer_stale());
        assert!(matches!(
            handle_key_event(press('y'), &mut app, &config),
            Action::None
        ));

        // Not waiting: nothing to answer.
        app.update_state("m-2", Working);
        handle_key_event(press('a'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));
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
        assert!(app.detail_stale());
        app.detail = Some(TaskDetail::of(Pick::Task("feat".into())));
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
    fn c_picks_a_repository_then_what_the_chat_starts_from() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = tam_worktree::git::command()
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
        let web_wt = tmp.path().join("web--x");
        git(
            &web,
            &["worktree", "add", "-q", "-b", "x", web_wt.to_str().unwrap()],
        );
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = chat_app();

        assert!(matches!(
            handle_key_event(press('c'), &mut app, &config),
            Action::PickProject(Purpose::Chat)
        ));
        start_pick_project(&mut app, Purpose::Chat);
        let Mode::PickProject(picker, Purpose::Chat) = &mut app.mode else {
            panic!("not picking");
        };
        assert_eq!(picker.title, "New chat · pick a repository");
        let item = |display: &str, id: &std::path::Path| PickerItem {
            display: display.into(),
            id: id.to_str().unwrap().into(),
        };
        picker.set_items(vec![
            PickerItem {
                display: "enter path...".into(),
                id: "__enter_path__".into(),
            },
            item("api", &api),
            item("web", &web),
            item("x @web", &web_wt),
        ]);
        // One repository: tab picks nothing; a worktree stands for its
        // repository's main checkout.
        handle_key_event(key(KeyCode::Down), &mut app, &config);
        handle_key_event(key(KeyCode::Tab), &mut app, &config);
        let Mode::PickProject(picker, _) = &app.mode else {
            panic!("still picking");
        };
        assert!(picker.marked.is_empty());
        assert_eq!(picker.selected, 1, "tab did not move");
        handle_key_event(key(KeyCode::Down), &mut app, &config);
        handle_key_event(key(KeyCode::Down), &mut app, &config);
        let web_root = std::fs::canonicalize(&web).unwrap();
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::ChatHistory { dir } if dir == web_root
        ));
        assert!(matches!(app.mode, Mode::Normal));

        // Or a path typed (the picker's second item).
        start_pick_project(&mut app, Purpose::Chat);
        handle_key_event(key(KeyCode::Down), &mut app, &config);
        handle_key_event(key(KeyCode::Enter), &mut app, &config);
        assert!(matches!(app.mode, Mode::SpawnEnterPath(_, Purpose::Chat)));
        paste_into_prompt(api.to_str().unwrap(), &mut app, &config);
        let api_root = std::fs::canonicalize(&api).unwrap();
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::ChatHistory { dir } if dir == api_root
        ));

        // The daemon's history of the repository (newest first, those a
        // chat holds already left out): a new chat first, then each.
        let session =
            |id: &str, title: Option<&str>, prompt: &str, ago: u64| tam_proto::SessionInfo {
                id: id.into(),
                title: title.map(str::to_string),
                last_prompt: Some(prompt.into()),
                modified_at: task::now() - ago,
            };
        let open = |app: &mut App| {
            app.mode = Mode::PickSession(SessionPicker::new(
                web_root.clone(),
                vec![
                    session("s-1", Some("Release deployment"), "deploy 26.8.4", 7200),
                    session("s-2", None, "why is build 1234 slow", 9 * 86400),
                ],
            ));
        };
        open(&mut app);
        let Mode::PickSession(sessions) = &app.mode else {
            panic!("no sessions");
        };
        let ids: Vec<&str> = sessions
            .picker
            .filtered_items()
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(ids, [NEW_SESSION, "s-1", "s-2"]);
        assert_eq!(sessions.picker.title, "New chat in web");
        // Enter on the first: a new session.
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::CreateChat { dir, session: None } if dir == web_root
        ));
        assert!(matches!(app.mode, Mode::Normal));
        // Down to one: that session; tab picks nothing.
        open(&mut app);
        handle_key_event(key(KeyCode::Tab), &mut app, &config);
        handle_key_event(key(KeyCode::Down), &mut app, &config);
        handle_key_event(key(KeyCode::Down), &mut app, &config);
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::CreateChat { session: Some(s), .. } if s == "s-2"
        ));
        // The filter matches titles and prompts: what it leaves is what
        // enter takes, never a new chat it does not match.
        open(&mut app);
        for c in "deploy".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::CreateChat { session: Some(s), .. } if s == "s-1"
        ));
        open(&mut app);
        paste_into_prompt("1234", &mut app, &config);
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::CreateChat { session: Some(s), .. } if s == "s-2"
        ));
        open(&mut app);
        for c in "new".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::CreateChat { session: None, .. }
        ));
        // Nothing matches: enter does nothing; esc closes.
        open(&mut app);
        for c in "zzz".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        assert!(matches!(
            handle_key_event(key(KeyCode::Enter), &mut app, &config),
            Action::None
        ));
        handle_key_event(key(KeyCode::Esc), &mut app, &config);
        assert!(matches!(app.mode, Mode::Normal));

        // A custom command on `c` keeps it.
        app.commands = vec![CustomCommand {
            name: "code".into(),
            key: "c".into(),
            command: "true".into(),
        }];
        assert!(matches!(
            handle_key_event(press('c'), &mut app, &config),
            Action::CustomCommand { name, .. } if name == "code"
        ));
    }

    #[test]
    fn tab_picks_several_projects_in_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = tam_worktree::git::command()
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
        app.mode = Mode::PickProject(picker, Purpose::Task);
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // The path entry is an action, not a project: tab skips it.
        handle_picker_key(key(KeyCode::Tab), &mut app, &config);
        let Mode::PickProject(p, _) = &app.mode else {
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
        let Mode::PickProject(p, _) = &app.mode else {
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

        // Looked at 30s ago: the member the pane shows is due (every 10s),
        // the others are not (every 60s).
        for id in ["m-1", "m-2", "m-3"] {
            looked(&mut app, id, 30);
        }
        app.selected = app.tasks.iter().position(|t| t.name == "feat").unwrap();
        app.sync_focus();
        let focused = app.focused_member().unwrap().1.id().to_string();
        assert_eq!(app.branch_due().map(|(id, _)| id), Some(focused.clone()));
        looked(&mut app, &focused, 0);
        assert_eq!(app.branch_due(), None);

        // The oldest look goes first; a state change asks for a new one.
        looked(&mut app, "m-3", 90);
        app.update_state("m-1", tam_proto::AgentState::Ready);
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

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn prompts_edit_at_the_cursor() {
        let config = Config::default();
        let mut app = App::new();
        app.mode = Mode::NewTaskEnterName {
            project_dirs: vec![PathBuf::from("/src/api")],
            name: TextInput::from("feat-x"),
        };
        // ctrl-u clears (it typed a `u` before), ← then a character inserts.
        handle_key_event(
            key(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut app,
            &config,
        );
        for c in "fix-nav".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        for _ in 0..3 {
            handle_key_event(key(KeyCode::Left, KeyModifiers::NONE), &mut app, &config);
        }
        handle_key_event(press('x'), &mut app, &config);
        let action = handle_key_event(key(KeyCode::Enter, KeyModifiers::NONE), &mut app, &config);
        assert!(
            matches!(action, Action::DoNewTask { ref name, ref dirs } if name == "fix-xnav" && dirs.len() == 1),
        );

        // The filter: typed, edited, esc clears it, esc again closes it.
        let mut app = two_member_app(&[]);
        handle_key_event(press('/'), &mut app, &config);
        assert!(app.filter_active);
        for c in "wxb".chars() {
            handle_key_event(press(c), &mut app, &config);
        }
        handle_key_event(key(KeyCode::Left, KeyModifiers::NONE), &mut app, &config);
        handle_key_event(
            key(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
            &config,
        );
        assert_eq!(app.filter.value(), "wb");
        // Fuzzy, on members too: w-b in feat's member web.
        let names: Vec<&str> = app
            .visible_tasks()
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, ["feat"]);
        // Esc empties it, the task found still selected.
        app.filter = TextInput::from("lb");
        assert_eq!(app.selected_task().unwrap().name, "other");
        handle_key_event(key(KeyCode::Esc, KeyModifiers::NONE), &mut app, &config);
        assert!(app.filter_active && app.filter.is_empty());
        assert_eq!(app.selected_task().unwrap().name, "other");
        assert_eq!(app.visible_tasks().len(), 2);
        handle_key_event(key(KeyCode::Esc, KeyModifiers::NONE), &mut app, &config);
        assert!(!app.filter_active);

        // `?` shows the keys; any key closes them, and does nothing else.
        handle_key_event(press('?'), &mut app, &config);
        assert!(matches!(app.mode, Mode::Help));
        assert!(matches!(
            handle_key_event(press('q'), &mut app, &config),
            Action::None
        ));
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// Draw `app` (so that it knows where things are), `width`x`height`.
    fn draw(app: &mut App, width: u16, height: u16) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::render(f, app)).unwrap();
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn the_mouse_selects_and_scrolls() {
        let click = MouseEventKind::Down(MouseButton::Left);
        // Rows: header (0), column names (1), then
        // `api + web` (2), feat (3), ├ app (4), └ web (5), lib (6), other (7).
        let mut app = two_member_app(&[]);
        draw(&mut app, 80, 24);
        assert_eq!(app.selected_task().unwrap().name, "feat");
        handle_mouse(mouse(click, 10, 7), &mut app);
        assert_eq!(app.selected_task().unwrap().name, "other");
        // A group's header: nothing.
        handle_mouse(mouse(click, 10, 6), &mut app);
        assert_eq!(app.selected_task().unwrap().name, "other");
        // A member's row selects its task, and with the pane open, it.
        handle_mouse(mouse(click, 10, 5), &mut app);
        assert_eq!(app.selected_task().unwrap().name, "feat");
        app.peek = Peek::default();
        app.sync_focus();
        draw(&mut app, 80, 24);
        handle_mouse(mouse(click, 10, 5), &mut app);
        assert_eq!(app.focused_member().unwrap().1.id(), "m-2");
        handle_mouse(mouse(click, 10, 4), &mut app);
        assert_eq!(app.focused_member().unwrap().1.id(), "m-1");

        // The wheel: over the list it moves the selection, over the pane it
        // scrolls the screen.
        assert!(matches!(
            handle_mouse(mouse(MouseEventKind::ScrollDown, 10, 4), &mut app),
            Action::SelectNext
        ));
        let pane = app.hits.pane.unwrap();
        assert!(matches!(
            handle_mouse(mouse(MouseEventKind::ScrollUp, 10, pane.y + 2), &mut app),
            Action::ScrollPeek(Scroll::Up(3))
        ));
        // Outside both (the footer): nothing.
        assert!(matches!(
            handle_mouse(mouse(MouseEventKind::ScrollUp, 10, 23), &mut app),
            Action::None
        ));

        // A click closes the help.
        app.mode = Mode::Help;
        handle_mouse(mouse(click, 0, 0), &mut app);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn the_picker_opens_at_once_and_takes_the_projects_when_found() {
        let item = |name: &str| PickerItem {
            display: name.into(),
            id: format!("/src/{name}"),
        };
        let mut app = App::new();
        start_pick_project(&mut app, Purpose::Task);
        let Mode::PickProject(picker, Purpose::Task) = &mut app.mode else {
            panic!("the picker opens without waiting");
        };
        assert!(picker.loading);
        assert_eq!(picker.items.len(), 2, "the current directory and a path");
        // Highlight the path entry and pick nothing yet; the projects come.
        picker.select_next();
        app.set_projects(vec![item("api"), item("web")]);
        let Mode::PickProject(picker, Purpose::Task) = &mut app.mode else {
            panic!("still picking");
        };
        assert!(!picker.loading);
        let ids: Vec<&str> = picker.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["__cwd__", "__enter_path__", "/src/api", "/src/web"]);
        assert_eq!(picker.selected_item().unwrap().id, "__enter_path__", "kept");

        // A click picks an item on screen, the wheel moves.
        draw(&mut app, 80, 24);
        let (area, first) = app.hits.picker.unwrap();
        assert_eq!(first, 0);
        handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                area.x + 1,
                area.y + 3,
            ),
            &mut app,
        );
        handle_mouse(mouse(MouseEventKind::ScrollUp, 0, 0), &mut app);
        let Mode::PickProject(picker, Purpose::Task) = &app.mode else {
            panic!("still picking");
        };
        assert_eq!(picker.selected_item().unwrap().id, "/src/api");

        // The next time, those found are there from the start; found after
        // the picker closed, they are kept for it.
        app.mode = Mode::Normal;
        app.set_projects(vec![item("docs")]);
        start_pick_project(&mut app, Purpose::Task);
        let Mode::PickProject(picker, Purpose::Task) = &app.mode else {
            panic!("picking");
        };
        assert_eq!(picker.items.last().unwrap().id, "/src/docs");
    }
}
