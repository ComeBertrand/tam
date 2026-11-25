//! The daemon's terminal emulator of an agent's PTY: the screen a viewer
//! would see, kept at the PTY's size and fed by its reader.

use std::fmt::Write as _;
use std::num::NonZeroU16;
use std::panic::{catch_unwind, AssertUnwindSafe};

use tam_proto::ScreenInfo;
use tracing::error;

/// Size of a new agent's PTY, until a viewer resizes it.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// Rows of history kept above the screen.
pub const HISTORY_ROWS: usize = 1000;

pub struct Terminal {
    parser: vt100::Parser<Lines>,
    history: usize,
}

/// Parser callbacks: count the rows dropped from the top of the main
/// screen's history, to number lines (see [`ScreenInfo`]), keep the input
/// modes the parser doesn't, and answer the program's queries.
#[derive(Default)]
struct Lines {
    dropped: u64,
    /// The agent asked for focus events (`CSI ? 1004 h`).
    focus_reporting: bool,
    /// What a terminal answers to the queries in the output fed so far, in
    /// order, until [`Terminal::feed`] hands it over.
    answers: Vec<u8>,
}

/// DEC private mode: report focus in and out.
const FOCUS_REPORTING: u16 = 1004;

impl vt100::Callbacks for Lines {
    fn on_scroll(&mut self, _row: vt100::capture::RowContents<'_>, alternate_screen: bool) {
        if !alternate_screen {
            self.dropped += 1;
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i1 == Some(b'?') && params.contains(&&[FOCUS_REPORTING][..]) {
            match c {
                'h' => self.focus_reporting = true,
                'l' => self.focus_reporting = false,
                _ => {}
            }
        }
        // Device status reports (DSR), answered where they come in the
        // output: programs wait for them (`fzf --height` reads the cursor
        // position before it draws anything).
        if i1.is_none() && c == 'n' {
            match params {
                // Status: ready.
                [[5]] => self.answers.extend_from_slice(b"\x1b[0n"),
                // Cursor position, 1-based. Past the last column (a wrap
                // pending) is the last column, as xterm says.
                [[6]] => {
                    let (row, col) = screen.cursor_position();
                    let last = screen.size().1.get() - 1;
                    let answer = format!("\x1b[{};{}R", row + 1, col.min(last) + 1);
                    self.answers.extend_from_slice(answer.as_bytes());
                }
                _ => {}
            }
        }
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_history(cols, rows, HISTORY_ROWS)
    }

    pub fn with_history(cols: u16, rows: u16, history: usize) -> Self {
        Self {
            parser: parser(cols, rows, history, Lines::default()),
            history,
        }
    }

    /// Process output of the agent. Returns what a terminal answers to the
    /// queries in it (the cursor position, the status), for the program's
    /// input: empty for most output.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.guarded(|parser| parser.process(bytes));
        std::mem::take(&mut self.parser.callbacks_mut().answers)
    }

