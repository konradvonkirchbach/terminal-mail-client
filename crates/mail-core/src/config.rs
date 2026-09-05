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
    /// Email of the account that should be current on launch. `None`
    /// (the default) falls back to whichever is first in `accounts`.
    #[serde(default)]
    pub default_account: Option<String>,
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

fn data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "email_client")
        .ok_or_else(|| Error::Config("could not determine data directory".into()))?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Path to the local SQLite cache.
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("mail.db"))
}

/// Directory raw `.eml` bodies are cached under for one account, keyed by
/// its local (SQLite) account id rather than its email address so the
/// on-disk layout survives an account being renamed/re-added.
pub fn messages_dir(account_id: i64) -> Result<PathBuf> {
    Ok(data_dir()?.join(account_id.to_string()).join("messages"))
}

/// Directory queued-but-unsent raw messages are held under, for one
/// account, until `send::flush_outbox` manages to deliver them.
pub fn outbox_dir(account_id: i64) -> Result<PathBuf> {
    Ok(data_dir()?.join(account_id.to_string()).join("outbox"))
}

/// Restricts a directory we created ourselves to owner-only access
/// (`0700`). Our data directory holds the SQLite cache plus every
/// account's raw `.eml`/outbox files — full message content, not just
/// metadata — so relying on inherited/ambient umask (which on some
/// systems leaves new directories group- or world-readable) isn't
/// enough. A no-op on non-Unix targets, since this project doesn't
/// target them anyway.
pub fn restrict_dir(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = path;
    Ok(())
}

/// Same as `restrict_dir`, but `0600` (no execute bit — nothing here
/// needs to be traversed as a directory) for individual cache files:
/// the SQLite database itself, and each cached/queued `.eml`.
pub fn restrict_file(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
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
