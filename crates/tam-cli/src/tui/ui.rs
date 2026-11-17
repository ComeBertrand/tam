use ratatui::layout::{Constraint, HorizontalAlignment, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph, Wrap,
};
use ratatui::Frame;
use tam_proto::{AgentDetail, AgentState};

use crate::task::TaskStatus;

use super::app::{App, MemberChoice, Mode, Order, PaneTab, PickerState};
use super::input::TextInput;
use super::peek::{Peek, ScreenView};
use super::{fuzzy, table, triage};

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
    if let Some(pane) = pane {
        // Out of the app while drawn: the pane reads the rest of it.
        if let Some(mut peek) = app.peek.take() {
            render_pane(frame, pane, app, &mut peek);
            app.peek = Some(peek);
        }
    }
    render_footer(frame, footer_area, app);

    // Overlay popups
    app.hits.picker = match &mut app.mode {
        Mode::NewTaskPickProject(picker) => Some(render_picker_popup(frame, picker)),
        _ => None,
    };
    match &app.mode {
        Mode::Normal | Mode::NewTaskPickProject(_) => {}
        Mode::Help => render_help_popup(frame, app),
        Mode::NewTaskEnterName { name, project_dirs } => {
            let joins = app.tasks.iter().any(|t| t.name == name.value());
            render_new_task_popup(frame, name, project_dirs, joins);
        }
        Mode::SpawnEnterPath(path) => {
            render_enter_path_popup(frame, path);
        }
        Mode::ConfirmDropTask { name, member } => {
            render_confirm_drop_popup(frame, name, member.as_ref());
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
            render_repair_or_drop_popup(
                frame,
                name,
                lines,
                *repairable,
                *in_place,
                member.as_ref(),
            );
        }
    }
}

/// Where the task list and the detail pane go (`None`: the pane is
/// closed). Side by side when the pane has room for an agent's screen next
/// to a list of useful width; else the list above the pane, as tall as its
/// rows up to two fifths of the height.
fn split(area: Rect, app: &App, filter: bool) -> (Rect, Option<Rect>) {
    if app.peek.is_none() {
        return (area, None);
    }
    let list = (area.width / 5 * 2).clamp(LIST_MIN, LIST_MAX);
    if area.width >= list + PANE_MIN {
        let [list, pane] =
            Layout::horizontal([Constraint::Length(list), Constraint::Fill(1)]).areas(area);
        return (list, Some(pane));
    }
    // Header, rows, filter.
    let wanted = app.rows().len() as u16 + 1 + u16::from(filter);
    let height = wanted.max(3).min((area.height / 5 * 2).max(5));
    let [list, pane] =
        Layout::vertical([Constraint::Length(height), Constraint::Fill(1)]).areas(area);
    (list, Some(pane))
}

fn render_filter(frame: &mut Frame, area: Rect, app: &App) {
    let [prompt, text] =
        Layout::horizontal([Constraint::Length(3), Constraint::Fill(1)]).areas(area);
    frame.render_widget(Span::styled(" / ", Style::new().fg(Color::Cyan)), prompt);
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
    if !app.visible_tasks().is_empty() {
        table::render(frame, area, app);
        return;
    }
    let text = if app.filter.is_empty() {
        "No tasks. Press n to create one."
    } else {
        "No matching tasks."
    };
    let text = Paragraph::new(text)
        .alignment(HorizontalAlignment::Center)
        .fg(Color::DarkGray);
    let y = area.y + area.height / 2;
    frame.render_widget(text, Rect::new(area.x, y, area.width, 1));
}