    /// Follow a resize of the PTY. Sizes are at least 1×1.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (nonzero(cols), nonzero(rows));
        self.guarded(|parser| parser.set_size(rows, cols));
    }

    /// The live screen, or with `from` a window of history starting at that
    /// line (clamped to the history kept).
    pub fn screen(&mut self, from: Option<u64>) -> ScreenInfo {
        let oldest = self.parser.callbacks().dropped;
        let screen = self.parser.screen_mut();
        let history = screen.scrollback_len() as u64;
        let live = oldest + history;
        let back = from.map_or(0, |line| live.saturating_sub(line).min(history));
        // The offset is shared by all readers: back to the live screen
        // before anyone else looks.
        screen.set_scrollback(back as usize);
        let mut contents = screen.contents_formatted();
        // And how it wants its keys (cursor keys, pastes): a viewer that
        // types into it encodes them so.
        contents.push_str(&screen.input_mode_formatted());
        screen.set_scrollback(0);
        let (rows, cols) = screen.size();
        ScreenInfo {
            cols: cols.get(),
            rows: rows.get(),
            contents,
            top: live - back,
            oldest,
            live,
        }
    }

    /// What a viewer that just attached is sent before the agent's live
    /// output, to bring its terminal (already at the PTY's size) to the
    /// agent's state:
    /// - the history, as lines scrolled into the viewer's own scrollback
    ///   (with the viewer's screen before it). Wrapped rows are joined: the
    ///   viewer's terminal wraps them at its width. Only on the main screen:
    ///   the alternate screen has none, and the main one is out of reach.
    /// - the screen, row by row, then the cursor, the attributes and the
    ///   input modes (bracketed paste, mouse, focus events…).
    ///
    /// It draws on the rows of the viewer's screen the agent has (from the
    /// top), and erases only lines of them: rows below, such as the
    /// dashboard's status bar, stay.
    pub fn replay(&self) -> String {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (rows, cols) = (rows.get(), cols.get());
        let mut out = String::new();
        if screen.alternate_screen() {
            out.push_str("\x1b[?1049h");
        } else {
            let _ = write!(out, "\x1b[{rows};1H");
            write_history(&mut out, screen);
            // Scroll it all (and the viewer's screen when there is no
            // history) above the agent's rows.
            for _ in 0..rows {
                out.push_str("\x1b[m\r\n");
            }
        }
        for row in 0..rows {
            let _ = write!(out, "\x1b[{};1H\x1b[m\x1b[2K", row + 1);
            write_row(&mut out, screen, row, cols);
        }
        out.push_str(&screen.cursor_state_formatted());
        out.push_str(&screen.attributes_formatted());
        out.push_str(&screen.input_mode_formatted());
        if self.parser.callbacks().focus_reporting {
            out.push_str("\x1b[?1004h");
        }
        out
    }

    /// The text of the terminal, without formatting: its history (main
    /// screen), then its screen. Wrapped rows are joined.
    pub fn text(&self) -> String {
        let mut formatted = String::new();
        let mut state = vt100::capture::BasicFormattedCaptureState::new();
        let range = vt100::capture::BasicFormattedCaptureRange::Full(&mut state);
        let _ = self
            .parser
            .screen()
            .write_contents_formatted_basic(&mut formatted, range);
        let text: String = vt100::capture::basic_formatted_to_plain(&formatted).collect();
        text.trim_end().to_string()
    }

    /// Run `f` on the parser. A parser bug must not take the PTY reader
    /// (and with it the attach relay and silence detection) down: on a
    /// panic, start over on a blank screen.
    fn guarded(&mut self, f: impl FnOnce(&mut vt100::Parser<Lines>)) {
        if catch_unwind(AssertUnwindSafe(|| f(&mut self.parser))).is_ok() {
            return;
        }
        error!("terminal emulator failed on the agent's output; screen reset");
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        // Keep numbering forward: the history is gone.
        let callbacks = self.parser.callbacks();
        let lines = Lines {
            dropped: callbacks.dropped + screen.scrollback_len() as u64,
            focus_reporting: callbacks.focus_reporting,
            answers: Vec::new(),
        };
        self.parser = parser(cols.get(), rows.get(), self.history, lines);
    }
}

/// Write the history, oldest line first, each from the bottom row (where
/// the cursor is) so that it scrolls the line before up.
fn write_history(out: &mut String, screen: &vt100::Screen) {
    let history = screen.scrollback_len();
    if history == 0 {
        return;
    }
    // Paging through it grows each row to the screen's width: on a copy.
    let mut screen = screen.clone();
    let page = usize::from(screen.size().0.get());
    let mut wrapped = false;
    for start in (0..history).step_by(page) {
        // The window's rows are history[start..start + page].
        screen.set_scrollback(history - start);
        for row in 0..page.min(history - start) as u16 {
            if !wrapped {
                out.push_str("\x1b[m\r\n");
            }
            // History rows can be wider than the screen after a resize.
            write_row(out, &screen, row, u16::MAX);
            wrapped = screen.row_wrapped(row);
        }
    }
}

/// Write the first `cols` cells of a visible row, from the cursor, with
/// their attributes, up to the last non-blank one. Nothing but SGR and
/// characters: no cursor motion to other rows.
fn write_row(out: &mut String, screen: &vt100::Screen, row: u16, cols: u16) {
    let cells: Vec<&vt100::Cell> = (0..cols).map_while(|col| screen.cell(row, col)).collect();
    let blank = |cell: &&vt100::Cell| !cell.has_contents() && Sgr::of(cell) == Sgr::default();
    let end = cells.iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1);
    let mut current = Sgr::default();
    for cell in &cells[..end] {
        if cell.is_wide_continuation() {
            continue;
        }
        let sgr = Sgr::of(cell);
        if sgr != current {
            sgr.write(out);
            current = sgr;
        }
        out.push_str(if cell.has_contents() {
            cell.contents()
        } else {
            " "
        });
    }
    if current != Sgr::default() {
        out.push_str("\x1b[m");
    }
}

