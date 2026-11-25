//! The preview pane: an agent's screen as the daemon's terminal emulator
//! has it, live or scrolled back into its history.

use std::num::NonZeroU16;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use tam_proto::ScreenInfo;

/// The smallest pane an agent's terminal is resized to.
const MIN_COLS: u16 = 40;
const MIN_ROWS: u16 = 10;

/// The preview pane's state.
#[derive(Default)]
pub struct Peek {
    /// The agent shown: the selected task's, when it has one running.
    pub agent: Option<String>,
    /// The agent whose screen updates the daemon sends us.
    pub watching: Option<String>,
    /// The daemon counts that watch as a viewer (the pane has the
    /// keyboard): no notification about the agent meanwhile.
    pub viewing: bool,
    /// The agent's latest screen.
    pub screen: Option<AgentScreen>,
    /// Why there is no screen.
    pub error: Option<String>,
    /// Scrolled back: the line on the pane's first row. `None` follows the
    /// live screen.
    pub from: Option<u64>,
    /// Rows of the pane at the last draw.
    pub height: u16,
    /// Columns and rows the pane has for the agent's screen when it shows
    /// nothing else (at the last draw).
    pub room: (u16, u16),
    /// The size last asked for the agent's terminal: asked once, so that
    /// two dashboards of different sizes do not fight over it.
    pub asked: Option<(u16, u16)>,
    /// Where the agent's screen was drawn at the last draw (empty when the
    /// pane showed none): where the mouse selects.
    pub area: Rect,
    /// Text selected with the mouse.
    pub selection: Option<Selection>,
}

/// A cell of the agent's screen: its line (numbered as in `ScreenInfo`:
/// a line keeps its number as it scrolls) and its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spot {
    pub line: u64,
    pub col: u16,
}

/// Text selected with the mouse in the pane, from the cell the button went
/// down on to the one it is on (or was let go on), both included, in
/// reading order. The dashboard's own: a terminal drops its selection when
/// anything is drawn on its lines (kitty: on any of the lines it spans),
/// and the dashboard redraws while an agent works. On lines, so it stays on
/// its text as the agent's output scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Spot,
    pub head: Spot,
    /// The button is still down.
    pub dragging: bool,
}

impl Selection {
    /// Its first and last cells.
    fn bounds(&self) -> (Spot, Spot) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    fn contains(&self, spot: Spot) -> bool {
        let (first, last) = self.bounds();
        first <= spot && spot <= last
    }
}

/// How to move through an agent's screen and history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    /// Lines up (`k` in scroll mode, the mouse wheel).
    Up(u16),
    /// Lines down.
    Down(u16),
    Oldest,
    Live,
}

impl Peek {
    /// Show `info`, the latest screen of the agent.
    pub fn set_screen(&mut self, info: &ScreenInfo) {
        let screen = AgentScreen::new(info);
        // A resize moves rows between the screen and its history, and the
        // alternate screen has none (the live screen's first line drops).
        if self
            .screen
            .as_ref()
            .is_some_and(|s| s.size() != screen.size() || screen.live < s.live)
        {
            self.from = None;
            // Its lines are no longer those numbered so.
            self.selection = None;
        }
        // The lines scrolled back to may have left the history.
        if let Some(from) = &mut self.from {
            *from = (*from).max(screen.oldest);
        }
        self.screen = Some(screen);
        self.error = None;
    }

    /// The size the agent's terminal should take to fill the pane, when it
    /// has another and was not asked for this one yet.
    pub fn wanted_size(&self) -> Option<(u16, u16)> {
        let (cols, rows) = self.room;
        let (has_rows, has_cols) = self.screen.as_ref()?.size();
        // Too small to be a terminal: the agent keeps its size.
        if cols < MIN_COLS || rows < MIN_ROWS {
            return None;
        }
        ((has_cols, has_rows) != self.room && self.asked != Some(self.room)).then_some(self.room)
    }

    /// When scrolled back: how many lines of history the pane shows above
    /// the live screen (0 when it shows part of the live screen only).
    pub fn scrolled_back(&self) -> Option<u64> {
        let screen = self.screen.as_ref()?;
        self.from?;
        Some(screen.live.saturating_sub(self.first_line(screen)))
    }

    /// Line on the pane's first row (once the screen asked for came).
    fn first_line(&self, screen: &AgentScreen) -> u64 {
        match self.from {
            Some(line) => line,
            None => screen.top + u64::from(screen.first_row(None, self.height)),
        }
    }

