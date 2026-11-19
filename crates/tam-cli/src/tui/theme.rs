//! The dashboard's colours, by role. Two themes: `DARK`, a 24-bit palette
//! for terminals that have one, and `ANSI`, the terminal's own 16 colours
//! (what tests draw with). The agent's screen (`peek.rs`) keeps the
//! agent's colours: nothing here paints a background behind it.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

use crate::task::TaskStatus;

pub struct Theme {
    /// Keys, titles, the selection's bar, what the filter matched.
    pub accent: Color,
    /// Text on a filled chip.
    pub on_accent: Color,
    /// Secondary text.
    pub muted: Color,
    /// Borders, rules, tree lines.
    pub subtle: Color,
    /// Text on a bar.
    pub bar_fg: Color,
    /// The header's and footer's background.
    pub bar_bg: Color,
    /// A dialog's background.
    pub popup_bg: Color,
    /// The selected row.
    pub selection: Style,
    pub ok: Color,
    pub warn: Color,
    pub bad: Color,
    pub run: Color,
    pub block: Color,
    pub question: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub static DARK: Theme = Theme {
    accent: rgb(0x7aa2f7),
    on_accent: rgb(0x1a1b26),
    muted: rgb(0x737aa2),
    subtle: rgb(0x3b4261),
    bar_fg: rgb(0xc0caf5),
    bar_bg: rgb(0x1f2335),
    popup_bg: rgb(0x1f2335),
    selection: Style::new().bg(rgb(0x2e3c64)).add_modifier(Modifier::BOLD),
    ok: rgb(0x9ece6a),
    warn: rgb(0xe0af68),
    bad: rgb(0xf7768e),
    run: rgb(0x73daca),
    block: rgb(0xff9e64),
    question: rgb(0xbb9af7),
};

pub static ANSI: Theme = Theme {
    accent: Color::Cyan,
    on_accent: Color::Black,
    muted: Color::DarkGray,
    subtle: Color::DarkGray,
    bar_fg: Color::Reset,
    bar_bg: Color::Reset,
    popup_bg: Color::Reset,
    selection: Style::new().add_modifier(Modifier::REVERSED),
    ok: Color::Green,
    warn: Color::Yellow,
    bad: Color::Red,
    run: Color::Blue,
    block: Color::Red,
    question: Color::Magenta,
};

static THEME: OnceLock<&'static Theme> = OnceLock::new();

/// The theme in use: `ANSI` until `set`.
pub fn get() -> &'static Theme {
    THEME.get().copied().unwrap_or(&ANSI)
}

/// Choose the theme, once: the configured one (`dark`, `ansi`), else the
/// 24-bit one when the terminal says it has 24-bit colour.
pub fn set(configured: Option<&str>) {
    let truecolor = std::env::var("COLORTERM")
        .is_ok_and(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"));
    let theme = match configured {
        Some("dark") => &DARK,
        Some("ansi") => &ANSI,
        _ if truecolor => &DARK,
        _ => &ANSI,
    };
    let _ = THEME.set(theme);
}

impl Theme {
    pub fn status(&self, status: &TaskStatus) -> Color {
        match status {
            TaskStatus::Run => self.run,
            TaskStatus::Ready => self.warn,
            TaskStatus::Block => self.block,
            TaskStatus::Question => self.question,
            TaskStatus::Error | TaskStatus::Gone => self.bad,
            TaskStatus::Idle | TaskStatus::Stale => self.muted,
        }
    }

    /// A filled label: ` text ` on `color`.
    pub fn chip(&self, color: Color) -> Style {
        Style::new()
            .fg(self.on_accent)
            .bg(color)
            .add_modifier(Modifier::BOLD)
    }

    /// The header and the footer.
    pub fn bar(&self) -> Style {
        Style::new().fg(self.bar_fg).bg(self.bar_bg)
    }

    /// A dialog's inside.
    pub fn popup(&self) -> Style {
        Style::new().bg(self.popup_bg)
    }
}
