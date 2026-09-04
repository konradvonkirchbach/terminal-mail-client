use std::io::{self, Write};

use mail_core::config::{AccountConfig, Config};
use mail_core::Account;

/// First-run interactive wizard: prompts for one account's connection
/// details on stdin, stores the password in the OS keyring, and writes
/// the config file. Runs before the TUI takes over the terminal.
pub fn run() -> anyhow::Result<Account> {
    println!("No accounts configured yet — let's add one.");
    println!("(Stored at {:?}; the password goes to your OS keyring, never to disk.)\n",
        mail_core::config::config_path()?);

    let email = prompt("Email address")?;
    let imap_host = prompt("IMAP host (e.g. imap.gmail.com)")?;
    let imap_port = prompt_with_default("IMAP port", "993")?.parse().unwrap_or(993);
    let smtp_host = prompt("SMTP host (e.g. smtp.gmail.com)")?;
    let smtp_port = prompt_with_default("SMTP port", "587")?.parse().unwrap_or(587);
    let password = rpassword::prompt_password("Password (or app password): ")?;

    let account_config = AccountConfig {
        email: email.clone(),
        display_name: None,
        imap_host,
        imap_port,
        smtp_host,
        smtp_port,
        auth: Default::default(),
        fetch_limit: 50,
    };

    let account = Account::new(account_config.clone());
    account.set_password(&password)?;

    let mut config = Config::load()?;
    config.accounts.push(account_config);
    config.save()?;

    println!("\nSaved. Launching...\n");
    Ok(account)
}

fn prompt(label: &str) -> anyhow::Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_with_default(label: &str, default: &str) -> anyhow::Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}