    pub fn scroll(&mut self, scroll: Scroll) {
        let Some(screen) = &self.screen else {
            return;
        };
        let shown = self.first_line(screen);
        // Live again once the pane would reach the live screen's bottom.
        let bottom = screen.live + u64::from(screen.size().0);
        let height = u64::from(self.height);
        let up = |lines: u64| {
            let line = shown.saturating_sub(lines).max(screen.oldest);
            if line == shown {
                self.from
            } else {
                Some(line)
            }
        };
        let down = |lines: u64| {
            self.from
                .map(|_| shown + lines)
                .filter(|line| line + height < bottom)
        };
        self.from = match scroll {
            Scroll::Up(lines) => up(u64::from(lines)),
            Scroll::Down(lines) => down(u64::from(lines)),
            Scroll::Oldest if screen.oldest < shown => Some(screen.oldest),
            Scroll::Oldest => self.from,
            Scroll::Live => None,
        };
    }

    /// The cell of the agent's screen drawn at `(x, y)`; outside the
    /// screen's area (a drag past the pane's edge), the nearest one.
    fn spot_at(&self, x: u16, y: u16) -> Option<Spot> {
        let screen = self.screen.as_ref()?;
        let area = self.area;
        let (rows, cols) = screen.size();
        let width = area.width.min(cols);
        if width == 0 || area.height == 0 {
            return None;
        }
        let x = x.clamp(area.x, area.x + width - 1);
        let y = y.clamp(area.y, area.bottom() - 1);
        let first = screen.first_row(self.from, area.height);
        let row = (first + (y - area.y)).min(rows - 1);
        Some(Spot {
            line: screen.top + u64::from(row),
            col: x - area.x,
        })
    }

    /// The button went down at `(x, y)`: on the agent's screen, a new
    /// selection, empty until dragged (`false` elsewhere).
    pub fn select_from(&mut self, x: u16, y: u16) -> bool {
        let on_screen = self.area.contains(ratatui::layout::Position::new(x, y));
        let Some(spot) = self.spot_at(x, y).filter(|_| on_screen) else {
            return false;
        };
        self.selection = Some(Selection {
            anchor: spot,
            head: spot,
            dragging: true,
        });
        true
    }

    /// Dragged to `(x, y)`: `false` when no selection is being made.
    pub fn select_to(&mut self, x: u16, y: u16) -> bool {
        let spot = self.spot_at(x, y);
        let Some(selection) = self.selection.as_mut().filter(|s| s.dragging) else {
            return false;
        };
        if let Some(spot) = spot {
            selection.head = spot;
        }
        true
    }

    /// Let go at `(x, y)`: the text selected. A click alone selects nothing
    /// (nor do blanks), and leaves no selection.
    pub fn select_end(&mut self, x: u16, y: u16) -> Option<String> {
        self.select_to(x, y);
        let selection = self.selection.as_mut().filter(|s| s.dragging)?;
        selection.dragging = false;
        let text = (selection.anchor != selection.head)
            .then(|| self.selected_text())
            .flatten();
        if text.is_none() {
            self.selection = None;
        }
        text
    }

    /// Part of the selection is out of the window the pane has (scrolled
    /// into history while dragging, the output or the wheel moving it):
    /// `selected_text` has the rest only.
    pub fn selection_cut(&self) -> bool {
        let (Some(screen), Some(selection)) = (&self.screen, self.selection) else {
            return false;
        };
        let (first, last) = selection.bounds();
        first.line < screen.top || last.line >= screen.top + u64::from(screen.size().0)
    }

    /// The selected text the screen has: wrapped rows joined, no blanks
    /// at the end of a line. `None` when it is all blank.
    pub fn selected_text(&self) -> Option<String> {
        let (screen, selection) = (self.screen.as_ref()?, self.selection?);
        let (first, last) = selection.bounds();
        let (rows, cols) = screen.size();
        let top = screen.top;
        let bottom = top + u64::from(rows) - 1;
        if last.line < top || first.line > bottom {
            return None;
        }
        let (start_row, mut start_col) = match first.line.checked_sub(top) {
            Some(row) => (row as u16, first.col),
            None => (0, 0),
        };
        let (end_row, end_col) = if last.line > bottom {
            (rows - 1, cols)
        } else {
            ((last.line - top) as u16, last.col + 1)
        };
        // Half of a wide character: all of it.
        let vt = screen.screen();
        if start_col > 0
            && vt
                .cell(start_row, start_col)
                .is_some_and(vt100::Cell::is_wide_continuation)
        {
            start_col -= 1;
        }
        let text = vt.contents_between(start_row, start_col, end_row, end_col.min(cols));
        (!text.trim().is_empty()).then_some(text)
    }

