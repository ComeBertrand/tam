use std::num::NonZeroU16;

use ratatui::layout::{Constraint, HorizontalAlignment, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, Padding, Paragraph, Row, Table,
    TableState, Wrap,
};
use ratatui::Frame;
use tam_proto::{AgentDetail, AgentState};

use crate::task::{Task, TaskStatus};

use super::app::{App, Mode, PickerState};

pub fn render(frame: &mut Frame, app: &mut App) {
    let show_filter = app.filter_active || !app.filter.is_empty();
    let filter_height = if show_filter { 1 } else { 0 };

    if app.peek.is_some() {
        let [header_area, filter_area, table_area, preview_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(filter_height),
            Constraint::Percentage(35),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        render_header(frame, header_area, app);
        if show_filter {
            render_filter(frame, filter_area, app);
        }
        render_tasks(frame, table_area, app);

        let selected = app.selected_task();
        let task_name = selected.map(|t| t.name.as_str()).unwrap_or("—");
        let detail = selected
            .and_then(|t| t.agent_info())
            .and_then(|a| agent_detail_line(a.state, &a.detail));
        let content = app.peek.as_deref().unwrap_or("");
        render_preview(frame, preview_area, task_name, detail, content);

        render_footer(frame, footer_area, app);
    } else {
        let [header_area, filter_area, table_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(filter_height),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        render_header(frame, header_area, app);
        if show_filter {
            render_filter(frame, filter_area, app);
        }
        render_tasks(frame, table_area, app);

        render_footer(frame, footer_area, app);
    }

    // Overlay popups
    match &app.mode {
        Mode::Normal => {}
        Mode::NewTaskPickProject(picker) => {
            render_picker_popup(frame, picker);
        }
        Mode::NewTaskEnterName { name, .. } => {
            render_new_task_popup(frame, name);
        }
        Mode::SpawnEnterPath(path) => {
            render_enter_path_popup(frame, path);
        }
        Mode::ConfirmDropTask { name } => {
            render_confirm_drop_popup(frame, name);
        }
        Mode::ConfirmForceDrop { name, report, .. } => {
            render_force_drop_popup(frame, name, report);
        }
        Mode::Notice { title, lines } => {
            render_notice_popup(frame, title, lines);
        }
    }
}

fn render_filter(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" /", Style::new().fg(Color::Cyan))];
    spans.push(Span::raw(&app.filter));
    if app.filter_active {
        spans.push(Span::styled("█", Style::new().fg(Color::DarkGray)));
    }
    frame.render_widget(Line::from(spans), area);
}

fn render_tasks(frame: &mut Frame, area: Rect, app: &App) {
    let visible = app.visible_tasks();
    if visible.is_empty() {
        if app.filter.is_empty() {
            let text = Paragraph::new("No tasks. Press n to create one.")
                .alignment(HorizontalAlignment::Center)
                .fg(Color::DarkGray);
            let y = area.y + area.height / 2;
            frame.render_widget(text, Rect::new(area.x, y, area.width, 1));
        } else {
            let text = Paragraph::new("No matching tasks.")
                .alignment(HorizontalAlignment::Center)
                .fg(Color::DarkGray);
            let y = area.y + area.height / 2;
            frame.render_widget(text, Rect::new(area.x, y, area.width, 1));
        }
    } else {
        render_table(frame, area, &visible, app.selected);
    }
}

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
                format!(" ({needs} needs input)"),
                Style::new().fg(Color::Yellow),
            ));
        }
    }

    frame.render_widget(Line::from(spans), area);
}

