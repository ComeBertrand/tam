//! The dashboard's lines in the activity log (`src` `tui`; see
//! `crate::activity`): what the user did, in which mode, what it led to and
//! how long that took.
//!
//! - `tui.start`, `tui.stop`: the session, its size and totals.
//! - `tui.input`: a key (named: `j`, `enter`, `ctrl-]`), a click or a paste
//!   that did something, with the mode it came in (`mode`), the mode it
//!   left for (`to`, with `mode_ms` spent in the one left), the action it
//!   ran (`action`, its target, `ms` it took), where the selection is then
//!   (`id`, `on`), what the status line or a notice says then. Never what
//!   is typed: keys into the pane are counted (`typed_bytes`, `writes`,
//!   `pastes`, given when the pane gives the keyboard back), the
//!   characters typed into a prompt too (`chars`).
//! - `tui.mode`: a mode left without an input (the agent typed into
//!   exited, a dialog closed by an event).
//! - `tui.slow`: a redraw that took long, by step.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Map, Value};
use tam_daemon::activity::{self, Activity, Phases};

use super::app::{App, Mode, Purpose};
use super::{keys, Action};

/// A redraw that took this long is logged (`tui.slow`).
const SLOW: Duration = Duration::from_millis(200);

pub struct Recorder {
    log: &'static Activity,
    started: Instant,
    /// The mode the dashboard is in, and since when.
    mode: &'static str,
    mode_since: Instant,
    /// What was done in it that is counted, not logged.
    counted: Counted,
    inputs: u64,
    actions: u64,
    frames: u64,
    slow: u64,
}

#[derive(Default)]
struct Counted {
    typed_bytes: u64,
    writes: u64,
    pastes: u64,
    chars: u64,
    dropped_keys: u64,
}

/// What the dashboard showed before an input, to tell what it changed.
pub struct Before {
    mode: &'static str,
    status: Option<String>,
    notice: bool,
    /// A key now is dropped: typed for an agent the pane no longer has the
    /// keyboard for (see `App::drops_key`).
    dropping: bool,
}

