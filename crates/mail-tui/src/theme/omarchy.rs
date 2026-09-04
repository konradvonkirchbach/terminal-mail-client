use serde::Deserialize;

use super::palette::{parse_hex, Theme};

/// Mirrors the keys present in every Omarchy-generated
/// `$XDG_STATE_HOME/omarchy/current/theme/colors.toml`, regardless of
/// which theme is active. Unknown extra keys (hyprland_*, darker_background,
/// light_foreground, brown/orange on some themes, etc.) are ignored.
#[derive(Debug, Deserialize)]
struct OmarchyColors {
    accent: String,
    selection: String,
    muted: String,
    background: String,
    foreground: String,
    bright_foreground: String,
    red: String,
    yellow: String,
    green: String,
    cyan: String,
    blue: String,
    magenta: String,
}

/// Reads the live, already-resolved Omarchy palette. Returns `None` if
/// Omarchy isn't installed/active, the file is missing, or any of the
/// expected keys fail to parse as a hex color — callers should fall back
/// to `Theme::fallback()` in that case.
pub fn load() -> Option<Theme> {
    let base = directories::BaseDirs::new()?;
    let path = base
        .state_dir()?
        .join("omarchy/current/theme/colors.toml");

    let raw = std::fs::read_to_string(path).ok()?;
    let colors: OmarchyColors = toml::from_str(&raw).ok()?;

    Some(Theme {
        background: parse_hex(&colors.background)?,
        foreground: parse_hex(&colors.foreground)?,
        bright_foreground: parse_hex(&colors.bright_foreground)?,
        accent: parse_hex(&colors.accent)?,
        selection: parse_hex(&colors.selection)?,
        muted: parse_hex(&colors.muted)?,
        red: parse_hex(&colors.red)?,
        yellow: parse_hex(&colors.yellow)?,
        green: parse_hex(&colors.green)?,
        cyan: parse_hex(&colors.cyan)?,
        blue: parse_hex(&colors.blue)?,
        magenta: parse_hex(&colors.magenta)?,
    })
}
