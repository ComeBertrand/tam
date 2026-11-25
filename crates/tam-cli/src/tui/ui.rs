use ratatui::layout::{Constraint, HorizontalAlignment, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph, Wrap,
};
use ratatui::Frame;
use tam_proto::{AgentDetail, AgentState};

use crate::chat::{time_left, Closes};
use crate::task::TaskStatus;

use super::app::{
    App, Focused, MemberChoice, Mode, Order, PickerState, Purpose, SessionPicker, NEW_SESSION,
};
use super::input::TextInput;
use super::peek::{Peek, ScreenView};
use super::{answer, fuzzy, table, theme, triage};

/// The task list's narrowest and widest beside the pane.
const LIST_MIN: u16 = 50;
const LIST_MAX: u16 = 72;
/// The pane's narrowest beside the list: an agent's screen is 80 columns
/// until someone attaches to it.
const PANE_MIN: u16 = 82;

pub fn render(frame: &mut Frame, app: &mut App) {
    let show_filter = app.filter_active || !app.filter.is_empty();
    let [header_area, body, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, header_area, app);
    let (list, pane) = split(body, app, show_filter);
    let [filter_area, table_area] = Layout::vertical([
        Constraint::Length(u16::from(show_filter)),
        Constraint::Fill(1),
    ])
    .areas(list);
    if show_filter {
        render_filter(frame, filter_area, app);
    }
    render_tasks(frame, table_area, app);
    app.hits.pane = pane;
    if pane.is_none() {
        app.peek.area = Rect::default();
    }
    if let Some(pane) = pane {
        // Out of the app while drawn: the pane reads the rest of it.
        let mut peek = std::mem::take(&mut app.peek);
        render_pane(frame, pane, app, &mut peek);
        app.peek = peek;
    }
    render_footer(frame, footer_area, app);

    // Overlay popups, over a dimmed dashboard
    if !matches!(
        app.mode,
        Mode::Normal | Mode::Terminal { .. } | Mode::Scroll { .. }
    ) {
        let dim = Style::reset().fg(theme::get().muted);
        // Not the footer: it has the dialog's keys.
        frame.buffer_mut().set_style(header_area.union(body), dim);
    }
    app.hits.picker = match &mut app.mode {
        Mode::PickProject(picker, _) => Some(render_picker_popup(frame, picker)),
        Mode::PickSession(sessions) => Some(render_session_popup(frame, sessions)),
        _ => None,
    };
    let stale = app.answer_stale();
    if let Mode::Answer(dialog) = &mut app.mode {
        answer::render(frame, dialog, stale);
    }
    match &app.mode {
        Mode::Normal
        | Mode::Terminal { .. }
        | Mode::Scroll { .. }
        | Mode::PickProject(..)
        | Mode::PickSession(_)
        | Mode::Answer(_) => {}
        Mode::Help => render_help_popup(frame, app),
        Mode::ConfirmCloseChat { label, .. } => render_close_chat_popup(frame, label),
        Mode::ConfirmCloseShell { label, .. } => render_close_shell_popup(frame, label),
        Mode::RenameChat { name, title, .. } => {
            render_rename_chat_popup(frame, name, title.as_deref());
        }
        Mode::NewTaskEnterName { name, project_dirs } => {
            let joins = app.tasks.iter().any(|t| t.name == name.value());
            render_new_task_popup(frame, name, project_dirs, joins);
        }
        Mode::SpawnEnterPath(path, _) => {
            render_enter_path_popup(frame, path);
        }
        Mode::ConfirmDropTask { name, member } => {
            let shells = drop_closes(app, name, member.as_ref());
            render_confirm_drop_popup(frame, name, member.as_ref(), shells.as_deref());
        }
        Mode::ConfirmForceDrop {
            name,
            target,
            report,
            ..
        } => {
            render_force_drop_popup(frame, name, name != target, report);
        }
        Mode::Notice { title, lines } => {
            render_notice_popup(frame, title, lines);
        }
        Mode::ConfirmRepairOrDrop {
            name,
            lines,
            repairable,
            in_place,
            member,
        } => {
            let mut lines = lines.clone();
            lines.extend(drop_closes(app, name, member.as_ref()));
            render_repair_or_drop_popup(
                frame,
                name,
                &lines,
                *repairable,
                *in_place,
                member.as_ref(),
            );
        }
    }
}

/// Where the task list and the detail pane go. Side by side when the pane has room for an agent's screen next
/// to a list of useful width; else the list above the pane, as tall as its
/// rows up to half of the height.
fn split(area: Rect, app: &App, filter: bool) -> (Rect, Option<Rect>) {
    let list = (area.width / 5 * 2).clamp(LIST_MIN, LIST_MAX);
    if area.width >= list + PANE_MIN {
        let [list, pane] =
            Layout::horizontal([Constraint::Length(list), Constraint::Fill(1)]).areas(area);
        return (list, Some(pane));
    }
    // Header, rows, filter.
    let wanted = app.rows().len() as u16 + 1 + u16::from(filter);
    let height = wanted.max(3).min((area.height / 2).max(5));
    let [list, pane] =
        Layout::vertical([Constraint::Length(height), Constraint::Fill(1)]).areas(area);
    (list, Some(pane))
}

fn render_filter(frame: &mut Frame, area: Rect, app: &App) {
    let [prompt, text] =
        Layout::horizontal([Constraint::Length(3), Constraint::Fill(1)]).areas(area);
    frame.render_widget(
        Span::styled(" / ", Style::new().fg(theme::get().accent)),
        prompt,
    );
    app.filter.render(
        frame,
        text,
        Style::new(),
        app.filter_active && matches!(app.mode, Mode::Normal),
    );
}

fn render_tasks(frame: &mut Frame, area: Rect, app: &mut App) {
    app.hits.list = Rect::default();
    app.hits.rows.clear();
    if !app.visible_entries().is_empty() {
        table::render(frame, area, app);
        return;
    }
    let text = if app.filter.is_empty() {
        "No tasks or chats. n: new task, c: new chat."
    } else {
        "Nothing matches."
    };
    let text = Paragraph::new(text)
        .alignment(HorizontalAlignment::Center)
        .fg(theme::get().muted);
    let y = area.y + area.height / 2;
    frame.render_widget(text, Rect::new(area.x, y, area.width, 1));
}

/// The chat and task counts, how many need the user, and how the list is
/// ordered.
fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let tasks = app.tasks.len();
    let chats = app.chats.len();
    let total = tasks + chats;
    let needs = app.needs_attention_count();

    let t = theme::get();
    frame.render_widget(Block::new().style(t.bar()), area);
    let tick = app.spin.unwrap_or(0);
    let mut spans = if t.rainbow {
        // The logo, each cell on the next hue.
        " tam "
            .chars()
            .enumerate()
            .map(|(i, c)| Span::styled(c.to_string(), t.chip(theme::rainbow(i * 2, tick))))
            .collect()
    } else {
        vec![Span::styled(" tam ", t.chip(t.accent))]
    };
    spans.push(Span::raw(" "));

    let count = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
    if total == 0 {
        spans.push(Span::raw("no tasks"));
    } else {
        let counts: Vec<String> = [(chats, "chat"), (tasks, "task")]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, what)| count(n, what))
            .collect();
        spans.push(Span::raw(format!("{} ", counts.join("  "))));
        if needs > 0 {
            spans.push(Span::styled(
                format!(" {needs} need{} you ", if needs == 1 { "s" } else { "" }),
                t.chip(t.warn),
            ));
        }
    }
    let line = Line::from(spans);
    let used = line.width();
    frame.render_widget(line, area);
    let order = match app.order {
        Order::Repo => "by repository ",
        Order::Attention => "attention first ",
    };
    if total > 0 && used + 1 + order.len() <= area.width as usize {
        // In its own corner: a line styles the whole area it is given, and
        // would take the chips' colours.
        let width = order.len() as u16;
        let corner = Rect::new(area.right() - width, area.y, width, 1);
        frame.render_widget(
            Line::styled(order, Style::new().fg(theme::get().muted)),
            corner,
        );
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &mut App) {
    let t = theme::get();
    frame.render_widget(Block::new().style(t.bar()), area);
    let key = Style::new().fg(t.accent).add_modifier(Modifier::BOLD);
    if let Some(msg) = app.status_message() {
        let line = Line::from(format!(" {msg}")).fg(theme::get().warn);
        frame.render_widget(line, area);
    } else {
        let hints = match app.mode {
            Mode::Normal => {
                let hints = normal_hints(app);
                frame.render_widget(fit_hints(hints, area.width).fg(t.muted), area);
                return;
            }
            Mode::NewTaskEnterName { .. } => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":create  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::PickProject(_, Purpose::Task) => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":select  "),
                Span::styled("tab", Style::new().bold()),
                Span::raw(":pick several  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::PickSession(_) => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":start the chat  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::ConfirmDropTask { ref member, .. } => {
                let mut hints = vec![
                    Span::styled(" y", Style::new().bold()),
                    Span::raw("/"),
                    Span::styled("enter", Style::new().bold()),
                    Span::raw(":drop  "),
                    Span::styled("b", Style::new().bold()),
                    Span::raw(":drop + delete branch  "),
                ];
                if member.is_some() {
                    hints.extend([
                        Span::styled("m", Style::new().bold()),
                        Span::raw("/"),
                        Span::styled("M", Style::new().bold()),
                        Span::raw(":member only  "),
                    ]);
                }
                hints.extend([
                    Span::styled("any", Style::new().bold()),
                    Span::raw(":cancel"),
                ]);
                hints
            }
            Mode::ConfirmCloseChat { .. } | Mode::ConfirmCloseShell { .. } => vec![
                Span::styled(" y", Style::new().bold()),
                Span::raw("/"),
                Span::styled("enter", Style::new().bold()),
                Span::raw(":close  "),
                Span::styled("any", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::RenameChat { .. } => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":rename (empty: Claude's title)  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::ConfirmForceDrop { .. } => vec![
                Span::styled(" y", Style::new().bold()),
                Span::raw(":drop anyway  "),
                Span::styled("any", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::Notice { .. } | Mode::Help => vec![
                Span::styled(" any", Style::new().bold()),
                Span::raw(":close"),
            ],
            Mode::Terminal { ref agent } => vec![
                Span::styled(" ctrl-]", Style::new().bold()),
                Span::raw(":back to the list  "),
                Span::raw(match app.shells.iter().any(|s| s.id == *agent) {
                    true => "every other key goes to the shell",
                    false => "every other key goes to the agent",
                }),
            ],
            Mode::Scroll { .. } => vec![
                Span::styled(" j/k", Style::new().bold()),
                Span::raw(":line down/up  "),
                Span::styled("g/G", Style::new().bold()),
                Span::raw(":oldest/live  "),
                Span::styled("enter", Style::new().bold()),
                Span::raw(":type  "),
                Span::styled("ctrl-]", Style::new().bold()),
                Span::raw(":back to the list"),
            ],
            Mode::Answer(ref dialog) => {
                let mut hints = vec![Span::raw(" ")];
                for (key, what) in dialog.hints(app.answer_stale()) {
                    hints.push(Span::styled(key, Style::new().bold()));
                    hints.push(Span::raw(format!(":{what}  ")));
                }
                hints
            }
            Mode::ConfirmRepairOrDrop {
                repairable,
                in_place,
                ref member,
                ..
            } => {
                let mut hints = Vec::new();
                if repairable {
                    hints.extend([
                        Span::styled(" r", Style::new().bold()),
                        Span::raw(":recreate worktree "),
                    ]);
                }
                hints.extend([
                    Span::styled(" y", Style::new().bold()),
                    Span::raw(":drop  "),
                ]);
                if in_place {
                    hints.extend([
                        Span::styled("b", Style::new().bold()),
                        Span::raw(":drop + delete branch  "),
                    ]);
                }
                if member.is_some() {
                    hints.extend([
                        Span::styled("m", Style::new().bold()),
                        Span::raw(":member only  "),
                    ]);
                }
                hints.extend([
                    Span::styled("any", Style::new().bold()),
                    Span::raw(":cancel"),
                ]);
                hints
            }
            _ => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":select  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
        };
        // The keys stand out, as in `fit_hints`.
        let hints: Vec<Span> = hints
            .into_iter()
            .map(|s| {
                if s.style.add_modifier.contains(Modifier::BOLD) {
                    s.style(key)
                } else {
                    s
                }
            })
            .collect();
        frame.render_widget(Line::from(hints).fg(t.muted), area);
    }
}

/// A key hint in the footer: the key, what it does, and how much it
/// matters (0 most: kept when the footer cannot show them all).
type Hint<'a> = (&'a str, String, u8);

/// The list's keys, for the selection.
fn normal_hints(app: &App) -> Vec<Hint<'_>> {
    let selected = app.selected_task();
    let chat = app.selected_chat();
    let shell = app.selected_shell();
    let agent = app.focused().and_then(|f| f.agent());
    let has_agent = agent.is_some();
    // An agent's terminal, or a shell's.
    let terminal = has_agent || shell.is_some();
    let asks = agent.is_some_and(|a| a.detail.ask.is_some());
    let free = |key: char| app.key_taken(key).is_none();
    let mut hints = Vec::new();
    if asks {
        hints.push(("a", "answer".into(), 0));
    }
    if terminal {
        hints.push(("enter", "type".into(), 0));
        hints.push(("f", "full screen".into(), 3));
    }
    if shell.is_some() {
        hints.push(("d", "close".into(), 1));
    } else if has_agent {
        if free('x') {
            hints.push(("x", "stop".into(), 1));
        }
    } else if chat.is_some() {
        hints.push(("enter", "resume".into(), 0));
    } else if selected.is_some() {
        hints.push(("r", "run".into(), 0));
        hints.push(("R", "new session".into(), 2));
    }
    if let Some(chat) = chat {
        hints.push(("d", "close".into(), 1));
        if free('p') {
            let keep = if chat.info.keep { "unkeep" } else { "keep" };
            hints.push(("p", keep.into(), 2));
        }
        if free('N') {
            hints.push(("N", "rename".into(), 3));
        }
    }
    {
        let peek = &app.peek;
        if selected.is_some_and(|t| t.members.len() > 1) {
            hints.push(("h/l", "member".into(), 1));
        }
        if terminal && free('v') {
            hints.push(("v", "scroll".into(), 3));
        }
        if peek.from.is_some() {
            hints.push(("end", "live".into(), 1));
        }
    }
    // A gone worktree: `d` offers to recreate it too.
    let gone = selected.is_some_and(|t| {
        t.unhealthy_members()
            .any(|m| crate::health::repairable(&m.info))
    });
    if selected.is_some() {
        let drop = if gone { "recreate/drop" } else { "drop" };
        hints.push(("d", drop.into(), 1));
    }
    if free('!') && app.selected_entry().is_some() {
        hints.push(("!", "shell here".into(), 3));
    }
    hints.push(("n", "new task".into(), 2));
    if free('c') {
        hints.push(("c", "new chat".into(), 2));
    }
    // The first to go when the footer is short.
    hints.push(("s", "new shell".into(), 6));
    hints.push(("/", "filter".into(), 3));
    let order = match app.order {
        Order::Repo => "attention first",
        Order::Attention => "by repository",
    };
    hints.push(("t", order.into(), 4));
    hints.push(("T", "theme".into(), 5));
    for cmd in &app.commands {
        hints.push((cmd.key.as_str(), cmd.name.clone(), 5));
    }
    hints.push(("?", "help".into(), 0));
    hints.push(("q", "quit".into(), 0));
    hints
}

/// The hints that fit in `width` columns, in order: the least important
/// (the last of them first) are left out.
fn fit_hints(mut hints: Vec<Hint<'_>>, width: u16) -> Line<'static> {
    let shown = |hints: &[Hint]| -> usize {
        hints
            .iter()
            .map(|(key, what, _)| Span::raw(*key).width() + 1 + Span::raw(what.as_str()).width())
            .sum::<usize>()
            + 2 * hints.len().saturating_sub(1)
            + 1
    };
    while !hints.is_empty() && shown(&hints) > width as usize {
        let least = (0..hints.len())
            .max_by_key(|&i| (hints[i].2, i))
            .expect("not empty");
        hints.remove(least);
    }
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, what, _)) in hints.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(theme::get().accent).bold(),
        ));
        spans.push(Span::raw(format!(":{what}")));
    }
    Line::from(spans)
}

