mod omarchy;
mod palette;

pub use palette::Theme;

/// Resolves the active theme according to `mode`: "auto" tries the live
/// Omarchy palette first and falls back to the bundled default; "off"
/// always uses the bundled default.
pub fn resolve(mode: &str) -> Theme {
    if mode == "auto" {
        if let Some(theme) = omarchy::load() {
            return theme;
        }
    }
    Theme::fallback()
}
