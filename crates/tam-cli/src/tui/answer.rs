//! The answer dialog (`a`): the request a member's agent waits on, in full,
//! and the keys that answer it from the dashboard (`Request::Answer`; the
//! daemon's side is `tam_daemon::answer`).
//!
//! The dialog answers the request it opened on, never another: the daemon
//! refuses an answer to any other, and the dialog says so as soon as the
//! request is no longer the agent's (`App::answer_stale`), keys then only
//! closing it. Keys typed before it was first drawn do nothing: an answer
//! needs the request to have been seen.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding};
use ratatui::Frame;
use tam_proto::{Ask, AskAnswer, AskKind, AskQuestion, QuestionAnswer};

use super::input::TextInput;
use super::theme;
use super::triage::{clean, width_of};

/// The dialog's widest.
const MAX_WIDTH: u16 = 100;

pub struct AnswerDialog {
    /// The member's agent (agent ids are member ids).
    pub agent: String,
    /// How to name the member (`<task>/<member>`).
    pub label: String,
    /// The request as shown when the dialog opened: the one it answers.
    pub ask: Ask,
    pub step: Step,
    /// Drawn at least once.
    pub shown: bool,
    /// First row of the request shown (when it is longer than the dialog).
    pub scroll: usize,
}

/// Where the dialog is.
pub enum Step {
    /// A permission or a plan: allow, or say no.
    Decide,
    /// Saying no: the reason, which may stay empty.
    Reason(TextInput),
    Questions(Questions),
}

/// Answering questions, one at a time.
pub struct Questions {
    /// The question shown; the count of questions is the review of the
    /// answers before they are sent (with several questions).
    pub current: usize,
    /// The option highlighted; the count of options is the row to type
    /// an answer of one's own.
    pub highlighted: usize,
    /// The answers so far, one per question.
    pub answers: Vec<QuestionAnswer>,
    /// Typing an answer of one's own to the current question.
    pub typing: Option<TextInput>,
}

/// What a key does to the dialog.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    /// Close it, sending nothing.
    Close,
    /// Close it and send this answer.
    Send(AskAnswer),
}

impl AnswerDialog {
    pub fn new(agent: String, label: String, ask: Ask) -> Self {
        let step = match &ask.kind {
            AskKind::Questions { questions } => Step::Questions(Questions {
                current: 0,
                highlighted: 0,
                answers: vec![QuestionAnswer::default(); questions.len()],
                typing: None,
            }),
            _ => Step::Decide,
        };
        Self {
            agent,
            label,
            ask,
            step,
            shown: false,
            scroll: 0,
        }
    }

    /// Handle a key. `stale`: the request is no longer the agent's.
    pub fn key(&mut self, key: KeyEvent, stale: bool) -> Outcome {
        if stale {
            return Outcome::Close;
        }
        if !self.shown {
            return Outcome::Stay;
        }
        let Self {
            ask, step, scroll, ..
        } = self;
        let cut = cut(ask);
        match step {
            Step::Decide => match key.code {
                // What was not shown is not allowed (the daemon refuses it).
                KeyCode::Char('y') if !cut => return Outcome::Send(AskAnswer::Allow),
                KeyCode::Char('n') => *step = Step::Reason(TextInput::default()),
                KeyCode::Esc => return Outcome::Close,
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *scroll += 1,
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::PageDown => *scroll += 10,
                KeyCode::Home => *scroll = 0,
                // Brought back to the last page when drawn.
                KeyCode::End => *scroll = usize::MAX,
                _ => {}
            },
            Step::Reason(reason) => match key.code {
                KeyCode::Enter => {
                    let reason = reason.value().trim();
                    return Outcome::Send(AskAnswer::Deny {
                        reason: (!reason.is_empty()).then(|| reason.to_string()),
                    });
                }
                KeyCode::Esc => *step = Step::Decide,
                _ => {
                    reason.handle(key);
                }
            },
            Step::Questions(q) => {
                let AskKind::Questions { questions } = &ask.kind else {
                    return Outcome::Close;
                };
                return q.key(key, questions);
            }
        }
        Outcome::Stay
    }