/// What the agent waits on or reported, on one line: the pending
/// permission or question, the error, or a tool call refused by auto mode.
pub(super) fn agent_detail_line(
    state: AgentState,
    detail: &AgentDetail,
) -> Option<(String, Color)> {
    let pending = |label: &str, color| {
        detail.pending.as_ref().map(|p| match &p.tool {
            Some(tool) if label == "permission" => {
                (format!("{label} · {tool}: {}", p.summary), color)
            }
            _ => (format!("{label}: {}", p.summary), color),
        })
    };
    match state {
        AgentState::Blocked => pending("permission", theme::get().bad),
        AgentState::Question => pending("question", theme::get().question),
        AgentState::Error => detail.error.as_ref().map(|e| {
            let text = match &e.message {
                Some(m) => format!("error · {}: {}", e.kind, first_line(m)),
                None => format!("error · {}", e.kind),
            };
            (text, theme::get().bad)
        }),
        _ => detail.denied.as_ref().map(|d| {
            let mut text = format!("denied · {}", d.tool);
            if let Some(s) = &d.summary {
                text.push_str(&format!(": {s}"));
            }
            if let Some(r) = &d.reason {
                text.push_str(&format!(" ({})", first_line(r)));
            }
            (text, theme::get().warn)
        }),
    }
}

pub(super) fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// The pane, about the focused member of the selected task or the selected
/// chat: its agent's screen; without an agent, how its last run ended, its
/// branch (a member's) and its messages (see `triage`). The bottom border
/// says what the screen may no longer show: why its turn failed, what auto
/// mode refused, messages waiting for the agent, and when a chat in its
/// last day closes.
fn render_pane(frame: &mut Frame, area: Rect, app: &App, peek: &mut Peek) {
    let focused = app.focused();
    let label = focused.map_or_else(|| "—".to_string(), |f| f.label());
    let agent = focused.and_then(|f| f.agent());

    let title = match peek.scrolled_back() {
        Some(0) => format!(" {label} · scrolled back "),
        Some(1) => format!(" {label} · 1 line into history "),
        Some(n) => format!(" {label} · {n} lines into history "),
        None => format!(" {label} "),
    };
    let t = theme::get();
    // The pane has the keyboard (typed into, or scrolled): its border says
    // so. Scrolling (`v`) in a colour of its own: never taken for typing.
    let typing = matches!(app.mode, Mode::Terminal { .. });
    let keyboard = app.mode.pane_agent().is_some();
    let color = match app.mode {
        Mode::Scroll { .. } => t.scrolled,
        _ => t.accent,
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(if keyboard {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(Style::new().fg(if keyboard { color } else { t.subtle }))
        .title(Line::styled(title, Style::new().fg(color).bold()));
    let chip = match app.mode {
        Mode::Terminal { .. } => Some(" typing · ctrl-] back to the list "),
        Mode::Scroll { .. } => Some(" scrolling · j/k · ctrl-] back to the list "),
        _ => None,
    };
    if let Some(chip) = chip {
        block = block.title_top(Line::styled(chip, t.chip(color)).right_aligned());
    }
    // Not a request it waits on: its screen shows that one.
    let waiting = |state| matches!(state, AgentState::Blocked | AgentState::Question);
    if let Some((text, color)) = agent
        .filter(|a| !waiting(a.state))
        .and_then(|a| agent_detail_line(a.state, &a.detail))
    {
        block = block.title_bottom(Line::styled(format!(" {text} "), Style::new().fg(color)));
    }
    let shown = app.pick();
    let detail = app
        .detail
        .as_ref()
        .filter(|d| shown.as_ref() == Some(&d.of));
    let queued = match (focused, detail, agent) {
        (Some(focused), Some(detail), Some(_)) => detail
            .messages
            .iter()
            .filter(|m| m.to == focused.id() && m.delivered_at.is_none())
            .count(),
        _ => 0,
    };
    if let Some(text) = focused.and_then(|f| f.chat()).and_then(|c| closing(app, c)) {
        block = block.title_bottom(
            Line::styled(format!(" {text} "), Style::new().fg(t.warn)).right_aligned(),
        );
    }
    if queued > 0 {
        let s = if queued == 1 { "" } else { "s" };
        block = block.title_bottom(
            Line::styled(
                format!(" {queued} message{s} waiting for it "),
                Style::new().fg(t.warn),
            )
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if t.rainbow && !keyboard {
        rainbow_border(frame, area, app.spin.unwrap_or(0));
    }
    peek.room = (inner.width, inner.height);

    let view = match focused {
        Some(Focused::Member(task, member)) if agent.is_none() => triage::triage(
            task,
            member,
            detail,
            app.branches.get(member.id()),
            crate::task::now(),
        ),
        Some(Focused::Chat(chat)) if agent.is_none() => triage::chat(
            chat,
            detail,
            app.starting.as_deref() == Some(chat.info.id.as_str()),
            app.key_taken('p').is_none(),
            crate::task::now(),
        ),
        _ => {
            render_screen(frame, inner, peek, typing);
            return;
        }
    };
    frame.render_widget(
        Paragraph::new(triage::fit(view, inner.width, inner.height)),
        inner,
    );
    peek.height = 0;
    peek.area = Rect::default();
}

/// When a chat in its last day closes, and how to keep it (when `p` is the
/// dashboard's).
fn closing(app: &App, chat: &crate::chat::Chat) -> Option<String> {
    let closes = Closes::of(&chat.info, crate::task::now());
    if !closes.last_day() {
        return None;
    }
    let when = match closes {
        Closes::In(left) => format!("closes in {}", time_left(left)),
        _ => "closes when idle".into(),
    };
    Some(match app.key_taken('p') {
        None => format!("{when} · p keeps it"),
        Some(_) => when,
    })
}

/// The border of `area` fading through the hues, moving clockwise round
/// it as ticks go, its titles left as they are.
fn rainbow_border(frame: &mut Frame, area: Rect, tick: usize) {
    let (path, len) = border_path(area);
    let buf = frame.buffer_mut();
    for (x, y, pos) in path {
        let cell = &mut buf[(x, y)];
        if matches!(cell.symbol(), "─" | "│" | "╭" | "╮" | "╰" | "╯") {
            cell.set_fg(theme::rainbow_around(pos, len, tick));
        }
    }
}

/// The cells of `area`'s edge, clockwise from its top left corner, each
/// with how far along it is (a step down or up counts double: cells are
/// tall), and the length of the way round.
fn border_path(area: Rect) -> (Vec<(u16, u16, usize)>, usize) {
    if area.width < 2 || area.height < 2 {
        return (Vec::new(), 0);
    }
    let (left, top) = (area.left(), area.top());
    let (right, bottom) = (area.right() - 1, area.bottom() - 1);
    let mut path = Vec::new();
    let mut pos = 0;
    for x in left..right {
        path.push((x, top, pos));
        pos += 1;
    }
    for y in top..bottom {
        path.push((right, y, pos));
        pos += 2;
    }
    for x in (left + 1..=right).rev() {
        path.push((x, bottom, pos));
        pos += 1;
    }
    for y in (top + 1..=bottom).rev() {
        path.push((left, y, pos));
        pos += 2;
    }
    (path, pos)
}

/// The agent's screen, as the daemon's terminal emulator has it: its live
/// screen (the content's bottom when the area is shorter) or history, cut
/// at the area's right edge.
fn render_screen(frame: &mut Frame, area: Rect, peek: &mut Peek, typing: bool) {
    peek.height = area.height;
    // Set where a screen is drawn: nothing to select otherwise.
    peek.area = Rect::default();
    let message = match (&peek.agent, &peek.screen, &peek.error) {
        (None, _, _) => "(no agent running)".to_string(),
        (Some(_), Some(screen), _) => {
            let first = screen.first_row(peek.from, area.height);
            let view = ScreenView {
                screen: screen.screen(),
                first,
            };
            frame.render_widget(view, area);
            peek.area = area;
            peek.draw_selection(frame.buffer_mut());
            // The agent's cursor, where typing goes (on its live screen).
            if let Some((row, col)) = screen.cursor().filter(|_| typing && peek.from.is_none()) {
                if row >= first && row - first < area.height && col < area.width {
                    frame.set_cursor_position((area.x + col, area.y + row - first));
                }
            }
            return;
        }
        (Some(_), None, Some(error)) => format!("(error: {error})"),
        (Some(_), None, None) => String::new(),
    };
    frame.render_widget(Paragraph::new(message).fg(theme::get().muted), area);
}

/// The project picker; returns where its items are and the first one
/// shown (for the mouse).
fn render_picker_popup(frame: &mut Frame, picker: &mut PickerState) -> (Rect, usize) {
    let area = frame
        .area()
        .centered(Constraint::Percentage(60), Constraint::Percentage(60));
    frame.render_widget(Clear, area);

    let title = if picker.loading {
        format!(" {} · looking for projects… ", picker.title)
    } else {
        format!(" {} ", picker.title)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().accent))
        .title(title)
        .title_style(Style::new().bold().fg(theme::get().accent))
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [prompt, text] =
        Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(filter_area);
    frame.render_widget(
        Span::styled("> ", Style::new().fg(theme::get().accent)),
        prompt,
    );
    picker.filter.render(frame, text, Style::new(), true);

    // The highlighted item in view, the list moving only as far as needed.
    let filtered = picker.filtered();
    let height = list_area.height as usize;
    let mut offset = picker.offset.min(filtered.len().saturating_sub(height));
    if picker.selected < offset {
        offset = picker.selected;
    } else if height > 0 && picker.selected >= offset + height {
        offset = picker.selected + 1 - height;
    }
    let matched = Style::new().fg(theme::get().accent).bold();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, (item, positions))| {
            let style = if i == picker.selected {
                theme::get().selection
            } else {
                Style::new()
            };
            // Picked with tab: a mark, and the order they were picked in.
            let mark = match picker.marked.iter().position(|m| *m == item.id) {
                Some(n) => Span::styled(
                    format!("{} ", n + 1),
                    Style::new().fg(theme::get().accent).bold(),
                ),
                None if picker.marked.is_empty() => Span::raw(""),
                None => Span::raw("  "),
            };
            let mut line = fuzzy::highlight(&item.display, positions, Style::new(), matched);
            line.spans.insert(0, mark);
            ListItem::new(line).style(style)
        })
        .collect();
    frame.render_widget(List::new(items), list_area);
    picker.offset = offset;
    (list_area, offset)
}

/// What a new chat starts from: `+ new chat`, then the repository's
/// sessions, newest first: title, last prompt (dim), age. Returns where
/// its items are and the first one shown (for the mouse).
fn render_session_popup(frame: &mut Frame, sessions: &mut SessionPicker) -> (Rect, usize) {
    let t = theme::get();
    let area = frame
        .area()
        .centered(Constraint::Percentage(70), Constraint::Percentage(60));
    frame.render_widget(Clear, area);
    let SessionPicker {
        picker,
        sessions: list,
        ..
    } = sessions;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(t.popup())
        .border_style(Style::new().fg(t.accent))
        .title(format!(" {} ", picker.title))
        .title_style(Style::new().bold().fg(t.accent))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [prompt, text] =
        Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(filter_area);
    frame.render_widget(Span::styled("> ", Style::new().fg(t.accent)), prompt);
    picker.filter.render(frame, text, Style::new(), true);

    let filtered = picker.filtered();
    let height = list_area.height as usize;
    let mut offset = picker.offset.min(filtered.len().saturating_sub(height));
    if picker.selected < offset {
        offset = picker.selected;
    } else if height > 0 && picker.selected >= offset + height {
        offset = picker.selected + 1 - height;
    }
    let width = list_area.width as usize;
    let fit_cell = |line, width: usize| table::ellipsize(line, width as u16);
    // Titles in a column of their own (at most half the width), the age
    // at the right edge.
    const AGE: usize = 4;
    let title_width = list
        .iter()
        .filter_map(|s| s.title.as_deref())
        .map(|title| Span::raw(title).width())
        .max()
        .unwrap_or(0)
        .min(width / 2);
    let matched = Style::new().fg(t.accent).bold();
    let dim = Style::new().fg(t.muted);
    let now = crate::task::now();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, (item, positions))| {
            let style = if i == picker.selected {
                t.selection
            } else {
                Style::new()
            };
            let session = list
                .iter()
                .find(|s| s.id == item.id)
                .filter(|_| item.id != NEW_SESSION);
            let Some(session) = session else {
                return ListItem::new(fuzzy::highlight(
                    &item.display,
                    positions,
                    Style::new().fg(t.accent),
                    matched,
                ))
                .style(style);
            };
            // The display is the title, then the prompt: the matched
            // characters of each, highlighted where each is drawn.
            let title_chars = session
                .title
                .as_deref()
                .map_or(0, |title| title.chars().count());
            let gap = SessionPicker::TITLE_GAP.chars().count();
            let in_title: Vec<usize> = positions
                .iter()
                .copied()
                .filter(|&p| session.title.is_none() || p < title_chars)
                .collect();
            let in_prompt: Vec<usize> = positions
                .iter()
                .filter(|&&p| p >= title_chars + gap)
                .map(|p| p - title_chars - gap)
                .collect();
            let age = Span::styled(
                format!(
                    "{:>AGE$}",
                    triage::age(now.saturating_sub(session.modified_at))
                ),
                dim,
            );
            let mut line = match (&session.title, &session.last_prompt) {
                (Some(title), prompt) => {
                    let title = fuzzy::highlight(title, &in_title, Style::new(), matched);
                    let title = fit_cell(title, title_width);
                    let pad = title_width.saturating_sub(title.width());
                    let mut spans = title.spans;
                    spans.push(Span::raw(" ".repeat(pad + 2)));
                    let room = width.saturating_sub(title_width + 2 + 1 + AGE);
                    if let Some(prompt) = prompt {
                        let prompt =
                            fuzzy::highlight(&triage::one_line(prompt), &in_prompt, dim, matched);
                        spans.extend(fit_cell(prompt, room).spans);
                    }
                    Line::from(spans)
                }
                (None, _) => {
                    let text = fuzzy::highlight(&item.display, &in_title, Style::new(), matched);
                    fit_cell(text, width.saturating_sub(1 + AGE))
                }
            };
            let pad = width.saturating_sub(line.width() + AGE);
            line.spans.push(Span::raw(" ".repeat(pad)));
            line.spans.push(age);
            ListItem::new(line).style(style)
        })
        .collect();
    frame.render_widget(List::new(items), list_area);
    picker.offset = offset;
    (list_area, offset)
}

/// The repositories of `dirs`, as the popup names them.
fn repo_names(dirs: &[std::path::PathBuf]) -> String {
    let names: Vec<String> = dirs
        .iter()
        .map(|d| {
            d.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| d.display().to_string())
        })
        .collect();
    names.join(", ")
}

/// The task name input, with where its worktrees go: a new task, or more
/// members of the existing task of that name (`joins`).
fn render_new_task_popup(
    frame: &mut Frame,
    name: &TextInput,
    dirs: &[std::path::PathBuf],
    joins: bool,
) {
    // 2 content rows + 2 border + 2 vertical padding = 6
    let area = frame
        .area()
        .centered(Constraint::Length(60), Constraint::Length(6));
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().accent))
        .title(" New Task ")
        .title_style(Style::new().bold().fg(theme::get().accent))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [name_row, where_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    let label = "Task name: ";
    let [label_area, text] =
        Layout::horizontal([Constraint::Length(label.len() as u16), Constraint::Fill(1)])
            .areas(name_row);
    frame.render_widget(Span::raw(label), label_area);
    name.render(frame, text, Style::new().bold(), true);
    let repos = repo_names(dirs);
    let where_line = if joins {
        Line::from(vec![
            Span::styled("adds ", Style::new().fg(theme::get().warn)),
            Span::raw(repos),
            Span::styled(" to this existing task", Style::new().fg(theme::get().warn)),
        ])
    } else {
        Line::from(vec![Span::raw("in "), Span::raw(repos)]).fg(theme::get().muted)
    };
    frame.render_widget(where_line, where_row);
}

fn render_enter_path_popup(frame: &mut Frame, path: &TextInput) {
    // 1 content row + 2 border + 2 vertical padding = 5
    let area = frame
        .area()
        .centered(Constraint::Length(60), Constraint::Length(5));
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().accent))
        .title(" Enter Path ")
        .title_style(Style::new().bold().fg(theme::get().accent))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [prompt, text] =
        Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Span::styled("> ", Style::new().fg(theme::get().accent)),
        prompt,
    );
    path.render(frame, text, Style::new(), true);
}