    /// The selection over the screen as drawn in `area`: its cells in
    /// reverse video, as terminals show theirs (the theme paints nothing
    /// behind the agent's screen).
    pub fn draw_selection(&self, buf: &mut Buffer) {
        let (Some(screen), Some(selection)) = (&self.screen, self.selection) else {
            return;
        };
        let area = self.area;
        let first = screen.first_row(self.from, area.height);
        let (rows, cols) = screen.size();
        let width = area.width.min(cols);
        for (y, row) in (area.y..area.bottom()).zip(first..rows) {
            let line = screen.top + u64::from(row);
            for col in 0..width {
                if selection.contains(Spot { line, col }) {
                    buf[(area.x + col, y)].modifier.toggle(Modifier::REVERSED);
                }
            }
        }
    }
}

/// An agent's screen (a window of `rows` lines), drawn from a reply.
pub struct AgentScreen {
    parser: vt100::Parser,
    /// Line numbers, see `ScreenInfo`.
    top: u64,
    oldest: u64,
    live: u64,
}

impl AgentScreen {
    /// The agent asked for application cursor keys.
    pub fn application_cursor(&self) -> bool {
        self.screen().application_cursor()
    }

    /// The agent asked for pastes between markers.
    pub fn bracketed_paste(&self) -> bool {
        self.screen().bracketed_paste()
    }

    /// Where the agent's cursor is on its screen, `(row, column)`; `None`
    /// when it hides it.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        let screen = self.screen();
        (!screen.hide_cursor()).then(|| screen.cursor_position())
    }

    pub fn new(info: &ScreenInfo) -> Self {
        let size = |n: u16| NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN);
        let mut parser = vt100::Parser::new(size(info.rows), size(info.cols), 0);
        parser.process(info.contents.as_bytes());
        Self {
            parser,
            top: info.top,
            oldest: info.oldest,
            live: info.live,
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// `(rows, cols)`.
    fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.screen().size();
        (rows.get(), cols.get())
    }

    /// Rows down to the last one drawn on, or to the cursor when visible.
    fn content_end(&self) -> u16 {
        let screen = self.screen();
        let (rows, cols) = self.size();
        let drawn = |cell: &vt100::Cell| {
            !cell.contents().trim().is_empty()
                || cell.bgcolor() != vt100::Color::Default
                || cell.inverse()
        };
        let last = (0..rows)
            .rev()
            .find(|&row| (0..cols).any(|col| screen.cell(row, col).is_some_and(drawn)));
        let end = last.map_or(0, |row| row + 1);
        if screen.hide_cursor() {
            end
        } else {
            end.max(screen.cursor_position().0 + 1)
        }
    }

    /// The screen row on the first row of a pane `height` rows tall. Live,
    /// the content's last rows when they don't all fit; scrolled back, the
    /// row of line `from`.
    pub fn first_row(&self, from: Option<u64>, height: u16) -> u16 {
        match from {
            None => self.content_end().saturating_sub(height),
            Some(line) => {
                let last = u64::from(self.size().0 - 1);
                line.saturating_sub(self.top).min(last) as u16
            }
        }
    }
}

/// Rows `first..` of a screen, from the area's top-left corner, cut at its
/// right edge.
pub struct ScreenView<'a> {
    pub screen: &'a vt100::Screen,
    pub first: u16,
}