    /// The keys of the dialog where it is, for the footer.
    pub fn hints(&self, stale: bool) -> Vec<(&'static str, &'static str)> {
        if stale {
            return vec![("any", "close")];
        }
        let several =
            |q: &AskKind| matches!(q, AskKind::Questions { questions } if questions.len() > 1);
        match &self.step {
            Step::Decide if cut(&self.ask) => {
                vec![("n", "say no"), ("↑/↓", "scroll"), ("esc", "cancel")]
            }
            Step::Decide => {
                let allow = match self.ask.kind {
                    AskKind::Plan { .. } => "approve",
                    _ => "allow once",
                };
                vec![
                    ("y", allow),
                    ("n", "say no"),
                    ("↑/↓", "scroll"),
                    ("esc", "cancel"),
                ]
            }
            Step::Reason(_) => vec![("enter", "send the no"), ("esc", "back")],
            Step::Questions(q) if q.typing.is_some() => vec![("enter", "ok"), ("esc", "back")],
            Step::Questions(q) if q.current == q.answers.len() => {
                vec![("enter", "send"), ("←", "change"), ("esc", "cancel")]
            }
            Step::Questions(q) => {
                let multi = match &self.ask.kind {
                    AskKind::Questions { questions } => questions[q.current].multi_select,
                    _ => false,
                };
                let mut hints = vec![("↑/↓", "choose")];
                if multi {
                    hints.push(("space", "tick"));
                    hints.push(("enter", "done"));
                } else {
                    hints.push(("enter", "select"));
                }
                if several(&self.ask.kind) {
                    hints.push(("←/→", "question"));
                }
                hints.push(("esc", "cancel"));
                hints
            }
        }
    }
}