/// A cell's attributes, as SGR parameters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Sgr {
    fg: Option<vt100::Color>,
    bg: Option<vt100::Color>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Sgr {
    fn of(cell: &vt100::Cell) -> Self {
        let color = |c| (c != vt100::Color::Default).then_some(c);
        Self {
            fg: color(cell.fgcolor()),
            bg: color(cell.bgcolor()),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    /// From the default attributes to these.
    fn write(&self, out: &mut String) {
        out.push_str("\x1b[0");
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.inverse, "7"),
        ] {
            if on {
                out.push(';');
                out.push_str(code);
            }
        }
        for (color, base, bright, extended) in [(self.fg, 30, 90, 38), (self.bg, 40, 100, 48)] {
            let _ = match color {
                None | Some(vt100::Color::Default) => Ok(()),
                Some(vt100::Color::Idx(i)) if i < 8 => write!(out, ";{}", base + i),
                Some(vt100::Color::Idx(i)) if i < 16 => write!(out, ";{}", bright + i - 8),
                Some(vt100::Color::Idx(i)) => write!(out, ";{extended};5;{i}"),
                Some(vt100::Color::Rgb(r, g, b)) => write!(out, ";{extended};2;{r};{g};{b}"),
            };
        }
        out.push('m');
    }
}

fn parser(cols: u16, rows: u16, history: usize, lines: Lines) -> vt100::Parser<Lines> {
    vt100::Parser::new_with_callbacks(nonzero(rows), nonzero(cols), history, lines)
}