impl Widget for ScreenView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        let width = area.width.min(cols.get());
        for (y, row) in (area.y..area.bottom()).zip(self.first..rows.get()) {
            let mut col = 0;
            while col < width {
                let Some(cell) = self.screen.cell(row, col) else {
                    break;
                };
                let x = area.x + col;
                let style = cell_style(cell);
                if cell.is_wide_continuation() {
                    // Drawn with the wide character before it.
                    col += 1;
                    continue;
                }
                if cell.is_wide() && col + 1 >= width {
                    // Half of it would show.
                    buf[(x, y)].set_symbol(" ").set_style(style);
                    break;
                }
                let symbol = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                buf[(x, y)].set_symbol(symbol).set_style(style);
                if cell.is_wide() {
                    buf[(x + 1, y)].reset();
                    col += 1;
                }
                col += 1;
            }
        }
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::new()
        .fg(color(cell.fgcolor()))
        .bg(color(cell.bgcolor()));
    for (on, modifier) in [
        (cell.bold(), Modifier::BOLD),
        (cell.dim(), Modifier::DIM),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
        (cell.inverse(), Modifier::REVERSED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    style
}

fn color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_is_asked_once_for_the_size_of_the_pane() {
        let screen = |cols, rows| ScreenInfo {
            cols,
            rows,
            contents: String::new(),
            top: 0,
            oldest: 0,
            live: 0,
        };
        let mut peek = Peek::default();
        assert_eq!(peek.wanted_size(), None, "no screen yet");
        peek.set_screen(&screen(80, 24));
        assert_eq!(peek.wanted_size(), None, "not drawn yet");
        peek.room = (120, 40);
        assert_eq!(peek.wanted_size(), Some((120, 40)));
        // Asked: not again, even if another viewer resized it since.
        peek.asked = Some((120, 40));
        assert_eq!(peek.wanted_size(), None);
        // The pane changed: asked again.
        peek.room = (100, 40);
        assert_eq!(peek.wanted_size(), Some((100, 40)));
        peek.set_screen(&screen(100, 40));
        assert_eq!(peek.wanted_size(), None, "it fits");
        // A pane too small to be a terminal leaves the agent alone.
        peek.room = (30, 8);
        assert_eq!(peek.wanted_size(), None);
    }

    /// A reply for a `cols`×`rows` screen showing `output`.
    fn info(cols: u16, rows: u16, output: &str, top: u64, oldest: u64, live: u64) -> ScreenInfo {
        let size = |n: u16| NonZeroU16::new(n).unwrap();
        let mut parser = vt100::Parser::new(size(rows), size(cols), 0);
        parser.process(output.as_bytes());
        ScreenInfo {
            cols,
            rows,
            contents: parser.screen().contents_formatted(),
            top,
            oldest,
            live,
        }
    }

    fn lines(range: std::ops::RangeInclusive<u32>) -> String {
        range.map(|i| format!("line {i}\r\n")).collect()
    }

    /// The lines, without a newline after the last.
    fn rows(range: std::ops::RangeInclusive<u32>) -> String {
        let lines: Vec<String> = range.map(|i| format!("line {i}")).collect();
        lines.join("\r\n")
    }

    /// The pane's text: `height` rows of `width`.
    fn draw(peek: &Peek, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let screen = peek.screen.as_ref().unwrap();
        ScreenView {
            screen: screen.screen(),
            first: screen.first_row(peek.from, height),
        }
        .render(area, &mut buf);
        (0..height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < width {
                    let symbol = buf[(x, y)].symbol();
                    row.push_str(symbol);
                    // The cell after a wide character is not drawn.
                    x += ratatui::text::Span::raw(symbol).width().max(1) as u16;
                }
                row.trim_end().to_string()
            })
            .collect()
    }

    fn peek(info: &ScreenInfo, height: u16) -> Peek {
        let mut peek = Peek {
            height,
            ..Default::default()
        };
        peek.set_screen(info);
        peek
    }

    #[test]
    fn live_view_shows_the_bottom_of_the_content_cut_to_the_pane() {
        // 10 rows, content on the first 6: the pane (3 rows) shows 4-6.
        let screen = info(30, 10, &format!("\x1b[?25l{}", lines(1..=6)), 0, 0, 0);
        let p = peek(&screen, 3);
        assert_eq!(draw(&p, 30, 3), ["line 4", "line 5", "line 6"]);
        // Narrower than the screen: cut on the right.
        assert_eq!(draw(&p, 5, 3), ["line", "line", "line"]);
        // Taller than the content: from the top.
        assert_eq!(
            draw(&p, 30, 8)[..7],
            ["line 1", "line 2", "line 3", "line 4", "line 5", "line 6", ""]
        );
        // A visible cursor counts as content.
        let screen = info(30, 10, &format!("{}\r\n\r\n$ ", lines(1..=2)), 0, 0, 0);
        assert_eq!(draw(&peek(&screen, 2), 30, 2), ["", "$"]);
    }

    #[test]
    fn wide_characters_are_drawn_once_and_never_halved() {
        let screen = info(10, 2, "a漢b", 0, 0, 0);
        let p = peek(&screen, 2);
        assert_eq!(draw(&p, 10, 2)[0], "a漢b");
        // The pane ends in the middle of the wide character.
        assert_eq!(draw(&p, 2, 2)[0], "a");
    }

    #[test]
    fn lines_move_through_history_and_back_to_live() {
        // A 4-row screen, line N reading "line N+1": lines 20-23 on the
        // screen (the cursor's row blank), 10-19 in history.
        let screen = info(20, 4, &lines(21..=23), 20, 10, 20);
        let mut p = peek(&screen, 3);
        // The pane shows the content's bottom, cursor included: 21-23.
        assert_eq!(draw(&p, 20, 3), ["line 22", "line 23", ""]);
        p.scroll(Scroll::Down(1));
        assert_eq!(p.from, None, "already live");

        // A line up from what the pane shows.
        p.scroll(Scroll::Up(1));
        assert_eq!(p.from, Some(20));
        assert_eq!(p.scrolled_back(), Some(0));
        p.scroll(Scroll::Up(1));
        assert_eq!(p.from, Some(19));
        assert_eq!(p.scrolled_back(), Some(1));
        for _ in 0..20 {
            p.scroll(Scroll::Up(1));
        }
        // Stops at the oldest line kept.
        assert_eq!(p.from, Some(10));
        p.scroll(Scroll::Down(1));
        assert_eq!(p.from, Some(11));
        p.scroll(Scroll::Live);
        assert_eq!(p.from, None);
        p.scroll(Scroll::Oldest);
        assert_eq!(p.from, Some(10));
        for _ in 0..10 {
            p.scroll(Scroll::Down(1));
        }
        assert_eq!(p.from, Some(20));
        // The next line reaches the live screen's bottom: live again.
        p.scroll(Scroll::Down(1));
        assert_eq!(p.from, None);
    }

    #[test]
    fn the_wheel_scrolls_by_lines() {
        let mut p = peek(&info(20, 4, &lines(21..=23), 20, 10, 20), 3);
        p.scroll(Scroll::Up(3));
        assert_eq!(p.from, Some(18));
        p.scroll(Scroll::Up(20));
        assert_eq!(p.from, Some(10), "stops at the oldest line");
        p.scroll(Scroll::Down(3));
        assert_eq!(p.from, Some(13));
        for _ in 0..3 {
            p.scroll(Scroll::Down(3));
        }
        assert_eq!(p.from, None, "live again at the bottom");
    }

    #[test]
    fn scrolled_back_view_stays_on_its_lines() {
        let mut p = peek(&info(20, 4, &lines(21..=23), 20, 10, 20), 3);
        p.scroll(Scroll::Up(2));
        assert_eq!(p.from, Some(19));
        // The daemon's window from line 19, after more output.
        p.set_screen(&info(20, 4, &rows(20..=23), 19, 10, 25));
        assert_eq!(draw(&p, 20, 3), ["line 20", "line 21", "line 22"]);
        // Line 19 left the history: the oldest kept.
        p.set_screen(&info(20, 4, &lines(21..=24), 20, 20, 30));
        assert_eq!(p.from, Some(20));
        // A resize: live again.
        p.set_screen(&info(30, 5, &lines(21..=24), 30, 20, 30));
        assert_eq!(p.from, None);
        // So is the agent switching to its alternate screen.
        p.scroll(Scroll::Oldest);
        assert_eq!(p.from, Some(20));
        p.set_screen(&info(30, 5, "full screen", 20, 20, 20));
        assert_eq!(p.from, None);
    }

    #[test]
    fn nothing_to_scroll_stays_live() {
        let mut p = peek(&info(20, 10, &lines(1..=3), 0, 0, 0), 5);
        p.scroll(Scroll::Up(1));
        p.scroll(Scroll::Oldest);
        assert_eq!(p.from, None);
    }

    /// `p` drawn in `area` (where the mouse finds it), its selection over.
    fn drawn(p: &mut Peek, area: Rect) -> Buffer {
        p.area = area;
        p.height = area.height;
        let mut buf = Buffer::empty(Rect::new(0, 0, area.right() + 2, area.bottom() + 2));
        let screen = p.screen.as_ref().unwrap();
        ScreenView {
            screen: screen.screen(),
            first: screen.first_row(p.from, area.height),
        }
        .render(area, &mut buf);
        p.draw_selection(&mut buf);
        buf
    }

    /// The columns of row `y` of `area` drawn in reverse video.
    fn reversed(buf: &Buffer, area: Rect, y: u16) -> Vec<u16> {
        (0..area.width)
            .filter(|&x| {
                buf[(area.x + x, area.y + y)]
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect()
    }

    #[test]
    fn a_drag_selects_text_that_stays_on_its_lines() {
        // Lines 10-13 read "line 1" to "line 4", drawn from (2, 1).
        let mut p = peek(
            &info(20, 4, &format!("\x1b[?25l{}", rows(1..=4)), 10, 0, 10),
            4,
        );
        let area = Rect::new(2, 1, 20, 4);
        drawn(&mut p, area);
        // From the "1" of line 10 to the "e" of line 12, both included.
        assert!(p.select_from(2 + 5, 1));
        assert!(p.select_to(2 + 3, 1 + 2));
        assert_eq!(
            p.select_end(2 + 3, 1 + 2).as_deref(),
            Some("1\nline 2\nline")
        );
        let buf = drawn(&mut p, area);
        assert_eq!(reversed(&buf, area, 0), (5..20).collect::<Vec<_>>());
        assert_eq!(reversed(&buf, area, 1), (0..20).collect::<Vec<_>>());
        assert_eq!(reversed(&buf, area, 2), [0, 1, 2, 3]);
        assert!(reversed(&buf, area, 3).is_empty());
        // Upwards: the same text.
        assert!(p.select_from(2 + 3, 1 + 2));
        assert_eq!(p.select_end(2 + 5, 1).as_deref(), Some("1\nline 2\nline"));
        assert!(!p.selection_cut());

        // More output scrolls the screen a line up: the selection goes
        // with its text, and has what the screen still shows.
        p.set_screen(&info(
            20,
            4,
            &format!("\x1b[?25l{}", rows(2..=5)),
            11,
            0,
            11,
        ));
        let buf = drawn(&mut p, area);
        assert_eq!(reversed(&buf, area, 0), (0..20).collect::<Vec<_>>());
        assert_eq!(reversed(&buf, area, 1), [0, 1, 2, 3]);
        assert!(reversed(&buf, area, 2).is_empty());
        assert_eq!(p.selected_text().as_deref(), Some("line 2\nline"));
        assert!(p.selection_cut(), "line 10 left the window");
        // A resize: the lines are no longer those numbered so.
        p.set_screen(&info(30, 4, &rows(2..=5), 11, 0, 11));
        assert_eq!(p.selection, None);
    }

    #[test]
    fn a_selection_takes_whole_characters_and_joins_wrapped_rows() {
        let area = Rect::new(0, 0, 10, 3);
        // A click alone selects nothing, and leaves no selection.
        let mut p = peek(&info(10, 3, "abcdefghijklmno", 0, 0, 0), 3);
        drawn(&mut p, area);
        assert!(p.select_from(3, 0));
        assert_eq!(p.select_end(3, 0), None);
        assert_eq!(p.selection, None);
        // Nor does a drag over blanks.
        assert!(p.select_from(0, 2));
        assert_eq!(p.select_end(5, 2), None);
        // Outside the screen, a click starts nothing, a drag nothing either.
        assert!(!p.select_from(10, 0));
        assert!(!p.select_to(5, 1));
        // A row the line wraps from: one line.
        assert!(p.select_from(2, 0));
        assert_eq!(p.select_end(1, 1).as_deref(), Some("cdefghijkl"));
        // Dragged past the pane's edges: to its last cell, then its first.
        assert!(p.select_from(2, 0));
        assert!(p.select_to(40, 40));
        assert_eq!(p.selection.unwrap().head, Spot { line: 2, col: 9 });
        assert_eq!(p.select_end(0, 0).as_deref(), Some("abc"));

        // From the second half of a wide character: all of it.
        let mut p = peek(&info(10, 2, "a漢b c", 0, 0, 0), 2);
        drawn(&mut p, Rect::new(0, 0, 10, 2));
        assert!(p.select_from(2, 0));
        assert_eq!(p.select_end(3, 0).as_deref(), Some("漢b"));
    }

    #[test]
    fn cells_keep_their_style() {
        let screen = info(20, 2, "\x1b[1;31mred\x1b[m \x1b[7mrev", 0, 0, 0);
        let p = peek(&screen, 2);
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let s = p.screen.as_ref().unwrap();
        ScreenView {
            screen: s.screen(),
            first: 0,
        }
        .render(area, &mut buf);
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1));
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(3, 0)].fg, Color::Reset);
        assert!(buf[(4, 0)].modifier.contains(Modifier::REVERSED));
    }
}