fn render_table(frame: &mut Frame, area: Rect, tasks: &[&Task], selected: usize) {
    let header = Row::new([
        Cell::from("STATUS"),
        Cell::from("REPO"),
        Cell::from("TASK"),
        Cell::from("AGENT"),
        Cell::from("DIR"),
        Cell::from("CTX"),
    ])
    .style(Style::new().fg(Color::DarkGray))
    .bottom_margin(0);

    let rows: Vec<Row> = tasks
        .iter()
        .map(|task| {
            let status = task.status();
            let (icon, color) = status_display(&status);
            let dir = task
                .dir()
                .map(|d| shorten_home(&d.display().to_string()))
                .unwrap_or_default();
            let agent = task
                .agent_info()
                .map(|a| a.provider.as_str())
                .unwrap_or("-");
            let ctx = task
                .agent_info()
                .and_then(|a| a.context_percent)
                .map(|p| context_display(p))
                .unwrap_or_else(|| Span::raw(""));

            Row::new([
                Cell::from(Span::styled(icon, Style::new().fg(color))),
                Cell::from(task.repo_name()),
                Cell::from(task.name.as_str()),
                Cell::from(agent),
                Cell::from(dir),
                Cell::from(ctx),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10), // STATUS
        Constraint::Length(15), // REPO
        Constraint::Length(15), // TASK
        Constraint::Length(10), // AGENT
        Constraint::Fill(1),    // DIR
        Constraint::Length(6),  // CTX
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::NONE))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    let mut table_state = TableState::default();
    if !tasks.is_empty() {
        table_state.select(Some(selected));
    }

    frame.render_stateful_widget(table, area, &mut table_state);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &mut App) {
    if let Some(msg) = app.status_message() {
        let line = Line::from(format!(" {msg}")).fg(Color::Yellow);
        frame.render_widget(line, area);
    } else {
        let hints = match app.mode {
            Mode::Normal => {
                let selected = app.selected_task();
                let has_agent = selected.is_some_and(|t| t.agent_info().is_some());

                let mut hints = Vec::new();
                if has_agent {
                    hints.extend([
                        Span::styled(" enter", Style::new().bold()),
                        Span::raw(":attach  "),
                        Span::styled("s", Style::new().bold()),
                        Span::raw(":stop  "),
                    ]);
                } else if selected.is_some() {
                    hints.extend([
                        Span::styled(" r", Style::new().bold()),
                        Span::raw(":run  "),
                        Span::styled("R", Style::new().bold()),
                        Span::raw(":new session  "),
                    ]);
                }
                hints.extend([
                    Span::styled("n", Style::new().bold()),
                    Span::raw(":new  "),
                    Span::styled("/", Style::new().bold()),
                    Span::raw(":filter  "),
                    Span::styled("p", Style::new().bold()),
                    Span::raw(":peek  "),
                ]);
                for cmd in &app.commands {
                    hints.push(Span::styled(cmd.key.clone(), Style::new().bold()));
                    hints.push(Span::raw(format!(":{} ", cmd.name)));
                }
                hints.extend([
                    Span::styled("d", Style::new().bold()),
                    Span::raw(":drop  "),
                    Span::styled("q", Style::new().bold()),
                    Span::raw(":quit"),
                ]);
                hints
            }
            Mode::NewTaskEnterName { .. } => vec![
                Span::styled(" enter", Style::new().bold()),
                Span::raw(":create  "),
                Span::styled("esc", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::ConfirmDropTask { .. } => vec![
                Span::styled(" y", Style::new().bold()),
                Span::raw("/"),
                Span::styled("enter", Style::new().bold()),
                Span::raw(":drop  "),
                Span::styled("b", Style::new().bold()),
                Span::raw(":drop + delete branch  "),
                Span::styled("any", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::ConfirmForceDrop { .. } => vec![
                Span::styled(" y", Style::new().bold()),
                Span::raw(":drop anyway  "),
                Span::styled("any", Style::new().bold()),
                Span::raw(":cancel"),
            ],
            Mode::Notice { .. } => vec![
                Span::styled(" any", Style::new().bold()),
                Span::raw(":close"),
            ],
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

/// What the agent waits on or reported, for the peek pane: the pending
/// permission or question, the error, or a tool call refused by auto mode.
fn agent_detail_line(state: AgentState, detail: &AgentDetail) -> Option<(String, Color)> {
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

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn render_preview(
    frame: &mut Frame,
    area: Rect,
    task_name: &str,
    detail: Option<(String, Color)>,
    raw_content: &str,
) {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(format!(" {task_name} "));
    if let Some((text, color)) = detail {
        block = block.title_bottom(Line::styled(format!(" {text} "), Style::new().fg(color)));
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(cols) = NonZeroU16::new(inner.width).filter(|_| inner.height > 0) else {
        return;
    };

    const PARSE_ROWS: NonZeroU16 = NonZeroU16::new(500).unwrap();
    let mut parser = vt100::Parser::new(PARSE_ROWS, cols, 0);
    parser.process(raw_content.as_bytes());

    if parser.screen().alternate_screen() {
        parser.process(b"\x1b[?1049l");
    }
    let screen = parser.screen();

    let mut all_lines: Vec<Line> = (0..PARSE_ROWS.get())
        .map(|row| {
            let mut spans = Vec::new();
            for col in 0..inner.width {
                let cell = screen.cell(row, col).unwrap();
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                let style = vt100_style_to_ratatui(cell);
                if contents.is_empty() {
                    spans.push(Span::styled(" ".to_string(), style));
                } else {
                    spans.push(Span::styled(contents.to_string(), style));
                }
            }
            Line::from(spans)
        })
        .collect();

    while all_lines.last().is_some_and(|l| line_is_blank(l)) {
        all_lines.pop();
    }

    let display_lines: Vec<Line> = if all_lines.len() > inner.height as usize {
        all_lines.split_off(all_lines.len() - inner.height as usize)
    } else {
        all_lines
    };

    frame.render_widget(Paragraph::new(display_lines), inner);
}

fn line_is_blank(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

fn vt100_style_to_ratatui(cell: &vt100::Cell) -> Style {
    let mut style = Style::new();
    style = style.fg(vt100_color_to_ratatui(cell.fgcolor()));
    style = style.bg(vt100_color_to_ratatui(cell.bgcolor()));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn render_picker_popup(frame: &mut Frame, picker: &PickerState) {
    let area = frame
        .area()
        .centered(Constraint::Percentage(60), Constraint::Percentage(60));
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(" {} ", picker.title))
        .title_style(Style::new().bold().fg(Color::Cyan))
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    let filter_line = Line::from(vec![
        Span::styled("> ", Style::new().fg(Color::Cyan)),
        Span::raw(&picker.filter),
        Span::styled("█", Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(filter_line, filter_area);

    let filtered = picker.filtered_items();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == picker.selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            ListItem::new(item.display.as_str()).style(style)
        })
        .collect();

    frame.render_widget(List::new(items), list_area);
}

fn render_new_task_popup(frame: &mut Frame, name: &str) {
    // 1 content row + 2 border + 2 vertical padding = 5
    let area = frame
        .area()
        .centered(Constraint::Length(50), Constraint::Length(5));
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

    let name_line = Line::from(vec![
        Span::raw("Task name: "),
        Span::styled(name, Style::new().bold()),
        Span::styled("█", Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(name_line, inner);
}

fn render_enter_path_popup(frame: &mut Frame, path: &str) {
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

    let input_line = Line::from(vec![
        Span::styled("> ", Style::new().fg(Color::Cyan)),
        Span::raw(path),
        Span::styled("█", Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(input_line, inner);
}

const DROP_KEYS: &str = "y drop · b drop + delete branch · n cancel";

fn render_confirm_drop_popup(frame: &mut Frame, name: &str) {
    // Size to content: question and key line + padding + border
    let content_width = ("Drop task ".len() + name.len() + 1).max(DROP_KEYS.chars().count());
    // +8 for padding + border, 2 content rows + 2 border + 2 vertical padding = 6
    let width = (content_width as u16 + 8).max(30);
    let area = frame
        .area()
        .centered(Constraint::Length(width), Constraint::Length(6));
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

    let msg = Line::from(vec![
        Span::raw("Drop task "),
        Span::styled(name, Style::new().bold()),
        Span::raw("?"),
    ]);
    let keys = Line::from(vec![
        Span::styled("y", Style::new().bold().fg(Color::Red)),
        Span::raw(" drop · "),
        Span::styled("b", Style::new().bold().fg(Color::Red)),
        Span::raw(" drop + delete branch · "),
        Span::styled("n", Style::new().bold()),
        Span::raw(" cancel"),
    ]);
    frame.render_widget(Paragraph::new(vec![msg, keys]), inner);
}

/// The daemon refused a drop: show why (what it would lose), ask to force.
fn render_force_drop_popup(frame: &mut Frame, name: &str, report: &[String]) {
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Task "),
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

fn status_display(status: &TaskStatus) -> (&'static str, Color) {
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

fn context_display(pct: u8) -> Span<'static> {
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
        };
        let s = screen(&mut app, 80, 20);
        assert!(s.contains("Drop task feat?"), "{s}");
        assert!(s.contains(DROP_KEYS), "{s}");

        app.mode = Mode::ConfirmForceDrop {
            name: "feat".into(),
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
}