impl Questions {
    fn key(&mut self, key: KeyEvent, questions: &[AskQuestion]) -> Outcome {
        let count = questions.len();
        if let Some(input) = &mut self.typing {
            match key.code {
                KeyCode::Enter => {
                    let text = input.value().trim().to_string();
                    self.typing = None;
                    let multi = questions[self.current].multi_select;
                    let answer = &mut self.answers[self.current];
                    if text.is_empty() {
                        answer.other = None;
                        return Outcome::Stay;
                    }
                    // The question is answered (ticked options stay with it).
                    answer.other = Some(text);
                    if !multi {
                        answer.chosen.clear();
                    }
                    return self.next(count);
                }
                KeyCode::Esc => self.typing = None,
                _ => {
                    input.handle(key);
                }
            }
            return Outcome::Stay;
        }
        if self.current == count {
            // Reviewing the answers.
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    return match self.answers.iter().position(|a| !answered(a)) {
                        Some(i) => {
                            self.go(i, questions);
                            Outcome::Stay
                        }
                        None => Outcome::Send(AskAnswer::Answers {
                            answers: self.answers.clone(),
                        }),
                    };
                }
                KeyCode::Left | KeyCode::BackTab => self.go(count - 1, questions),
                KeyCode::Esc => return Outcome::Close,
                _ => {}
            }
            return Outcome::Stay;
        }
        let question = &questions[self.current];
        let rows = question.options.len() + 1;
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.highlighted = (self.highlighted + rows - 1) % rows
            }
            KeyCode::Down | KeyCode::Char('j') => self.highlighted = (self.highlighted + 1) % rows,
            KeyCode::Left | KeyCode::BackTab if count > 1 && self.current > 0 => {
                self.go(self.current - 1, questions)
            }
            KeyCode::Right | KeyCode::Tab if count > 1 => self.go(self.current + 1, questions),
            KeyCode::Char(c @ '1'..='9') => {
                let row = (c as usize) - ('1' as usize);
                if row < rows {
                    self.highlighted = row;
                    if question.multi_select && row < question.options.len() {
                        self.toggle(question);
                    } else {
                        return self.pick(question, count);
                    }
                }
            }
            KeyCode::Char(' ') if question.multi_select => self.toggle(question),
            KeyCode::Enter => return self.pick(question, count),
            _ => {}
        }
        Outcome::Stay
    }

    /// Enter on the highlighted row: type an answer, or take the option
    /// (a multi-select question takes the ticked ones, else the highlighted
    /// one) and go on.
    fn pick(&mut self, question: &AskQuestion, count: usize) -> Outcome {
        let Some(option) = question.options.get(self.highlighted) else {
            let typed = self.answers[self.current].other.as_deref().unwrap_or("");
            self.typing = Some(TextInput::from(typed));
            return Outcome::Stay;
        };
        let answer = &mut self.answers[self.current];
        if !question.multi_select {
            answer.chosen = vec![option.label.clone()];
            answer.other = None;
        } else if !answered(answer) {
            answer.chosen = vec![option.label.clone()];
        }
        self.next(count)
    }

    /// Tick or untick the highlighted option of a multi-select question
    /// (the row to type an answer opens it).
    fn toggle(&mut self, question: &AskQuestion) {
        let Some(option) = question.options.get(self.highlighted) else {
            let typed = self.answers[self.current].other.as_deref().unwrap_or("");
            self.typing = Some(TextInput::from(typed));
            return;
        };
        let answer = &mut self.answers[self.current];
        let ticked = !answer.chosen.contains(&option.label);
        let mut chosen: Vec<String> = question
            .options
            .iter()
            .map(|o| &o.label)
            .filter(|l| {
                if **l == option.label {
                    ticked
                } else {
                    answer.chosen.contains(l)
                }
            })
            .cloned()
            .collect();
        chosen.dedup();
        answer.chosen = chosen;
    }

    /// The current question is answered: go to the next one not answered,
    /// else send (one question) or review (several).
    fn next(&mut self, count: usize) -> Outcome {
        if count == 1 {
            return Outcome::Send(AskAnswer::Answers {
                answers: self.answers.clone(),
            });
        }
        let next = (self.current + 1..count)
            .chain(0..self.current)
            .find(|&i| !answered(&self.answers[i]))
            .unwrap_or(count);
        self.current = next;
        self.highlighted = 0;
        Outcome::Stay
    }

    /// Show question `i` (the count of questions: the review), its first
    /// chosen option highlighted.
    fn go(&mut self, i: usize, questions: &[AskQuestion]) {
        self.current = i.min(questions.len());
        self.highlighted = questions
            .get(self.current)
            .and_then(|q| {
                let chosen = self.answers[self.current].chosen.first()?;
                q.options.iter().position(|o| &o.label == chosen)
            })
            .unwrap_or(0);
    }
}

/// The request was too long to come whole: it can only be said no to here.
fn cut(ask: &Ask) -> bool {
    matches!(
        ask.kind,
        AskKind::Permission { cut: true, .. } | AskKind::Plan { cut: true, .. }
    )
}

fn answered(answer: &QuestionAnswer) -> bool {
    !answer.chosen.is_empty() || answer.other.is_some()
}

/// What an answer says, for the status line.
pub fn describe(ask: &Ask, answer: &AskAnswer) -> String {
    match (answer, &ask.kind) {
        (AskAnswer::Allow, AskKind::Plan { .. }) => "plan approved".into(),
        (AskAnswer::Allow, _) => format!("{} allowed once", ask.tool),
        (AskAnswer::Deny { reason: None }, _) => "no".into(),
        (AskAnswer::Deny { reason: Some(r) }, _) => format!("no ({r})"),
        (AskAnswer::Answers { .. }, _) => "answers".into(),
    }
}

