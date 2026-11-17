//! A line of text being typed (the filter, the pickers' filter, a task
//! name, a path): the cursor moves, and edits happen at it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::Frame;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    /// Characters before the cursor.
    cursor: usize,
}

impl From<&str> for TextInput {
    /// The text, the cursor at its end.
    fn from(value: &str) -> Self {
        Self {
            value: value.to_string(),
            cursor: value.chars().count(),
        }
    }
}

impl TextInput {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// Apply an editing key: `false` when it is not one (it is the
    /// caller's: enter, esc, tab, up, down...).
    ///
    /// Typing inserts; `←`/`→`, `home`/`end`, `ctrl-a`/`ctrl-e`,
    /// `ctrl-b`/`ctrl-f` move; `ctrl-←`/`ctrl-→` (or `alt-b`/`alt-f`) move
    /// by word; `backspace`/`ctrl-h`, `delete`/`ctrl-d` delete a character;
    /// `ctrl-w`/`alt-backspace` the word before the cursor, `alt-d` the one
    /// after; `ctrl-u`/`ctrl-k` everything before/after the cursor.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if ctrl => match c {
                'a' => self.cursor = 0,
                'e' => self.cursor = self.len(),
                'b' => self.cursor = self.cursor.saturating_sub(1),
                'f' => self.cursor = (self.cursor + 1).min(self.len()),
                'h' => self.delete(self.cursor.saturating_sub(1), self.cursor),
                'd' => self.delete(self.cursor, (self.cursor + 1).min(self.len())),
                'w' => self.delete(self.word_start(), self.cursor),
                'u' => self.delete(0, self.cursor),
                'k' => self.delete(self.cursor, self.len()),
                _ => return false,
            },
            KeyCode::Char(c) if alt => match c {
                'b' => self.cursor = self.word_start(),
                'f' => self.cursor = self.word_end(),
                'd' => self.delete(self.cursor, self.word_end()),
                _ => return false,
            },
            KeyCode::Char(c) => {
                let at = self.byte(self.cursor);
                self.value.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if ctrl || alt => self.delete(self.word_start(), self.cursor),
            KeyCode::Backspace => self.delete(self.cursor.saturating_sub(1), self.cursor),
            KeyCode::Delete => self.delete(self.cursor, (self.cursor + 1).min(self.len())),
            KeyCode::Left if ctrl || alt => self.cursor = self.word_start(),
            KeyCode::Right if ctrl || alt => self.cursor = self.word_end(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            _ => return false,
        }
        true
    }

    /// Draw the text in `area` (one row), scrolled to keep the cursor in
    /// view, and put the terminal's cursor there when `focused`.
    pub fn render(&self, frame: &mut Frame, area: Rect, style: Style, focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (text, column) = self.view(area.width);
        frame.render_widget(Span::styled(text, style), area);
        if focused {
            frame.set_cursor_position(Position::new(area.x + column, area.y));
        }
    }

    /// What `width` columns show, the cursor in view (on the last column
    /// at most, where it sits after the text), and the cursor's column.
    fn view(&self, width: u16) -> (String, u16) {
        let width = width as usize;
        let chars: Vec<char> = self.value.chars().collect();
        let w = |c: &char| Span::raw(c.to_string()).width();
        // From the first character that leaves the cursor in view.
        let mut start = self.cursor;
        let mut before = 0;
        while start > 0 && before + w(&chars[start - 1]) < width {
            start -= 1;
            before += w(&chars[start]);
        }
        if before < width.saturating_sub(1) {
            // Room left of the cursor: show from the start if it fits.
            start = 0;
            before = chars[..self.cursor].iter().map(w).sum();
            while before >= width && start < self.cursor {
                before -= w(&chars[start]);
                start += 1;
            }
        }
        let mut shown = String::new();
        let mut used = 0;
        for c in &chars[start..] {
            if used + w(c) > width {
                break;
            }
            used += w(c);
            shown.push(*c);
        }
        (shown, before as u16)
    }

    fn len(&self) -> usize {
        self.value.chars().count()
    }

    /// The byte offset of character `index`.
    fn byte(&self, index: usize) -> usize {
        self.value
            .char_indices()
            .nth(index)
            .map_or(self.value.len(), |(i, _)| i)
    }

    /// Remove characters `from..to`, the cursor at `from`.
    fn delete(&mut self, from: usize, to: usize) {
        if from < to {
            let (a, b) = (self.byte(from), self.byte(to));
            self.value.replace_range(a..b, "");
        }
        self.cursor = from;
    }

    /// Where the word before the cursor starts (words: letters, digits, `_`).
    fn word_start(&self) -> usize {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        while i > 0 && !is_word(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(chars[i - 1]) {
            i -= 1;
        }
        i
    }

    /// Where the word at or after the cursor ends.
    fn word_end(&self) -> usize {
        let chars: Vec<char> = self.value.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && !is_word(chars[i]) {
            i += 1;
        }
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        i
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn typed(text: &str) -> TextInput {
        let mut input = TextInput::default();
        for c in text.chars() {
            assert!(input.handle(key(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        input
    }

    /// The text with `|` at the cursor.
    fn shown(input: &TextInput) -> String {
        let mut s = input.value.clone();
        s.insert(input.byte(input.cursor), '|');
        s
    }

    #[test]
    fn moves_and_edits_at_the_cursor() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let mut input = typed("~/src/tam");
        assert_eq!(shown(&input), "~/src/tam|");
        input.handle(key(KeyCode::Left, ctrl));
        assert_eq!(shown(&input), "~/src/|tam");
        input.handle(key(KeyCode::Char('x'), none));
        assert_eq!(shown(&input), "~/src/x|tam");
        input.handle(key(KeyCode::Backspace, none));
        input.handle(key(KeyCode::Home, none));
        input.handle(key(KeyCode::Delete, none));
        assert_eq!(shown(&input), "|/src/tam");
        input.handle(key(KeyCode::Char('f'), alt));
        assert_eq!(shown(&input), "/src|/tam");
        input.handle(key(KeyCode::Char('k'), ctrl));
        assert_eq!(shown(&input), "/src|");
        input.handle(key(KeyCode::Char('w'), ctrl));
        assert_eq!(shown(&input), "/|");
        input.handle(key(KeyCode::Char('u'), ctrl));
        assert_eq!(shown(&input), "|");
        // Not an editing key: the caller's.
        assert!(!input.handle(key(KeyCode::Enter, none)));
        assert!(!input.handle(key(KeyCode::Char('c'), ctrl)));

        // Multi-byte characters are characters.
        let mut input = typed("été");
        input.handle(key(KeyCode::Left, none));
        input.handle(key(KeyCode::Backspace, none));
        assert_eq!(shown(&input), "é|é");
    }

    #[test]
    fn a_long_text_scrolls_to_keep_the_cursor_in_view() {
        let mut input = typed("abcdefghij");
        // The cursor after the text, on the last column.
        assert_eq!(input.view(5), ("ghij".into(), 4));
        input.handle(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(input.view(5), ("abcde".into(), 0));
        assert_eq!(input.view(20), ("abcdefghij".into(), 0));
        input.cursor = 7;
        assert_eq!(input.view(5).1, 4);
        assert!(input.view(5).0.starts_with("defg"));
        // Wide characters take two columns.
        let input = typed("漢字漢字");
        assert_eq!(input.view(5), ("漢字".into(), 4));
    }
}
