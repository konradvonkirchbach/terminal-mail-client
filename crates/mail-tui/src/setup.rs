use std::io::{self, Write};

use mail_core::config::{AccountConfig, Config};
use mail_core::{Account, Store};

/// First-run interactive wizard: prompts for one account's connection
/// details on stdin, stores the password in the OS keyring, and writes
/// the config file. Runs before the TUI takes over the terminal.
pub async fn run() -> anyhow::Result<Account> {
    println!("No accounts configured yet — let's add one.");
    println!(
        "(Stored at {:?}; the password goes to your OS keyring, never to disk.)\n",
        mail_core::config::config_path()?
    );

    let (account_config, password) = prompt_account_details()?;
    if !connection_works(&account_config, &password).await? {
        anyhow::bail!("not saved — run mailc again to re-enter the settings");
    }

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
pub async fn add_account() -> anyhow::Result<()> {
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
    if !connection_works(&account_config, &password).await? {
        anyhow::bail!("not saved — run mailc --add-account again to re-enter the settings");
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

/// `mailc --remove-account`: deletes an account's config entry, its
/// keyring secret, and its entire local cache (cached messages,
/// attachments, on-disk `.eml`/outbox files) — asks for confirmation
/// first since none of that is recoverable.
pub async fn remove_account() -> anyhow::Result<()> {
    let mut config = Config::load()?;
    if config.accounts.is_empty() {
        anyhow::bail!("no accounts configured");
    }

    let index = pick_account(&config.accounts, "remove")?;
    let account_config = config.accounts[index].clone();

    print!(
        "Remove {} and delete its local cache? This cannot be undone. [y/N] ",
        account_config.email
    );
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if !line.trim().eq_ignore_ascii_case("y") {
        println!("Cancelled.");
        return Ok(());
    }

    config.accounts.remove(index);
    if config.default_account.as_deref() == Some(account_config.email.as_str()) {
        config.default_account = None;
    }
    config.save()?;

    let account = Account::new(account_config.clone());
    if let Err(e) = account.delete_password() {
        eprintln!("warning: failed to remove keyring entry: {e}");
    }

    // The accounts row cascades to folders/messages/attachments/drafts/
    // outbox via foreign keys; only the on-disk files need a separate pass.
    let store = Store::open(&mail_core::config::db_path()?)?;
    let account_id = store
        .upsert_account(account_config.email.clone(), account_config.display_name.clone())
        .await?;
    store.delete_account(account_id).await?;
    let _ = std::fs::remove_dir_all(mail_core::config::messages_dir(account_id)?);
    let _ = std::fs::remove_dir_all(mail_core::config::outbox_dir(account_id)?);

    println!("Removed {}.", account_config.email);
    Ok(())
}

/// `mailc --set-default-account`: picks which configured account should
/// be current when `mailc` launches, instead of always whichever happens
/// to be first in the config file.
pub fn set_default_account() -> anyhow::Result<()> {
    let mut config = Config::load()?;
    if config.accounts.is_empty() {
        anyhow::bail!("no accounts configured yet — run `mailc` once to set one up first");
    }

    if config.accounts.len() == 1 {
        let email = config.accounts[0].email.clone();
        config.default_account = Some(email.clone());
        config.save()?;
        println!("Only one account configured ({email}) — nothing else for it to default over.");
        return Ok(());
    }

    let index = pick_account(&config.accounts, "set as default")?;
    let email = config.accounts[index].email.clone();
    config.default_account = Some(email.clone());
    config.save()?;
    println!("{email} will now be open when mailc launches.");
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

    let index = pick_account(&config.accounts, "update")?;
    let account = Account::new(config.accounts[index].clone());
    println!("Updating password for {}", account.config.email);
    let password = rpassword::prompt_password("New password (or app password): ")?;
    account.set_password(&password)?;
    println!("Saved to your OS keyring.");
    Ok(())
}

/// Logs in to both servers with the details just entered and reports how
/// that went. Returns whether to go ahead and save them: yes if both
/// worked, otherwise only if the user insists (a server can be down for a
/// moment, or the network may not be up yet).
async fn connection_works(config: &AccountConfig, password: &str) -> anyhow::Result<bool> {
    println!("\nChecking the connection...");
    let check = mail_core::check::connection(&Account::new(config.clone()), password).await;
    let report = |server: &str, host: &str, port: u16, result: &mail_core::Result<()>| match result {
        Ok(()) => println!("  {server} {host}:{port}: ok"),
        Err(e) => println!("  {server} {host}:{port}: {e}"),
    };
    report("IMAP", &config.imap_host, config.imap_port, &check.imap);
    report("SMTP", &config.smtp_host, config.smtp_port, &check.smtp);
    if check.ok() {
        return Ok(true);
    }
    print!("Save these settings anyway? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("y"))
}

/// Prompts for which configured account a CLI action applies to, when
/// there's more than one — returns its index into `accounts` directly
/// (no picker) when there's only one.
fn pick_account(accounts: &[AccountConfig], verb: &str) -> anyhow::Result<usize> {
    if accounts.len() == 1 {
        return Ok(0);
    }

    println!("Accounts:");
    for (i, a) in accounts.iter().enumerate() {
        println!("  {}) {}", i + 1, a.email);
    }
    let choice: usize = prompt(&format!("Which account to {verb}"))?
        .parse()
        .ok()
        .filter(|n| *n >= 1 && *n <= accounts.len())
        .ok_or_else(|| anyhow::anyhow!("enter a number from the list above"))?;
    Ok(choice - 1)
}

fn prompt_account_details() -> anyhow::Result<(AccountConfig, String)> {
    let email = prompt("Email address")?;
    let display_name = prompt("Your name, as recipients should see it (optional)")?;
    let imap_host = prompt("IMAP host (e.g. imap.gmail.com)")?;
    let imap_port = prompt_with_default("IMAP port (993 = TLS, 143 = STARTTLS)", "993")?
        .parse()
        .unwrap_or(993);
    let smtp_host = prompt("SMTP host (e.g. smtp.gmail.com)")?;
    let smtp_port = prompt_with_default("SMTP port (587 = STARTTLS, 465 = TLS)", "587")?
        .parse()
        .unwrap_or(587);
    let password = rpassword::prompt_password("Password (or app password): ")?;

    let account_config = AccountConfig {
        email,
        display_name: (!display_name.is_empty()).then_some(display_name),
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
