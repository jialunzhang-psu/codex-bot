use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local};
use clap::Subcommand;

use crate::accounts::PoolCatalog;
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
    #[command(alias = "add_accounts")]
    AddAccounts {
        labels: Vec<String>,
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

enum AccountScope {
    Resolved(AccountTarget),
    Ambiguous(Vec<AccountTarget>),
}

struct AccountTarget {
    project_name: Option<String>,
    codex_home: PathBuf,
    codex: CodexClient,
}

pub async fn run(
    config_path: &Path,
    selected_project: Option<&str>,
    command: AccountsCommand,
) -> Result<()> {
    let scope = resolve_account_scope(config_path, selected_project)?;

    match command {
        AccountsCommand::List => list_accounts(&scope),
        AccountsCommand::Add { label } => {
            login_and_add_account(require_target(&scope, "add")?, label).await
        }
        AccountsCommand::AddAccounts { labels } => {
            login_and_add_accounts(require_target(&scope, "add-accounts")?, labels).await
        }
        AccountsCommand::Login { label } => {
            login_and_add_account(require_target(&scope, "login")?, label).await
        }
        AccountsCommand::Switch { account } => {
            switch_account(require_target(&scope, "switch")?, &account)
        }
        AccountsCommand::Remove { account, all } => {
            remove_account(require_target(&scope, "remove")?, account, all)
        }
        AccountsCommand::Usage => show_usage(require_target(&scope, "usage")?).await,
    }
}

fn resolve_account_scope(
    config_path: &Path,
    selected_project: Option<&str>,
) -> Result<AccountScope> {
    let discovered_projects = Config::discover_projects(config_path)?;
    if discovered_projects.is_empty()
        || selected_project.is_some()
        || discovered_projects.len() == 1
    {
        let config = Config::load(config_path, selected_project)?;
        return Ok(AccountScope::Resolved(account_target_from_config(config)?));
    }

    let mut targets = Vec::new();
    let mut unique_homes = Vec::<PathBuf>::new();
    for project_name in discovered_projects {
        let config = Config::load(config_path, Some(&project_name))?;
        let target = account_target_from_config(config)?;
        if !unique_homes.iter().any(|path| path == &target.codex_home) {
            unique_homes.push(target.codex_home.clone());
        }
        targets.push(target);
    }

    if unique_homes.len() == 1 {
        let target = targets
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no codex projects found in {}", config_path.display()))?;
        Ok(AccountScope::Resolved(target))
    } else {
        Ok(AccountScope::Ambiguous(targets))
    }
}

fn account_target_from_config(config: Config) -> Result<AccountTarget> {
    let project_name = config.project_name.clone();
    let codex = CodexClient::new(&config.codex);
    let codex_home = codex.codex_home()?;
    Ok(AccountTarget {
        project_name,
        codex_home,
        codex,
    })
}

fn require_target<'a>(scope: &'a AccountScope, command: &str) -> Result<&'a AccountTarget> {
    match scope {
        AccountScope::Resolved(target) => Ok(target),
        AccountScope::Ambiguous(targets) => bail!(
            "accounts {command} needs --project because your config resolves to multiple CODEX_HOME values:\n{}",
            format_target_homes(targets)
        ),
    }
}

fn list_accounts(scope: &AccountScope) -> Result<()> {
    match scope {
        AccountScope::Resolved(target) => list_accounts_for_target(target),
        AccountScope::Ambiguous(targets) => list_accounts_global(targets),
    }
}

