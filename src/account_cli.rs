use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local};
use clap::Subcommand;

use crate::codex::{
    AddAccountResult, CodexClient, PoolAccountView, PoolList, PoolUsageReport, StoredAccount,
    UsageCredits, UsageReport,
};
use crate::config::Config;

#[derive(Debug, Subcommand)]
pub enum AccountsCommand {
    List,
    Add {
        label: Option<String>,
    },
    Login {
        label: Option<String>,
    },
    Switch {
        account: String,
    },
    Remove {
        account: Option<String>,
        #[arg(long, conflicts_with = "account")]
        all: bool,
    },
    Usage,
}

pub async fn run(
    config_path: &Path,
    selected_project: Option<&str>,
    command: AccountsCommand,
) -> Result<()> {
    let discovered_projects = Config::discover_projects(config_path)?;
    if selected_project.is_none() && discovered_projects.len() > 1 {
        bail!(
            "multiple projects found in {}. Pass --project for account commands. Available: {}",
            config_path.display(),
            discovered_projects.join(", ")
        );
    }

    let config = Config::load(config_path, selected_project)?;
    let codex = CodexClient::new(&config.codex);

    match command {
        AccountsCommand::List => list_accounts(&codex),
        AccountsCommand::Add { label } => add_current_account(&codex, label),
        AccountsCommand::Login { label } => login_and_add_account(&codex, label).await,
        AccountsCommand::Switch { account } => switch_account(&codex, &account),
        AccountsCommand::Remove { account, all } => remove_account(&codex, account, all),
        AccountsCommand::Usage => show_usage(&codex).await,
    }
}

fn list_accounts(codex: &CodexClient) -> Result<()> {
    let list = codex.list_accounts()?;
    if list.accounts.is_empty() {
        println!(
            "No pooled accounts found in {}.\nUse `accounts login [label]` or `accounts add [label]` to add one.",
            list.pool_dir.display()
        );
        return Ok(());
    }

    println!("{}", format_account_list(&list));
    Ok(())
}

fn add_current_account(codex: &CodexClient, label: Option<String>) -> Result<()> {
    let result = codex.add_current_account(label)?;
    println!("{}", format_add_account_completion(&result));
    Ok(())
}

async fn login_and_add_account(codex: &CodexClient, label: Option<String>) -> Result<()> {
    codex.run_device_login_interactive().await?;
    let result = codex.add_current_account(label)?;
    println!("{}", format_add_account_completion(&result));
    Ok(())
}

fn switch_account(codex: &CodexClient, query: &str) -> Result<()> {
    let list = codex.list_accounts()?;
    let matched = match_account(&list.accounts, query)?;
    let result = codex.switch_account(&matched.id)?;
    if result.switched {
        println!(
            "Switched active Codex account: {} -> {}.",
            result
                .from
                .as_ref()
                .map(format_stored_account)
                .unwrap_or_else(|| "(none)".to_string()),
            format_stored_account(&result.to),
        );
    } else {
        println!(
            "Account {} is already active.",
            format_stored_account(&result.to)
        );
    }
    Ok(())
}

fn remove_account(codex: &CodexClient, account: Option<String>, all: bool) -> Result<()> {
    if all {
        let result = codex.remove_all_accounts()?;
        println!(
            "Removed {} pooled account(s). Remaining: {}.",
            result.removed.len(),
            result.remaining_accounts
        );
        return Ok(());
    }

    let Some(query) = account else {
        bail!("usage: accounts remove <number|id|label> or accounts remove --all");
    };
    let list = codex.list_accounts()?;
    let matched = match_account(&list.accounts, &query)?;
    let result = codex.remove_account(&matched.id)?;
    println!(
        "Removed account: {}. Was active: {}. Remaining: {}.",
        format_stored_account(&result.account),
        yes_no(result.removed_was_active),
        result.remaining_accounts
    );
    Ok(())
}

