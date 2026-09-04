pub type Result<T> = std::result::Result<T, Error>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("no password found in keyring for {0}; run with --set-password once to store it")]
    MissingSecret(String),

    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),

    #[error("IMAP connection error: {0}")]
    ImapConnect(String),

    #[error("IMAP protocol error: {0}")]
    Imap(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("mail parsing error: {0}")]
    Parse(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
