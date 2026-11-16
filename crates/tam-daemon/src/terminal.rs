//! The daemon's terminal emulator of an agent's PTY: the screen a viewer
//! would see, kept at the PTY's size and fed by its reader.

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
/// screen's history, to number lines (see [`ScreenInfo`]).
#[derive(Default)]
struct Lines {
    dropped: u64,
}

impl vt100::Callbacks for Lines {
    fn on_scroll(&mut self, _row: vt100::capture::RowContents<'_>, alternate_screen: bool) {
        if !alternate_screen {
            self.dropped += 1;
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

    /// Process output of the agent.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.guarded(|parser| parser.process(bytes));
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
        let contents = screen.contents_formatted();
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
        let lines = Lines {
            dropped: self.parser.callbacks().dropped + screen.scrollback_len() as u64,
        };
        self.parser = parser(cols.get(), rows.get(), self.history, lines);
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