/// The task count, how many need the user, and how the list is ordered.
fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let total = app.tasks.len();
    let needs = app.needs_attention_count();

    let mut spans = vec![Span::styled(" tam", Style::new().bold()), Span::raw(" — ")];

    if total == 0 {
        spans.push(Span::raw("no tasks"));
    } else {
        spans.push(Span::raw(format!(
            "{total} task{}",
            if total == 1 { "" } else { "s" }
        )));
        if needs > 0 {
            spans.push(Span::styled(
                format!(" · {needs} need{} you", if needs == 1 { "s" } else { "" }),
                Style::new().fg(Color::Yellow),
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
        frame.render_widget(
            Line::styled(order, Style::new().fg(Color::DarkGray)).right_aligned(),
            area,
        );
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &mut App) {
    if let Some(msg) = app.status_message() {
        let line = Line::from(format!(" {msg}")).fg(Color::Yellow);
        frame.render_widget(line, area);
    } else {
        let hints = match app.mode {
            Mode::Normal => {
                let hints = normal_hints(app);
                frame.render_widget(fit_hints(hints, area.width).fg(Color::DarkGray), area);
                return;
            }
            Mode::NewTaskEnterName { .. } => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":create  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::NewTaskPickProject(_) => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":select  "),
                Span::styled("tab", Style::new().bold()),
                Span::raw(":pick several  "),
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
        frame.render_widget(Line::from(hints).fg(Color::DarkGray), area);
    }
}

/// A key hint in the footer: the key, what it does, and how much it
/// matters (0 most: kept when the footer cannot show them all).
type Hint<'a> = (&'a str, String, u8);

/// The task list's keys, for the selection.
fn normal_hints(app: &App) -> Vec<Hint<'_>> {
    let selected = app.selected_task();
    let has_agent = app
        .focused_member()
        .is_some_and(|(_, m)| m.agent_info.is_some());
    let mut hints = Vec::new();
    if has_agent {
        hints.push(("enter", "attach".into(), 0));
        hints.push(("s", "stop".into(), 1));
    } else if selected.is_some() {
        hints.push(("r", "run".into(), 0));
        hints.push(("R", "new session".into(), 2));
    }
    if let Some(peek) = &app.peek {
        if selected.is_some_and(|t| t.members.len() > 1) {
            hints.push(("←/→", "member".into(), 1));
        }
        let other = match app.tab {
            PaneTab::Triage => "terminal",
            PaneTab::Terminal => "triage",
        };
        hints.push(("tab", other.into(), 2));
        hints.push(("pgup/pgdn", "scroll".into(), 3));
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
    hints.push(("n", "new".into(), 2));
    hints.push(("p", "peek".into(), 1));
    hints.push(("/", "filter".into(), 3));
    let order = match app.order {
        Order::Repo => "attention first",
        Order::Attention => "by repository",
    };
    hints.push(("g", order.into(), 4));
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
        spans.push(Span::styled(key.to_string(), Style::new().bold()));
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
        AgentState::Blocked => pending("permission", Color::Red),
        AgentState::Question => pending("question", Color::Magenta),
        AgentState::Error => detail.error.as_ref().map(|e| {
            let text = match &e.message {
                Some(m) => format!("error · {}: {}", e.kind, first_line(m)),
                None => format!("error · {}", e.kind),
            };
            (text, Color::Red)
        }),
        _ => detail.denied.as_ref().map(|d| {
            let mut text = format!("denied · {}", d.tool);
            if let Some(s) = &d.summary {
                text.push_str(&format!(": {s}"));
            }
            if let Some(r) = &d.reason {
                text.push_str(&format!(" ({})", first_line(r)));
            }
            (text, Color::Yellow)
        }),
    }
}

pub(super) fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// The detail pane, about the focused member of the selected task. Triage
/// tab: what it needs from the user (see `triage`), its screen below.
/// Terminal tab: its screen on the whole pane, with what it waits on in the
/// bottom border.
fn render_pane(frame: &mut Frame, area: Rect, app: &App, peek: &mut Peek) {
    let focused = app.focused_member();
    let label = focused.map_or_else(|| "—".to_string(), |(t, m)| t.member_label(m));
    let agent = focused.and_then(|(_, m)| m.agent_info.as_ref());

    let title = match peek.scrolled_back() {
        Some(0) => format!(" {label} · scrolled back "),
        Some(n) => format!(" {label} · {n} lines into history "),
        None => format!(" {label} "),
    };
    let tab = |name: &'static str, shown: bool| {
        if shown {
            Span::styled(name, Style::new().bold())
        } else {
            Span::styled(name, Style::new().fg(Color::DarkGray))
        }
    };
    let tabs = Line::from(vec![
        Span::raw(" "),
        tab("triage", app.tab == PaneTab::Triage),
        Span::styled(" │ ", Style::new().fg(Color::DarkGray)),
        tab("terminal", app.tab == PaneTab::Terminal),
        Span::raw(" "),
    ]);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(title)
        .title_top(tabs.right_aligned());
    if app.tab == PaneTab::Terminal {
        if let Some((text, color)) = agent.and_then(|a| agent_detail_line(a.state, &a.detail)) {
            block = block.title_bottom(Line::styled(format!(" {text} "), Style::new().fg(color)));
        }
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some((task, member)) = focused.filter(|_| app.tab == PaneTab::Triage) else {
        render_screen(frame, inner, peek);
        return;
    };
    let detail = app.detail.as_ref().filter(|d| d.task == task.name);
    let view = triage::triage(
        task,
        member,
        detail,
        app.branches.get(member.id()),
        crate::task::now(),
    );
    // With an agent, its screen keeps at least half of the pane.
    let rows = if agent.is_some() {
        inner.height - inner.height / 2
    } else {
        inner.height
    };
    let lines = triage::fit(view, inner.width, rows);
    let used = lines.len() as u16;
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y, inner.width, used),
    );
    let below = inner.height.saturating_sub(used + 1);
    if agent.is_none() || below == 0 {
        peek.height = 0;
        return;
    }
    let rule = "─".repeat(inner.width as usize);
    frame.render_widget(
        Line::styled(rule, Style::new().fg(Color::DarkGray)),
        Rect::new(inner.x, inner.y + used, inner.width, 1),
    );
    let screen = Rect::new(inner.x, inner.y + used + 1, inner.width, below);
    render_screen(frame, screen, peek);
}