/// What came in: a key, the mouse, a paste.
pub enum Input<'a> {
    Key(&'a KeyEvent),
    Mouse(&'static str),
    Paste(usize),
}

impl Recorder {
    pub fn new(log: &'static Activity) -> Self {
        let now = Instant::now();
        Self {
            log,
            started: now,
            mode: "normal",
            mode_since: now,
            counted: Counted::default(),
            inputs: 0,
            actions: 0,
            frames: 0,
            slow: 0,
        }
    }

    /// The dashboard is up, `ms` after it started.
    pub fn start(&self, app: &App, size: (u16, u16), ms: f64) {
        self.log.record(
            "tui.start",
            json!({
                "ms": ms,
                "cols": size.0,
                "rows": size.1,
                "theme": super::theme::name(),
                "tasks": app.tasks.len(),
                "chats": app.chats.len(),
                "shells": app.shells.len(),
                "keyboard_protocol": super::KEYBOARD_PROTOCOL
                    .load(std::sync::atomic::Ordering::Relaxed),
            }),
        );
    }

    /// The dashboard ends (`error`: why).
    pub fn stop(&self, error: Option<&anyhow::Error>) {
        self.log.record(
            "tui.stop",
            json!({
                "ms": activity::ms_since(self.started),
                "inputs": self.inputs,
                "actions": self.actions,
                "frames": self.frames,
                "slow_frames": self.slow,
                "wait": true,
                "error": error.map(|e| activity::cut(&format!("{e:#}"), 500)),
            }),
        );
    }

    /// A redraw's steps were timed: logged when slow.
    pub fn frame(&mut self, phases: &Phases) {
        self.frames += 1;
        if phases.total_ms() >= activity::ms(SLOW) {
            self.slow += 1;
            self.log.record(
                "tui.slow",
                json!({ "ms": phases.total_ms(), "steps_ms": phases.steps(), "mode": self.mode }),
            );
        }
    }

    /// Before an input is handled.
    pub fn before(&self, app: &mut App) -> Before {
        Before {
            mode: mode_name(app),
            status: app.status_message().map(str::to_string),
            notice: matches!(app.mode, Mode::Notice { .. }),
            // What `handle_key_event` asks `drops_key`, read only.
            dropping: !matches!(app.mode, Mode::Terminal { .. })
                && app
                    .dropping_keys_until
                    .is_some_and(|until| Instant::now() < until),
        }
    }

    /// Bytes typed into the pane: counted until it gives the keyboard back.
    pub fn typed(&mut self, bytes: usize) {
        self.counted.typed_bytes += bytes as u64;
    }

    /// One write of typed bytes to the agent (a batch of inputs).
    pub fn wrote(&mut self) {
        self.counted.writes += 1;
    }

    /// An input was handled: it led to `action` (described before it ran),
    /// which took `took`. Logged unless it is a key typed into the pane or
    /// into a prompt (counted), or a move of the mouse that changed nothing.
    pub fn input(
        &mut self,
        app: &mut App,
        before: Before,
        input: Input,
        action: Option<(&'static str, Map<String, Value>)>,
        took: Duration,
    ) {
        self.inputs += 1;
        let to = mode_name(app);
        let changed = to != before.mode;
        let mut fields = Map::new();
        match input {
            Input::Key(key) => {
                // Meant for the agent, dropped: counted, never named.
                if before.dropping {
                    self.counted.dropped_keys += 1;
                    return;
                }
                // Into the pane: the agent's (`Action::Keys`, counted).
                if before.mode == "terminal" && !changed {
                    return;
                }
                if typed_into_prompt(before.mode, key) {
                    self.counted.chars += 1;
                    if !changed {
                        return;
                    }
                }
                fields.insert("input".into(), "key".into());
                fields.insert("key".into(), key_name(key).into());
            }
            Input::Mouse(kind) => {
                if action.is_none() && !changed && kind != "click" {
                    return;
                }
                fields.insert("input".into(), kind.into());
            }
            Input::Paste(len) => {
                self.counted.pastes += 1;
                // Into the pane (`Keys`, counted) or a prompt.
                if !changed && action.is_none() {
                    return;
                }
                fields.insert("input".into(), "paste".into());
                fields.insert("len".into(), len.into());
            }
        }
        fields.insert("mode".into(), before.mode.into());
        if let Some((name, target)) = action {
            self.actions += 1;
            fields.insert("action".into(), name.into());
            fields.extend(target);
            fields.insert("ms".into(), activity::ms(took).into());
        }
        if changed {
            fields.insert("to".into(), to.into());
            self.left_mode(&mut fields, to);
        }
        self.selection(app, &mut fields, before);
        self.log.record("tui.input", Value::Object(fields));
    }

    /// Called once per loop: a mode left without an input is `tui.mode`.
    pub fn sync(&mut self, app: &mut App) {
        let to = mode_name(app);
        if to == self.mode {
            return;
        }
        let mut fields = Map::new();
        fields.insert("from".into(), self.mode.into());
        fields.insert("to".into(), to.into());
        self.left_mode(&mut fields, to);
        if let Mode::Notice { title, .. } = &app.mode {
            fields.insert("notice".into(), activity::cut(title, 200).into());
        }
        if let Some(status) = app.status_message() {
            fields.insert("status".into(), activity::cut(status, 200).into());
        }
        self.log.record("tui.mode", Value::Object(fields));
    }

    /// The mode is now `to`: say how long the one left lasted, and what
    /// was counted in it.
    fn left_mode(&mut self, fields: &mut Map<String, Value>, to: &'static str) {
        let now = Instant::now();
        fields.insert("mode_ms".into(), activity::ms(now - self.mode_since).into());
        let counted = std::mem::take(&mut self.counted);
        for (name, n) in [
            ("typed_bytes", counted.typed_bytes),
            ("writes", counted.writes),
            ("pastes", counted.pastes),
            ("chars", counted.chars),
            ("dropped_keys", counted.dropped_keys),
        ] {
            if n > 0 {
                fields.insert(name.into(), n.into());
            }
        }
        self.mode = to;
        self.mode_since = now;
    }

    /// Where the selection is after an input, and what the dashboard says.
    fn selection(&self, app: &mut App, fields: &mut Map<String, Value>, before: Before) {
        if let Some(focused) = app.focused() {
            fields.insert("id".into(), focused.id().into());
            fields.insert("on".into(), focused.label().into());
        }
        if !before.notice {
            if let Mode::Notice { title, .. } = &app.mode {
                fields.insert("notice".into(), activity::cut(title, 200).into());
            }
        }
        let status = app.status_message().map(str::to_string);
        if status.is_some() && status != before.status {
            fields.insert(
                "status".into(),
                activity::cut(status.as_deref().unwrap_or_default(), 200).into(),
            );
        }
    }
}

/// The mode's name in the log; the filter counts as one.
pub fn mode_name(app: &App) -> &'static str {
    if app.filter_active {
        return "filter";
    }
    match &app.mode {
        Mode::Normal => "normal",
        Mode::PickProject(_, Purpose::Task) => "pick_repo.task",
        Mode::PickProject(_, Purpose::Chat) => "pick_repo.chat",
        Mode::PickProject(_, Purpose::Shell) => "pick_repo.shell",
        Mode::NewTaskEnterName { .. } => "task_name",
        Mode::SpawnEnterPath(..) => "enter_path",
        Mode::PickSession(_) => "pick_session",
        Mode::ConfirmDropTask { .. } => "confirm_drop",
        Mode::ConfirmForceDrop { .. } => "confirm_force_drop",
        Mode::Notice { .. } => "notice",
        Mode::ConfirmRepairOrDrop { .. } => "repair_or_drop",
        Mode::Help => "help",
        Mode::Answer(_) => "answer",
        Mode::Terminal { .. } => "terminal",
        Mode::Scroll { .. } => "scroll",
        Mode::ConfirmCloseChat { .. } => "confirm_close_chat",
        Mode::ConfirmCloseShell { .. } => "confirm_close_shell",
        Mode::RenameChat { .. } => "chat_name",
    }
}

/// A character typed into a prompt of `mode`: text, not a command.
fn typed_into_prompt(mode: &str, key: &KeyEvent) -> bool {
    let text = matches!(
        mode,
        "filter"
            | "pick_repo.task"
            | "pick_repo.chat"
            | "pick_repo.shell"
            | "task_name"
            | "enter_path"
            | "pick_session"
            | "chat_name"
            | "answer"
    );
    let printable = matches!(key.code, KeyCode::Char(_))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    text && (printable || key.code == KeyCode::Backspace)
}

/// How the log names a key: `j`, `G`, `enter`, `shift-enter`, `ctrl-]`.
pub fn key_name(key: &KeyEvent) -> String {
    if keys::is_leave(*key) {
        return "ctrl-]".into();
    }
    let mut name = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        name.push_str("ctrl-");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        name.push_str("alt-");
    }
    // A character's case says shift already.
    if key.modifiers.contains(KeyModifiers::SHIFT) && !matches!(key.code, KeyCode::Char(_)) {
        name.push_str("shift-");
    }
    match key.code {
        KeyCode::Char(' ') => name.push_str("space"),
        KeyCode::Char(c) => name.push(c),
        KeyCode::Enter => name.push_str("enter"),
        KeyCode::Esc => name.push_str("esc"),
        KeyCode::Tab => name.push_str("tab"),
        KeyCode::BackTab => name.push_str("backtab"),
        KeyCode::Backspace => name.push_str("backspace"),
        KeyCode::Delete => name.push_str("delete"),
        KeyCode::Up => name.push_str("up"),
        KeyCode::Down => name.push_str("down"),
        KeyCode::Left => name.push_str("left"),
        KeyCode::Right => name.push_str("right"),
        KeyCode::Home => name.push_str("home"),
        KeyCode::End => name.push_str("end"),
        KeyCode::PageUp => name.push_str("pageup"),
        KeyCode::PageDown => name.push_str("pagedown"),
        KeyCode::F(n) => name.push_str(&format!("f{n}")),
        other => name.push_str(&format!("{other:?}").to_lowercase()),
    }
    name
}

