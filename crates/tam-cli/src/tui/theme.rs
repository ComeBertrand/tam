//! The dashboard's colours, by role. Three themes: `DARK` and `UNICORN`
//! (brighter, with rainbows: see `Theme::rainbow`), 24-bit palettes for
//! terminals that have them, and `ANSI`, the terminal's own 16 colours
//! (what tests draw with). The agent's screen (`peek.rs`) keeps the
//! agent's colours: nothing here paints a background behind it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    /// The pane scrolling (`v`): its thick border, its chip, its title.
    /// Neither `accent`, typing's, nor `warn` where the palette has another
    /// yellow or orange.
    pub scrolled: Color,
    /// Rainbows: the logo, the selection's bar and the spinners cycle
    /// through the hues, rules and the pane's border fade through them
    /// (moving along the rules, clockwise round the border).
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
    scrolled: rgb(0xff9e64),
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
    scrolled: rgb(0xffb86c),
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
    scrolled: Color::Yellow,
    rainbow: false,
};

/// The themes by name, in the order `next` goes through them.
static THEMES: [(&str, &Theme); 3] = [("ansi", &ANSI), ("dark", &DARK), ("unicorn", &UNICORN)];

/// The theme in use, in `THEMES`: `ANSI` until `set`.
static THEME: AtomicUsize = AtomicUsize::new(0);

/// The theme in use.
pub fn get() -> &'static Theme {
    #[cfg(test)]
    if let Some(theme) = TEST_THEME.get() {
        return theme;
    }
    THEMES[THEME.load(Ordering::Relaxed)].1
}

#[cfg(test)]
thread_local! {
    /// A test's own theme (see `in_theme`): tests run side by side, one
    /// thread each, and the others draw in `ANSI`.
    static TEST_THEME: std::cell::Cell<Option<&'static Theme>> =
        const { std::cell::Cell::new(None) };
}

/// Draw in `theme` on this thread (a test's) until the guard is dropped,
/// leaving every other test's theme alone.
#[cfg(test)]
pub fn in_theme(theme: &'static Theme) -> InTheme {
    InTheme(TEST_THEME.replace(Some(theme)))
}

/// See `in_theme`: the theme before, back when dropped.
#[cfg(test)]
#[must_use]
pub struct InTheme(Option<&'static Theme>);

#[cfg(test)]
impl Drop for InTheme {
    fn drop(&mut self) {
        TEST_THEME.set(self.0);
    }
}

/// The name of the theme in use.
pub fn name() -> &'static str {
    THEMES[THEME.load(Ordering::Relaxed)].0
}

/// Switch to the next theme (until the dashboard quits: the configured
/// one comes back); returns its name.
pub fn next() -> &'static str {
    let next = after(THEME.load(Ordering::Relaxed));
    THEME.store(next, Ordering::Relaxed);
    THEMES[next].0
}

/// The theme after theme `i`: the 24-bit ones in turn. `ansi` (the first)
/// is left for them and only comes from the config.
fn after(i: usize) -> usize {
    1 + i % (THEMES.len() - 1)
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
    let hue = (i * HUE_STEP + 360 - (moving(tick) * HUE_SPEED) % 360) % 360;
    hsl(hue as f32, 0.9, 0.74)
}

/// The colour at `pos` along a closed line `len` long (a border, going
/// round): as `rainbow`, the line's end meeting its start.
pub fn rainbow_around(pos: usize, len: usize, tick: usize) -> Color {
    hsl(hue_around(pos, len, moving(tick)) as f32, 0.9, 0.74)
}

/// `tick`, or 0 when rainbows stand still.
fn moving(tick: usize) -> usize {
    if ANIMATE.load(Ordering::Relaxed) {
        tick
    } else {
        0
    }
}

/// The hue at `pos` along a closed line `len` long, at `tick`: about half
/// a rainbow's step from one position to the next, fitted so that a whole
/// number of turns of the hues goes round (no seam), moving towards
/// higher positions as fast as a rainbow does, in positions per tick.
fn hue_around(pos: usize, len: usize, tick: usize) -> f64 {
    let len = len.max(1) as f64;
    let turns = (len * HUE_STEP as f64 / 720.0).round().max(1.0);
    let step = 360.0 * turns / len;
    let shift = tick as f64 * step * HUE_SPEED as f64 / HUE_STEP as f64;
    (pos as f64 * step - shift).rem_euclid(360.0)
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
    let name = match configured {
        Some(name) => name,
        None if truecolor => "dark",
        None => "ansi",
    };
    let theme = THEMES.iter().position(|(n, _)| *n == name).unwrap_or(0);
    THEME.store(theme, Ordering::Relaxed);
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
        // The key goes from one 24-bit theme to the other, never to ansi.
        assert_eq!((after(0), after(1), after(2)), (1, 2, 1));
        let spans = rainbow_spans("────", 0, Style::new());
        assert_eq!(spans.len(), 4);
        assert_ne!(spans[0].style.fg, spans[3].style.fg);
    }

    #[test]
    fn a_rainbow_round_a_border_has_no_seam_and_goes_round() {
        // How far `b` is past `a`, round the hues.
        let ahead = |a: f64, b: f64| (b - a).rem_euclid(360.0);
        let close = |a: f64, b: f64| ahead(a, b).min(ahead(b, a)) < 1e-6;
        // Pane borders, small to wide.
        for len in 26..2000 {
            for tick in [0, 1, 7, 1000, 123_456_789] {
                let hue = |pos| hue_around(pos, len, tick);
                let step = ahead(hue(0), hue(1));
                assert!(
                    (HUE_STEP as f64 / 3.0..HUE_STEP as f64).contains(&step),
                    "{len}: {step}"
                );
                // From the last position back to the first: one more step.
                assert!(close(ahead(hue(len - 1), hue(0)), step), "{len}");
                assert!(close(hue(len), hue(0)), "{len}");
                // It moves towards higher positions, as a rainbow does.
                let later = hue_around(HUE_SPEED + 3, len, tick + HUE_STEP);
                assert!(close(later, hue(3)), "{len}");
            }
        }
        // 100 long: two turns of the hues, half a turn a quarter of the way.
        assert_ne!(rainbow_around(0, 100, 0), rainbow_around(25, 100, 0));
        assert_eq!(rainbow_around(0, 100, 0), rainbow_around(50, 100, 0));
    }
}
