//! The preview pane: an agent's screen as the daemon's terminal emulator
//! has it, live or scrolled back into its history.

use std::num::NonZeroU16;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use tam_proto::ScreenInfo;

/// The preview pane's state, while it is shown.
#[derive(Default)]
pub struct Peek {
    /// The agent shown: the selected task's, when it has one running.
    pub agent: Option<String>,
    /// The agent whose screen updates the daemon sends us.
    pub watching: Option<String>,
    /// The agent's latest screen.
    pub screen: Option<AgentScreen>,
    /// Why there is no screen.
    pub error: Option<String>,
    /// Scrolled back: the line on the pane's first row. `None` follows the
    /// live screen.
    pub from: Option<u64>,
    /// Rows of the pane at the last draw.
    pub height: u16,
}

/// How to move through an agent's screen and history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    PageUp,
    PageDown,
    /// Lines up (the mouse wheel).
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
        }
        // The lines scrolled back to may have left the history.
        if let Some(from) = &mut self.from {
            *from = (*from).max(screen.oldest);
        }
        self.screen = Some(screen);
        self.error = None;
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
        let page = u64::from(self.height.saturating_sub(1).max(1));
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
            Scroll::PageUp => up(page),
            Scroll::PageDown => down(page),
            Scroll::Up(lines) => up(u64::from(lines)),
            Scroll::Down(lines) => down(u64::from(lines)),
            Scroll::Oldest if screen.oldest < shown => Some(screen.oldest),
            Scroll::Oldest => self.from,
            Scroll::Live => None,
        };
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
    fn paging_moves_through_history_and_back_to_live() {
        // A 4-row screen, line N reading "line N+1": lines 20-23 on the
        // screen (the cursor's row blank), 10-19 in history.
        let screen = info(20, 4, &lines(21..=23), 20, 10, 20);
        let mut p = peek(&screen, 3);
        // The pane shows the content's bottom, cursor included: 21-23.
        assert_eq!(draw(&p, 20, 3), ["line 22", "line 23", ""]);
        p.scroll(Scroll::PageDown);
        assert_eq!(p.from, None, "already live");

        p.scroll(Scroll::PageUp);
        assert_eq!(p.from, Some(19));
        assert_eq!(p.scrolled_back(), Some(1));
        for _ in 0..5 {
            p.scroll(Scroll::PageUp);
        }
        // Stops at the oldest line kept.
        assert_eq!(p.from, Some(10));
        p.scroll(Scroll::PageUp);
        assert_eq!(p.from, Some(10));
        p.scroll(Scroll::PageDown);
        assert_eq!(p.from, Some(12));
        p.scroll(Scroll::Live);
        assert_eq!(p.from, None);
        p.scroll(Scroll::Oldest);
        assert_eq!(p.from, Some(10));
        for _ in 0..5 {
            p.scroll(Scroll::PageDown);
        }
        assert_eq!(p.from, Some(20));
        // The next page reaches the live screen's bottom: live again.
        p.scroll(Scroll::PageDown);
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
        p.scroll(Scroll::PageUp);
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
        p.scroll(Scroll::PageUp);
        p.scroll(Scroll::Oldest);
        assert_eq!(p.from, None);
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