fn list_accounts_for_target(target: &AccountTarget) -> Result<()> {
    let list = target.codex.list_accounts()?;
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

fn list_accounts_global(targets: &[AccountTarget]) -> Result<()> {
    let list = crate::accounts::list_pool_accounts()?;
    if list.accounts.is_empty() {
        println!(
            "No pooled accounts found in {}.\nUse `accounts login [label]` or `accounts add [label]` to add one.\n\nMultiple CODEX_HOME values were detected:\n{}",
            list.pool_dir.display(),
            format_target_homes(targets)
        );
        return Ok(());
    }

    println!("{}", format_global_account_list(&list, targets));
    Ok(())
}

async fn login_and_add_account(target: &AccountTarget, label: Option<String>) -> Result<()> {
    target.codex.run_device_login_interactive().await?;
    let result = target.codex.add_current_account(label)?;
    println!("{}", format_add_account_completion(&result));
    Ok(())
}

async fn login_and_add_accounts(target: &AccountTarget, labels: Vec<String>) -> Result<()> {
    if labels.is_empty() {
        bail!("usage: accounts add-accounts <label>...");
    }

    let total = labels.len();
    for (index, label) in labels.into_iter().enumerate() {
        login_and_add_account_with_retries(target, label.clone(), index + 1, total).await?;
        if index + 1 < total {
            println!();
        }
    }
    Ok(())
}

async fn login_and_add_account_with_retries(
    target: &AccountTarget,
    label: String,
    index: usize,
    total: usize,
) -> Result<()> {
    const RETRY_DELAYS_SECONDS: [u64; 3] = [30, 60, 120];

    let mut attempt = 0usize;
    loop {
        println!(
            "[{}/{}] Starting device login for label `{}`",
            index, total, label
        );
        match login_and_add_account(target, Some(label.clone())).await {
            Ok(()) => return Ok(()),
            Err(err)
                if is_device_code_rate_limited(&err) && attempt < RETRY_DELAYS_SECONDS.len() =>
            {
                let delay = RETRY_DELAYS_SECONDS[attempt];
                eprintln!(
                    "Device login rate-limited for label `{}`. Waiting {}s before retrying...",
                    label, delay
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn is_device_code_rate_limited(err: &anyhow::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("device code request failed")
        && (text.contains("429") || text.contains("too many requests"))
}

fn switch_account(target: &AccountTarget, query: &str) -> Result<()> {
    let list = target.codex.list_accounts()?;
    let matched = match_account(&list.accounts, query)?;
    let result = target.codex.switch_account(&matched.id)?;
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

fn remove_account(target: &AccountTarget, account: Option<String>, all: bool) -> Result<()> {
    if all {
        let result = target.codex.remove_all_accounts()?;
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
    let list = target.codex.list_accounts()?;
    let matched = match_account(&list.accounts, &query)?;
    let result = target.codex.remove_account(&matched.id)?;
    println!(
        "Removed account: {}. Was active: {}. Remaining: {}.",
        format_stored_account(&result.account),
        yes_no(result.removed_was_active),
        result.remaining_accounts
    );
    Ok(())
}

async fn show_usage(target: &AccountTarget) -> Result<()> {
    let report = target.codex.get_all_usage().await?;
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

fn format_global_account_list(list: &PoolCatalog, targets: &[AccountTarget]) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Codex account pool: {}", list.pool_dir.display()));
    lines.push("Active auth file: multiple CODEX_HOME values detected".to_string());
    for (index, account) in list.accounts.iter().enumerate() {
        lines.push(format!(
            "  {}. {} added={}",
            index + 1,
            format_stored_account(account),
            format_account_created_at(account.created_ms)
        ));
    }
    lines.push(
        "Pass `--project <name>` to inspect or modify the active account for one project."
            .to_string(),
    );
    lines.push("Project CODEX_HOME values:".to_string());
    for line in format_target_homes(targets).lines() {
        lines.push(line.to_string());
    }
    lines.join("\n")
}

fn format_target_homes(targets: &[AccountTarget]) -> String {
    targets
        .iter()
        .map(|target| {
            let name = target.project_name.as_deref().unwrap_or("(native config)");
            format!("- {name}: {}", target.codex_home.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
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

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use anyhow::anyhow;
    use tempfile::TempDir;

    use super::{AccountScope, is_device_code_rate_limited, resolve_account_scope};

    #[test]
    fn resolve_account_scope_collapses_shared_codex_home() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[[projects]]
name = "one"
[projects.agent]
type = "codex"
[projects.agent.options]
work_dir = "./one"
extra_env = ["CODEX_HOME=/tmp/shared-codex-home"]
[[projects.platforms]]
type = "telegram"
[projects.platforms.options]
token = "1"

[[projects]]
name = "two"
[projects.agent]
type = "codex"
[projects.agent.options]
work_dir = "./two"
extra_env = ["CODEX_HOME=/tmp/shared-codex-home"]
[[projects.platforms]]
type = "telegram"
[projects.platforms.options]
token = "2"
"#,
        )?;

        assert!(matches!(
            resolve_account_scope(&config_path, None)?,
            AccountScope::Resolved(_)
        ));
        Ok(())
    }

    #[test]
    fn resolve_account_scope_keeps_distinct_codex_homes_ambiguous() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[[projects]]
name = "one"
[projects.agent]
type = "codex"
[projects.agent.options]
work_dir = "./one"
extra_env = ["CODEX_HOME=/tmp/codex-home-one"]
[[projects.platforms]]
type = "telegram"
[projects.platforms.options]
token = "1"

[[projects]]
name = "two"
[projects.agent]
type = "codex"
[projects.agent.options]
work_dir = "./two"
extra_env = ["CODEX_HOME=/tmp/codex-home-two"]
[[projects.platforms]]
type = "telegram"
[projects.platforms.options]
token = "2"
"#,
        )?;

        assert!(matches!(
            resolve_account_scope(&config_path, None)?,
            AccountScope::Ambiguous(_)
        ));
        Ok(())
    }

    #[test]
    fn device_code_rate_limit_detection_is_specific() {
        let limited = anyhow!(
            "Error logging in with device code: device code request failed with status 429 Too Many Requests"
        );
        assert!(is_device_code_rate_limited(&limited));

        let other = anyhow!("codex login exited with status exit status: 1");
        assert!(!is_device_code_rate_limited(&other));
    }
}