/// Every key, by what it acts on, custom commands included: in two
/// columns when the terminal is wide enough, cut at its bottom.
fn render_help_popup(frame: &mut Frame, app: &App) {
    let entry = |key: &str, what: &str| (key.to_string(), what.to_string());
    let mut sections: Vec<(&str, Vec<(String, String)>)> = vec![
        ("Task, chat or shell", item_help(app)),
        (
            "Pane",
            vec![
                entry("h/l ←/→", "another member"),
                scroll_help(app),
                entry("home/end", "oldest line / live"),
            ],
        ),
        (
            "List",
            vec![
                entry("/", "filter (fuzzy)"),
                entry("gg/G", "first / last task"),
                entry("t", "by repo / attention"),
                entry("T", "next theme"),
                entry("click", "select (wheel: move)"),
            ],
        ),
        (
            "Typing",
            vec![
                entry("ctrl-←/→", "move by word"),
                entry("ctrl-w", "delete a word"),
                entry("ctrl-u/k", "delete to start/end"),
            ],
        ),
    ];
    if !app.commands.is_empty() {
        sections.push((
            "Custom commands",
            app.commands
                .iter()
                .map(|c| (c.key.clone(), c.name.clone()))
                .collect(),
        ));
    }
    sections.push(("Other", vec![entry("?", "this help"), entry("q", "quit")]));

    let keys = sections
        .iter()
        .flat_map(|(_, keys)| keys)
        .map(|(key, _)| Span::raw(key.as_str()).width())
        .max()
        .unwrap_or(0);
    let lines = |sections: &[(&str, Vec<(String, String)>)]| -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (title, entries) in sections {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                title.to_string(),
                Style::new().fg(theme::get().accent).bold(),
            ));
            for (key, what) in entries {
                lines.push(Line::from(vec![
                    Span::styled(format!("{key:<keys$}  "), Style::new().bold()),
                    Span::raw(what.clone()),
                ]));
            }
        }
        lines
    };
    let one = lines(&sections);
    let width_of = |lines: &[Line]| lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    // Two columns when one would not fit the height and two fit the width:
    // the first sections on the left, about half the rows.
    let screen = frame.area();
    let rows = |n: usize| n as u16 + 4;
    let split = (1..sections.len())
        .min_by_key(|&i| {
            let (l, r) = (lines(&sections[..i]).len(), lines(&sections[i..]).len());
            l.max(r)
        })
        .unwrap_or(sections.len());
    let (left, right) = (lines(&sections[..split]), lines(&sections[split..]));
    let two_wide = width_of(&left) + 4 + width_of(&right) + 6;
    let columns = if rows(one.len()) > screen.height && two_wide <= screen.width {
        vec![left, right]
    } else {
        vec![one]
    };
    let height = columns.iter().map(|c| rows(c.len())).max().unwrap_or(4);
    let width = match &columns[..] {
        [one] => width_of(one) + 6,
        _ => two_wide,
    };
    let area = screen.centered(
        Constraint::Length(width.min(screen.width)),
        Constraint::Length(height.min(screen.height)),
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().accent))
        .title(" Keys ")
        .title_style(Style::new().bold().fg(theme::get().accent))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    match &columns[..] {
        [one] => frame.render_widget(Paragraph::new(one.clone()), inner),
        [left, right] => {
            let [l, _, r] = Layout::horizontal([
                Constraint::Length(width_of(left)),
                Constraint::Length(4),
                Constraint::Fill(1),
            ])
            .areas(inner);
            frame.render_widget(Paragraph::new(left.clone()), l);
            frame.render_widget(Paragraph::new(right.clone()), r);
        }
        _ => {}
    }
}

/// The keys on a task or chat; one a custom command took says so.
/// `v`, unless a custom command took it (the custom commands' section
/// says which).
fn scroll_help(app: &App) -> (String, String) {
    match app.key_taken('v') {
        None => ("v".into(), "scroll (j/k, wheel)".into()),
        Some(_) => ("(v)".into(), "scroll: taken (wheel)".into()),
    }
}

fn item_help(app: &App) -> Vec<(String, String)> {
    let entry = |key: &str, what: &str| (key.to_string(), what.to_string());
    let yielding = |key: char, what: &str| match app.key_taken(key) {
        None => (key.to_string(), what.to_string()),
        // The custom commands' section says by which.
        Some(_) => (format!("({key})"), format!("{what}: taken")),
    };
    let mut keys = vec![
        entry("j/k ↑/↓", "select"),
        entry("enter", "type (ctrl-] back)"),
        entry("f", "full screen (⇧enter)"),
        entry("r/R", "run (R: new session)"),
        yielding('x', "stop / close a shell"),
        entry("a", "answer what it asks"),
        entry("d", "drop / close a chat or shell"),
        yielding('!', "a shell here"),
        yielding('p', "keep a chat, or not"),
        yielding('N', "name a chat"),
    ];
    match app.key_taken('c') {
        None => keys.push(entry("n/c/s", "new task / chat / shell")),
        Some(_) => keys.extend([
            entry("n", "new task"),
            entry("(c)", "new chat: taken"),
            entry("s", "new shell"),
        ]),
    }
    keys
}

/// Asks to close a chat: what closing does, and what it does not.
fn render_close_chat_popup(frame: &mut Frame, label: &str) {
    let red = |key: &'static str| Span::styled(key, Style::new().bold().fg(theme::get().bad));
    let lines = vec![
        Line::from(vec![
            Span::raw("Close chat "),
            Span::styled(label, Style::new().bold()),
            Span::raw("?"),
        ]),
        Line::raw("Its agent stops and tam forgets the chat. Nothing on"),
        Line::raw("disk changes, and Claude keeps the session: c makes the"),
        Line::raw("chat again from the repository's history."),
        Line::raw(""),
        Line::from(vec![
            red("y"),
            Span::raw(" close · "),
            Span::styled("n", Style::new().bold()),
            Span::raw(" cancel"),
        ]),
    ];
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (content_width as u16 + 8).max(30);
    let height = lines.len() as u16 + 4;
    let area = frame
        .area()
        .centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().bad))
        .title(" Close Chat ")
        .title_style(Style::new().bold().fg(theme::get().bad))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Asks to close a shell (`label`: `Shell #3 of <where it runs>`): what
/// closing does.
fn render_close_shell_popup(frame: &mut Frame, label: &str) {
    let red = |key: &'static str| Span::styled(key, Style::new().bold().fg(theme::get().bad));
    let lines = vec![
        Line::from(vec![
            Span::raw("Close "),
            Span::styled(label, Style::new().bold()),
            Span::raw("?"),
        ]),
        Line::raw("It hangs up: what runs in it stops, and its screen goes."),
        Line::raw("Nothing on disk changes."),
        Line::raw(""),
        Line::from(vec![
            red("y"),
            Span::raw(" close · "),
            Span::styled("n", Style::new().bold()),
            Span::raw(" cancel"),
        ]),
    ];
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (content_width as u16 + 8).max(30);
    let height = lines.len() as u16 + 4;
    let area = frame
        .area()
        .centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().bad))
        .title(" Close Shell ")
        .title_style(Style::new().bold().fg(theme::get().bad))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// What a drop of task `name` (or of `member` alone) closes of its shells,
/// when it has any.
fn drop_closes(app: &App, name: &str, member: Option<&MemberChoice>) -> Option<String> {
    let task = app.tasks.iter().find(|t| t.name == name)?;
    let count: usize = task
        .members
        .iter()
        .map(|m| app.member_shells(m.id()).len())
        .sum();
    let what = crate::shell::closes_with_it(count)?;
    Some(match member {
        None => format!("{what}."),
        Some(m) => match app.member_shells(&m.id).len() {
            0 => format!("{what}, none with {} alone.", m.label),
            n => format!("{what}, {} with {} alone.", crate::shell::count(n), m.label),
        },
    })
}

/// A chat's name to type; empty, it shows Claude's `title` again.
fn render_rename_chat_popup(frame: &mut Frame, name: &TextInput, title: Option<&str>) {
    // 2 content rows + 2 border + 2 vertical padding = 6
    let area = frame
        .area()
        .centered(Constraint::Length(60), Constraint::Length(6));
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().accent))
        .title(" Rename Chat ")
        .title_style(Style::new().bold().fg(theme::get().accent))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [name_row, hint_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    let label = "Name: ";
    let [label_area, text] =
        Layout::horizontal([Constraint::Length(label.len() as u16), Constraint::Fill(1)])
            .areas(name_row);
    frame.render_widget(Span::raw(label), label_area);
    name.render(frame, text, Style::new().bold(), true);
    let hint = match title {
        Some(title) => format!("empty: Claude's title, {title}"),
        None => "empty: Claude's title, once it has one".into(),
    };
    frame.render_widget(Line::raw(hint).fg(theme::get().muted), hint_row);
}

/// Asks to drop a task, or (with `member`, the focused member of a task
/// with several) that member alone; `shells` says which shells close.
fn render_confirm_drop_popup(
    frame: &mut Frame,
    name: &str,
    member: Option<&MemberChoice>,
    shells: Option<&str>,
) {
    let red = |key: &'static str| Span::styled(key, Style::new().bold().fg(theme::get().bad));
    let mut lines = match member {
        None => vec![Line::from(vec![
            Span::raw("Drop task "),
            Span::styled(name, Style::new().bold()),
            Span::raw("?"),
        ])],
        Some(m) => vec![
            Line::from(vec![
                Span::raw("Drop task "),
                Span::styled(name, Style::new().bold()),
                Span::raw(", or "),
                Span::styled(m.label.as_str(), Style::new().bold()),
                Span::raw(" only?"),
            ]),
            Line::from(vec![
                red("m"),
                Span::raw(format!(" drop {} only · ", m.label)),
                red("M"),
                Span::raw(" + delete its branch"),
            ]),
        ],
    };
    if let Some(shells) = shells {
        lines.insert(1, Line::raw(shells));
    }
    lines.push(Line::from(vec![
        red("y"),
        Span::raw(if member.is_some() {
            " drop task · "
        } else {
            " drop · "
        }),
        red("b"),
        Span::raw(" drop + delete branch · "),
        Span::styled("n", Style::new().bold()),
        Span::raw(" cancel"),
    ]));

    // Size to content: +8 for padding + border, +4 rows for border + padding
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (content_width as u16 + 8).max(30);
    let height = lines.len() as u16 + 4;
    let area = frame
        .area()
        .centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().bad))
        .title(" Confirm Drop ")
        .title_style(Style::new().bold().fg(theme::get().bad))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The daemon refused a drop (of a task, or of one `member`): show why
/// (what it would lose), ask to force.
fn render_force_drop_popup(frame: &mut Frame, name: &str, member: bool, report: &[String]) {
    let mut lines = vec![
        Line::from(vec![
            Span::raw(if member { "" } else { "Task " }),
            Span::styled(name, Style::new().bold()),
            Span::raw(" not dropped:"),
        ]),
        Line::raw(""),
    ];
    lines.extend(report.iter().map(|l| Line::raw(l.as_str())));
    lines.extend([
        Line::raw(""),
        Line::from(vec![
            Span::raw("Drop anyway? "),
            Span::styled("y", Style::new().bold().fg(theme::get().bad)),
            Span::raw("/"),
            Span::styled("n", Style::new().bold()),
        ]),
    ]);

    // Size to content (clipped to the screen): +8 / +4 for padding + border
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (content_width as u16).saturating_add(8).max(30);
    let height = (lines.len() as u16).saturating_add(4);
    let area = frame
        .area()
        .centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().bad))
        .title(" Drop Refused ")
        .title_style(Style::new().bold().fg(theme::get().bad))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A task with gone or unusable worktrees: what is wrong, and the choice
/// between recreating them (when possible) and dropping the task.
fn render_repair_or_drop_popup(
    frame: &mut Frame,
    name: &str,
    problems: &[String],
    repairable: bool,
    in_place: bool,
    member: Option<&MemberChoice>,
) {
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Task "),
            Span::styled(name, Style::new().bold()),
            Span::raw(":"),
        ]),
        Line::raw(""),
    ];
    lines.extend(problems.iter().map(|l| Line::raw(l.as_str())));
    lines.push(Line::raw(""));
    let mut keys = Vec::new();
    if repairable {
        keys.extend([
            Span::styled("r", Style::new().bold().fg(theme::get().ok)),
            Span::raw(" recreate · "),
        ]);
    }
    keys.extend([
        Span::styled("y", Style::new().bold().fg(theme::get().bad)),
        Span::raw(" drop · "),
    ]);
    if in_place {
        keys.extend([
            Span::styled("b", Style::new().bold().fg(theme::get().bad)),
            Span::raw(" drop + delete branch · "),
        ]);
    }
    if let Some(m) = member {
        keys.extend([
            Span::styled("m", Style::new().bold().fg(theme::get().bad)),
            Span::raw(format!(" drop {} only · ", m.label)),
        ]);
    }
    keys.extend([Span::styled("n", Style::new().bold()), Span::raw(" cancel")]);
    lines.push(Line::from(keys));

    // Size to content, wrapped at 100 columns (and the screen): +8 for
    // padding + border, + 4 rows for border + vertical padding.
    let longest = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (longest + 8).clamp(30, 100).min(frame.area().width);
    let inner_width = width.saturating_sub(8).max(1) as usize;
    let rows: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(inner_width).max(1))
        .sum();
    let area = frame.area().centered(
        Constraint::Length(width),
        Constraint::Length(rows as u16 + 4),
    );
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().warn))
        .title(" Worktree Gone ")
        .title_style(Style::new().bold().fg(theme::get().warn))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A message in a popup, wrapped to fit (at most 100 columns wide).
fn render_notice_popup(frame: &mut Frame, title: &str, lines: &[String]) {
    // +6 for border + horizontal padding
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let width = (longest + 6).clamp(30, 100).min(frame.area().width);
    let inner_width = width.saturating_sub(6).max(1) as usize;
    // Rows once wrapped (+1 slack for word wrapping), + border and padding
    let rows: usize = lines
        .iter()
        .map(|l| l.chars().count().div_ceil(inner_width).max(1))
        .sum();
    let area = frame.area().centered(
        Constraint::Length(width),
        Constraint::Length(rows as u16 + 5),
    );
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(theme::get().warn))
        .title(format!(" {title} "))
        .title_style(Style::new().bold().fg(theme::get().warn))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text: Vec<Line> = lines.iter().map(|l| Line::raw(l.as_str())).collect();
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

pub(super) fn status_display(status: &TaskStatus) -> (&'static str, Color) {
    (status.indicator(), theme::get().status(status))
}

/// How full the agent's context is: the percentage, and a gauge of
/// `CONTEXT_GAUGE` cells.
pub(super) fn context_display(pct: u8) -> Line<'static> {
    let t = theme::get();
    let color = if pct >= 90 {
        t.bad
    } else if pct >= 70 {
        t.warn
    } else {
        t.ok
    };
    let filled = (usize::from(pct.min(100)) * CONTEXT_GAUGE).div_ceil(100);
    Line::from(vec![
        Span::styled(format!("{pct:>3}% "), Style::new().fg(color)),
        Span::styled("━".repeat(filled), Style::new().fg(color)),
        Span::styled(
            "━".repeat(CONTEXT_GAUGE - filled),
            Style::new().fg(t.subtle),
        ),
    ])
}

/// Cells of the context gauge.
pub(super) const CONTEXT_GAUGE: usize = 4;

