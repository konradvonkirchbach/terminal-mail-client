use std::io::{self, Write};

use mail_core::config::{AccountConfig, Config};
use mail_core::Account;

/// First-run interactive wizard: prompts for one account's connection
/// details on stdin, stores the password in the OS keyring, and writes
/// the config file. Runs before the TUI takes over the terminal.
pub fn run() -> anyhow::Result<Account> {
    println!("No accounts configured yet — let's add one.");
    println!(
        "(Stored at {:?}; the password goes to your OS keyring, never to disk.)\n",
        mail_core::config::config_path()?
    );

    let (account_config, password) = prompt_account_details()?;

    let account = Account::new(account_config.clone());
    account.set_password(&password)?;

    let mut config = Config::load()?;
    config.accounts.push(account_config);
    config.save()?;

    println!("\nSaved. Launching...\n");
    Ok(account)
}

/// `mailc --add-account`: prompts for another account and appends it to
/// the existing config, without touching the ones already there. Doesn't
/// launch the TUI — run `mailc` normally afterward to use it.
pub fn add_account() -> anyhow::Result<()> {
    let mut config = Config::load()?;

    println!("Adding another account.");
    println!(
        "(Stored at {:?}; the password goes to your OS keyring, never to disk.)\n",
        mail_core::config::config_path()?
    );

    let (account_config, password) = prompt_account_details()?;

    if config.accounts.iter().any(|a| a.email == account_config.email) {
        anyhow::bail!("an account for {} is already configured", account_config.email);
    }

    let account = Account::new(account_config.clone());
    account.set_password(&password)?;

    config.accounts.push(account_config);
    config.save()?;

    println!(
        "\nAdded {}. Restart mailc (or switch to it with Tab) to use it.",
        account.config.email
    );
    Ok(())
}

/// `mailc --set-password`: updates the stored password/app-password for
/// an already-configured account without touching anything else (host,
/// port, etc.) or wiping the config file. Doesn't touch the TUI at all.
pub fn set_password() -> anyhow::Result<()> {
    let config = Config::load()?;
    if config.accounts.is_empty() {
        anyhow::bail!("no account configured yet — run `mailc` once to set one up first");
    }

    let account_config = if config.accounts.len() == 1 {
        config.accounts.into_iter().next().unwrap()
    } else {
        println!("Multiple accounts configured:");
        for (i, a) in config.accounts.iter().enumerate() {
            println!("  {}) {}", i + 1, a.email);
        }
        let choice: usize = prompt("Which account")?
            .parse()
            .ok()
            .filter(|n| *n >= 1 && *n <= config.accounts.len())
            .ok_or_else(|| anyhow::anyhow!("enter a number from the list above"))?;
        config.accounts.into_iter().nth(choice - 1).unwrap()
    };

    let account = Account::new(account_config);
    println!("Updating password for {}", account.config.email);
    let password = rpassword::prompt_password("New password (or app password): ")?;
    account.set_password(&password)?;
    println!("Saved to your OS keyring.");
    Ok(())
}

fn prompt_account_details() -> anyhow::Result<(AccountConfig, String)> {
    let email = prompt("Email address")?;
    let imap_host = prompt("IMAP host (e.g. imap.gmail.com)")?;
    let imap_port = prompt_with_default("IMAP port", "993")?.parse().unwrap_or(993);
    let smtp_host = prompt("SMTP host (e.g. smtp.gmail.com)")?;
    let smtp_port = prompt_with_default("SMTP port", "587")?.parse().unwrap_or(587);
    let password = rpassword::prompt_password("Password (or app password): ")?;

    let account_config = AccountConfig {
        email,
        display_name: None,
        imap_host,
        imap_port,
        smtp_host,
        smtp_port,
        auth: Default::default(),
        fetch_limit: 50,
    };
    Ok((account_config, password))
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
