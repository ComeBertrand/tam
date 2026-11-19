//! The dashboard's colours, by role. Three themes: `DARK` and `UNICORN`
//! (brighter, with rainbows: see `Theme::rainbow`), 24-bit palettes for
//! terminals that have them, and `ANSI`, the terminal's own 16 colours
//! (what tests draw with). The agent's screen (`peek.rs`) keeps the
//! agent's colours: nothing here paints a background behind it.

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Rainbows: the logo, the selection's bar and the spinners cycle
    /// through the hues, rules and the pane's border fade through them.
    pub rainbow: bool,
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
    rainbow: false,
};

/// Dracula's colours.
pub static UNICORN: Theme = Theme {
    accent: rgb(0xff79c6),
    on_accent: rgb(0x282a36),
    muted: rgb(0x8490c0),
    subtle: rgb(0x4d5270),
    bar_fg: rgb(0xf8f8f2),
    bar_bg: rgb(0x2f2b45),
    popup_bg: rgb(0x282a36),
    selection: Style::new().bg(rgb(0x4a3f73)).add_modifier(Modifier::BOLD),
    ok: rgb(0x50fa7b),
    warn: rgb(0xf1fa8c),
    bad: rgb(0xff5555),
    run: rgb(0x8be9fd),
    block: rgb(0xffb86c),
    question: rgb(0xbd93f9),
    rainbow: true,
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
    rainbow: false,
};

static THEME: OnceLock<&'static Theme> = OnceLock::new();

/// The theme in use: `ANSI` until `set`.
pub fn get() -> &'static Theme {
    THEME.get().copied().unwrap_or(&ANSI)
}

/// Whether the rainbows move.
static ANIMATE: AtomicBool = AtomicBool::new(true);

/// Whether the dashboard has something to animate all the time (the
/// rainbows), not only while an agent works.
pub fn animated() -> bool {
    get().rainbow && ANIMATE.load(Ordering::Relaxed)
}

/// Hues from one cell to the next in a rainbow, in degrees.
const HUE_STEP: usize = 14;
/// Hues a rainbow moves by per tick.
const HUE_SPEED: usize = 9;

/// The colour of cell `i` of a rainbow at `tick` (it moves towards the
/// right as ticks go, and stands still when animation is off): bright
/// pastels, the same lightness all along.
pub fn rainbow(i: usize, tick: usize) -> Color {
    let tick = if ANIMATE.load(Ordering::Relaxed) {
        tick
    } else {
        0
    };
    let hue = (i * HUE_STEP + 360 - (tick * HUE_SPEED) % 360) % 360;
    hsl(hue as f32, 0.9, 0.74)
}

/// `text`, each character in the next colour of a rainbow.
pub fn rainbow_spans(text: &str, tick: usize, style: Style) -> Vec<ratatui::text::Span<'static>> {
    text.chars()
        .enumerate()
        .map(|(i, c)| ratatui::text::Span::styled(c.to_string(), style.fg(rainbow(i, tick))))
        .collect()
}

fn hsl(h: f32, s: f32, l: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |v: f32| ((v + m) * 255.0).round() as u8;
    Color::Rgb(byte(r), byte(g), byte(b))
}

/// Choose the theme, once: the configured one (`dark`, `unicorn`, `ansi`), else the
/// 24-bit one when the terminal says it has 24-bit colour. `animate`:
/// whether rainbows move.
pub fn set(configured: Option<&str>, animate: bool) {
    ANIMATE.store(animate, Ordering::Relaxed);
    let truecolor = std::env::var("COLORTERM")
        .is_ok_and(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"));
    let theme = match configured {
        Some("dark") => &DARK,
        Some("unicorn") => &UNICORN,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rainbow_goes_through_the_hues_at_one_lightness() {
        assert_eq!(hsl(0.0, 1.0, 0.5), Color::Rgb(255, 0, 0));
        assert_eq!(hsl(120.0, 1.0, 0.5), Color::Rgb(0, 255, 0));
        assert_eq!(hsl(240.0, 1.0, 0.5), Color::Rgb(0, 0, 255));
        // Cell to cell another colour, and the same again a turn later.
        assert_ne!(rainbow(0, 0), rainbow(1, 0));
        assert_eq!(rainbow(0, 0), rainbow(0, 360 / HUE_SPEED));
        // It moves to the right: a cell takes its left neighbour's colour.
        assert_eq!(rainbow(HUE_SPEED, HUE_STEP), rainbow(0, 0));
        let spans = rainbow_spans("────", 0, Style::new());
        assert_eq!(spans.len(), 4);
        assert_ne!(spans[0].style.fg, spans[3].style.fg);
    }
}
