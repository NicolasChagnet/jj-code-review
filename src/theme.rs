//! Colours.
//!
//! The reference maki plugin derives its diff tints by blending a hue into
//! the *theme's* background at a low fraction, so the tints sit correctly on
//! either a light or a dark terminal. That is the behaviour here: blend the
//! same hues over whatever background we think the terminal has.
//!
//! Getting that background is the hard part. Querying it (OSC 11) needs a
//! round-trip on stdin before the TUI starts, and terminals that do not
//! answer would stall the launch, so the query is opt-in on `JCR_BG`:
//!
//!   JCR_BG=#1e1e1e   use this background
//!   JCR_BG=dark      assume a dark terminal
//!   JCR_BG=light     assume a light terminal
//!   JCR_BG=none      no tints at all; plain terminal default colours
//!
//! With no `JCR_BG` we assume dark, which is what most terminals are, and
//! `NO_COLOR` forces `none`. Every tint is a real background colour, so an
//! unreadable combination is impossible to produce by accident: text is only
//! ever drawn in terminal-default or accent colours over the blended band.

use ratatui::style::Color;

/// Blend fractions and hues, copied from the reference plugin.
const ADD_HUE: (u8, u8, u8) = (0x3f, 0xb9, 0x50);
const ADD_FRACTION: f32 = 0.18;
const DEL_HUE: (u8, u8, u8) = (0xf8, 0x51, 0x49);
const DEL_FRACTION: f32 = 0.18;
const SEL_HUE: (u8, u8, u8) = (0x58, 0xa6, 0xff);
const SEL_FRACTION: f32 = 0.30;
const COM_HUE: (u8, u8, u8) = (0xe3, 0xb3, 0x41);
const COM_FRACTION: f32 = 0.22;

/// Terminal backgrounds we blend into, used only when the user has not told
/// us the real one.
const DARK_BG: (u8, u8, u8) = (0x1e, 0x1e, 0x1e);
const LIGHT_BG: (u8, u8, u8) = (0xfa, 0xfa, 0xfa);

/// The resolved palette for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub add_bg: Option<Color>,
    pub del_bg: Option<Color>,
    pub sel_bg: Option<Color>,
    pub com_bg: Option<Color>,
    /// Chrome behind the status and hint bars.
    pub bar_bg: Option<Color>,
    /// Accent for annotation markers and comment text.
    pub comment_fg: Color,
    /// Strong accent for pane titles and the focus indicator.
    pub accent_fg: Color,
    pub dim_fg: Color,
}

// Terminal default \u2014 never a hardcoded RGB, so it adapts to the palette.
const DEFAULT_FG: Color = Color::Reset;
const DIM: Color = Color::DarkGray;

impl Theme {
    /// Builds the palette from the `JCR_BG` / `NO_COLOR` environment.
    pub fn from_env() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            return Self::mono();
        }
        let mut background = Some(DARK_BG);
        if let Some(value) = std::env::var_os("JCR_BG") {
            let value = value.to_string_lossy();
            background = match value.trim().to_ascii_lowercase().as_str() {
                "none" | "default" | "off" => None,
                "dark" => Some(DARK_BG),
                "light" => Some(LIGHT_BG),
                hex => parse_hex(hex).or(Some(DARK_BG)),
            };
        }
        Theme::on(background)
    }

    /// Tints derived from `background`; `None` means terminal defaults.
    pub fn on(background: Option<(u8, u8, u8)>) -> Self {
        let Some(bg) = background else {
            return Self::mono();
        };
        let light = is_light(bg);
        // Move away from the background for chrome, so bands stay visible.
        let chrome = if light {
            (0xe4, 0xe4, 0xe4)
        } else {
            (0x2a, 0x2a, 0x2a)
        };
        Self {
            add_bg: Some(blend(bg, ADD_HUE, ADD_FRACTION)),
            del_bg: Some(blend(bg, DEL_HUE, DEL_FRACTION)),
            sel_bg: Some(blend(bg, SEL_HUE, SEL_FRACTION)),
            com_bg: Some(blend(bg, COM_HUE, COM_FRACTION)),
            bar_bg: Some(rgb(chrome)),
            comment_fg: if light {
                Color::Rgb(0x8a, 0x6d, 0x0f)
            } else {
                Color::Rgb(0xe3, 0xb3, 0x41)
            },
            accent_fg: Color::Cyan,
            dim_fg: DIM,
        }
    }

    /// No tints: borders, markers and the terminal's own foreground only.
    pub fn mono() -> Self {
        Self {
            add_bg: None,
            del_bg: None,
            sel_bg: None,
            com_bg: None,
            bar_bg: None,
            comment_fg: DEFAULT_FG,
            accent_fg: DEFAULT_FG,
            dim_fg: DIM,
        }
    }

    pub fn tint(&self, kind: crate::diff::LineKind) -> Option<Color> {
        match kind {
            crate::diff::LineKind::Add => self.add_bg,
            crate::diff::LineKind::Del => self.del_bg,
            crate::diff::LineKind::Context => None,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::on(Some(DARK_BG))
    }
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// `top` mixed into `base` by `t` (0..1), the plugin's blend.
fn blend(base: (u8, u8, u8), top: (u8, u8, u8), t: f32) -> Color {
    let mix = |b: u8, t2: u8| (f32::from(b) + (f32::from(t2) - f32::from(b)) * t).round() as u8;
    rgb((mix(base.0, top.0), mix(base.1, top.1), mix(base.2, top.2)))
}

/// Perceived luminance, for choosing the light or dark chrome.
fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    let l = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
    l > 128.0
}