fn nonzero(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a viewer draws from a screen reply: its rows as text.
    fn rows(info: &ScreenInfo) -> Vec<String> {
        let mut parser = vt100::Parser::new(nonzero(info.rows), nonzero(info.cols), 0);
        parser.process(info.contents.as_bytes());
        parser.screen().rows(0, info.cols).collect()
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\r\n")).collect()
    }

    #[test]
    fn screen_is_drawn_at_the_terminal_size() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b[1mhello\x1b[m world");
        let info = t.screen(None);
        assert_eq!((info.cols, info.rows), (20, 4));
        assert_eq!(rows(&info), ["hello world", "", "", ""]);
        // Formatting comes along.
        assert!(info.contents.contains("\x1b[1m"), "{:?}", info.contents);
        assert_eq!((info.top, info.oldest, info.live), (0, 0, 0));
    }

    #[test]
    fn history_lines_keep_their_numbers() {
        let mut t = Terminal::with_history(20, 3, 5);
        t.feed(lines(4).as_bytes());
        // Lines 1-2 scrolled into history, the cursor sits on the third row.
        let live = t.screen(None);
        assert_eq!((live.oldest, live.live, live.top), (0, 2, 2));
        assert_eq!(rows(&live), ["line 3", "line 4", ""]);

        let back = t.screen(Some(1));
        assert_eq!(back.top, 1);
        assert_eq!(rows(&back), ["line 2", "line 3", "line 4"]);
        // Asking does not leave the screen scrolled back.
        assert_eq!(rows(&t.screen(None)), ["line 3", "line 4", ""]);

        // Past the history kept: the oldest line kept.
        t.feed(lines(10).as_bytes());
        let live = t.screen(None);
        assert_eq!((live.oldest, live.live), (7, 12));
        let back = t.screen(Some(0));
        assert_eq!(back.top, 7);
        assert_eq!(rows(&back)[0], "line 4");
        // Line 7 is still "line 4" once more output scrolled by.
        t.feed(b"more\r\n");
        let back = t.screen(Some(7));
        assert_eq!((back.top, back.oldest), (8, 8));
        // ...it was dropped: the window starts at the oldest line kept.
        assert_eq!(rows(&back)[0], "line 5");
        // Below the live screen's first line: the live screen.
        assert_eq!(t.screen(Some(100)).top, t.screen(None).live);
    }

    #[test]
    fn clearing_history_keeps_numbering() {
        let mut t = Terminal::with_history(20, 2, 10);
        t.feed(lines(5).as_bytes());
        let before = t.screen(None).live;
        t.feed(b"\x1b[3J");
        let after = t.screen(None);
        assert_eq!((after.oldest, after.live), (before, before));
    }

    #[test]
    fn alternate_screen_has_no_history() {
        let mut t = Terminal::with_history(20, 2, 10);
        t.feed(lines(5).as_bytes());
        t.feed(b"\x1b[?1049hfull screen");
        let info = t.screen(Some(0));
        assert_eq!(info.oldest, info.live);
        assert_eq!(rows(&info), ["full screen", ""]);
        // Back on the main screen, its history is there again.
        t.feed(b"\x1b[?1049l");
        let info = t.screen(Some(0));
        assert_eq!(info.top, 0);
        assert_eq!(rows(&info)[0], "line 1");
    }

    /// A viewer's terminal after the replay: `rows`×`cols`, plus a status
    /// bar below that the replay must leave alone (as the dashboard's).
    fn viewer(t: &Terminal) -> vt100::Parser {
        let info = t.parser.screen().size();
        let (rows, cols) = (info.0.get(), info.1.get());
        let mut viewer = vt100::Parser::new(nonzero(rows + 1), nonzero(cols), 100);
        viewer.process(
            format!(
                "old screen\x1b[{};1Hstatus bar\x1b[1;{rows}r\x1b[H",
                rows + 1
            )
            .as_bytes(),
        );
        viewer.process(t.replay().as_bytes());
        viewer
    }

    #[test]
    fn replay_reproduces_the_screen_and_history() {
        let mut t = Terminal::with_history(20, 3, 10);
        t.feed(b"line 1\r\nline 2\r\nthis line is longer than twenty\r\nline 4\r\n");
        t.feed(b"\x1b[1;31mred\x1b[m \x1b[48;5;200mbg\x1b[m\r\n$ \x1b[4m");
        t.feed(b"\x1b[?2004h\x1b[?1004h");
        let v = viewer(&t);
        let (agent, screen) = (t.parser.screen(), v.screen());
        assert_eq!(
            screen.rows(0, 20).take(3).collect::<Vec<_>>(),
            agent.rows(0, 20).collect::<Vec<_>>()
        );
        assert_eq!(screen.cursor_position(), agent.cursor_position());
        assert_eq!(screen.cell(1, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
        assert!(screen.cell(1, 0).unwrap().bold());
        assert_eq!(screen.cell(1, 4).unwrap().bgcolor(), vt100::Color::Idx(200));
        // Input modes, and what the agent prints next keeps its attributes.
        assert!(screen.bracketed_paste());
        assert!(t.replay().ends_with("\x1b[?1004h"));
        assert!(screen.underline());
        // The status bar below the agent's rows is untouched.
        assert_eq!(screen.rows(0, 20).nth(3).unwrap(), "status bar");

        // The viewer's scrollback: its old screen, then the history, the
        // wrapped line joined (and wrapped again at the viewer's width). A
        // line wrapped from history onto the screen would come in two.
        let mut all = String::new();
        let mut state = vt100::capture::BasicFormattedCaptureState::new();
        let range = vt100::capture::BasicFormattedCaptureRange::Full(&mut state);
        v.screen()
            .write_contents_formatted_basic(&mut all, range)
            .unwrap();
        let all: String = vt100::capture::basic_formatted_to_plain(&all).collect();
        let lines: Vec<&str> = all.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines,
            [
                "old screen",
                "line 1",
                "line 2",
                "this line is longer than twenty",
                "line 4",
                "red bg",
                "$ ",
                "status bar"
            ]
        );
    }

    #[test]
    fn replay_of_the_alternate_screen() {
        let mut t = Terminal::with_history(20, 3, 10);
        t.feed(b"shell\r\n\x1b[?1049h\x1b[2;3Hfull screen");
        let v = viewer(&t);
        assert!(v.screen().alternate_screen());
        assert_eq!(v.screen().rows(0, 20).nth(1).unwrap(), "  full screen");
        assert_eq!(v.screen().cursor_position(), (1, 13));
    }

    #[test]
    fn focus_reporting_follows_the_agent() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"\x1b[?1004;2004h");
        assert!(t.replay().contains("\x1b[?1004h"));
        t.feed(b"\x1b[?1004l");
        assert!(!t.replay().contains("\x1b[?1004h"));
    }

    #[test]
    fn the_cursor_position_and_the_status_are_answered_where_asked() {
        let mut t = Terminal::new(10, 4);
        assert_eq!(t.feed(b"ab\r\ncd"), b"");
        // Where the query comes in the output, not where the chunk ends.
        assert_eq!(t.feed(b"\x1b[6nmore\x1b[5n"), b"\x1b[2;3R\x1b[0n");
        // Handed over once.
        assert_eq!(t.feed(b"x"), b"");
        // A wrap pending after the last column: that column.
        t.feed(b"\x1b[4;1H0123456789");
        assert_eq!(t.feed(b"\x1b[6n"), b"\x1b[4;10R");
        // Other queries go unanswered: device attributes, the DEC form of
        // the cursor position, colours.
        assert_eq!(t.feed(b"\x1b[c\x1b[>c\x1b[?6n\x1b]11;?\x07"), b"");
    }

    #[test]
    fn text_is_history_then_screen() {
        let mut t = Terminal::with_history(10, 2, 10);
        t.feed(b"one\r\n\x1b[1mtwo\x1b[m\r\nthree and more\r\n");
        assert_eq!(t.text(), "one\ntwo\nthree and more");
    }

    #[test]
    fn resize_follows_the_pty_and_ignores_zero() {
        let mut t = Terminal::new(80, 24);
        t.resize(100, 30);
        let info = t.screen(None);
        assert_eq!((info.cols, info.rows), (100, 30));
        t.resize(0, 0);
        let info = t.screen(None);
        assert_eq!((info.cols, info.rows), (1, 1));
    }
}
