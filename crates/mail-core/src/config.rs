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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A scratch directory under the OS temp dir, unique to this test
    /// process/thread/call — deliberately not the real config/data dirs,
    /// since these tests must never touch the user's actual mail cache.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mail-core-test-{label}-{}-{:?}",
                std::process::id(),
                std::time::Instant::now()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn restrict_dir_sets_owner_only_permissions() {
        let dir = ScratchDir::new("restrict-dir");
        // Loosen it first so the test actually exercises restrict_dir
        // rather than passing by accident on whatever mode it started at.
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();

        restrict_dir(&dir.0).unwrap();

        let mode = std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn restrict_file_sets_owner_only_permissions() {
        let dir = ScratchDir::new("restrict-file");
        let file = dir.0.join("secret.txt");
        std::fs::write(&file, b"hi").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        restrict_file(&file).unwrap();

        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn messages_dir_and_outbox_dir_are_scoped_by_account_id_and_kind() {
        let messages = messages_dir(42).unwrap();
        let outbox = outbox_dir(42).unwrap();
        assert!(messages.ends_with("42/messages"));
        assert!(outbox.ends_with("42/outbox"));
        assert_ne!(messages, outbox);
        assert_ne!(messages, messages_dir(7).unwrap());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut config = Config {
            default_account: Some("me@example.com".to_string()),
            ..Config::default()
        };
        config.accounts.push(AccountConfig {
            email: "me@example.com".to_string(),
            display_name: Some("Me".to_string()),
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            auth: AuthMethod::Password,
            fetch_limit: 50,
        });

        let raw = toml::to_string_pretty(&config).unwrap();
        let round_tripped: Config = toml::from_str(&raw).unwrap();

        assert_eq!(round_tripped.default_account, config.default_account);
        assert_eq!(round_tripped.accounts.len(), 1);
        assert_eq!(round_tripped.accounts[0].email, "me@example.com");
        assert_eq!(round_tripped.accounts[0].imap_port, 993);
    }

    #[test]
    fn missing_optional_account_fields_fall_back_to_documented_defaults() {
        let raw = r#"
            email = "me@example.com"
            imap_host = "imap.example.com"
            smtp_host = "smtp.example.com"
        "#;
        let account: AccountConfig = toml::from_str(raw).unwrap();
        assert_eq!(account.imap_port, 993);
        assert_eq!(account.smtp_port, 587);
        assert_eq!(account.fetch_limit, 50);
        assert!(matches!(account.auth, AuthMethod::Password));
    }
}