/// `#rrggbb` or `rrggbb`.
pub fn parse_hex(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_of(c: Color) -> (u8, u8, u8) {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("expected rgb, got {other:?}"),
        }
    }

    #[test]
    fn blend_matches_the_reference_plugin() {
        // code_review.lua: blend("#1e1e1e", "#3fb950", 0.18)
        let add = rgb_of(blend((0x1e, 0x1e, 0x1e), ADD_HUE, ADD_FRACTION));
        let expect =
            |b: u8, t: u8| (f32::from(b) + (f32::from(t) - f32::from(b)) * 0.18).round() as u8;
        assert_eq!(
            add,
            (expect(0x1e, 0x3f), expect(0x1e, 0xb9), expect(0x1e, 0x50))
        );
        assert!(add.1 > add.0 && add.1 > add.2, "green dominant: {add:?}");
    }

    #[test]
    fn blend_endpoints_are_exact() {
        assert_eq!(rgb_of(blend((10, 20, 30), (99, 99, 99), 0.0)), (10, 20, 30));
        assert_eq!(rgb_of(blend((10, 20, 30), (99, 99, 99), 1.0)), (99, 99, 99));
    }

    #[test]
    fn every_diff_tint_is_a_background_colour() {
        let t = Theme::on(Some(DARK_BG));
        for c in [t.add_bg, t.del_bg, t.sel_bg, t.com_bg, t.bar_bg] {
            assert!(c.is_some(), "dark theme defines all tints");
            assert!(matches!(c.unwrap(), Color::Rgb(..)));
        }
    }

    #[test]
    fn light_background_gets_light_tints() {
        let dark = Theme::on(Some(DARK_BG));
        let light = Theme::on(Some(LIGHT_BG));
        let luma = |c: Option<Color>| {
            let (r, g, b) = rgb_of(c.unwrap());
            0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
        };
        assert!(luma(light.add_bg) > luma(dark.add_bg), "light stays light");
        assert!(luma(light.bar_bg) > luma(dark.bar_bg));
        // The chrome must not collide with the background on either theme.
        assert_ne!(rgb_of(light.bar_bg.unwrap()), LIGHT_BG);
        assert_ne!(rgb_of(dark.bar_bg.unwrap()), DARK_BG);
    }

    #[test]
    fn tint_stays_close_to_the_background() {
        // A blended band must not become a solid block of colour: the text
        // sitting on it has to stay readable.
        for bg in [DARK_BG, LIGHT_BG] {
            let t = Theme::on(Some(bg));
            for tint in [t.add_bg, t.del_bg, t.sel_bg, t.com_bg] {
                let (r, g, b) = rgb_of(tint.unwrap());
                let distance = (i32::from(r) - i32::from(bg.0)).abs()
                    + (i32::from(g) - i32::from(bg.1)).abs()
                    + (i32::from(b) - i32::from(bg.2)).abs();
                assert!(
                    distance < 160,
                    "tint {tint:?} is too far from background {bg:?} to read text on"
                );
            }
        }
    }

    #[test]
    fn mono_theme_uses_terminal_defaults() {
        let t = Theme::mono();
        assert_eq!(t.add_bg, None);
        assert_eq!(t.bar_bg, None);
        assert_eq!(t.sel_bg, None);
        assert_eq!(t.com_bg, None);
        assert_eq!(t.comment_fg, Color::Reset);
        assert_eq!(t.accent_fg, Color::Reset);
    }

    #[test]
    fn parses_hex_backgrounds() {
        assert_eq!(parse_hex("#1e1e1e"), Some((0x1e, 0x1e, 0x1e)));
        assert_eq!(parse_hex("FAFAFA"), Some((0xfa, 0xfa, 0xfa)));
        assert_eq!(parse_hex(" #ffffff "), Some((255, 255, 255)));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn light_detection() {
        assert!(!is_light(DARK_BG));
        assert!(is_light(LIGHT_BG));
        assert!(!is_light((0x26, 0x26, 0x26)));
    }
}