/// What the log says of an action: its name and target. `None`: not one
/// (`None`), or counted (`Keys`).
pub fn describe(action: &Action) -> Option<(&'static str, Map<String, Value>)> {
    let (name, fields) = match action {
        Action::None | Action::Keys(_) => return None,
        Action::Quit => ("quit", json!({})),
        Action::SelectNext => ("select_next", json!({})),
        Action::SelectPrev => ("select_prev", json!({})),
        Action::Copy { text, cut } => {
            ("copy", json!({ "chars": text.chars().count(), "cut": cut }))
        }
        // `ms`: how long it was attached.
        Action::Attach { id, .. } => ("attach", json!({ "target": id, "wait": true })),
        Action::Resume { id, attach, .. } => (
            "resume",
            json!({ "target": id, "attach": attach, "wait": attach.then_some(true) }),
        ),
        Action::CloseChat { id, .. } => ("close_chat", json!({ "target": id })),
        Action::OpenShell { on } => {
            let (member, dir) = match on {
                tam_proto::ShellPlace::Member(m) => (Some(m), None),
                tam_proto::ShellPlace::Checkout(d) => (None, Some(d)),
            };
            ("open_shell", json!({ "member": member, "dir": dir }))
        }
        Action::CloseShell { id, .. } => ("close_shell", json!({ "target": id })),
        Action::KeepChat { id, keep, .. } => ("keep_chat", json!({ "target": id, "keep": keep })),
        Action::RenameChat { id, name, .. } => {
            ("rename_chat", json!({ "target": id, "name": name }))
        }
        Action::StopAgent { id, .. } => ("stop", json!({ "target": id })),
        Action::DropTask {
            name,
            force,
            delete_branch,
            ..
        } => (
            "drop",
            json!({ "target": name, "force": force, "delete_branch": delete_branch }),
        ),
        Action::PickProject(purpose) => (
            "pick_repo",
            json!({ "purpose": format!("{purpose:?}").to_lowercase() }),
        ),
        Action::ChatHistory { dir } => ("chat_history", json!({ "dir": dir })),
        Action::CreateChat { dir, session } => (
            "create_chat",
            json!({ "dir": dir, "session": session.is_some() }),
        ),
        Action::RunAgent { member } => ("run", json!({ "target": member })),
        Action::DoNewTask { name, dirs } => ("new_task", json!({ "task": name, "repos": dirs })),
        Action::CustomCommand { name, id, .. } => {
            ("custom_command", json!({ "command": name, "target": id }))
        }
        Action::RunNewSession { member } => ("run_new_session", json!({ "target": member })),
        Action::RepairTask { name } => ("repair", json!({ "target": name })),
        Action::ScrollPeek(scroll) => (
            "scroll",
            json!({ "scroll": format!("{scroll:?}").to_lowercase() }),
        ),
        Action::NextTheme => ("next_theme", json!({})),
        Action::Answer {
            agent, ask, answer, ..
        } => (
            "answer",
            json!({
                "target": agent,
                "ask": ask,
                "answer": activity::answer_kind(answer),
            }),
        ),
    };
    let fields = match fields {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    Some((name, fields))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn char_key(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn recorder() -> (Recorder, &'static Activity) {
        let log: &'static Activity = Box::leak(Box::new(Activity::in_memory("tui")));
        (Recorder::new(log), log)
    }

    fn lines(log: &Activity) -> Vec<Value> {
        log.lines()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// `key` came while in the app's mode; the key's handler left it in
    /// `then`.
    fn press(rec: &mut Recorder, app: &mut App, key: KeyEvent, then: Mode) {
        let before = rec.before(app);
        app.mode = then;
        rec.input(app, before, Input::Key(&key), None, Duration::ZERO);
    }

    #[test]
    fn an_input_says_the_mode_it_left_and_typing_is_counted() {
        let (mut rec, log) = recorder();
        let mut app = App::new();
        press(&mut rec, &mut app, char_key('?'), Mode::Help);
        press(&mut rec, &mut app, char_key('x'), Mode::Normal);
        let terminal = || Mode::Terminal {
            agent: "m-1".into(),
        };
        press(
            &mut rec,
            &mut app,
            key(KeyCode::Enter, KeyModifiers::NONE),
            terminal(),
        );
        // Typed into the pane: counted, never logged.
        for c in "secret".chars() {
            let before = rec.before(&mut app);
            rec.typed(1);
            rec.input(
                &mut app,
                before,
                Input::Key(&char_key(c)),
                None,
                Duration::ZERO,
            );
        }
        rec.wrote();
        let leave = key(KeyCode::Char(']'), KeyModifiers::CONTROL);
        press(&mut rec, &mut app, leave, Mode::Normal);

        let lines = lines(log);
        assert_eq!(lines.len(), 4, "{lines:#?}");
        assert!(lines.iter().all(|l| l["kind"] == "tui.input"));
        assert_eq!(lines[0]["key"], "?");
        assert_eq!(lines[0]["mode"], "normal");
        assert_eq!(lines[0]["to"], "help");
        assert_eq!(lines[1]["mode"], "help");
        assert_eq!(lines[1]["to"], "normal");
        assert!(lines[1]["mode_ms"].is_number());
        assert_eq!(lines[2]["to"], "terminal");
        assert_eq!(lines[3]["key"], "ctrl-]");
        assert_eq!(lines[3]["mode"], "terminal");
        assert_eq!(lines[3]["typed_bytes"], 6);
        assert_eq!(lines[3]["writes"], 1);
        assert!(!log.lines().concat().contains("\"s\""));
    }

    #[test]
    fn keys_dropped_after_the_pane_gave_the_keyboard_back_are_not_named() {
        let (mut rec, log) = recorder();
        let mut app = App::new();
        app.mode = Mode::Terminal {
            agent: "sh-1".into(),
        };
        rec.sync(&mut app);
        // The shell closed: the pane gave the keyboard back, unasked.
        app.stop_typing("the shell closed");
        rec.sync(&mut app);
        for c in "secret".chars() {
            let before = rec.before(&mut app);
            assert!(before.dropping);
            rec.input(
                &mut app,
                before,
                Input::Key(&char_key(c)),
                None,
                Duration::ZERO,
            );
        }
        // Typing paused: keys are the dashboard's again.
        app.dropping_keys_until = None;
        press(&mut rec, &mut app, char_key('?'), Mode::Help);
        let lines = lines(log);
        assert!(
            !lines
                .iter()
                .any(|l| ["s", "e", "c", "r", "t"].iter().any(|k| l["key"] == *k)),
            "{lines:#?}"
        );
        let help = lines.last().unwrap();
        assert_eq!(help["key"], "?");
        assert_eq!(help["dropped_keys"], 6);
    }

    #[test]
    fn text_typed_into_a_prompt_is_counted_not_logged() {
        let (mut rec, log) = recorder();
        let mut app = App::new();
        app.filter_active = true;
        rec.sync(&mut app);
        for c in "abc".chars() {
            press(&mut rec, &mut app, char_key(c), Mode::Normal);
        }
        let before = rec.before(&mut app);
        app.filter_active = false;
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);
        rec.input(&mut app, before, Input::Key(&esc), None, Duration::ZERO);
        let lines = lines(log);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        // Opened by something else than an input this recorder saw.
        assert_eq!(lines[0]["kind"], "tui.mode");
        assert_eq!(lines[0]["to"], "filter");
        assert_eq!(lines[1]["key"], "esc");
        assert_eq!(lines[1]["mode"], "filter");
        assert_eq!(lines[1]["chars"], 3);
    }

    #[test]
    fn an_action_says_its_target_and_how_long_it_took() {
        let (mut rec, log) = recorder();
        let mut app = App::new();
        let before = rec.before(&mut app);
        let action = Action::StopAgent {
            id: "m-1".into(),
            label: "fix".into(),
        };
        app.set_status("Stopped fix".into(), Duration::from_secs(3));
        rec.input(
            &mut app,
            before,
            Input::Key(&char_key('x')),
            describe(&action),
            Duration::from_millis(12),
        );
        let lines = lines(log);
        assert_eq!(lines[0]["action"], "stop");
        assert_eq!(lines[0]["target"], "m-1");
        assert_eq!(lines[0]["ms"], 12.0);
        assert_eq!(lines[0]["status"], "Stopped fix");
        assert!(lines[0].get("to").is_none());
    }

    #[test]
    fn keys_are_named() {
        assert_eq!(key_name(&key(KeyCode::Char('j'), KeyModifiers::NONE)), "j");
        assert_eq!(key_name(&key(KeyCode::Char('G'), KeyModifiers::SHIFT)), "G");
        assert_eq!(
            key_name(&key(KeyCode::Enter, KeyModifiers::SHIFT)),
            "shift-enter"
        );
        assert_eq!(
            key_name(&key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            "ctrl-]"
        );
        assert_eq!(
            key_name(&key(KeyCode::Char('5'), KeyModifiers::CONTROL)),
            "ctrl-]"
        );
        assert_eq!(
            key_name(&key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            "ctrl-u"
        );
        assert_eq!(
            key_name(&key(KeyCode::Char(' '), KeyModifiers::NONE)),
            "space"
        );
    }

    #[test]
    fn text_typed_into_prompts_is_not_a_command() {
        let c = key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(typed_into_prompt("filter", &c));
        assert!(typed_into_prompt("chat_name", &c));
        assert!(typed_into_prompt("answer", &c));
        assert!(!typed_into_prompt("normal", &c));
        assert!(!typed_into_prompt("confirm_drop", &c));
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!typed_into_prompt("filter", &enter));
        let ctrl_u = key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(!typed_into_prompt("filter", &ctrl_u));
    }

    #[test]
    fn actions_say_no_text() {
        let answer = Action::Answer {
            agent: "m-1".into(),
            ask: "ask-1".into(),
            answer: tam_proto::AskAnswer::Deny {
                reason: Some("SECRET".into()),
            },
            label: "t".into(),
            what: "SECRET".into(),
        };
        let (name, fields) = describe(&answer).unwrap();
        assert_eq!(name, "answer");
        assert_eq!(fields["answer"], "deny_with_reason");
        assert!(!Value::Object(fields).to_string().contains("SECRET"));
        let copy = Action::Copy {
            text: "SECRET".into(),
            cut: false,
        };
        let (_, fields) = describe(&copy).unwrap();
        assert_eq!(fields["chars"], 6);
        assert!(!Value::Object(fields).to_string().contains("SECRET"));
        let custom = Action::CustomCommand {
            name: "open".into(),
            command: "SECRET {dir}".into(),
            task: "t".into(),
            id: "m-1".into(),
            dir: "/x".into(),
            provider: "claude".into(),
        };
        let (_, fields) = describe(&custom).unwrap();
        assert!(!Value::Object(fields).to_string().contains("SECRET"));
        assert!(describe(&Action::Keys(b"SECRET".to_vec())).is_none());
    }
}