async fn show_usage(codex: &CodexClient) -> Result<()> {
    let report = codex.get_all_usage().await?;
    println!("{}", format_pool_usage_report(&report));
    Ok(())
}

fn format_add_account_completion(result: &AddAccountResult) -> String {
    let verb = if result.created_new {
        "Added account to the pool"
    } else {
        "Account is already in the pool"
    };
    format!(
        "{verb}: {account}\nTotal pooled accounts: {total}",
        verb = verb,
        account = format_stored_account(&result.account),
        total = result.total_accounts
    )
}

fn format_account_list(list: &PoolList) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Codex account pool: {}", list.pool_dir.display()));
    lines.push(format!(
        "Active auth file: {}",
        list.codex_auth_path.display()
    ));
    for (index, account) in list.accounts.iter().enumerate() {
        let marker = if account.active { "*" } else { " " };
        lines.push(format!(
            "{marker} {}. {} added={}",
            index + 1,
            format_pool_account(account),
            format_account_created_at(account.created_ms)
        ));
    }
    if !list.active_in_pool {
        lines.push("Current active auth is not one of the pooled accounts.".to_string());
    }
    lines.push(
        "Use `accounts switch <number|id|label>` to switch, or `accounts remove <number|id|label|--all>` to delete."
            .to_string(),
    );
    lines.join("\n")
}

fn format_pool_usage_report(report: &PoolUsageReport) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Codex account usage: {}",
        report.pool_dir.display()
    ));
    lines.push(format!(
        "Active auth file: {}",
        report.codex_auth_path.display()
    ));
    if !report.active_in_pool && report.accounts.iter().any(|account| account.pooled) {
        lines.push("Current active auth is not one of the pooled accounts.".to_string());
    }
    for account in &report.accounts {
        lines.push(String::new());
        let marker = if account.active { "*" } else { " " };
        let scope = if account.pooled { "pooled" } else { "current" };
        lines.push(format!(
            "{marker} {} ({scope}) added={}",
            format_stored_account(&account.account),
            format_account_created_at(account.account.created_ms)
        ));
        lines.push(format!("Status: {}", account.status));
        if let Some(message) = &account.message {
            lines.push(format!("Message: {}", truncate_text(message, 240)));
        }
        if let Some(usage) = &account.usage {
            append_usage_lines(&mut lines, usage);
        }
    }
    lines.join("\n")
}

fn append_usage_lines(lines: &mut Vec<String>, report: &UsageReport) {
    lines.push(format!("Account: {}", usage_account_display(report)));
    lines.push(format!(
        "Provider: {}",
        if report.provider.trim().is_empty() {
            "codex"
        } else {
            report.provider.as_str()
        }
    ));

    if let Some(credits) = &report.credits {
        lines.push(format!("Credits: {}", format_credits(credits)));
    }

    if report.buckets.is_empty() {
        lines.push("No usage buckets were returned.".to_string());
        return;
    }

    for bucket in &report.buckets {
        lines.push(format!(
            "{}: allowed={}, limit_reached={}",
            bucket.name,
            yes_no(bucket.allowed),
            yes_no(bucket.limit_reached)
        ));
        if bucket.windows.is_empty() {
            lines.push("No windows reported.".to_string());
            continue;
        }
        for window in &bucket.windows {
            let remaining = (100 - window.used_percent).max(0);
            lines.push(format!(
                "{}: remaining={}%, resets in {}",
                usage_window_label(window.window_seconds),
                remaining,
                format_reset_after(window.reset_after_seconds)
            ));
        }
    }
}

fn format_pool_account(account: &PoolAccountView) -> String {
    let stored = StoredAccount {
        id: account.id.clone(),
        label: account.label.clone(),
        created_ms: account.created_ms,
    };
    format_stored_account(&stored)
}

