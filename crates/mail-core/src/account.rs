use crate::config::AccountConfig;
use crate::error::{Error, Result};

const KEYRING_SERVICE: &str = "email_client";

/// A configured account plus the ability to fetch its secret from the OS
/// keyring. The config file never stores the password itself.
#[derive(Debug, Clone)]
pub struct Account {
    pub config: AccountConfig,
}

impl Account {
    pub fn new(config: AccountConfig) -> Self {
        Self { config }
    }

    fn keyring_entry(&self) -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(KEYRING_SERVICE, &self.config.email)?)
    }

    /// Reads the account's password from the OS keyring (libsecret via
    /// zbus on Linux). Returns `Error::MissingSecret` if nothing has been
    /// stored yet.
    pub fn password(&self) -> Result<String> {
        let entry = self.keyring_entry()?;
        entry
            .get_password()
            .map_err(|e| match e {
                keyring::Error::NoEntry => Error::MissingSecret(self.config.email.clone()),
                other => Error::Keyring(other),
            })
    }

    pub fn set_password(&self, password: &str) -> Result<()> {
        let entry = self.keyring_entry()?;
        entry.set_password(password)?;
        Ok(())
    }

    /// Removes the stored secret entirely — used when the account itself
    /// is being removed. Treats "nothing was stored" as success rather
    /// than an error, since the end state (no secret in the keyring) is
    /// the same either way.
    pub fn delete_password(&self) -> Result<()> {
        let entry = self.keyring_entry()?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Keyring(e)),
        }
    }
}
