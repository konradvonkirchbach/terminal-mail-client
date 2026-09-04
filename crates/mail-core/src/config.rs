use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::error::{Error, Result};

/// Top-level config file, `~/.config/email_client/config.toml`.
///
/// `accounts` is a list from day one (even with a single entry) so that
/// adding multi-account support later never requires a breaking config
/// migration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub email: String,
    #[serde(default)]
    pub display_name: Option<String>,

    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,

    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,

    #[serde(default)]
    pub auth: AuthMethod,

    /// How many recent messages to pull into memory in Phase 1 (no local
    /// cache yet). Kept small by default so startup stays fast.
    #[serde(default = "default_fetch_limit")]
    pub fetch_limit: u32,
}

fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    587
}
fn default_fetch_limit() -> u32 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    /// Plain/app-password auth. The password itself is never stored in
    /// this file — only looked up from the OS keyring using the account's
    /// email as the keyring entry key.
    #[default]
    Password,
    // OAuth2 { provider: OAuthProvider } — added in a later phase.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    /// "auto" (read the live Omarchy theme), "off" (always use the
    /// bundled fallback palette).
    #[serde(default = "default_theme_mode")]
    pub mode: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            mode: default_theme_mode(),
        }
    }
}

fn default_theme_mode() -> String {
    "auto".to_string()
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "email_client")
        .ok_or_else(|| Error::Config("could not determine config directory".into()))?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)?;
        toml::from_str(&raw).map_err(|e| Error::Config(format!("{path:?}: {e}")))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw =
            toml::to_string_pretty(self).map_err(|e| Error::Config(format!("serialize: {e}")))?;
        std::fs::write(&path, raw)?;
        Ok(())
    }
}