fn format_stored_account(account: &StoredAccount) -> String {
    match account.label.as_deref() {
        Some(label) => format!("{label} [{}]", account.id),
        None => account.id.clone(),
    }
}

fn format_account_created_at(created_ms: u128) -> String {
    let millis = created_ms.min(u64::MAX as u128) as u64;
    let time = SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
    let local: DateTime<Local> = DateTime::from(time);
    local.format("%m-%d %H:%M").to_string()
}

fn match_account<'a>(accounts: &'a [PoolAccountView], query: &str) -> Result<&'a PoolAccountView> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("No account matched."));
    }

    if let Ok(index) = trimmed.parse::<usize>() {
        if (1..=accounts.len()).contains(&index) {
            return Ok(&accounts[index - 1]);
        }
    }

    let lower = trimmed.to_ascii_lowercase();
    let exact: Vec<&PoolAccountView> = accounts
        .iter()
        .filter(|account| {
            account.id.eq_ignore_ascii_case(trimmed)
                || account
                    .label
                    .as_deref()
                    .is_some_and(|label| label.eq_ignore_ascii_case(trimmed))
        })
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }
    if exact.len() > 1 {
        return Err(ambiguous_account_error(trimmed, &exact));
    }

    let prefix: Vec<&PoolAccountView> = accounts
        .iter()
        .filter(|account| {
            account.id.to_ascii_lowercase().starts_with(&lower)
                || account
                    .label
                    .as_deref()
                    .is_some_and(|label| label.to_ascii_lowercase().starts_with(&lower))
        })
        .collect();
    if prefix.len() == 1 {
        return Ok(prefix[0]);
    }
    if prefix.len() > 1 {
        return Err(ambiguous_account_error(trimmed, &prefix));
    }

    let contains: Vec<&PoolAccountView> = accounts
        .iter()
        .filter(|account| {
            account.id.to_ascii_lowercase().contains(&lower)
                || account
                    .label
                    .as_deref()
                    .is_some_and(|label| label.to_ascii_lowercase().contains(&lower))
        })
        .collect();
    if contains.len() == 1 {
        return Ok(contains[0]);
    }
    if contains.len() > 1 {
        return Err(ambiguous_account_error(trimmed, &contains));
    }

    Err(anyhow!("No account matched: {trimmed}"))
}

fn ambiguous_account_error(query: &str, matches: &[&PoolAccountView]) -> anyhow::Error {
    let options = matches
        .iter()
        .map(|account| format_pool_account(account))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!("Multiple accounts matched {query:?}: {options}")
}

fn usage_account_display(report: &UsageReport) -> String {
    let base = if !report.email.trim().is_empty() {
        report.email.clone()
    } else if !report.account_id.trim().is_empty() {
        report.account_id.clone()
    } else if !report.user_id.trim().is_empty() {
        report.user_id.clone()
    } else {
        "-".to_string()
    };

    if report.plan.trim().is_empty() {
        base
    } else {
        format!("{base} ({})", report.plan)
    }
}

fn format_credits(credits: &UsageCredits) -> String {
    if credits.unlimited {
        return "unlimited".to_string();
    }
    if let Some(balance) = &credits.balance {
        return format!(
            "available={}, balance={}",
            yes_no(credits.has_credits),
            balance
        );
    }
    format!("available={}", yes_no(credits.has_credits))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn usage_window_label(seconds: i64) -> String {
    match seconds {
        18_000 => "5h limit".to_string(),
        604_800 => "7d limit".to_string(),
        86_400 => "24h limit".to_string(),
        _ if seconds > 0 => format!("{} window", format_duration(seconds)),
        _ => "window".to_string(),
    }
}

fn format_reset_after(seconds: i64) -> String {
    if seconds <= 0 {
        "-".to_string()
    } else {
        format_duration(seconds)
    }
}

fn format_duration(total_seconds: i64) -> String {
    let total_seconds = total_seconds.max(0);
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{total_seconds}s")
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut out = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}
