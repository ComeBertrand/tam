//! Keys typed into the pane, as the bytes a terminal sends for them: what
//! the agent's PTY gets (`Request::Keys`) while the pane has the keyboard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const ESC: u8 = 0x1b;

/// The key that gives the keyboard back to the task list: ctrl-], as when
/// attached. (Without the kitty protocol crossterm reads its byte, 0x1d, as
/// ctrl-5.)
pub fn is_leave(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
}

/// The bytes of `key`, as xterm sends them; `None` for a key a terminal has
/// no bytes for (a lone modifier, a media key). `app_cursor`: the agent
/// asked for application cursor keys (`ESC O A` for up, not `ESC [ A`).
pub fn encode(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // xterm's modifier parameter: 1 without any.
    let param = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    // `ESC [ X`, `ESC O X` in application mode, `ESC [ 1 ; m X` with modifiers.
    let letter = |x: u8, application: bool| match (param, application) {
        (1, true) => vec![ESC, b'O', x],
        (1, false) => vec![ESC, b'[', x],
        _ => format!("\x1b[1;{param}{}", x as char).into_bytes(),
    };
    // `ESC [ n ~`, `ESC [ n ; m ~` with modifiers.
    let tilde = |n: u8| match param {
        1 => format!("\x1b[{n}~").into_bytes(),
        _ => format!("\x1b[{n};{param}~").into_bytes(),
    };
    // Alt: the key's bytes after an escape.
    let meta = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, ESC);
        }
        bytes
    };
    Some(match key.code {
        KeyCode::Char(c) => {
            let mut bytes = [0; 4];
            meta(match control(c).filter(|_| ctrl) {
                Some(byte) => vec![byte],
                None => c.encode_utf8(&mut bytes).as_bytes().to_vec(),
            })
        }
        KeyCode::Enter => meta(vec![b'\r']),
        KeyCode::Tab if shift => b"\x1b[Z".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Tab => meta(vec![b'\t']),
        KeyCode::Backspace => meta(vec![if ctrl { 0x08 } else { 0x7f }]),
        KeyCode::Esc => vec![ESC],
        KeyCode::Up => letter(b'A', app_cursor),
        KeyCode::Down => letter(b'B', app_cursor),
        KeyCode::Right => letter(b'C', app_cursor),
        KeyCode::Left => letter(b'D', app_cursor),
        KeyCode::Home => letter(b'H', app_cursor),
        KeyCode::End => letter(b'F', app_cursor),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => letter(b'P' + n - 1, true),
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5]),
        _ => return None,
    })
}

/// The control character of `c` with ctrl held (ctrl-a is 1…). crossterm
/// reads the bytes 0x1c to 0x1f as ctrl-4 to ctrl-7: they go back as they
/// came.
fn control(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        ' ' | '@' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Pasted `text`, as a terminal sends it: between the paste markers when
/// the agent turned bracketed paste on (a marker inside the text would end
/// the paste early: left out), else typed, a carriage return per line end.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let text = text.replace("\x1b[201~", "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn bytes(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode(key(code, modifiers), false).unwrap()
    }

    #[test]
    fn keys_are_the_bytes_a_terminal_sends() {
        let none = KeyModifiers::NONE;
        let (shift, alt, ctrl) = (
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        );
        assert_eq!(bytes(KeyCode::Char('a'), none), b"a");
        assert_eq!(bytes(KeyCode::Char('A'), shift), b"A");
        assert_eq!(bytes(KeyCode::Char('é'), none), "é".as_bytes());
        assert_eq!(bytes(KeyCode::Char('c'), ctrl), [3]);
        assert_eq!(bytes(KeyCode::Char('d'), ctrl), [4]);
        assert_eq!(bytes(KeyCode::Char(' '), ctrl), [0]);
        assert_eq!(bytes(KeyCode::Char('b'), alt), b"\x1bb");
        // No control character for it: the character.
        assert_eq!(bytes(KeyCode::Char('1'), ctrl), b"1");
        assert_eq!(bytes(KeyCode::Enter, none), b"\r");
        assert_eq!(bytes(KeyCode::Enter, alt), b"\x1b\r");
        assert_eq!(bytes(KeyCode::Tab, none), b"\t");
        assert_eq!(bytes(KeyCode::BackTab, shift), b"\x1b[Z");
        assert_eq!(bytes(KeyCode::Backspace, none), [0x7f]);
        assert_eq!(bytes(KeyCode::Backspace, alt), [ESC, 0x7f]);
        assert_eq!(bytes(KeyCode::Esc, none), [ESC]);
        assert_eq!(bytes(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(bytes(KeyCode::Left, ctrl), b"\x1b[1;5D");
        assert_eq!(bytes(KeyCode::Right, shift | alt), b"\x1b[1;4C");
        assert_eq!(bytes(KeyCode::Home, none), b"\x1b[H");
        assert_eq!(bytes(KeyCode::Delete, none), b"\x1b[3~");
        assert_eq!(bytes(KeyCode::PageUp, none), b"\x1b[5~");
        assert_eq!(bytes(KeyCode::PageDown, shift), b"\x1b[6;2~");
        assert_eq!(bytes(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(bytes(KeyCode::F(5), none), b"\x1b[15~");
        assert_eq!(bytes(KeyCode::F(12), none), b"\x1b[24~");
        assert_eq!(encode(key(KeyCode::CapsLock, none), false), None);
        // Application cursor keys, unless a modifier is held.
        assert_eq!(encode(key(KeyCode::Up, none), true).unwrap(), b"\x1bOA");
        assert_eq!(encode(key(KeyCode::Up, ctrl), true).unwrap(), b"\x1b[1;5A");
    }

    #[test]
    fn ctrl_bracket_leaves_however_the_terminal_reports_it() {
        let ctrl = KeyModifiers::CONTROL;
        assert!(is_leave(key(KeyCode::Char(']'), ctrl)));
        assert!(is_leave(key(KeyCode::Char('5'), ctrl)));
        assert!(!is_leave(key(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert!(!is_leave(key(KeyCode::Char('c'), ctrl)));
        // The other control bytes crossterm reads as digits go back as bytes.
        assert_eq!(bytes(KeyCode::Char('4'), ctrl), [0x1c]);
        assert_eq!(bytes(KeyCode::Char('7'), ctrl), [0x1f]);
    }

    #[test]
    fn a_paste_is_marked_or_typed() {
        assert_eq!(paste("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste("a\x1b[201~b", true), b"\x1b[200~ab\x1b[201~");
        assert_eq!(paste("a\r\nb\nc", false), b"a\rb\rc");
    }
}