/// The agent's screen, as the daemon's terminal emulator has it: its live
/// screen (the content's bottom when the area is shorter) or history, cut
/// at the area's right edge.
fn render_screen(frame: &mut Frame, area: Rect, peek: &mut Peek) {
    peek.height = area.height;
    let message = match (&peek.agent, &peek.screen, &peek.error) {
        (None, _, _) => "(no agent running)".to_string(),
        (Some(_), Some(screen), _) => {
            let view = ScreenView {
                screen: screen.screen(),
                first: screen.first_row(peek.from, area.height),
            };
            frame.render_widget(view, area);
            return;
        }
        (Some(_), None, Some(error)) => format!("(error: {error})"),
        (Some(_), None, None) => String::new(),
    };
    frame.render_widget(Paragraph::new(message).fg(Color::DarkGray), area);
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
        .border_style(Style::new().fg(Color::Cyan))
        .title(title)
        .title_style(Style::new().bold().fg(Color::Cyan))
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let [prompt, text] =
        Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(filter_area);
    frame.render_widget(Span::styled("> ", Style::new().fg(Color::Cyan)), prompt);
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
    let matched = Style::new().fg(Color::Cyan).bold();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, (item, positions))| {
            let style = if i == picker.selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            // Picked with tab: a mark, and the order they were picked in.
            let mark = match picker.marked.iter().position(|m| *m == item.id) {
                Some(n) => Span::styled(format!("{} ", n + 1), Style::new().fg(Color::Cyan).bold()),
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
        .border_style(Style::new().fg(Color::Cyan))
        .title(" New Task ")
        .title_style(Style::new().bold().fg(Color::Cyan))
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
            Span::styled("adds ", Style::new().fg(Color::Yellow)),
            Span::raw(repos),
            Span::styled(" to this existing task", Style::new().fg(Color::Yellow)),
        ])
    } else {
        Line::from(vec![Span::raw("in "), Span::raw(repos)]).fg(Color::DarkGray)
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
        .border_style(Style::new().fg(Color::Cyan))
        .title(" Enter Path ")
        .title_style(Style::new().bold().fg(Color::Cyan))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [prompt, text] =
        Layout::horizontal([Constraint::Length(2), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(Span::styled("> ", Style::new().fg(Color::Cyan)), prompt);
    path.render(frame, text, Style::new(), true);
}

/// Every key, by what it acts on, custom commands included: in two
/// columns when the terminal is wide enough, cut at its bottom.
fn render_help_popup(frame: &mut Frame, app: &App) {
    let entry = |key: &str, what: &str| (key.to_string(), what.to_string());
    let mut sections: Vec<(&str, Vec<(String, String)>)> = vec![
        (
            "Task",
            vec![
                entry("j/k ↑/↓", "select"),
                entry("enter", "attach (ctrl-] back)"),
                entry("r", "run, resuming"),
                entry("R", "run, new session"),
                entry("s", "stop the agent"),
                entry("d", "drop (or recreate)"),
                entry("n", "new task"),
            ],
        ),
        (
            "List",
            vec![
                entry("/", "filter (fuzzy)"),
                entry("g", "by repo / attention"),
                entry("click", "select a row"),
                entry("wheel", "move the selection"),
            ],
        ),
        (
            "Pane",
            vec![
                entry("p", "show / hide"),
                entry("tab", "triage / terminal"),
                entry("←/→", "another member"),
                entry("pgup/pgdn", "scroll (wheel too)"),
                entry("home/end", "oldest line / live"),
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
                Style::new().fg(Color::Cyan).bold(),
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
        .border_style(Style::new().fg(Color::Cyan))
        .title(" Keys ")
        .title_style(Style::new().bold().fg(Color::Cyan))
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

/// Asks to drop a task, or (with `member`, the focused member of a task
/// with several) that member alone.
fn render_confirm_drop_popup(frame: &mut Frame, name: &str, member: Option<&MemberChoice>) {
    let red = |key: &'static str| Span::styled(key, Style::new().bold().fg(Color::Red));
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
        .border_style(Style::new().fg(Color::Red))
        .title(" Confirm Drop ")
        .title_style(Style::new().bold().fg(Color::Red))
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
            Span::styled("y", Style::new().bold().fg(Color::Red)),
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
        .border_style(Style::new().fg(Color::Red))
        .title(" Drop Refused ")
        .title_style(Style::new().bold().fg(Color::Red))
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
            Span::styled("r", Style::new().bold().fg(Color::Green)),
            Span::raw(" recreate · "),
        ]);
    }
    keys.extend([
        Span::styled("y", Style::new().bold().fg(Color::Red)),
        Span::raw(" drop · "),
    ]);
    if in_place {
        keys.extend([
            Span::styled("b", Style::new().bold().fg(Color::Red)),
            Span::raw(" drop + delete branch · "),
        ]);
    }
    if let Some(m) = member {
        keys.extend([
            Span::styled("m", Style::new().bold().fg(Color::Red)),
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
        .border_style(Style::new().fg(Color::Yellow))
        .title(" Worktree Gone ")
        .title_style(Style::new().bold().fg(Color::Yellow))
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
        .border_style(Style::new().fg(Color::Yellow))
        .title(format!(" {title} "))
        .title_style(Style::new().bold().fg(Color::Yellow))
        .padding(Padding::new(2, 2, 1, 1));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text: Vec<Line> = lines.iter().map(|l| Line::raw(l.as_str())).collect();
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

pub(super) fn status_display(status: &TaskStatus) -> (&'static str, Color) {
    match status {
        TaskStatus::Run => ("● run", Color::Blue),
        TaskStatus::Input => ("▲ input", Color::Yellow),
        TaskStatus::Block => ("▲ block", Color::Red),
        TaskStatus::Question => ("? question", Color::Magenta),
        TaskStatus::Error => ("✗ error", Color::Red),
        TaskStatus::Idle => ("○ idle", Color::DarkGray),
        TaskStatus::Stale => ("◌ stale", Color::DarkGray),
        TaskStatus::Gone => ("✗ gone", Color::Red),
    }
}

pub(super) fn context_display(pct: u8) -> Span<'static> {
    let color = if pct >= 90 {
        Color::Red
    } else if pct >= 70 {
        Color::Yellow
    } else {
        Color::Green
    };
    Span::styled(format!("{pct}%"), Style::new().fg(color))
}

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
    use tam_proto::{AgentError, DeniedTool, Pending};

    fn text(state: AgentState, d: &AgentDetail) -> Option<String> {
        agent_detail_line(state, d).map(|(t, _)| t)
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
        assert!(text(AgentState::Input, &AgentDetail::default()).is_none());
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
        app.peek = Some(Peek::default());
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("(no agent running)"), "{s}");
        assert!(s.contains("pgup/pgdn:scroll"), "{s}");
        // Narrower: the footer keeps what matters most, help among them.
        let s = screen(&mut app, 60, 20);
        assert!(!s.contains("pgup/pgdn:scroll"), "{s}");
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
        app.peek = Some(peek);
        let s = screen(&mut app, 60, 20);
        assert!(s.contains("│line 12"), "{s}");
        assert!(!s.contains("history"), "{s}");

        // Scrolled back to line 4.
        let peek = app.peek.as_mut().unwrap();
        peek.from = Some(4);
        info.top = 4;
        peek.set_screen(&info);
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("— · 6 lines into history"), "{s}");
        assert!(s.contains("end:live"), "{s}");
    }

    // -- The triage pane ---------------------------------------------------

    use crate::task::Task;
    use crate::tui::app::{PaneTab, TaskDetail};
    use crate::tui::branch::{BranchState, Observation};
    use crate::tui::input::TextInput;
    use tam_proto::{AgentInfo, MemberHealth, MemberInfo, MessageInfo, RunInfo, TaskInfo};

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
        app.peek = Some(match focused {
            Some(id) => agent_screen(&id, PERMISSION_DIALOG),
            None => Peek::default(),
        });
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
    fn triage_shows_a_permission_request_with_its_input_and_the_screen_below() {
        let mut app = triage_app(
            vec![member_info("m-1", "app")],
            vec![agent_in("m-1", AgentState::Blocked, 190, blocked())],
        );
        app.detail = Some(TaskDetail {
            task: "feat".into(),
            messages: vec![
                message("msg-1", None, "m-1", "please also run clippy", 600),
                message("msg-2", Some("m-1"), "m-9", "done, see the\nbranch", 300),
            ],
            ..Default::default()
        });
        app.branches.insert(
            "m-1".into(),
            observed(BranchState {
                base: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
                files: 3,
                insertions: 40,
                deletions: 2,
                uncommitted: 1,
            }),
        );
        let s = screen(&mut app, 100, 34);
        assert!(s.contains("┌ feat ─"), "{s}");
        assert!(s.contains("triage │ terminal"), "{s}");
        assert!(
            s.contains("│▲ blocked for 3m · permission for Bash  (claude)"),
            "{s}"
        );
        assert!(
            s.contains("│  cargo test --workspace -- --nocapture"),
            "{s}"
        );
        assert!(
            s.contains(
                "│branch feat · 2 ahead, 1 behind origin/main · 3 files +40 -2 · 1 uncommitted"
            ),
            "{s}"
        );
        assert!(s.contains("│ 10m ← user: please also run clippy"), "{s}");
        assert!(s.contains("│  5m → feat/m-9: done, see the branch"), "{s}");
        // The screen, below a rule: the dialog's choices.
        assert!(s.contains("│──────"), "{s}");
        assert!(s.contains("│ > 1. Yes"), "{s}");
        assert!(s.contains("tab:terminal"), "{s}");

        // The tab shown is the bold one.
        let tab_styles = |app: &mut App| {
            let backend = ratatui::backend::TestBackend::new(100, 34);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| render(f, app)).unwrap();
            let buf = terminal.backend().buffer().clone();
            let row = (0..34)
                .find(|&y| (0..100).any(|x| buf[(x, y)].symbol() == "┌"))
                .unwrap();
            let text: String = (0..100).map(|x| buf[(x, row)].symbol()).collect();
            let at = |word: &str| {
                let x = text[..text.find(word).unwrap()].chars().count() as u16;
                buf[(x, row)].modifier.contains(Modifier::BOLD)
            };
            (at("triage"), at("terminal"))
        };
        assert_eq!(tab_styles(&mut app), (true, false));

        // The terminal tab: the screen alone, what it waits on below.
        app.tab = PaneTab::Terminal;
        assert_eq!(tab_styles(&mut app), (false, true));
        let s = screen(&mut app, 100, 34);
        assert!(!s.contains("blocked for"), "{s}");
        assert!(s.contains("│> run the tests"), "{s}");
        assert!(
            s.contains("permission · Bash: cargo test --workspace -- --nocapture"),
            "{s}"
        );
        assert!(s.contains("tab:triage"), "{s}");
    }

    #[test]
    fn triage_shows_a_question_an_error_and_auto_mode_refusals() {
        let question = AgentDetail {
            pending: Some(Pending {
                tool: Some("AskUserQuestion".into()),
                summary: "Which database should the cache use? (+1 more)".into(),
            }),
            ..Default::default()
        };
        let mut app = triage_app(
            vec![member_info("m-1", "app")],
            vec![agent_in("m-1", AgentState::Question, 45, question)],
        );
        let s = screen(&mut app, 100, 30);
        assert!(s.contains("? question for 45s  (claude)"), "{s}");
        assert!(
            s.contains("│  Which database should the cache use? (+1 more)"),
            "{s}"
        );
        // Git not asked yet.
        assert!(s.contains("│branch feat · …"), "{s}");

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
            s.contains("✗ error for 2h · turn failed: rate_limit"),
            "{s}"
        );
        assert!(s.contains("│  API Error: Rate limit reached"), "{s}");

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
        assert!(s.contains("● working for 20s  (claude)"), "{s}");
        assert!(
            s.contains("│  auto mode refused Bash: rm -rf build (destructive)"),
            "{s}"
        );
    }

    #[test]
    fn triage_shows_the_last_message_cut_to_leave_the_screen_room() {
        let paragraph = "The cache now expires entries after their TTL, and the tests \
                         cover eviction under memory pressure as well.";
        let last = format!(
            "Done. Summary:\n\n{}\n\nShall I open the pull request?",
            [paragraph; 12].join("\n")
        );
        let detail = AgentDetail {
            last_message: Some(last),
            ..Default::default()
        };
        let mut app = triage_app(
            vec![member_info("m-1", "app")],
            vec![agent_in("m-1", AgentState::Input, 750, detail)],
        );
        let s = screen(&mut app, 100, 34);
        assert!(s.contains("▲ input for 12m · turn finished"), "{s}");
        assert!(s.contains("│  Done. Summary:"), "{s}");
        // Cut, with the branch still shown and the screen kept below.
        assert!(s.contains("…"), "{s}");
        assert!(s.contains("│branch feat"), "{s}");
        assert!(s.contains("│ > 1. Yes"), "{s}");

        // 80x24: the pane is short, the screen still has half of it.
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("▲ input for 12m · turn finished"), "{s}");
        assert!(s.contains("│──────"), "{s}");
    }

    #[test]
    fn triage_of_a_member_without_agent_says_how_its_last_run_ended() {
        let mut app = triage_app(vec![member_info("m-1", "app")], vec![]);
        app.detail = Some(TaskDetail {
            task: "feat".into(),
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
        assert!(s.contains("✗ gone · no agent"), "{s}");
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
    fn triage_of_two_members_shows_the_one_that_needs_the_user() {
        let mut app = triage_app(
            vec![member_info("m-1", "app"), member_info("m-2", "web")],
            vec![
                agent_in("m-1", AgentState::Working, 30, AgentDetail::default()),
                agent_in("m-2", AgentState::Blocked, 190, blocked()),
            ],
        );
        let s = screen(&mut app, 100, 34);
        // Both members, the blocked one focused: named in the title.
        assert!(s.contains("┌ feat/web ─"), "{s}");
        assert!(s.contains("│  ● app  working 30s"), "{s}");
        assert!(
            s.contains("│▸ ▲ web  blocked 3m  permission · Bash: cargo test"),
            "{s}"
        );
        assert!(s.contains("│▲ blocked for 3m · permission for Bash"), "{s}");
        assert!(s.contains("←/→:member"), "{s}");

        // → shows the other one (and enter would attach to it).
        app.cycle_member(1);
        app.sync_focus();
        assert_eq!(app.peek_agent().as_deref(), Some("m-1"));
        app.peek.as_mut().unwrap().agent = Some("m-1".into());
        let s = screen(&mut app, 100, 34);
        assert!(s.contains("┌ feat/app ─"), "{s}");
        assert!(s.contains("│▸ ● app  working 30s"), "{s}");
        assert!(s.contains("│● working for 30s  (claude)"), "{s}");
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
        app.mode = Mode::NewTaskPickProject(picker);
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
            agent_in("m-6", AgentState::Input, 750, finished),
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
        app.peek = Some(match app.peek_agent() {
            Some(id) => agent_screen(&id, PERMISSION_DIALOG),
            None => Peek::default(),
        });
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
    fn the_list_groups_tasks_by_repository_with_member_rows() {
        let mut app = dashboard();
        let s = screen(&mut app, 80, 24);
        show("80x24", &s);
        // A header per repository, the task in two under both.
        for group in ["backend", "backend + frontend", "frontend", "rustpallet"] {
            assert!(s.lines().any(|l| l.trim_end() == group), "{group}: {s}");
        }
        assert!(!s.contains("DIR"), "{s}");
        // The task name is not cut at 15 columns; time in state and branch.
        assert!(
            s.contains("▲ block      3m fix-allotment-rounding ↑2 *1  claude 67%"),
            "{s}"
        );
        assert!(s.contains("◌ stale     45d spike-cache"), "{s}");
        assert!(s.contains("↑5 ↓12"), "{s}");
        assert!(s.contains("○ idle       2d nav"), "{s}");
        // A task in two repositories: its status is its most urgent
        // member's, a row per member says the rest.
        assert!(s.contains("? question   5m unit-pricing"), "{s}");
        assert!(
            s.contains("● run        1m  ├ backend             ↑1 *3  claude 12%"),
            "{s}"
        );
        assert!(s.contains("? question   5m  └ frontend"), "{s}");

        let s = screen(&mut app, 140, 30);
        show("140x30", &s);
        // Wide: what each member waits on.
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
        assert!(rows[0].starts_with("▲ block"), "{s}");
        assert!(rows[1].starts_with("? question"), "{s}");
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
        assert!(first.starts_with("STATUS"), "{s}");
        assert!(first.contains("┌ unit-pricing/frontend"), "{s}");
        // The member the keys act on is marked.
        assert!(s.contains("└▸frontend"), "{s}");

        // Narrower: the pane below a list as tall as its rows allow.
        let s = screen(&mut app, 120, 40);
        show("120x40 pane", &s);
        assert!(s.lines().nth(1).unwrap().starts_with("STATUS"), "{s}");
        assert!(!s.lines().nth(1).unwrap().contains('┌'), "{s}");
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
        // The agent column goes first, then context.
        assert!(!s.contains("AGENT"), "{s}");
        assert!(s.contains("fix-allotment-rounding"), "{s}");
        let s = screen(&mut app, 36, 20);
        show("36x20", &s);
        assert!(s.contains("fix-allotment-round…"), "{s}");
        assert!(s.contains(" tam — 7 tasks · 3 need you "), "{s}");

        // Just wide enough for the pane beside the list, and just not.
        let mut app = with_pane(dashboard());
        let s = screen(&mut app, 136, 30);
        show("136x30 pane", &s);
        assert!(s.lines().nth(1).unwrap().contains('┌'), "{s}");
        let s = screen(&mut app, 135, 30);
        show("135x30 pane", &s);
        assert!(!s.lines().nth(1).unwrap().contains('┌'), "{s}");
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
        // 10 rows: header, footer, the column names and 7 rows.
        let s = screen(&mut app, 80, 10);
        assert!(s.lines().nth(2).unwrap().starts_with("api"), "{s}");
        select(&mut app, "zz-both");
        let s = screen(&mut app, 80, 10);
        show("scrolled", &s);
        // The task in both repositories, its header and both members.
        assert!(s.contains("api + web"), "{s}");
        // The member keys would act on is marked.
        assert!(s.contains("├▸api"), "{s}");
        assert!(s.contains("└ web"), "{s}");
        // Back up one: the list moves only as far as it must.
        app.select_prev();
        let s = screen(&mut app, 80, 10);
        assert!(s.contains("task-11"), "{s}");
        assert!(s.contains("└ web"), "kept in view: {s}");
        app.selected = 0;
        let s = screen(&mut app, 80, 10);
        assert!(s.lines().nth(2).unwrap().starts_with("api"), "{s}");
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
            "attach (ctrl-] back)",
            "by repo / attention",
            "triage / terminal",
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
        assert!(s.contains("attach (ctrl-] back)"), "{s}");
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
        assert_eq!(buf[(x, row)].fg, Color::Cyan);
        assert_eq!(buf[(x + 1, row)].fg, Color::Reset);
        assert_eq!(buf[(x + 2, row)].fg, Color::Cyan);
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
        app.mode = Mode::NewTaskPickProject(picker);
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
        app.mode = Mode::NewTaskPickProject(picker);
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
                Mode::NewTaskPickProject(PickerState::new(
                    "Pick project",
                    vec![item("api"), item("web")],
                )),
                Mode::NewTaskEnterName {
                    project_dirs: vec!["/src/api".into()],
                    name: TextInput::from("a-rather-long-task-name-for-a-narrow-popup"),
                },
                Mode::SpawnEnterPath(TextInput::from("~/src")),
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
        // The new-task name, longer than its popup: the end, where one types.
        let mut app = App::new();
        app.mode = modes().swap_remove(3);
        let s = screen(&mut app, 80, 24);
        show("long name", &s);
        assert!(s.contains("narrow-popup"), "{s}");
    }
}