/// A row of the dialog's bottom part.
enum Foot<'a> {
    Line(Line<'static>),
    /// A text being typed, after a prompt.
    Input(&'static str, &'a TextInput),
}

/// Draw the dialog. `stale`: the request is no longer the agent's.
pub fn render(frame: &mut Frame, dialog: &mut AnswerDialog, stale: bool) {
    dialog.shown = true;
    let screen = frame.area();
    let width = screen.width.min(MAX_WIDTH);
    // Border and padding: 1 + 2 on each side.
    let inner_width = usize::from(width.saturating_sub(6)).max(1);
    let (color, head, body, highlight) = content(dialog, inner_width);
    let foot_rows = foot(dialog, stale).len();

    // Border and padding: 2 rows above and below; a blank row between parts.
    let fixed = 4 + head.len() + 1 + 1 + foot_rows;
    let height = (fixed + body.len()).min(usize::from(screen.height)) as u16;
    let body_rows = usize::from(height).saturating_sub(fixed);
    let mut scroll = dialog.scroll;
    if let Some(row) = highlight {
        // The highlighted option in view.
        if row < scroll {
            scroll = row;
        } else if row >= scroll + body_rows {
            scroll = row + 1 - body_rows.max(1);
        }
    }
    dialog.scroll = scroll.min(body.len().saturating_sub(body_rows));

    let area = screen.centered(Constraint::Length(width), Constraint::Length(height));
    frame.render_widget(Clear, area);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(theme::get().popup())
        .border_style(Style::new().fg(color))
        .title(format!(" Answer {} ", dialog.label))
        .title_style(Style::new().bold().fg(color))
        .padding(Padding::new(2, 2, 1, 1));
    if body.len() > body_rows {
        let first = dialog.scroll + 1;
        let last = (dialog.scroll + body_rows).min(body.len());
        block = block.title_bottom(
            Line::from(format!(" rows {first}–{last} of {} ", body.len())).fg(theme::get().muted),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let shown_body = body.into_iter().skip(dialog.scroll).take(body_rows);
    let rows: Vec<Foot> = head
        .into_iter()
        .chain([Line::raw("")])
        .chain(shown_body)
        .chain([Line::raw("")])
        .map(Foot::Line)
        .chain(foot(dialog, stale))
        .collect();
    for (y, part) in (inner.y..inner.y + inner.height).zip(rows) {
        match part {
            Foot::Line(line) => {
                frame.render_widget(line, Rect::new(inner.x, y, inner.width, 1));
            }
            Foot::Input(prompt, input) => {
                let prompt_width = (width_of(prompt) as u16).min(inner.width);
                frame.render_widget(
                    Span::styled(prompt, Style::new().fg(theme::get().accent)),
                    Rect::new(inner.x, y, prompt_width, 1),
                );
                let text = Rect::new(inner.x + prompt_width, y, inner.width - prompt_width, 1);
                input.render(frame, text, Style::new(), true);
            }
        }
    }
}

/// The dialog's color, its head (what is asked), its body (the request, in
/// rows of `width` columns) and the body row of the highlighted option.
fn content(
    dialog: &AnswerDialog,
    width: usize,
) -> (Color, Vec<Line<'static>>, Vec<Line<'static>>, Option<usize>) {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(theme::get().muted);
    let cut_note = |body: &mut Vec<Line<'static>>, cut: bool| {
        if cut {
            body.push(Line::styled("(cut here: its terminal shows the rest)", dim));
        }
    };
    match &dialog.ask.kind {
        AskKind::Permission { input, cut } => {
            let head = vec![Line::from(vec![
                Span::raw("Allow "),
                Span::styled(dialog.ask.tool.clone(), bold),
                Span::raw(" to run with this input?"),
            ])];
            let mut body: Vec<Line> = rows(input, width).into_iter().map(Line::raw).collect();
            cut_note(&mut body, *cut);
            (theme::get().bad, head, body, None)
        }
        AskKind::Plan { plan, cut } => {
            let head = vec![
                Line::styled("Approve this plan?", bold),
                Line::styled(
                    "Approved, the agent leaves plan mode; edits keep asking.",
                    dim,
                ),
            ];
            let mut body: Vec<Line> = rows(plan, width).into_iter().map(Line::raw).collect();
            cut_note(&mut body, *cut);
            (theme::get().bad, head, body, None)
        }
        AskKind::Questions { questions } => {
            let Step::Questions(q) = &dialog.step else {
                return (theme::get().question, Vec::new(), Vec::new(), None);
            };
            let (head, body, highlight) = questions_view(questions, q, width);
            (theme::get().question, head, body, highlight)
        }
    }
}

/// Questions: a tab per question (with several), the current one with its
/// options, or the review of the answers.
fn questions_view(
    questions: &[AskQuestion],
    q: &Questions,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Option<usize>) {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(theme::get().muted);
    let mut head = Vec::new();
    if questions.len() > 1 {
        let mut tabs = Vec::new();
        for (i, question) in questions.iter().enumerate() {
            let name = question
                .header
                .clone()
                .unwrap_or_else(|| format!("Question {}", i + 1));
            let mark = if answered(&q.answers[i]) { "✓ " } else { "" };
            let style = if i == q.current {
                bold.underlined()
            } else {
                dim
            };
            tabs.push(Span::styled(format!("{mark}{}", shown(&name)), style));
            tabs.push(Span::styled(" │ ", dim));
        }
        let review = if q.current == questions.len() {
            bold.underlined()
        } else {
            dim
        };
        tabs.push(Span::styled("Send", review));
        head.push(Line::from(tabs));
    }

    let mut body = Vec::new();
    if q.current == questions.len() {
        head.push(Line::styled("Send these answers?", bold));
        for (question, answer) in questions.iter().zip(&q.answers) {
            body.extend(
                rows(&question.question, width)
                    .into_iter()
                    .map(|r| Line::styled(r, bold)),
            );
            let text = answer
                .chosen
                .iter()
                .cloned()
                .chain(answer.other.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let text = if text.is_empty() {
                "(no answer)".into()
            } else {
                text
            };
            body.extend(
                rows(&format!("→ {text}"), width.saturating_sub(2))
                    .into_iter()
                    .map(|r| Line::raw(format!("  {r}"))),
            );
        }
        return (head, body, None);
    }

    let question = &questions[q.current];
    let answer = &q.answers[q.current];
    head.extend(
        rows(&question.question, width)
            .into_iter()
            .map(|r| Line::styled(r, bold)),
    );
    if question.multi_select {
        head.push(Line::styled("Several answers can be ticked.", dim));
    }
    let mut highlight = None;
    for (i, option) in question.options.iter().enumerate() {
        let on = i == q.highlighted;
        if on {
            highlight = Some(body.len());
        }
        let tick = match (question.multi_select, answer.chosen.contains(&option.label)) {
            (true, true) => "[x] ",
            (true, false) => "[ ] ",
            (false, true) => "● ",
            (false, false) => "",
        };
        let prefix = format!("{}{}. {tick}", if on { "❯ " } else { "  " }, i + 1);
        let indent = width_of(&prefix);
        let label_rows = rows(&option.label, width.saturating_sub(indent).max(1));
        for (j, r) in label_rows.into_iter().enumerate() {
            let lead = if j == 0 {
                prefix.clone()
            } else {
                " ".repeat(indent)
            };
            let style = if on {
                bold.fg(theme::get().question)
            } else {
                Style::new()
            };
            body.push(Line::from(vec![
                Span::styled(lead, style),
                Span::styled(r, style),
            ]));
        }
        // Not when it only repeats the label (as models often write them).
        if let Some(description) = option.description.as_ref().filter(|d| **d != option.label) {
            for r in rows(description, width.saturating_sub(indent).max(1)) {
                body.push(Line::styled(format!("{}{r}", " ".repeat(indent)), dim));
            }
        }
    }
    let own = question.options.len();
    let on = q.highlighted == own;
    if on {
        highlight = Some(body.len());
    }
    let text = match &answer.other {
        Some(other) => format!("Your answer: {}", shown(other)),
        None => "Type an answer".into(),
    };
    let style = if on {
        bold.fg(theme::get().question)
    } else {
        Style::new()
    };
    body.push(Line::styled(
        format!("{}{}. {text}", if on { "❯ " } else { "  " }, own + 1),
        style,
    ));
    (head, body, highlight)
}

/// The dialog's bottom rows: what the keys do, the text being typed, or
/// why nothing can be sent.
fn foot(dialog: &AnswerDialog, stale: bool) -> Vec<Foot<'_>> {
    let key = |k: &'static str, color: Color| Span::styled(k, Style::new().bold().fg(color));
    if stale {
        return vec![
            Foot::Line(Line::styled(
                "Answered or changed meanwhile: nothing will be sent.",
                Style::new().fg(theme::get().warn).bold(),
            )),
            Foot::Line(Line::styled(
                "Any key closes.",
                Style::new().fg(theme::get().muted),
            )),
        ];
    }
    match &dialog.step {
        Step::Decide if cut(&dialog.ask) => vec![
            Foot::Line(Line::styled(
                "Too long to show in full: allow it in its terminal (enter attaches).",
                Style::new().fg(theme::get().warn),
            )),
            Foot::Line(Line::from(vec![
                key("n", theme::get().bad),
                Span::raw(" say no · "),
                key("esc", Color::Reset),
                Span::raw(" cancel"),
            ])),
        ],
        Step::Decide => {
            let allow = match dialog.ask.kind {
                AskKind::Plan { .. } => " approve · ",
                _ => " allow once · ",
            };
            vec![Foot::Line(Line::from(vec![
                key("y", theme::get().ok),
                Span::raw(allow),
                key("n", theme::get().bad),
                Span::raw(" say no · "),
                key("esc", Color::Reset),
                Span::raw(" cancel"),
            ]))]
        }
        Step::Reason(reason) => vec![
            Foot::Line(Line::raw(
                "Why not? Optional: the agent reads it and goes on. Without one, it stops.",
            )),
            Foot::Input("> ", reason),
        ],
        Step::Questions(q) => match &q.typing {
            Some(input) => vec![Foot::Input("Your answer: ", input)],
            None => vec![Foot::Line(Line::from(
                dialog
                    .hints(false)
                    .into_iter()
                    .enumerate()
                    .flat_map(|(i, (k, what))| {
                        let sep = if i == 0 { "" } else { " · " };
                        [
                            Span::raw(sep),
                            key(k, Color::Reset),
                            Span::raw(format!(" {what}")),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ))],
        },
    }
}

/// An agent's text as the dialog shows it: control characters as spaces,
/// characters that hide or reorder text (zero-width, bidirectional
/// controls) made visible, so what is shown is what is answered.
fn shown(text: &str) -> String {
    clean(text)
        .chars()
        .map(|c| match c {
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}' => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// `text` in rows of at most `width` columns, its lines and indentation
/// kept (continuation rows indented as their line): cut after the last
/// word that fits, else inside the word.
fn rows(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in shown(text).lines() {
        let line = line.trim_end();
        let indent = &line[..line.len() - line.trim_start().len()];
        let hang = if width_of(indent) * 2 < width {
            indent
        } else {
            ""
        };
        let mut rest = line.to_string();
        let mut lead = indent.len();
        while width_of(&rest) > width {
            // The longest start that fits, past the indentation.
            let mut fit = 0;
            for (i, c) in rest.char_indices() {
                if width_of(&rest[..i + c.len_utf8()]) > width {
                    break;
                }
                fit = i + c.len_utf8();
            }
            if fit <= lead {
                fit = lead + rest[lead..].chars().next().map_or(1, char::len_utf8);
            }
            let cut = if rest[fit..].starts_with(' ') {
                fit
            } else {
                match rest[..fit].rfind(' ') {
                    Some(space) if space > lead => space,
                    _ => fit,
                }
            };
            out.push(rest[..cut].trim_end().to_string());
            let tail = rest[cut..].trim_start_matches(' ');
            rest = format!("{hang}{tail}");
            lead = hang.len();
        }
        out.push(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use tam_proto::AskOption;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn permission() -> Ask {
        Ask {
            id: "ask-1".into(),
            tool: "Bash".into(),
            kind: AskKind::Permission {
                input: "command: rm -rf build".into(),
                cut: false,
            },
        }
    }

    fn question(text: &str, multi: bool, labels: &[&str]) -> AskQuestion {
        AskQuestion {
            question: text.into(),
            header: None,
            options: labels
                .iter()
                .map(|l| AskOption {
                    label: l.to_string(),
                    description: None,
                })
                .collect(),
            multi_select: multi,
        }
    }

    fn questions(qs: Vec<AskQuestion>) -> Ask {
        Ask {
            id: "ask-2".into(),
            tool: "AskUserQuestion".into(),
            kind: AskKind::Questions { questions: qs },
        }
    }

    fn open(ask: Ask) -> AnswerDialog {
        let mut d = AnswerDialog::new("m-1".into(), "feat".into(), ask);
        d.shown = true;
        d
    }

    fn answers(outcome: Outcome) -> Vec<QuestionAnswer> {
        match outcome {
            Outcome::Send(AskAnswer::Answers { answers }) => answers,
            other => panic!("expected answers, got {other:?}"),
        }
    }

    fn picked(chosen: &[&str], other: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            chosen: chosen.iter().map(|c| c.to_string()).collect(),
            other: other.map(str::to_string),
        }
    }

    #[test]
    fn keys_before_the_dialog_was_seen_do_nothing() {
        let mut d = AnswerDialog::new("m-1".into(), "feat".into(), permission());
        assert_eq!(d.key(ch('y'), false), Outcome::Stay);
        d.shown = true;
        assert_eq!(d.key(ch('y'), false), Outcome::Send(AskAnswer::Allow));
    }

    #[test]
    fn a_request_cut_short_can_only_be_said_no_to() {
        let mut d = open(Ask {
            kind: AskKind::Permission {
                input: "content: …".into(),
                cut: true,
            },
            ..permission()
        });
        assert_eq!(d.key(ch('y'), false), Outcome::Stay);
        assert!(!d.hints(false).iter().any(|(k, _)| *k == "y"));
        d.key(ch('n'), false);
        assert_eq!(
            d.key(key(KeyCode::Enter), false),
            Outcome::Send(AskAnswer::Deny { reason: None })
        );
    }

    #[test]
    fn a_stale_request_takes_no_answer() {
        let mut d = open(permission());
        assert_eq!(d.key(ch('y'), true), Outcome::Close);
    }

    #[test]
    fn no_with_or_without_a_reason() {
        let mut d = open(permission());
        assert_eq!(d.key(ch('n'), false), Outcome::Stay);
        // y while typing the reason is a letter of it.
        for c in "try y".chars() {
            assert_eq!(d.key(ch(c), false), Outcome::Stay);
        }
        assert_eq!(
            d.key(key(KeyCode::Enter), false),
            Outcome::Send(AskAnswer::Deny {
                reason: Some("try y".into())
            })
        );
        let mut d = open(permission());
        d.key(ch('n'), false);
        assert_eq!(
            d.key(key(KeyCode::Enter), false),
            Outcome::Send(AskAnswer::Deny { reason: None })
        );
        // esc goes back, then cancels.
        let mut d = open(permission());
        d.key(ch('n'), false);
        assert_eq!(d.key(key(KeyCode::Esc), false), Outcome::Stay);
        assert!(matches!(d.step, Step::Decide));
        assert_eq!(d.key(key(KeyCode::Esc), false), Outcome::Close);
        // enter alone never allows.
        assert_eq!(d.key(key(KeyCode::Enter), false), Outcome::Stay);
    }

    #[test]
    fn one_question_is_sent_once_answered() {
        let mut d = open(questions(vec![question("Size?", false, &["S", "L"])]));
        d.key(key(KeyCode::Down), false);
        assert_eq!(
            answers(d.key(key(KeyCode::Enter), false)),
            [picked(&["L"], None)]
        );
        let mut d = open(questions(vec![question("Size?", false, &["S", "L"])]));
        assert_eq!(answers(d.key(ch('2'), false)), [picked(&["L"], None)]);
        // An answer of one's own, on the last row.
        let mut d = open(questions(vec![question("Size?", false, &["S", "L"])]));
        d.key(key(KeyCode::Up), false);
        assert_eq!(d.key(key(KeyCode::Enter), false), Outcome::Stay);
        for c in "XL".chars() {
            d.key(ch(c), false);
        }
        assert_eq!(
            answers(d.key(key(KeyCode::Enter), false)),
            [picked(&[], Some("XL"))]
        );
    }

    #[test]
    fn several_answers_are_ticked_and_reviewed() {
        let mut d = open(questions(vec![
            question("Colors?", true, &["Red", "Green", "Blue"]),
            question("Size?", false, &["S", "L"]),
        ]));
        d.key(ch(' '), false);
        d.key(ch('3'), false);
        d.key(ch('3'), false);
        d.key(ch('3'), false);
        // Typed too: it comes after the options, and ends the question.
        d.key(ch('4'), false);
        for c in "Teal".chars() {
            d.key(ch(c), false);
        }
        assert_eq!(d.key(key(KeyCode::Enter), false), Outcome::Stay);
        let Step::Questions(q) = &d.step else {
            panic!()
        };
        assert_eq!(q.current, 1);
        // Then the review.
        assert_eq!(d.key(ch('1'), false), Outcome::Stay);
        let Step::Questions(q) = &d.step else {
            panic!()
        };
        assert_eq!(q.current, 2);
        // Back to change one.
        d.key(key(KeyCode::Left), false);
        assert_eq!(d.key(ch('2'), false), Outcome::Stay);
        assert_eq!(
            answers(d.key(key(KeyCode::Enter), false)),
            [picked(&["Red", "Blue"], Some("Teal")), picked(&["L"], None)]
        );
    }

    #[test]
    fn the_review_sends_only_when_all_are_answered() {
        let mut d = open(questions(vec![
            question("A?", false, &["1", "2"]),
            question("B?", false, &["1", "2"]),
        ]));
        d.key(key(KeyCode::Right), false);
        d.key(key(KeyCode::Right), false);
        assert_eq!(d.key(key(KeyCode::Enter), false), Outcome::Stay);
        let Step::Questions(q) = &d.step else {
            panic!()
        };
        assert_eq!(q.current, 0);
        // Enter on a multi-select question with nothing ticked takes the
        // highlighted option.
        let mut d = open(questions(vec![question("C?", true, &["x", "y"])]));
        assert_eq!(
            answers(d.key(key(KeyCode::Enter), false)),
            [picked(&["x"], None)]
        );
    }

    #[test]
    fn rows_keep_lines_and_indentation() {
        assert_eq!(rows("a\n  b c d", 5), ["a", "  b c", "  d"]);
        assert_eq!(rows("one two three", 7), ["one two", "three"]);
        assert_eq!(rows("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(rows("x\n\ny", 4), ["x", "", "y"]);
        assert_eq!(rows("rm\u{202E}fr", 10), ["rm\u{FFFD}fr"]);
    }
}
