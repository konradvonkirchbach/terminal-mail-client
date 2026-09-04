use ratatui::style::Color;

/// Semantic palette used throughout the TUI. Field names mirror the keys
/// Omarchy's generated `colors.toml` uses, so `omarchy::load()` can map
/// straight across; `Theme::fallback()` bundles the same shape using the
/// `god-of-war` values as a sane default on non-Omarchy systems.
// The non-red ANSI slots aren't painted anywhere yet — they're reserved
// for the future categorization/priority color coding the plan calls for
// (flagged=yellow, sent=green, etc.), kept here now so that work is additive.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub bright_foreground: Color,
    pub accent: Color,
    pub selection: Color,
    pub muted: Color,
    pub red: Color,
    pub yellow: Color,
    pub green: Color,
    pub cyan: Color,
    pub blue: Color,
    pub magenta: Color,
}

impl Theme {
    /// Bundled default, used when no live Omarchy theme file is found
    /// (e.g. running off Omarchy) or theming is turned off in config.
    pub fn fallback() -> Self {
        Self {
            background: hex(0x14, 0x17, 0x1c),
            foreground: hex(0xd6, 0xcf, 0xc0),
            bright_foreground: hex(0xf0, 0xe9, 0xd8),
            accent: hex(0xa8, 0x38, 0x2c),
            selection: hex(0x3a, 0x3f, 0x47),
            muted: hex(0x56, 0x5c, 0x66),
            red: hex(0xb3, 0x38, 0x2c),
            yellow: hex(0xc9, 0xa2, 0x4b),
            green: hex(0x6f, 0x8f, 0x5c),
            cyan: hex(0x5e, 0xa8, 0xb0),
            blue: hex(0x4f, 0x7f, 0xa8),
            magenta: hex(0x8a, 0x5a, 0x7a),
        }
    }
}

const fn hex(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

pub fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}
