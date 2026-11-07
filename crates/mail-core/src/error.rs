pub type Result<T> = std::result::Result<T, Error>;

impl From<lettre::transport::smtp::Error> for Error {
    fn from(e: lettre::transport::smtp::Error) -> Self {
        Error::Smtp {
            permanent: e.is_permanent(),
            message: e.to_string(),
        }
    }
}

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

    /// `permanent` is lettre's classification of the server's reply: a
    /// 5xx (bad credentials, a rejected recipient, a message too large)
    /// won't succeed on retry, so it must be shown to the user rather than
    /// queued. Everything else (connection failures, timeouts, 4xx) might.
    #[error("SMTP error: {message}")]
    Smtp { permanent: bool, message: String },

    #[error("invalid address: {0}")]
    InvalidAddress(String),

    #[error("couldn't build the message: {0}")]
    Compose(String),

    #[error("local cache error: {0}")]
    Store(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("mail parsing error: {0}")]
    Parse(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
}