pub fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::answer::{AnswerDialog, Step};
    use tam_proto::{AgentError, Ask, AskKind, AskOption, AskQuestion, DeniedTool, Pending};

    fn text(state: AgentState, d: &AgentDetail) -> Option<String> {
        agent_detail_line(state, d).map(|(t, _)| t)
    }

    #[test]
    fn the_border_path_goes_clockwise_round_the_edge() {
        for (w, h) in [(2, 2), (3, 2), (2, 5), (82, 40)] {
            let area = Rect::new(5, 3, w, h);
            let (path, len) = border_path(area);
            assert_eq!(path.len(), 2 * usize::from(w + h) - 4, "{w}x{h}");
            assert_eq!(path[0], (5, 3, 0), "from the top left corner");
            // Every edge cell once, nothing inside.
            let cells: std::collections::HashSet<_> =
                path.iter().map(|&(x, y, _)| (x, y)).collect();
            assert_eq!(cells.len(), path.len());
            for &(x, y) in &cells {
                assert!(x == 5 || y == 3 || x + 1 == 5 + w || y + 1 == 3 + h);
            }
            // Cell to cell, and back to the start: a step across is one, a
            // step down or up two.
            let next = path.iter().skip(1).copied().chain([(5, 3, len)]);
            for (&(x, y, pos), (nx, ny, npos)) in path.iter().zip(next) {
                let step = match (i32::from(nx) - i32::from(x), i32::from(ny) - i32::from(y)) {
                    (1 | -1, 0) => 1,
                    (0, 1 | -1) => 2,
                    d => panic!("{w}x{h}: ({x}, {y}) to ({nx}, {ny}): {d:?}"),
                };
                assert_eq!(npos, pos + step, "{w}x{h}: ({x}, {y})");
            }
            // Clockwise: right along the top first.
            if w > 2 {
                assert_eq!((path[1].0, path[1].1), (6, 3));
            }
        }
        assert_eq!(border_path(Rect::new(0, 0, 1, 10)), (Vec::new(), 0));
    }

    #[test]
    fn detail_line_per_state() {
        let mut d = AgentDetail {
            pending: Some(Pending {
                tool: Some("Bash".into()),
                summary: "touch x".into(),
            }),
            error: Some(AgentError {
                kind: "rate_limit".into(),
                message: Some("API Error: Rate limit reached\nretry later".into()),
            }),
            denied: Some(DeniedTool {
                tool: "Bash".into(),
                summary: Some("rm -rf build".into()),
                reason: Some("destructive".into()),
            }),
            ..Default::default()
        };
        assert_eq!(
            text(AgentState::Blocked, &d).unwrap(),
            "permission · Bash: touch x"
        );
        assert_eq!(
            text(AgentState::Error, &d).unwrap(),
            "error · rate_limit: API Error: Rate limit reached"
        );
        assert_eq!(
            text(AgentState::Working, &d).unwrap(),
            "denied · Bash: rm -rf build (destructive)"
        );
        d.pending.as_mut().unwrap().summary = "Tea or coffee?".into();
        assert_eq!(
            text(AgentState::Question, &d).unwrap(),
            "question: Tea or coffee?"
        );
        assert!(text(AgentState::Ready, &AgentDetail::default()).is_none());
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn drop_dialogs_show_choices_and_lost_work() {
        let mut app = App::new();
        app.mode = Mode::ConfirmDropTask {
            name: "feat".into(),
            member: None,
        };
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("Drop task feat?"), "{s}");
        assert!(
            s.contains("y drop · b drop + delete branch · n cancel"),
            "{s}"
        );

        app.mode = Mode::ConfirmForceDrop {
            name: "feat".into(),
            target: "feat".into(),
            delete_branch: false,
            report: vec![
                "/wt/app--feat (branch feat)".into(),
                "  1 uncommitted change:".into(),
                "    ?? notes.txt".into(),
            ],
        };
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("Task feat not dropped:"), "{s}");
        assert!(s.contains("    ?? notes.txt"), "{s}");
        assert!(s.contains("Drop anyway? y/n"), "{s}");
    }

    #[test]
    fn repair_or_drop_dialog_says_what_is_wrong() {
        let mut app = App::new();
        app.mode = Mode::ConfirmRepairOrDrop {
            name: "feat".into(),
            lines: vec!["worktree /w/app--feat is gone, branch 'feat' is still there".into()],
            repairable: true,
            in_place: false,
            member: None,
        };
        let s = screen(&mut app, 100, 20);
        assert!(s.contains("Worktree Gone"), "{s}");
        assert!(s.contains("is gone, branch 'feat' is still there"), "{s}");
        assert!(s.contains("r recreate · y drop · n cancel"), "{s}");

        app.mode = Mode::ConfirmRepairOrDrop {
            name: "feat".into(),
            lines: vec!["worktree /w/app--feat is gone and cannot be recreated: \
                         branch 'feat' no longer exists"
                .into()],
            repairable: false,
            in_place: true,
            member: None,
        };
        let s = screen(&mut app, 60, 20);
        assert!(!s.contains("recreate ·"), "{s}");
        assert!(s.contains("y drop · b drop + delete branch"), "{s}");
        assert!(s.contains("no longer exists"), "wrapped, not cut: {s}");
    }

    #[test]
    fn failed_drop_notice_shows_the_whole_message() {
        // The daemon's error for a worktree git cannot remove.
        let message = "Task 'feat5' kept (its agent was stopped): git worktree remove --force \
                       /home/someone/.local/share/worktrees/repo--feat5 failed: fatal: cannot \
                       remove a locked working tree;\nuse 'remove -f -f' to override or unlock first";
        let mut app = App::new();
        app.mode = Mode::Notice {
            title: "Drop Failed".into(),
            lines: message.lines().map(str::to_string).collect(),
        };
        let s = screen(&mut app, 100, 24);
        assert!(s.contains("Drop Failed"), "{s}");
        assert!(
            s.contains("Task 'feat5' kept (its agent was stopped):"),
            "{s}"
        );
        assert!(s.contains("unlock first"), "{s}");
        // Narrow terminal: still all there, wrapped (down to the last word).
        let s = screen(&mut app, 50, 24);
        assert!(s.contains("Task 'feat5' kept"), "{s}");
        assert!(s.contains("│  first"), "{s}");
    }

    #[test]
    fn preview_pane_shows_the_agent_screen() {
        let mut app = App::new();
        app.peek = Peek::default();
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("(no agent running)"), "{s}");
        // Nothing to scroll.
        assert!(!s.contains("v:scroll"), "{s}");
        // Narrower: the footer keeps what matters most, help among them.
        let s = screen(&mut app, 36, 20);
        assert!(s.contains("?:help  q:quit"), "{s}");

        // Lines 0-9 in history, the live screen (5 rows) shows 10-12.
        let size = |n| std::num::NonZeroU16::new(n).unwrap();
        let mut vt = vt100::Parser::new(size(5), size(80), 0);
        vt.process(b"\x1b[?25lline 10\r\nline 11\r\nline 12");
        let mut info = tam_proto::ScreenInfo {
            cols: 80,
            rows: 5,
            contents: vt.screen().contents_formatted(),
            top: 10,
            oldest: 0,
            live: 10,
        };
        let mut peek = Peek {
            agent: Some("m-1".into()),
            ..Default::default()
        };
        peek.set_screen(&info);
        app.peek = peek;
        let s = screen(&mut app, 60, 20);
        assert!(s.contains("│line 12"), "{s}");
        assert!(!s.contains("history"), "{s}");

        // Scrolled back to line 4.
        let peek = &mut app.peek;
        peek.from = Some(4);
        info.top = 4;
        peek.set_screen(&info);
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("— · 6 lines into history"), "{s}");
        assert!(s.contains("end:live"), "{s}");
        // One line up.
        let peek = &mut app.peek;
        peek.from = Some(9);
        info.top = 9;
        peek.set_screen(&info);
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("— · 1 line into history"), "{s}");
    }

    #[test]
    fn scrolling_has_a_colour_of_its_own_in_each_theme() {
        // Lines 0-9 in history, the live screen (5 rows) shows 10-12.
        let size = |n| std::num::NonZeroU16::new(n).unwrap();
        let mut vt = vt100::Parser::new(size(5), size(80), 0);
        vt.process(b"\x1b[?25lline 10\r\nline 11\r\nline 12");
        let live_screen = tam_proto::ScreenInfo {
            cols: 80,
            rows: 5,
            contents: vt.screen().contents_formatted(),
            top: 10,
            oldest: 0,
            live: 10,
        };
        // From line 4.
        let history = tam_proto::ScreenInfo {
            top: 4,
            ..live_screen.clone()
        };
        let live = |app: &mut App| {
            app.peek.from = None;
            app.peek.set_screen(&live_screen);
        };
        let back = |app: &mut App| {
            app.peek.from = Some(4);
            app.peek.set_screen(&history);
        };
        let mut app = App::new();
        app.peek = Peek {
            agent: Some("m-1".into()),
            ..Default::default()
        };
        // The pane's top left corner, and the colours there: the corner's,
        // the title's first letter (after the corner and a space), and the
        // typing or scrolling chip's background when there is one.
        let colours = |app: &mut App| {
            let backend = ratatui::backend::TestBackend::new(80, 20);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| render(f, app)).unwrap();
            let buf = terminal.backend().buffer();
            let (x, y) = (0..20)
                .flat_map(|y| (0..80).map(move |x| (x, y)))
                .find(|&(x, y)| matches!(buf[(x, y)].symbol(), "╭" | "┏"))
                .expect("the pane");
            let row: String = (0..80).map(|x| buf[(x, y)].symbol()).collect();
            let chip = row
                .find(" typing ·")
                .or_else(|| row.find(" scrolling ·"))
                .map(|i| buf[(row[..i].chars().count() as u16, y)].bg);
            (row, buf[(x, y)].fg, buf[(x + 2, y)].fg, chip)
        };

        for theme in [&theme::ANSI, &theme::DARK, &theme::UNICORN] {
            let _theme = theme::in_theme(theme);
            let t = theme::get();
            assert_ne!(t.scrolled, t.accent);
            // ANSI has one yellow: `warn`'s.
            if !std::ptr::eq(theme, &theme::ANSI) {
                assert_ne!(t.scrolled, t.warn);
            }

            // Live: the title in the accent colour.
            app.mode = Mode::Normal;
            live(&mut app);
            let (row, _, title, chip) = colours(&mut app);
            assert!(row.contains("╭ — ─"), "{row}");
            assert_eq!(title, t.accent, "{row}");
            assert_eq!(chip, None);

            // Scrolling (`v`): its border, its chip and its title in a
            // colour of its own, at the live screen and scrolled back.
            app.mode = Mode::Scroll {
                agent: "m-1".into(),
            };
            let (row, border, title, chip) = colours(&mut app);
            assert!(row.contains("┏ — ━") && row.contains("scrolling"), "{row}");
            let scrolling = (border, title, chip);
            assert_eq!(scrolling, (t.scrolled, t.scrolled, Some(t.scrolled)));
            back(&mut app);
            let (row, border, title, chip) = colours(&mut app);
            assert!(row.contains("┏ — · 6 lines into history"), "{row}");
            let scrolling = (border, title, chip);
            assert_eq!(scrolling, (t.scrolled, t.scrolled, Some(t.scrolled)));

            // Scrolled back with the wheel outside `v`: the accent colour,
            // in the list's mode and typing.
            app.mode = Mode::Normal;
            let (row, _, title, _) = colours(&mut app);
            assert!(row.contains("╭ — · 6 lines into history"), "{row}");
            assert_eq!(title, t.accent, "{row}");
            app.mode = Mode::Terminal {
                agent: "m-1".into(),
            };
            let (row, border, title, chip) = colours(&mut app);
            assert!(row.contains("┏ — · 6 lines into history"), "{row}");
            assert_eq!((border, title, chip), (t.accent, t.accent, Some(t.accent)));

            // Typing at the live screen: unchanged.
            live(&mut app);
            let (row, border, title, chip) = colours(&mut app);
            assert!(row.contains("┏ — ━"), "{row}");
            assert_eq!((border, title, chip), (t.accent, t.accent, Some(t.accent)));
        }
    }

    // -- The triage pane ---------------------------------------------------

    use crate::chat::Chat;
    use crate::task::Task;
    use crate::tui::app::{Pick, TaskDetail};
    use crate::tui::branch::{BranchState, Observation};
    use crate::tui::input::TextInput;
    use tam_proto::{
        AgentInfo, ChatInfo, MemberHealth, MemberInfo, MessageInfo, RunInfo, ShellInfo, TaskInfo,
    };

    fn member_info(id: &str, name: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: name.into(),
            repo_root: None,
            branch: "feat".into(),
            dir: format!("/w/{name}--feat").into(),
            health: Some(MemberHealth::Ok {
                branch: Some("feat".into()),
            }),
        }
    }

    /// An agent for member `id`, in `state` for `secs` seconds.
    fn agent_in(id: &str, state: AgentState, secs: u64, detail: AgentDetail) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            provider: "claude".into(),
            dir: "/w".into(),
            state,
            pid: Some(1),
            uptime_secs: 600,
            viewers: 0,
            context_percent: None,
            task: Some("feat".into()),
            detail: Box::new(detail),
            state_since: Some(crate::task::now() - secs),
        }
    }

    /// A screen showing `rows` lines (cursor hidden).
    fn agent_screen(id: &str, rows: &[&str]) -> Peek {
        let size = |n| std::num::NonZeroU16::new(n).unwrap();
        let mut vt = vt100::Parser::new(size(24), size(80), 0);
        vt.process(format!("\x1b[?25l{}", rows.join("\r\n")).as_bytes());
        let mut peek = Peek {
            agent: Some(id.into()),
            ..Default::default()
        };
        peek.set_screen(&tam_proto::ScreenInfo {
            cols: 80,
            rows: 24,
            contents: vt.screen().contents_formatted(),
            top: 0,
            oldest: 0,
            live: 0,
        });
        peek
    }

    const PERMISSION_DIALOG: &[&str] = &[
        "> run the tests",
        "",
        "● Bash(cargo test --workspace)",
        "",
        " Bash command",
        "   cargo test --workspace -- --nocapture",
        " Do you want to proceed?",
        " > 1. Yes",
        "   2. No, and tell Claude what to do differently",
    ];

    /// The dashboard showing task `feat` (members `members`, agents
    /// `agents`), its pane open on the focused member's screen.
    fn triage_app(members: Vec<MemberInfo>, agents: Vec<AgentInfo>) -> App {
        let info = TaskInfo {
            name: "feat".into(),
            members,
            created_at: 0,
            run_count: 1,
            last_activity: Some(crate::task::now()),
        };
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &agents)]);
        app.sync_focus();
        let focused = app.peek_agent();
        app.peek = match focused {
            Some(id) => agent_screen(&id, PERMISSION_DIALOG),
            None => Peek::default(),
        };
        app
    }

    fn message(id: &str, from: Option<&str>, to: &str, text: &str, ago: u64) -> MessageInfo {
        MessageInfo {
            id: id.into(),
            from: from.map(str::to_string),
            from_label: from.map_or("user".into(), |f| format!("feat/{f}")),
            to: to.into(),
            to_label: format!("feat/{to}"),
            text: text.into(),
            hop: 0,
            reply_to: None,
            sent_at: crate::task::now() - ago,
            delivered_at: Some(crate::task::now() - ago),
        }
    }

    fn observed(state: BranchState) -> Observation {
        Observation {
            state: Some(Ok(state)),
            at: Some(std::time::Instant::now()),
            running: false,
        }
    }

    fn blocked() -> AgentDetail {
        AgentDetail {
            pending: Some(Pending {
                tool: Some("Bash".into()),
                summary: "cargo test --workspace -- --nocapture".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_pane_is_the_screen_of_the_agent() {
        let mut app = triage_app(
            vec![member_info("m-1", "app")],
            vec![agent_in("m-1", AgentState::Blocked, 190, blocked())],
        );
        app.detail = Some(TaskDetail {
            messages: vec![message("msg-1", None, "m-1", "please also run clippy", 600)],
            ..TaskDetail::of(Pick::Task("feat".into()))
        });
        let s = screen(&mut app, 100, 34);
        assert!(s.contains("╭ feat ─"), "{s}");
        // Its screen from the pane's first row, nothing said twice: the
        // request it waits on is the dialog on its screen.
        let title = s.lines().position(|l| l.contains("╭ feat ─")).unwrap();
        assert_eq!(
            s.lines()
                .nth(title + 1)
                .unwrap()
                .trim_end_matches([' ', '│']),
            "│> run the tests",
            "{s}"
        );
        assert!(s.contains("│ > 1. Yes"), "{s}");
        assert!(!s.contains("blocked for"), "{s}");
        assert!(!s.contains("permission · Bash"), "{s}");
        assert!(!s.contains("please also run clippy"), "{s}");
        assert!(!s.contains("waiting for it"), "{s}");
        assert!(!s.contains("tab:"), "{s}");

        // A message it did not get yet: said in the bottom border.
        let mut queued = message("msg-2", None, "m-1", "and fmt", 5);
        queued.delivered_at = None;
        app.detail.as_mut().unwrap().messages.push(queued);
        let s = screen(&mut app, 100, 34);
        assert!(s.contains("─ 1 message waiting for it ╯"), "{s}");
    }

    /// Blocked on `ask`, which the dashboard can answer.
    fn asking(ask: Ask) -> AgentDetail {
        AgentDetail {
            pending: Some(Pending {
                tool: Some(ask.tool.clone()),
                summary: "(summary)".into(),
            }),
            ask: Some(ask),
            ..Default::default()
        }
    }

    fn bash(input: &str) -> Ask {
        Ask {
            id: "ask-1".into(),
            tool: "Bash".into(),
            kind: AskKind::Permission {
                input: input.into(),
                cut: false,
            },
        }
    }

    fn colors_and_size() -> Ask {
        let option = |label: &str, description: Option<&str>| AskOption {
            label: label.into(),
            description: description.map(str::to_string),
        };
        Ask {
            id: "ask-2".into(),
            tool: "AskUserQuestion".into(),
            kind: AskKind::Questions {
                questions: vec![
                    AskQuestion {
                        question: "Which colors?".into(),
                        header: Some("Colors".into()),
                        options: vec![option("Red", Some("warm")), option("Blue", None)],
                        multi_select: true,
                    },
                    AskQuestion {
                        question: "Which size?".into(),
                        header: Some("Size".into()),
                        options: vec![option("Small", Some("Small")), option("Large", None)],
                        multi_select: false,
                    },
                ],
            },
        }
    }

    /// The dashboard with the answer dialog open on `ask`, as drawn.
    fn answering(state: AgentState, ask: Ask) -> App {
        let mut app = triage_app(
            vec![member_info("m-1", "app")],
            vec![agent_in("m-1", state, 30, asking(ask.clone()))],
        );
        app.mode = Mode::Answer(AnswerDialog::new("m-1".into(), "feat".into(), ask));
        app
    }

    fn dialog(app: &mut App) -> &mut AnswerDialog {
        match &mut app.mode {
            Mode::Answer(dialog) => dialog,
            _ => panic!("no answer dialog"),
        }
    }

    #[test]
    fn the_answer_dialog_shows_the_request_and_when_it_is_too_late() {
        let input = "command: rm -rf build\ndescription: Clean";
        let mut app = answering(AgentState::Blocked, bash(input));
        let s = screen(&mut app, 100, 30);
        show("answer: permission", &s);
        assert!(s.contains("╭ Answer feat ─"), "{s}");
        assert!(s.contains("│  Allow Bash to run with this input?"), "{s}");
        assert!(s.contains("│  command: rm -rf build"), "{s}");
        assert!(s.contains("│  description: Clean"), "{s}");
        assert!(s.contains("│  y allow once · n say no · esc cancel"), "{s}");
        assert!(s.contains(" y:allow once  n:say no"), "{s}");
        assert!(dialog(&mut app).shown);

        // Saying no: the reason being typed, at the cursor.
        dialog(&mut app).step = Step::Reason(TextInput::from("use make clean"));
        let (s, cursor) = screen_and_cursor(&mut app, 100, 30);
        show("answer: reason", &s);
        assert!(
            s.contains("│  Why not? Optional: the agent reads it"),
            "{s}"
        );
        assert!(s.contains("│  > use make clean"), "{s}");
        assert!(s.contains(" enter:send the no  esc:back"), "{s}");
        let (x, y) = cursor.expect("the cursor where one types");
        let row = s.lines().nth(y as usize).unwrap();
        let typed = "> use make clean";
        let start = row.find(typed).expect("the cursor on the reason's row");
        assert_eq!(x as usize, row[..start].chars().count() + typed.len());

        // The request is answered in the terminal meanwhile: nothing will
        // be sent.
        let mut info = app
            .find_member("m-1")
            .unwrap()
            .1
            .agent_info
            .clone()
            .unwrap();
        info.detail.ask = None;
        app.update_agent(info);
        let s = screen(&mut app, 100, 30);
        show("answer: too late", &s);
        assert!(
            s.contains("│  Answered or changed meanwhile: nothing will be sent."),
            "{s}"
        );
        assert!(s.contains(" any:close"), "{s}");
    }

    #[test]
    fn a_request_too_long_to_show_can_only_be_said_no_to() {
        let ask = Ask {
            kind: AskKind::Permission {
                input: "content: …".into(),
                cut: true,
            },
            ..bash("")
        };
        let mut app = answering(AgentState::Blocked, ask);
        let s = screen(&mut app, 100, 30);
        show("answer: cut", &s);
        assert!(
            s.contains("│  (cut here: its terminal shows the rest)"),
            "{s}"
        );
        assert!(
            s.contains("│  Too long to show in full: allow it in its terminal (enter attaches)."),
            "{s}"
        );
        assert!(s.contains("│  n say no · esc cancel"), "{s}");
        assert!(!s.contains("allow once"), "{s}");
    }

    #[test]
    fn a_long_plan_scrolls_in_the_dialog() {
        let plan: String = (1..=60).map(|i| format!("{i}. step {i}\n")).collect();
        let ask = Ask {
            id: "ask-3".into(),
            tool: "ExitPlanMode".into(),
            kind: AskKind::Plan { plan, cut: false },
        };
        let mut app = answering(AgentState::Blocked, ask);
        let s = screen(&mut app, 100, 30);
        show("answer: plan", &s);
        assert!(s.contains("│  Approve this plan?"), "{s}");
        assert!(s.contains("leaves plan mode; edits keep asking"), "{s}");
        assert!(s.contains("│  1. step 1 "), "{s}");
        assert!(!s.contains("step 60"), "{s}");
        assert!(s.contains("│  y approve · n say no · esc cancel"), "{s}");
        assert!(s.contains(" rows 1–21 of 60 "), "{s}");
        // To the end: the last step, and the plan is cut.
        dialog(&mut app).scroll = usize::MAX;
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("│  60. step 60"), "{s}");
        assert!(s.contains(" rows 40–60 of 60 "), "{s}");
    }

    #[test]
    fn the_question_dialog_ticks_options_and_reviews_the_answers() {
        let mut app = answering(AgentState::Question, colors_and_size());
        let s = screen(&mut app, 100, 30);
        show("answer: questions", &s);
        assert!(s.contains("│  Colors │ Size │ Send"), "{s}");
        assert!(s.contains("│  Which colors?"), "{s}");
        assert!(s.contains("│  Several answers can be ticked."), "{s}");
        assert!(s.contains("│  ❯ 1. [ ] Red"), "{s}");
        assert!(s.contains("│           warm"), "{s}");
        assert!(s.contains("│    2. [ ] Blue"), "{s}");
        assert!(s.contains("│    3. Type an answer"), "{s}");
        assert!(
            s.contains("space tick · enter done · ←/→ question · esc cancel"),
            "{s}"
        );

        let Step::Questions(q) = &mut dialog(&mut app).step else {
            panic!()
        };
        q.answers[0].chosen = vec!["Red".into()];
        q.answers[0].other = Some("Teal".into());
        q.answers[1].chosen = vec!["Large".into()];
        q.current = 2;
        let s = screen(&mut app, 100, 30);
        show("answer: review", &s);
        assert!(s.contains("│  ✓ Colors │ ✓ Size │ Send"), "{s}");
        assert!(s.contains("│  Send these answers?"), "{s}");
        assert!(s.contains("│    → Red, Teal"), "{s}");
        assert!(s.contains("│    → Large"), "{s}");
    }

    #[test]
    fn the_pane_keeps_what_the_screen_may_no_longer_show() {
        let error = AgentDetail {
            error: Some(AgentError {
                kind: "rate_limit".into(),
                message: Some("API Error: Rate limit reached".into()),
            }),
            ..Default::default()
        };
        let mut app = triage_app(
            vec![member_info("m-1", "app")],
            vec![agent_in("m-1", AgentState::Error, 7200, error)],
        );
        let s = screen(&mut app, 100, 30);
        assert!(
            s.contains("╰ error · rate_limit: API Error: Rate limit reached ─"),
            "{s}"
        );

        let denied = AgentDetail {
            denied: Some(DeniedTool {
                tool: "Bash".into(),
                summary: Some("rm -rf build".into()),
                reason: Some("destructive".into()),
            }),
            ..Default::default()
        };
        let mut app = triage_app(
            vec![member_info("m-1", "app")],
            vec![agent_in("m-1", AgentState::Working, 20, denied)],
        );
        let s = screen(&mut app, 100, 30);
        assert!(
            s.contains("╰ denied · Bash: rm -rf build (destructive) ─"),
            "{s}"
        );
    }

    #[test]
    fn without_an_agent_the_pane_says_how_its_last_run_ended() {
        let mut app = triage_app(vec![member_info("m-1", "app")], vec![]);
        app.detail = Some(TaskDetail {
            of: Pick::Task("feat".into()),
            runs: vec![RunInfo {
                member: "m-1".into(),
                provider: "claude".into(),
                session_id: None,
                started_at: crate::task::now() - 7300,
                ended_at: Some(crate::task::now() - 3700),
                exit_code: Some(1),
            }],
            messages: vec![{
                let mut m = message("msg-1", None, "m-1", "rebase on main", 60);
                m.delivered_at = None;
                m
            }],
            stale: false,
        });
        let s = screen(&mut app, 100, 30);
        assert!(
            s.contains("○ idle · no agent · claude ended 1h ago, exit 1"),
            "{s}"
        );
        assert!(s.contains("1m ← user: (queued) rebase on main"), "{s}");
        // No screen to show.
        assert!(!s.contains("no agent running"), "{s}");
        assert!(s.contains("r:run"), "{s}");

        // Its worktree gone: what is wrong, and what to do.
        let mut gone = member_info("m-1", "app");
        gone.health = Some(MemberHealth::Gone {
            cannot_repair: None,
        });
        let mut app = triage_app(vec![gone], vec![]);
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("∅ gone · no agent"), "{s}");
        assert!(
            s.contains("worktree /w/app--feat is gone, branch 'feat' is still there"),
            "{s}"
        );
        assert!(
            s.contains("d: recreate the worktree, or drop the task"),
            "{s}"
        );
        assert!(!s.contains("│branch feat"), "{s}");
    }

    #[test]
    fn the_pane_of_two_members_shows_the_one_that_needs_the_user() {
        let mut app = triage_app(
            vec![member_info("m-1", "app"), member_info("m-2", "web")],
            vec![
                agent_in("m-1", AgentState::Working, 30, AgentDetail::default()),
                agent_in("m-2", AgentState::Blocked, 190, blocked()),
            ],
        );
        let s = screen(&mut app, 100, 34);
        // The blocked one focused: named in the title, marked in the list.
        assert!(s.contains("╭ feat/web ─"), "{s}");
        assert!(s.contains("└▸web"), "{s}");
        assert!(s.contains("h/l:member"), "{s}");

        // → shows the other one (and enter would attach to it).
        app.cycle_member(1);
        app.sync_focus();
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));
        app.peek.agent = Some("m-1".into());
        let s = screen(&mut app, 100, 34);
        assert!(s.contains("╭ feat/app ─"), "{s}");
        assert!(s.contains("├▸app"), "{s}");
    }

    #[test]
    fn new_task_popup_says_where_the_worktrees_go() {
        let info: TaskInfo = serde_json::from_value(serde_json::json!({
            "name": "feat",
            "members": [{
                "id": "m-1", "name": "api", "repo": "api",
                "branch": "feat", "dir": "/w/api--feat",
            }],
            "created_at": 0,
        }))
        .unwrap();
        let mut app = App::new();
        app.set_tasks(vec![Task::from_info(info, &[])]);
        let dirs = vec![
            std::path::PathBuf::from("/src/web"),
            std::path::PathBuf::from("/src/docs"),
        ];
        app.mode = Mode::NewTaskEnterName {
            project_dirs: dirs.clone(),
            name: "other".into(),
        };
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("Task name: other"), "{s}");
        assert!(s.contains("in web, docs"), "{s}");

        // An existing task's name: they join it.
        app.mode = Mode::NewTaskEnterName {
            project_dirs: dirs,
            name: "feat".into(),
        };
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("adds web, docs to this existing task"), "{s}");
    }

    #[test]
    fn picked_projects_show_in_the_order_picked() {
        use super::super::app::PickerItem;
        let item = |name: &str| PickerItem {
            display: name.into(),
            id: format!("/src/{name}"),
        };
        let mut picker =
            PickerState::new("Pick project", vec![item("api"), item("web"), item("docs")]);
        picker.marked = vec!["/src/web".into(), "/src/api".into()];
        let mut app = App::new();
        app.mode = Mode::PickProject(picker, Purpose::Task);
        let s = screen(&mut app, 80, 24);
        if std::env::var_os("TAM_SHOW_SCREEN").is_some() {
            println!("{s}");
        }
        assert!(s.contains("│ 2 api"), "{s}");
        assert!(s.contains("│ 1 web"), "{s}");
        assert!(s.contains("│   docs"), "{s}");
        assert!(s.contains("tab:pick several"), "{s}");
    }

    #[test]
    fn member_drop_dialogs_name_the_member() {
        let mut app = App::new();
        app.mode = Mode::ConfirmDropTask {
            name: "feat".into(),
            member: Some(MemberChoice {
                id: "m-2".into(),
                label: "feat/web".into(),
            }),
        };
        let s = screen(&mut app, 100, 20);
        if std::env::var_os("TAM_SHOW_SCREEN").is_some() {
            println!("{s}");
        }
        assert!(s.contains("Drop task feat, or feat/web only?"), "{s}");
        assert!(
            s.contains("m drop feat/web only · M + delete its branch"),
            "{s}"
        );
        assert!(
            s.contains("y drop task · b drop + delete branch · n cancel"),
            "{s}"
        );
        assert!(s.contains("m/M:member only"), "{s}");

        app.mode = Mode::ConfirmForceDrop {
            name: "feat/web".into(),
            target: "m-2".into(),
            delete_branch: false,
            report: vec!["    ?? notes.txt".into()],
        };
        let s = screen(&mut app, 100, 20);
        assert!(s.contains("│  feat/web not dropped:"), "{s}");
    }

    // -- The task list -----------------------------------------------------

    /// Print a screen when `TAM_SHOW_SCREEN` is set (to look at a layout).
    /// A drawn buffer as text with ANSI colours, to `cat` in a terminal.
    fn ansi(buf: &ratatui::buffer::Buffer) -> String {
        fn color(c: Color, fg: bool) -> String {
            let base = if fg { 30 } else { 40 };
            let named = |n: u8| (base + n).to_string();
            let bright = |n: u8| (base + 60 + n).to_string();
            match c {
                Color::Reset => (base + 9).to_string(),
                Color::Black => named(0),
                Color::Red => named(1),
                Color::Green => named(2),
                Color::Yellow => named(3),
                Color::Blue => named(4),
                Color::Magenta => named(5),
                Color::Cyan => named(6),
                Color::Gray => named(7),
                Color::DarkGray => bright(0),
                Color::LightRed => bright(1),
                Color::LightGreen => bright(2),
                Color::LightYellow => bright(3),
                Color::LightBlue => bright(4),
                Color::LightMagenta => bright(5),
                Color::LightCyan => bright(6),
                Color::White => bright(7),
                Color::Indexed(i) => format!("{};5;{i}", base + 8),
                Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
            }
        }
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                let mut codes = vec!["0".to_string(), color(cell.fg, true), color(cell.bg, false)];
                for (modifier, code) in [
                    (Modifier::BOLD, "1"),
                    (Modifier::DIM, "2"),
                    (Modifier::ITALIC, "3"),
                    (Modifier::UNDERLINED, "4"),
                    (Modifier::REVERSED, "7"),
                ] {
                    if cell.modifier.contains(modifier) {
                        codes.push(code.to_string());
                    }
                }
                out.push_str(&format!("\x1b[{}m{}", codes.join(";"), cell.symbol()));
            }
            out.push_str("\x1b[0m\n");
        }
        out
    }

    /// Not a test: writes the fixture dashboard in colour, at several
    /// sizes and in both themes, to `$TAM_UI_DUMP` (a directory).
    /// `TAM_UI_DUMP=/tmp/ui cargo test -p tam-cli dump_the_dashboard -- --ignored`
    #[test]
    #[ignore]
    fn dump_the_dashboard_in_colour() {
        let dir = std::path::PathBuf::from(std::env::var("TAM_UI_DUMP").unwrap());
        theme::set(
            Some(&std::env::var("TAM_UI_THEME").unwrap_or("dark".into())),
            true,
        );
        let draw = |app: &mut App, w: u16, h: u16, name: &str| {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, app)).unwrap();
            let text = ansi(terminal.backend().buffer());
            std::fs::write(dir.join(format!("{name}-{w}x{h}.ansi")), text).unwrap();
        };
        for (w, h) in [(100, 30), (160, 40), (200, 50)] {
            let mut app = chat_dashboard();
            app.spin = Some(2);
            select(&mut app, "unit-pricing");
            let mut app = with_pane(app);
            draw(&mut app, w, h, "dashboard");
            app.mode = Mode::Help;
            draw(&mut app, w, h, "help");
            app.mode = Mode::ConfirmDropTask {
                name: "unit-pricing".into(),
                member: None,
            };
            draw(&mut app, w, h, "drop");
            app.mode = Mode::Normal;
            app.toggle_order();
            draw(&mut app, w, h, "attention");
            app.toggle_order();
            // A chat in its last day, without an agent.
            select_chat(&mut app, "c-0000dddd");
            let mut app = with_pane(app);
            draw(&mut app, w, h, "chat");
            app.mode = Mode::ConfirmCloseChat {
                id: "c-0000dddd".into(),
                label: "Chat design".into(),
            };
            draw(&mut app, w, h, "close");
        }
    }

    #[test]
    fn the_header_chips_keep_their_colours_next_to_the_order() {
        let mut app = dashboard();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let t = theme::get();
        // ` tam ` and ` 3 need you `: dark text on their colours.
        assert_eq!((buf[(1, 0)].fg, buf[(1, 0)].bg), (t.on_accent, t.accent));
        let need = (0..80).find(|&x| buf[(x, 0)].symbol() == "3").unwrap();
        assert_eq!(
            (buf[(need, 0)].fg, buf[(need, 0)].bg),
            (t.on_accent, t.warn)
        );
        assert_eq!(buf[(78, 0)].fg, t.muted, "by repository");
    }

    fn show(label: &str, s: &str) {
        if std::env::var_os("TAM_SHOW_SCREEN").is_some() {
            println!("--- {label}\n{s}");
        }
    }

    /// A member `name` (repository `repo`) of a task on branch `branch`.
    fn member_in(id: &str, name: &str, repo: &str, branch: &str) -> MemberInfo {
        MemberInfo {
            id: id.into(),
            name: name.into(),
            repo: repo.into(),
            repo_root: None,
            branch: branch.into(),
            dir: format!("/w/{repo}--{branch}").into(),
            health: Some(MemberHealth::Ok {
                branch: Some(branch.into()),
            }),
        }
    }

    fn task_of(name: &str, members: Vec<MemberInfo>, idle_for: u64) -> TaskInfo {
        TaskInfo {
            name: name.into(),
            members,
            created_at: 0,
            run_count: 1,
            last_activity: Some(crate::task::now() - idle_for),
        }
    }

    fn branch_of(ahead: u32, behind: u32, uncommitted: usize) -> Observation {
        observed(BranchState {
            base: Some("origin/main".into()),
            ahead,
            behind,
            uncommitted,
            ..Default::default()
        })
    }

    /// A day's dashboard: tasks in three repositories, one in two of them.
    fn dashboard() -> App {
        let question = AgentDetail {
            pending: Some(Pending {
                tool: Some("AskUserQuestion".into()),
                summary: "Keep the old endpoint for a release?".into(),
            }),
            ..Default::default()
        };
        let finished = AgentDetail {
            last_message: Some("Done: rounding now happens once, at the end.\n\nMore.".into()),
            ..Default::default()
        };
        let mut gone = member_in("m-7", "frontend", "frontend", "old-banner");
        gone.health = Some(MemberHealth::Gone {
            cannot_repair: None,
        });
        let tasks = vec![
            task_of(
                "fix-allotment-rounding",
                vec![member_in(
                    "m-1",
                    "backend",
                    "backend",
                    "fix-allotment-rounding",
                )],
                600,
            ),
            task_of(
                "perf",
                vec![member_in("m-2", "backend", "backend", "perf")],
                60,
            ),
            task_of(
                "spike-cache",
                vec![member_in("m-3", "backend", "backend", "spike-cache")],
                45 * 86400,
            ),
            task_of(
                "unit-pricing",
                vec![
                    member_in("m-4", "backend", "backend", "unit-pricing"),
                    member_in("m-5", "frontend", "frontend", "unit-pricing"),
                ],
                30,
            ),
            task_of(
                "offload-explain",
                vec![member_in(
                    "m-6",
                    "rustpallet",
                    "rustpallet",
                    "offload-explain",
                )],
                900,
            ),
            task_of("old-banner", vec![gone], 3 * 86400),
            task_of(
                "nav",
                vec![member_in("m-8", "frontend", "frontend", "nav")],
                2 * 86400,
            ),
        ];
        let agents = vec![
            agent_in("m-1", AgentState::Blocked, 190, blocked()),
            agent_in("m-2", AgentState::Working, 20, AgentDetail::default()),
            agent_in("m-4", AgentState::Working, 75, AgentDetail::default()),
            agent_in("m-5", AgentState::Question, 300, question),
            agent_in("m-6", AgentState::Ready, 750, finished),
        ];
        let mut agents = agents;
        agents[0].context_percent = Some(67);
        agents[2].context_percent = Some(12);
        let mut app = App::new();
        app.set_tasks(
            tasks
                .into_iter()
                .map(|t| Task::from_info(t, &agents))
                .collect(),
        );
        for (id, (ahead, behind, dirty)) in [
            ("m-1", (2, 0, 1)),
            ("m-2", (0, 0, 0)),
            ("m-3", (5, 12, 0)),
            ("m-4", (1, 0, 3)),
            ("m-6", (3, 1, 0)),
        ] {
            app.branches
                .insert(id.into(), branch_of(ahead, behind, dirty));
        }
        app
    }

    /// The dashboard with its pane open on the selected task.
    fn with_pane(mut app: App) -> App {
        app.sync_focus();
        app.peek = match app.peek_agent() {
            Some(id) => agent_screen(&id, PERMISSION_DIALOG),
            None => Peek::default(),
        };
        app
    }

    fn select(app: &mut App, task: &str) {
        app.selected = app
            .visible_tasks()
            .iter()
            .position(|t| t.name == task)
            .unwrap();
    }

    #[test]
    fn v_scrolls_the_pane_and_says_how_to_leave() {
        let mut app = dashboard();
        select(&mut app, "unit-pricing");
        let mut app = with_pane(app);
        let agent = app.peek_agent().expect("an agent");
        let s = screen(&mut app, 160, 40);
        let footer = s.lines().last().unwrap();
        assert!(footer.contains("v:scroll"), "{footer}");

        app.mode = Mode::Scroll { agent };
        let s = screen(&mut app, 160, 40);
        show("scrolling", &s);
        // The pane has the keyboard: a thick border, and how to leave.
        assert!(
            s.contains("scrolling · j/k · ctrl-] back to the list"),
            "{s}"
        );
        assert!(s.contains('┏'), "{s}");
        let footer = s.lines().last().unwrap();
        for hint in [
            "j/k:line down/up",
            "g/G:oldest/live",
            "enter:type",
            "ctrl-]:back to the list",
        ] {
            assert!(footer.contains(hint), "{hint}: {footer}");
        }

        // A custom command took `v`: its key, in the help too.
        app.mode = Mode::Normal;
        app.commands = vec![crate::config::CustomCommand {
            name: "view".into(),
            key: "v".into(),
            command: "true".into(),
        }];
        let s = screen(&mut app, 160, 40);
        let footer = s.lines().last().unwrap();
        assert!(
            !footer.contains("v:scroll") && footer.contains("v:view"),
            "{footer}"
        );
        app.mode = Mode::Help;
        let s = screen(&mut app, 100, 30);
        show("help, v taken", &s);
        assert!(s.contains("(v)       scroll: taken (wheel)"), "{s}");
    }

    #[test]
    fn the_list_groups_tasks_by_repository_with_member_rows() {
        let mut app = dashboard();
        let s = screen(&mut app, 80, 44);
        show("80x44", &s);
        // A header per repository, the task in two under both.
        for group in ["backend", "backend + frontend", "frontend", "rustpallet"] {
            // The repositories, then a rule.
            let named = |l: &str| l.trim_end_matches([' ', '─']) == format!(" {group}");
            assert!(s.lines().any(named), "{group}: {s}");
        }
        assert!(!s.contains("DIR"), "{s}");
        // The task name is not cut at 15 columns; time in state and branch.
        assert!(
            s.contains(
                "▲ block      3m fix-allotment-rounding                         ↑2 *1   67% ━━━━"
            ),
            "{s}"
        );
        assert!(s.contains("◌ stale     45d spike-cache"), "{s}");
        assert!(s.contains("↑5 ↓12"), "{s}");
        assert!(s.contains("○ idle       2d nav"), "{s}");
        // A task in two repositories: its row has no status of its own,
        // a row per member has.
        assert!(s.lines().any(|l| l.trim() == "unit-pricing"), "{s}");
        assert!(
            s.contains(
                "● run        1m  ├ backend                                     ↑1 *3   12% ━━━━"
            ),
            "{s}"
        );
        assert!(s.contains("? question   5m  └ frontend"), "{s}");

        let s = screen(&mut app, 120, 44);
        show("120x44", &s);
        // A wide list (above the pane): what each member waits on.
        assert!(s.contains("permission · Bash: cargo test"), "{s}");
        assert!(s.contains("question: Keep the old endpoint"), "{s}");
        assert!(s.contains("Done: rounding now happens once"), "{s}");
        assert!(
            s.contains("worktree /w/frontend--old-banner is gone"),
            "{s}"
        );
    }

    #[test]
    fn attention_first_puts_who_needs_the_user_on_top() {
        let mut app = dashboard();
        select(&mut app, "nav");
        app.toggle_order();
        assert_eq!(app.selected_task().unwrap().name, "nav", "selection kept");
        let s = screen(&mut app, 80, 24);
        show("attention 80x24", &s);
        assert!(s.contains("attention first"), "{s}");
        let rows: Vec<&str> = s.lines().skip(2).collect();
        assert!(rows[0].trim_start().starts_with("▲ block"), "{s}");
        assert!(rows[1].trim_start().starts_with("unit-pricing "), "{s}");
        assert!(rows[3].trim_start().starts_with("? question"), "{s}");
        assert!(s.contains("REPO"), "{s}");
        assert!(s.contains("backend + frontend"), "{s}");
    }

    #[test]
    fn a_wide_terminal_puts_the_pane_beside_the_list() {
        let mut app = dashboard();
        select(&mut app, "unit-pricing");
        let mut app = with_pane(app);
        let s = screen(&mut app, 200, 40);
        show("200x40 pane", &s);
        // The list on the left, the pane on the right from the top.
        let first = s.lines().nth(1).unwrap();
        assert!(first.trim_start().starts_with("STATUS"), "{s}");
        assert!(first.contains("╭ unit-pricing/frontend"), "{s}");
        // The member the keys act on is marked.
        assert!(s.contains("└▸frontend"), "{s}");

        // Narrower: the pane below a list as tall as its rows allow.
        let s = screen(&mut app, 120, 40);
        show("120x40 pane", &s);
        assert!(
            s.lines().nth(1).unwrap().trim_start().starts_with("STATUS"),
            "{s}"
        );
        assert!(!s.lines().nth(1).unwrap().contains('╭'), "{s}");
        let s = screen(&mut app, 80, 24);
        show("80x24 pane", &s);
        // The selected task's members stay in view.
        assert!(s.contains("├ backend"), "{s}");
        assert!(s.contains("└▸frontend"), "{s}");
    }

    #[test]
    fn narrow_lists_drop_columns_before_cutting_names() {
        let mut app = dashboard();
        let s = screen(&mut app, 50, 20);
        show("50x20", &s);
        // The context column goes first.
        assert!(!s.contains("CTX"), "{s}");
        assert!(s.contains("fix-allotment-rounding"), "{s}");
        let s = screen(&mut app, 30, 20);
        show("30x20", &s);
        assert!(s.contains("fix-allotment-rou…"), "{s}");
        assert!(s.contains(" tam  7 tasks  3 need you "), "{s}");

        // Just wide enough for the pane beside the list, and just not.
        let mut app = with_pane(dashboard());
        let s = screen(&mut app, 136, 30);
        show("136x30 pane", &s);
        assert!(s.lines().nth(1).unwrap().contains('╭'), "{s}");
        let s = screen(&mut app, 135, 30);
        show("135x30 pane", &s);
        assert!(!s.lines().nth(1).unwrap().contains('╭'), "{s}");
    }

    #[test]
    fn a_long_list_scrolls_to_show_the_selected_task_and_its_members() {
        let mut tasks: Vec<TaskInfo> = (0..12)
            .map(|i| {
                let name = format!("task-{i:02}");
                task_of(
                    &name,
                    vec![member_in(&format!("m-{i}"), "api", "api", &name)],
                    60,
                )
            })
            .collect();
        tasks.push(task_of(
            "zz-both",
            vec![
                member_in("m-a", "api", "api", "zz-both"),
                member_in("m-w", "web", "web", "zz-both"),
            ],
            60,
        ));
        let mut app = App::new();
        app.set_tasks(tasks.into_iter().map(|t| Task::from_info(t, &[])).collect());
        // 18 rows: header, footer, the pane, and for the list (half of
        // the rest) the column names and 7 rows.
        let s = screen(&mut app, 80, 18);
        assert!(
            s.lines().nth(2).unwrap().trim_start().starts_with("api"),
            "{s}"
        );
        select(&mut app, "zz-both");
        let s = screen(&mut app, 80, 18);
        show("scrolled", &s);
        // The task in both repositories, its header and both members.
        assert!(s.contains("api + web"), "{s}");
        // The member keys would act on is marked.
        assert!(s.contains("├▸api"), "{s}");
        assert!(s.contains("└ web"), "{s}");
        // Back up one: the list moves only as far as it must.
        app.select_prev();
        let s = screen(&mut app, 80, 18);
        assert!(s.contains("task-11"), "{s}");
        assert!(s.contains("└ web"), "kept in view: {s}");
        app.selected = 0;
        let s = screen(&mut app, 80, 18);
        assert!(
            s.lines().nth(2).unwrap().trim_start().starts_with("api"),
            "{s}"
        );
    }

    // -- Chats --------------------------------------------------------------

    fn chat_info(id: &str, name: Option<&str>, title: Option<&str>, repo: &str) -> ChatInfo {
        ChatInfo {
            id: id.into(),
            name: name.map(str::to_string),
            repo: repo.into(),
            dir: format!("/src/{repo}").into(),
            created_at: crate::task::now() - 5 * 86400,
            keep: false,
            session: Some(format!("s-{id}")),
            last_turn_at: Some(crate::task::now() - 3600),
            closes_at: Some(crate::task::now() + 2 * 86400),
            run_count: 1,
            title: title.map(str::to_string),
            last_prompt: None,
        }
    }

    /// The day's dashboard with four chats: one ready, one working, one
    /// kept and asking, one without an agent in its last day.
    fn chat_dashboard() -> App {
        let mut app = dashboard();
        let release = chat_info(
            "c-0000aaaa",
            None,
            Some("Release deployment on staging"),
            "backend",
        );
        let rounding = chat_info(
            "c-0000bbbb",
            None,
            Some("Why rounding differs between runs"),
            "backend",
        );
        let mut support = chat_info(
            "c-0000cccc",
            Some("Answering the support thread"),
            Some("Support thread"),
            "backend",
        );
        support.keep = true;
        support.closes_at = None;
        let mut design = chat_info("c-0000dddd", None, Some("Chat design"), "tam");
        design.last_turn_at = Some(crate::task::now() - 2 * 86400 - 19 * 3600);
        design.closes_at = Some(crate::task::now() + 5 * 3600 - 30);
        design.last_prompt = Some("can we put the time left on the border instead?".into());
        let question = AgentDetail {
            pending: Some(Pending {
                tool: Some("AskUserQuestion".into()),
                summary: "Reply in English or French?".into(),
            }),
            ..Default::default()
        };
        let mut agents = vec![
            agent_in("c-0000aaaa", AgentState::Ready, 120, AgentDetail::default()),
            agent_in(
                "c-0000bbbb",
                AgentState::Working,
                10,
                AgentDetail::default(),
            ),
            agent_in("c-0000cccc", AgentState::Question, 300, question),
        ];
        agents[0].context_percent = Some(41);
        agents[1].context_percent = Some(12);
        agents[2].context_percent = Some(63);
        app.set_chats(
            [release, rounding, support, design]
                .into_iter()
                .map(|info| Chat::new(info, &agents))
                .collect(),
        );
        app
    }

    fn select_chat(app: &mut App, id: &str) {
        app.selected = app
            .visible_entries()
            .iter()
            .position(|e| e.pick() == Pick::Chat(id.into()))
            .unwrap();
        app.sync_focus();
    }

    /// The colours of the cells of `text` on the screen (its first
    /// occurrence).
    fn cells_of(app: &mut App, width: u16, height: u16, text: &str) -> Vec<(String, Color)> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..height {
            let row: Vec<&str> = (0..width).map(|x| buf[(x, y)].symbol()).collect();
            let line: String = row.concat();
            if let Some(at) = line.find(text) {
                let x = line[..at].chars().count() as u16;
                let n = text.chars().count() as u16;
                return (x..x + n)
                    .map(|x| (buf[(x, y)].symbol().to_string(), buf[(x, y)].fg))
                    .collect();
            }
        }
        panic!("{text} not on screen");
    }

    #[test]
    fn chats_come_first_in_a_section_of_their_own() {
        let mut app = chat_dashboard();
        app.selected = 0;
        let s = screen(&mut app, 80, 44);
        show("chats 80x44", &s);
        assert!(s.contains(" tam  4 chats  7 tasks  5 need you "), "{s}");
        let rows: Vec<&str> = s.lines().skip(2).collect();
        assert_eq!(rows[0].trim_end_matches([' ', '─']), " chats", "{s}");
        // In creation order, their repository where a task has its branch.
        assert!(
            rows[1].starts_with("▌◆ ready      2m Release deployment on staging    "),
            "{s}"
        );
        assert!(rows[1].ends_with(" backend  41% ━━━━"), "{s}");
        assert!(
            rows[2].contains("10s Why rounding differs between runs"),
            "{s}"
        );
        assert!(rows[3].contains("5m Answering the support thread ∞"), "{s}");
        // No agent: since its last turn.
        assert!(rows[4].starts_with(" ○ idle       2d Chat design ◔"), "{s}");
        assert!(rows[4].contains(" tam"), "{s}");
        assert_eq!(rows[5].trim_end_matches([' ', '─']), " backend", "{s}");

        // In its last day: the name dimmed, the mark in the warning
        // colour; kept: the mark dim.
        let t = theme::get();
        let design = cells_of(&mut app, 80, 44, "Chat design ◔");
        assert!(
            design[..11].iter().all(|(_, fg)| *fg == t.muted),
            "{design:?}"
        );
        assert_eq!(design[12], ("◔".into(), t.warn));
        let support = cells_of(&mut app, 80, 44, "thread ∞");
        assert_eq!(support[0].1, Color::Reset);
        assert_eq!(support[7], ("∞".into(), t.muted));

        // Chats alone: counted as such.
        let mut alone = App::new();
        alone.set_chats(app.chats.clone());
        let s = screen(&mut alone, 80, 24);
        assert!(s.contains(" tam  4 chats  2 need you "), "{s}");

        // Neither named nor titled yet: its id, dimmed.
        app.upsert_chat(chat_info("c-0000eeee", None, None, "backend"));
        let new = cells_of(&mut app, 80, 44, "c-0000eeee");
        assert!(new.iter().all(|(_, fg)| *fg == t.muted), "{new:?}");

        // Wide, the list above the pane: what a chat waits on, what one
        // without an agent was last asked.
        let s = screen(&mut app, 120, 44);
        show("chats 120x44", &s);
        assert!(s.contains("question: Reply in English or French?"), "{s}");
        assert!(s.contains("can we put the time left on the border"), "{s}");
    }

    #[test]
    fn attention_first_mixes_chats_and_tasks() {
        let mut app = chat_dashboard();
        select_chat(&mut app, "c-0000dddd");
        app.toggle_order();
        assert_eq!(
            app.selected_chat().map(|c| c.info.id.as_str()),
            Some("c-0000dddd"),
            "selection kept"
        );
        let s = screen(&mut app, 100, 30);
        show("chats attention 100x30", &s);
        assert!(!s.contains(" chats ─"), "{s}");
        let rows: Vec<&str> = s.lines().skip(2).collect();
        let starts = |i: usize, text: &str| rows[i].trim_start().starts_with(text);
        // What waits on the user (a question or a permission alike), by
        // name: the chat, the blocked task, the task with a question.
        assert!(
            starts(0, "? question   5m Answering the support thread ∞"),
            "{s}"
        );
        assert!(starts(1, "▲ block"), "{s}");
        assert!(starts(2, "unit-pricing"), "{s}");
        // A chat's repository in REPO, `chat` where a task has its branch.
        let chat_row = rows[0];
        let repo = chat_row.find("backend").unwrap();
        assert!(chat_row[repo..].contains(" chat "), "{s}");
        assert!(s.contains("Release deployment on staging"), "{s}");
    }

    #[test]
    fn a_cut_chat_name_keeps_its_mark() {
        let mut app = chat_dashboard();
        let mut design = app.chats[3].info.clone();
        design.title = Some("Chat design for the dashboard's chats section".into());
        app.upsert_chat(design);
        let s = screen(&mut app, 50, 20);
        show("chats 50x20", &s);
        let row = s.lines().find(|l| l.contains("Chat design")).unwrap();
        assert!(row.contains("… ◔"), "{s}");
        assert!(s.contains("Answering the support … ∞"), "{s}");
    }

    #[test]
    fn the_filter_finds_chats_by_name_title_and_repository() {
        let mut app = chat_dashboard();
        app.filter_active = true;
        app.filter = TextInput::from("rounding");
        let s = screen(&mut app, 80, 24);
        show("filter rounding", &s);
        assert!(s.contains("Why rounding differs between runs"), "{s}");
        assert!(s.contains("fix-allotment-rounding"), "{s}");
        assert!(!s.contains("Release deployment"), "{s}");
        // A chat's repository.
        app.filter = TextInput::from("tam");
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("Chat design"), "{s}");
        assert!(!s.contains("Why rounding"), "{s}");
        assert!(!s.contains("backend ─"), "{s}");
        // Its title, when it has a name too.
        app.filter = TextInput::from("Support thread");
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("Answering the support thread"), "{s}");
        app.filter = TextInput::from("zzz");
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("Nothing matches."), "{s}");
    }

    #[test]
    fn a_chat_s_pane_is_its_screen_and_says_when_it_closes() {
        let mut app = chat_dashboard();
        select_chat(&mut app, "c-0000aaaa");
        let mut app = with_pane(app);
        let s = screen(&mut app, 100, 34);
        show("chat pane", &s);
        assert!(s.contains("╭ Release deployment on staging ─"), "{s}");
        assert!(s.contains("> run the tests"), "{s}");
        assert!(!s.contains("closes in"), "{s}");

        // Without an agent: what enter does, what it was last asked; in
        // its last day, when it closes on the border.
        select_chat(&mut app, "c-0000dddd");
        let mut app = with_pane(app);
        let s = screen(&mut app, 100, 34);
        show("idle chat pane", &s);
        assert!(s.contains("╭ Chat design ─"), "{s}");
        assert!(s.contains("○ idle · no agent"), "{s}");
        assert!(s.contains("enter resumes its session"), "{s}");
        assert!(
            s.contains("last prompt: can we put the time left on the border instead?"),
            "{s}"
        );
        assert!(s.contains("in /src/tam"), "{s}");
        assert!(s.contains(" closes in 5h · p keeps it ╯"), "{s}");
        // Its agent starting: said so.
        app.starting = Some("c-0000dddd".into());
        let s = screen(&mut app, 100, 34);
        assert!(s.contains("starting · its agent is on its way"), "{s}");
        assert!(!s.contains("enter resumes"), "{s}");
        app.starting = None;
        // Due: closed as soon as it is idle.
        let mut design = app.chats[3].info.clone();
        design.closes_at = Some(crate::task::now() - 60);
        app.upsert_chat(design);
        let s = screen(&mut app, 100, 34);
        assert!(s.contains(" closes when idle · p keeps it ╯"), "{s}");
        // `p` taken by a custom command: not offered.
        app.commands = vec![crate::config::CustomCommand {
            name: "preview".into(),
            key: "p".into(),
            command: "true".into(),
        }];
        let s = screen(&mut app, 100, 34);
        assert!(s.contains(" closes when idle ╯"), "{s}");
        assert!(!s.contains(":keep"), "{s}");
    }

    #[test]
    fn the_footer_and_help_have_a_chat_s_keys() {
        let mut app = chat_dashboard();
        select_chat(&mut app, "c-0000dddd");
        let s = screen(&mut app, 160, 30);
        let footer = s.lines().last().unwrap();
        for hint in ["enter:resume", "d:close", "p:keep", "N:rename"] {
            assert!(footer.contains(hint), "{hint}: {footer}");
        }
        assert!(!footer.contains("r:run"), "{footer}");
        select_chat(&mut app, "c-0000cccc");
        let s = screen(&mut app, 160, 30);
        let footer = s.lines().last().unwrap();
        for hint in ["enter:type", "x:stop", "p:unkeep"] {
            assert!(footer.contains(hint), "{hint}: {footer}");
        }
        // A key a custom command took: its command's, in the help too
        // (wide enough for every hint).
        app.commands = vec![crate::config::CustomCommand {
            name: "notes".into(),
            key: "N".into(),
            command: "true".into(),
        }];
        let s = screen(&mut app, 180, 30);
        let footer = s.lines().last().unwrap();
        assert!(
            !footer.contains("N:rename") && footer.contains("N:notes"),
            "{s}"
        );
        app.mode = Mode::Help;
        let s = screen(&mut app, 100, 30);
        show("help, N taken", &s);
        assert!(s.contains("(N)       name a chat: taken"), "{s}");
        assert!(s.contains("N         notes"), "{s}");
    }

    #[test]
    fn the_sessions_a_new_chat_can_start_from() {
        let session = |id: &str, title: Option<&str>, prompt: Option<&str>, ago: u64| {
            tam_proto::SessionInfo {
                id: id.into(),
                title: title.map(str::to_string),
                last_prompt: prompt.map(str::to_string),
                modified_at: crate::task::now() - ago,
            }
        };
        let mut app = chat_dashboard();
        app.mode = Mode::PickSession(SessionPicker::new(
            "/src/backend".into(),
            vec![
                session(
                    "s-1",
                    Some("Release deployment on staging"),
                    Some("deploy 26.8.4 on staging-qr"),
                    2 * 3600,
                ),
                session(
                    "s-2",
                    Some("Rounding in allotments"),
                    Some("ok, now write the test"),
                    3 * 86400,
                ),
                session("s-3", None, Some("why is build 1234 slow"), 9 * 86400),
            ],
        ));
        let (s, cursor) = screen_and_cursor(&mut app, 100, 30);
        show("sessions", &s);
        assert!(s.contains("New chat in backend"), "{s}");
        assert!(cursor.is_some(), "the filter's cursor");
        let at = |text: &str| s.lines().position(|l| l.contains(text)).unwrap();
        // A new chat first, then the sessions as the daemon listed them.
        assert!(at("│ + new chat") < at("deploy 26.8.4"), "{s}");
        assert!(at("deploy 26.8.4") < at("write the test"), "{s}");
        assert!(at("write the test") < at("build 1234"), "{s}");
        // Title, last prompt, age.
        let release = s.lines().nth(at("deploy 26.8.4")).unwrap();
        assert!(
            release.contains("Release deployment on staging  deploy 26.8.4 on staging-qr"),
            "{s}"
        );
        assert!(release.contains("on staging-qr      2h │"), "{s}");
        // Without a title: its last prompt in the title's place.
        let slow = s.lines().find(|l| l.contains("why is build 1234")).unwrap();
        assert!(
            slow.contains("│ why is build 1234 slow ") && slow.contains("9d │"),
            "{s}"
        );
        assert!(s.contains("enter:start the chat"), "{s}");
        // Filtered: the matched letters of the prompt highlighted.
        if let Mode::PickSession(sessions) = &mut app.mode {
            sessions.picker.filter = TextInput::from("qr");
        }
        let qr = cells_of(&mut app, 100, 30, "staging-qr");
        assert_eq!(qr[8].1, theme::get().accent, "{qr:?}");
        let s = screen(&mut app, 100, 30);
        assert!(!s.contains("+ new chat") && !s.contains("Rounding"), "{s}");
        // Narrow: cut, not garbled.
        let s = screen(&mut app, 40, 12);
        show("sessions 40x12", &s);
    }

    #[test]
    fn the_chat_dialogs() {
        let mut app = chat_dashboard();
        app.mode = Mode::ConfirmCloseChat {
            id: "c-0000dddd".into(),
            label: "Chat design".into(),
        };
        let s = screen(&mut app, 100, 30);
        show("close chat", &s);
        assert!(s.contains("Close chat Chat design?"), "{s}");
        assert!(s.contains("Claude keeps the session"), "{s}");
        assert!(s.contains("y/enter:close"), "{s}");
        app.mode = Mode::RenameChat {
            id: "c-0000dddd".into(),
            label: "Chat design".into(),
            title: Some("Chat design".into()),
            name: TextInput::from("dashboard"),
        };
        let (s, cursor) = screen_and_cursor(&mut app, 100, 30);
        show("rename chat", &s);
        assert!(s.contains("Name: dashboard"), "{s}");
        assert!(s.contains("empty: Claude's title, Chat design"), "{s}");
        assert!(cursor.is_some());
    }

    // -- Shells -------------------------------------------------------------

    /// A shell in the worktree of `member` (of `app`).
    fn worktree_shell(app: &App, id: &str, member: &str, number: u32) -> ShellInfo {
        let (_, member) = app.find_member(member).unwrap();
        ShellInfo {
            id: id.into(),
            member: Some(member.id().into()),
            dir: member.info.dir.clone(),
            repo: member.info.repo.clone(),
            number,
            pid: 1,
            opened_at: crate::task::now() - 600,
            viewers: 0,
        }
    }

    /// A shell on the main checkout of `repo`.
    fn checkout_shell(id: &str, repo: &str, number: u32) -> ShellInfo {
        ShellInfo {
            id: id.into(),
            member: None,
            dir: format!("/src/{repo}").into(),
            repo: repo.into(),
            number,
            pid: 1,
            opened_at: crate::task::now() - 600,
            viewers: 0,
        }
    }

    /// The chats' dashboard with shells: #1 and #3 in the worktree of
    /// fix-allotment-rounding, #1 below each member of unit-pricing, and
    /// three on main checkouts, in the order they were opened.
    fn shell_dashboard() -> App {
        let mut app = chat_dashboard();
        let shells = vec![
            checkout_shell("sh-0000000a", "backend", 1),
            worktree_shell(&app, "sh-00000011", "m-1", 1),
            checkout_shell("sh-0000000b", "frontend", 1),
            worktree_shell(&app, "sh-00000013", "m-1", 3),
            worktree_shell(&app, "sh-00000041", "m-4", 1),
            checkout_shell("sh-0000000c", "backend", 2),
            worktree_shell(&app, "sh-00000051", "m-5", 1),
        ];
        app.set_shells(shells);
        app
    }

    fn select_shell(app: &mut App, id: &str) {
        app.selected = app
            .visible_entries()
            .iter()
            .position(|e| e.pick() == Pick::Shell(id.into()))
            .unwrap();
        app.sync_focus();
    }

    #[test]
    fn shells_hang_below_their_task_and_have_a_section() {
        let mut app = shell_dashboard();
        app.selected = 0;
        let s = screen(&mut app, 80, 50);
        show("shells 80x50", &s);
        // Never counted, never in "need you".
        assert!(s.contains(" tam  4 chats  7 tasks  5 need you "), "{s}");
        let rows: Vec<&str> = s.lines().skip(2).collect();
        let at = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap();
        // After the chats, the shells on main checkouts: their name and
        // repository, no status, time or context; in the order opened.
        assert_eq!(rows[5].trim_end_matches([' ', '─']), " shells", "{s}");
        let section: Vec<(&str, &str)> = rows[6..9]
            .iter()
            .map(|r| {
                let (name, repo) = r.trim().split_once("  ").unwrap();
                (name, repo.trim())
            })
            .collect();
        assert_eq!(
            section,
            [
                ("Shell #1", "backend"),
                ("Shell #1", "frontend"),
                ("Shell #2", "backend")
            ],
            "{s}"
        );
        assert!(rows[6].starts_with(&" ".repeat(17)), "{s}");
        assert_eq!(rows[9].trim_end_matches([' ', '─']), " backend", "{s}");
        // On a branch below a task with one member, by number.
        let task = at("fix-allotment-rounding");
        assert_eq!(
            rows[task + 1].trim_end(),
            format!("{}├ Shell #1", " ".repeat(18))
        );
        assert_eq!(
            rows[task + 2].trim_end(),
            format!("{}└ Shell #3", " ".repeat(18))
        );
        assert!(rows[task + 3].contains("perf"), "{s}");
        // Below their member in a task with several, the task's branch
        // going on past a member that is not the last.
        let task = at("unit-pricing");
        assert!(rows[task + 1].contains(" ├ backend"), "{s}");
        assert_eq!(
            rows[task + 2].trim_end(),
            format!("{}│ └ Shell #1", " ".repeat(18))
        );
        assert!(rows[task + 3].contains(" └ frontend"), "{s}");
        assert_eq!(
            rows[task + 4].trim_end(),
            format!("{}  └ Shell #1", " ".repeat(18))
        );
        // The branches in the rules' colour, a section's repository dim.
        let t = theme::get();
        let branch = cells_of(&mut app, 80, 50, "│ └ Shell #1");
        assert_eq!(branch[0].1, t.subtle);
        assert_eq!(branch[2].1, t.subtle);
        assert_eq!(branch[4].1, Color::Reset);
        let repo = cells_of(&mut app, 80, 50, "frontend  ");
        assert_eq!(repo[0].1, t.muted, "{repo:?}");

        // Wide: no note on a shell's row.
        let s = screen(&mut app, 120, 50);
        let row = s.lines().find(|l| l.contains("└ Shell #3")).unwrap();
        assert_eq!(row.trim(), "└ Shell #3", "{s}");
    }

    #[test]
    fn attention_first_keeps_shells_below_their_task_and_the_others_last() {
        let mut app = shell_dashboard();
        app.toggle_order();
        let s = screen(&mut app, 100, 50);
        show("shells attention 100x50", &s);
        assert!(!s.contains(" shells ─"), "{s}");
        let rows: Vec<&str> = s
            .lines()
            .skip(2)
            .take_while(|l| !l.contains('╭'))
            .map(|l| l.trim_start_matches('▌'))
            .collect();
        // Wherever its task sorts.
        assert!(rows[1].trim_start().starts_with("▲ block"), "{s}");
        assert!(rows[2].trim_start().starts_with("├ Shell #1"), "{s}");
        assert!(rows[3].trim_start().starts_with("└ Shell #3"), "{s}");
        assert!(rows[4].trim_start().starts_with("unit-pricing"), "{s}");
        assert!(rows[6].trim_start().starts_with("│ └ Shell #1"), "{s}");
        assert!(rows[8].trim_start().starts_with("└ Shell #1"), "{s}");
        // Those on main checkouts last, in the order opened: their
        // repository in REPO, `shell` where a task has its branch.
        let last: Vec<String> = rows[rows.len() - 3..]
            .iter()
            .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(
            last,
            [
                "Shell #1 backend shell",
                "Shell #1 frontend shell",
                "Shell #2 backend shell"
            ],
            "{s}"
        );
        assert!(rows[rows.len() - 4].contains("old-banner"), "{s}");
    }

    #[test]
    fn the_filter_shows_a_shell_with_its_task_or_by_its_repository() {
        let mut app = shell_dashboard();
        app.filter_active = true;
        app.filter = TextInput::from("rounding");
        let s = screen(&mut app, 80, 40);
        show("shells filter rounding", &s);
        // With its task.
        assert!(s.contains("├ Shell #1") && s.contains("└ Shell #3"), "{s}");
        assert!(!s.contains(" shells ─"), "{s}");
        // A repository: the shells on its main checkout, and the tasks in
        // it with theirs.
        app.filter = TextInput::from("frontend");
        let s = screen(&mut app, 80, 40);
        show("shells filter frontend", &s);
        let section: Vec<&str> = s
            .lines()
            .skip_while(|l| !l.contains(" shells ─"))
            .skip(1)
            .take_while(|l| !l.contains('─'))
            .collect();
        assert_eq!(section.len(), 1, "{s}");
        assert!(
            section[0].contains("Shell #1") && section[0].contains("frontend"),
            "{s}"
        );
        assert!(s.contains("  └ Shell #1"), "{s}");
        assert!(!s.contains("Shell #3"), "{s}");
    }

    #[test]
    fn a_shell_s_pane_is_its_screen_named_after_where_it_runs() {
        let mut app = shell_dashboard();
        for (id, title) in [
            ("sh-00000013", "╭ Shell #3 · fix-allotment-rounding ─"),
            ("sh-00000041", "╭ Shell #1 · unit-pricing/backend ─"),
            ("sh-0000000b", "╭ Shell #1 · frontend ─"),
        ] {
            select_shell(&mut app, id);
            app.peek = agent_screen(id, &["$ cargo test", "running 12 tests"]);
            assert_eq!(app.peek_agent().as_deref(), Some(id));
            let s = screen(&mut app, 100, 40);
            show(id, &s);
            assert!(s.contains(title), "{s}");
            assert!(s.contains("$ cargo test"), "{s}");
            // No marked member: the keys act on the shell.
            assert!(!s.contains('▸'), "{s}");
        }
        // Typed into: the keys go to the shell.
        app.mode = Mode::Terminal {
            agent: "sh-0000000b".into(),
        };
        let s = screen(&mut app, 120, 40);
        let footer = s.lines().last().unwrap();
        assert!(
            footer.contains("every other key goes to the shell"),
            "{footer}"
        );
        assert!(s.contains("typing · ctrl-] back to the list"), "{s}");
    }

    #[test]
    fn the_shell_dialogs_and_what_a_drop_closes() {
        let mut app = shell_dashboard();
        app.mode = Mode::ConfirmCloseShell {
            id: "sh-00000013".into(),
            label: "Shell #3 of fix-allotment-rounding".into(),
        };
        let s = screen(&mut app, 100, 30);
        show("close shell", &s);
        assert!(
            s.contains("Close Shell #3 of fix-allotment-rounding?"),
            "{s}"
        );
        assert!(s.contains("Nothing on disk changes."), "{s}");
        assert!(s.contains("y/enter:close"), "{s}");

        // A drop says how many shells it closes.
        let drop = |app: &mut App, name: &str, member: Option<(&str, &str)>| {
            app.mode = Mode::ConfirmDropTask {
                name: name.into(),
                member: member.map(|(id, label)| MemberChoice {
                    id: id.into(),
                    label: label.into(),
                }),
            };
            screen(app, 100, 30)
        };
        let s = drop(&mut app, "fix-allotment-rounding", None);
        show("drop with shells", &s);
        assert!(s.contains("Its 2 shells close with it."), "{s}");
        let s = drop(
            &mut app,
            "unit-pricing",
            Some(("m-5", "unit-pricing/frontend")),
        );
        show("drop a member with shells", &s);
        assert!(
            s.contains("Its 2 shells close with it, 1 shell with unit-pricing/frontend alone."),
            "{s}"
        );
        let s = drop(&mut app, "perf", None);
        assert!(!s.contains("close with it"), "{s}");
        // A gone worktree's shell too.
        let shell = worktree_shell(&app, "sh-00000071", "m-7", 1);
        app.upsert_shell(shell);
        app.mode = Mode::ConfirmRepairOrDrop {
            name: "old-banner".into(),
            lines: vec!["worktree /w/frontend--old-banner is gone".into()],
            repairable: true,
            in_place: false,
            member: None,
        };
        let s = screen(&mut app, 100, 30);
        show("gone with a shell", &s);
        assert!(s.contains("Its shell closes with it."), "{s}");
    }

    #[test]
    fn the_footer_and_help_have_a_shell_s_keys() {
        let mut app = shell_dashboard();
        select_shell(&mut app, "sh-00000013");
        let s = screen(&mut app, 180, 30);
        let footer = s.lines().last().unwrap();
        for hint in [
            "enter:type",
            "f:full screen",
            "d:close",
            "v:scroll",
            "!:shell here",
            "s:new shell",
        ] {
            assert!(footer.contains(hint), "{hint}: {footer}");
        }
        for hint in ["x:stop", "r:run", "d:drop", "a:answer", "p:keep", "h/l"] {
            assert!(!footer.contains(hint), "{hint}: {footer}");
        }
        // On a task, stop is `x`.
        select(&mut app, "perf");
        let s = screen(&mut app, 180, 30);
        let footer = s.lines().last().unwrap();
        assert!(
            footer.contains("x:stop") && !footer.contains("s:stop"),
            "{footer}"
        );
        app.mode = Mode::Help;
        let s = screen(&mut app, 100, 40);
        show("help with shells", &s);
        for text in [
            "Task, chat or shell",
            "x         stop / close a shell",
            "d         drop / close a chat or shell",
            "!         a shell here",
            "n/c/s     new task / chat / shell",
        ] {
            assert!(s.contains(text), "{text}: {s}");
        }

        // `!` and `x` taken by custom commands: theirs, in the help too.
        let command = |key: &str, name: &str| crate::config::CustomCommand {
            name: name.into(),
            key: key.into(),
            command: "true".into(),
        };
        app.commands = vec![command("!", "htop"), command("x", "explore")];
        app.mode = Mode::Normal;
        let s = screen(&mut app, 180, 30);
        let footer = s.lines().last().unwrap();
        assert!(
            !footer.contains("x:stop") && !footer.contains("!:shell here"),
            "{footer}"
        );
        assert!(
            footer.contains("!:htop") && footer.contains("x:explore"),
            "{footer}"
        );
        app.mode = Mode::Help;
        let s = screen(&mut app, 100, 40);
        show("help, ! and x taken", &s);
        assert!(s.contains("(x)       stop / close a shell: taken"), "{s}");
        assert!(s.contains("(!)       a shell here: taken"), "{s}");
    }

    // -- Help, filter, prompts -------------------------------------------

    #[test]
    fn help_lists_every_key_and_the_custom_commands() {
        let mut app = dashboard();
        app.commands = vec![crate::config::CustomCommand {
            name: "open in editor".into(),
            key: "o".into(),
            command: "code {dir}".into(),
        }];
        app.mode = Mode::Help;
        let s = screen(&mut app, 80, 24);
        show("help 80x24", &s);
        for text in [
            "Keys",
            "type (ctrl-] back)",
            "by repo / attention",
            "Custom commands",
            "open in editor",
            "this help",
        ] {
            assert!(s.contains(text), "{text}: {s}");
        }
        assert!(s.contains("any:close"), "{s}");
        let s = screen(&mut app, 120, 40);
        show("help 120x40", &s);
        assert!(s.contains("open in editor"), "{s}");
        // Narrow and short: one column, cut, but not garbled.
        let s = screen(&mut app, 50, 20);
        show("help 50x20", &s);
        assert!(s.contains("type (ctrl-] back)"), "{s}");
    }

    /// The screen, and where the terminal's cursor is (when shown).
    fn screen_and_cursor(app: &mut App, width: u16, height: u16) -> (String, Option<(u16, u16)>) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let backend = terminal.backend();
        let at = backend.cursor_position();
        let cursor = backend.cursor_visible().then_some((at.x, at.y));
        let buf = backend.buffer();
        let s = (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        (s, cursor)
    }

    #[test]
    fn the_filter_matches_fuzzily_and_highlights_what_matched() {
        let mut app = dashboard();
        app.filter_active = true;
        app.filter = TextInput::from("unpr");
        let (s, cursor) = screen_and_cursor(&mut app, 80, 24);
        show("filter unpr", &s);
        assert!(s.contains(" / unpr"), "{s}");
        assert!(s.contains("unit-pricing"), "{s}");
        assert!(!s.contains("perf"), "{s}");
        // The terminal's cursor after the text.
        assert_eq!(cursor, Some((7, 1)));
        // A member's repository matches too: the tasks in rustpallet.
        app.filter = TextInput::from("rustp");
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("offload-explain"), "{s}");
        assert!(!s.contains("unit-pricing"), "{s}");
        // Matched characters are highlighted (not the others).
        app.filter = TextInput::from("nv");
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let row = (0..24)
            .find(|&y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("2d nav ")
            })
            .unwrap();
        let text: String = (0..80).map(|x| buf[(x, row)].symbol()).collect();
        let x = text[..text.find("2d nav ").unwrap()].chars().count() as u16 + 3;
        assert_eq!(buf[(x, row)].fg, theme::get().accent);
        assert_eq!(buf[(x + 1, row)].fg, Color::Reset);
        assert_eq!(buf[(x + 2, row)].fg, theme::get().accent);
        // Typing elsewhere: no cursor in the list.
        app.filter_active = false;
        let (_, cursor) = screen_and_cursor(&mut app, 80, 24);
        assert_eq!(cursor, None);
    }

    #[test]
    fn prompts_show_the_cursor_where_editing_happens() {
        let mut app = App::new();
        let mut name = TextInput::from("feat-x");
        name.handle(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::NONE,
        ));
        app.mode = Mode::NewTaskEnterName {
            project_dirs: vec!["/src/api".into()],
            name,
        };
        let (s, cursor) = screen_and_cursor(&mut app, 80, 24);
        let row = s
            .lines()
            .position(|l| l.contains("Task name: feat-x"))
            .unwrap() as u16;
        let col = s.lines().nth(row as usize).unwrap().find("feat-x").unwrap() as u16;
        // Before the `x` (the line has box-drawing characters: count chars).
        let line = s.lines().nth(row as usize).unwrap();
        let col_chars = line[..col as usize].chars().count() as u16;
        assert_eq!(cursor, Some((col_chars + 5, row)));

        // The picker's filter, its matches first, the matched letters marked.
        use super::super::app::PickerItem;
        let item = |name: &str| PickerItem {
            display: name.into(),
            id: format!("/src/{name}"),
        };
        let mut picker = PickerState::new(
            "Pick project",
            vec![
                item("rustpallet"),
                item("backend"),
                item("tam"),
                item("tam-web"),
            ],
        );
        picker.filter = TextInput::from("tw");
        picker.loading = true;
        app.mode = Mode::PickProject(picker, Purpose::Task);
        let (s, cursor) = screen_and_cursor(&mut app, 80, 24);
        show("picker tw", &s);
        assert!(s.contains("looking for projects…"), "{s}");
        assert!(s.contains("│ tam-web"), "{s}");
        assert!(!s.contains("backend"), "{s}");
        assert!(cursor.is_some());
    }

    #[test]
    fn a_long_picker_scrolls_to_the_highlighted_project() {
        use super::super::app::PickerItem;
        let items = (0..40)
            .map(|i| PickerItem {
                display: format!("project-{i:02}"),
                id: format!("/src/p{i}"),
            })
            .collect();
        let mut picker = PickerState::new("Pick project", items);
        picker.selected = 30;
        let mut app = App::new();
        app.mode = Mode::PickProject(picker, Purpose::Task);
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("project-30"), "{s}");
        assert!(!s.contains("project-00"), "{s}");
        let (area, first) = app.hits.picker.unwrap();
        assert!(first > 0 && first <= 30 && 30 < first + area.height as usize);
    }

    #[test]
    fn every_mode_draws_at_any_size() {
        use super::super::app::PickerItem;
        let modes = || {
            let item = |name: &str| PickerItem {
                display: name.into(),
                id: format!("/src/{name}"),
            };
            vec![
                Mode::Normal,
                Mode::Help,
                Mode::Terminal {
                    agent: "m-5".into(),
                },
                Mode::PickProject(
                    PickerState::new("Pick project", vec![item("api"), item("web")]),
                    Purpose::Task,
                ),
                Mode::NewTaskEnterName {
                    project_dirs: vec!["/src/api".into()],
                    name: TextInput::from("a-rather-long-task-name-for-a-narrow-popup"),
                },
                Mode::SpawnEnterPath(TextInput::from("~/src"), Purpose::Task),
                Mode::PickSession(SessionPicker::new(
                    "/src/api".into(),
                    vec![tam_proto::SessionInfo {
                        id: "s-1".into(),
                        title: Some("A title long enough to be cut somewhere".into()),
                        last_prompt: Some("and a prompt that is longer still, to cut".into()),
                        modified_at: 0,
                    }],
                )),
                Mode::ConfirmCloseChat {
                    id: "c-1".into(),
                    label: "a chat".into(),
                },
                Mode::ConfirmCloseShell {
                    id: "sh-00000013".into(),
                    label: "Shell #3 of fix-allotment-rounding".into(),
                },
                Mode::RenameChat {
                    id: "c-1".into(),
                    label: "a chat".into(),
                    title: Some("a chat".into()),
                    name: TextInput::from("a rather long name for a chat, longer than its popup"),
                },
                Mode::ConfirmDropTask {
                    name: "unit-pricing".into(),
                    member: Some(MemberChoice {
                        id: "m-5".into(),
                        label: "unit-pricing/frontend".into(),
                    }),
                },
                Mode::Notice {
                    title: "Drop Failed".into(),
                    lines: vec!["a reason".into()],
                },
                Mode::Answer(AnswerDialog::new(
                    "m-5".into(),
                    "unit-pricing/frontend".into(),
                    bash(&"a long command ".repeat(40)),
                )),
                Mode::Answer(AnswerDialog::new(
                    "m-5".into(),
                    "unit-pricing/frontend".into(),
                    colors_and_size(),
                )),
                Mode::Scroll {
                    agent: "m-5".into(),
                },
            ]
        };
        for (width, height) in [(1, 1), (10, 3), (24, 6), (40, 10), (80, 24), (250, 70)] {
            for pane in [false, true] {
                for mode in modes() {
                    let mut app = dashboard();
                    select(&mut app, "unit-pricing");
                    if pane {
                        app = with_pane(app);
                    }
                    app.filter_active = true;
                    let normal = matches!(mode, Mode::Normal);
                    app.mode = mode;
                    let s = screen(&mut app, width, height);
                    if normal && pane && width >= 80 {
                        show(&format!("filter + pane {width}x{height}"), &s);
                    }
                }
            }
        }
        // On a chat in its last day, without an agent (its mark, its view,
        // the closing border), and on one with a screen.
        for chat in ["c-0000dddd", "c-0000aaaa"] {
            for (width, height) in [(1, 1), (10, 3), (24, 6), (40, 10), (80, 24), (250, 70)] {
                for mode in modes() {
                    let mut app = chat_dashboard();
                    select_chat(&mut app, chat);
                    let mut app = with_pane(app);
                    app.mode = mode;
                    screen(&mut app, width, height);
                }
            }
        }
        // On a shell in a worktree and on one on a main checkout.
        for shell in ["sh-00000041", "sh-0000000c"] {
            for (width, height) in [(1, 1), (10, 3), (24, 6), (40, 10), (80, 24), (250, 70)] {
                for mode in modes() {
                    let mut app = shell_dashboard();
                    select_shell(&mut app, shell);
                    let mut app = with_pane(app);
                    app.mode = mode;
                    screen(&mut app, width, height);
                }
            }
        }
        // The new-task name, longer than its popup: the end, where one types.
        let mut app = App::new();
        app.mode = modes().swap_remove(4);
        let s = screen(&mut app, 80, 24);
        show("long name", &s);
        assert!(s.contains("narrow-popup"), "{s}");
    }
}
