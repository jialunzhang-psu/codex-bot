use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use crate::interact::pty::{PtyKey, PtySnapshot};
use crate::scenario::expect::Matcher;
use crate::scenario::runtime::ScenarioRuntime;

pub async fn expect_resume_list(
    runtime: &mut ScenarioRuntime,
    timeout: Duration,
) -> Result<PtySnapshot> {
    ensure_claude_cli_attached(runtime)?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = runtime.claude_cli_mut()?.snapshot()?;
        if is_resume_list_ready(&snapshot) {
            runtime.last_claude_snapshot = Some(snapshot_copy(&snapshot));
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "resume picker never became interactive\ncurrent screen:\n{}\n\ntranscript tail:\n{}",
                snapshot.screen,
                transcript_tail(&snapshot.transcript, 4000)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn resume_session(
    runtime: &mut ScenarioRuntime,
    index: usize,
    timeout: Duration,
) -> Result<PtySnapshot> {
    ensure_claude_cli_attached(runtime)?;
    {
        let cli = runtime.claude_cli_mut()?;
        cli.write_line("/resume")?;
    }
    let initial = expect_resume_list(runtime, timeout).await?;
    let total = current_resume_entries(&initial).len();
    if total == 0 {
        bail!("resume picker showed no entries")
    }
    if index >= total {
        bail!("resume index {} out of range for {} entries", index, total)
    }
    for _ in 0..index {
        runtime.claude_cli_mut()?.send_key(PtyKey::Down)?;
    }
    runtime.claude_cli_mut()?.send_key(PtyKey::Enter)?;
    let snapshot = wait_for_post_selection(runtime, timeout).await?;
    runtime.last_claude_snapshot = Some(snapshot_copy(&snapshot));
    Ok(snapshot)
}

pub fn capture_last_message(runtime: &mut ScenarioRuntime) -> Result<String> {
    ensure_claude_cli_attached(runtime)?;
    let snapshot = runtime.claude_cli_mut()?.snapshot()?;
    runtime.last_claude_snapshot = Some(snapshot_copy(&snapshot));
    let message = extract_real_message(&snapshot)
        .ok_or_else(|| anyhow!("expected a real Claude message in current CLI view"))?;
    runtime.last_claude_message = Some(message.clone());
    Ok(message)
}

pub fn expect_last_message(runtime: &ScenarioRuntime, matcher: &Matcher) -> Result<()> {
    let message = runtime
        .last_claude_message
        .as_deref()
        .ok_or_else(|| anyhow!("no Claude message captured yet"))?;
    matcher.check(message, "Claude last message")
}

pub async fn expect_view(
    runtime: &mut ScenarioRuntime,
    matcher: &Matcher,
    timeout: Duration,
) -> Result<PtySnapshot> {
    ensure_claude_cli_attached(runtime)?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = runtime.claude_cli_mut()?.snapshot()?;
        if matcher.check(&snapshot.screen, "Claude CLI view").is_ok()
            || matcher
                .check(&snapshot.transcript, "Claude CLI transcript")
                .is_ok()
        {
            runtime.last_claude_snapshot = Some(snapshot_copy(&snapshot));
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "timed out waiting for Claude CLI view\nscreen:\n{}\n\ntranscript tail:\n{}",
                snapshot.screen,
                transcript_tail(&snapshot.transcript, 4000)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn ensure_claude_cli_attached(runtime: &ScenarioRuntime) -> Result<()> {
    if runtime.claude_cli.is_some() {
        Ok(())
    } else {
        bail!("Claude CLI is not attached")
    }
}

fn is_resume_list_ready(snapshot: &PtySnapshot) -> bool {
    snapshot.screen.contains("Resume Session")
        && snapshot.screen.contains("Search…")
        && contains_any(snapshot, &["Ctrl+A", "Space to preview", "Type to search"])
        && current_highlighted_entry(snapshot).is_some()
}

fn current_resume_entries(snapshot: &PtySnapshot) -> Vec<String> {
    snapshot
        .screen
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix('❯').or_else(|| line.strip_prefix("  ")))
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('/'))
        .map(ToString::to_string)
        .collect()
}

fn current_highlighted_entry(snapshot: &PtySnapshot) -> Option<String> {
    for line in snapshot.screen.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('❯') {
            let entry = rest.trim();
            if !entry.is_empty() && !entry.starts_with('/') {
                return Some(entry.to_string());
            }
        }
    }
    None
}

fn extract_last_assistant_message(snapshot: &PtySnapshot) -> Option<String> {
    let mut messages = Vec::new();
    let mut current = Vec::new();

    for line in snapshot
        .screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.starts_with('●') {
            if !current.is_empty() {
                messages.push(current.join(" "));
                current.clear();
            }
            let content = line.trim_start_matches('●').trim();
            if !content.is_empty() {
                current.push(content.to_string());
            }
            continue;
        }

        if current.is_empty() {
            continue;
        }

        if line.starts_with('⎿')
            || line.starts_with('❯')
            || line.starts_with('?')
            || line.starts_with('✻')
            || line.starts_with('─')
            || line.contains("for shortcuts")
            || line.contains("low · /effort")
            || line.contains("Claude Code has switched")
            || line.contains("Resume Session")
            || line.contains("Search…")
            || line.contains("Command copied to clipboard")
        {
            messages.push(current.join(" "));
            current.clear();
            continue;
        }

        current.push(line.to_string());
    }

    if !current.is_empty() {
        messages.push(current.join(" "));
    }

    messages.into_iter().rev().find(|message| {
        !message.is_empty()
            && !message.starts_with("Bash(")
            && !message.starts_with("Update(")
            && !message.starts_with("Write(")
            && !message.starts_with("Read ")
            && !message.contains("Added ")
            && !message.contains("removed ")
    })
}

fn extract_last_user_message(snapshot: &PtySnapshot) -> Option<String> {
    snapshot
        .screen
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('❯'))
        .map(|line| line.trim_start_matches('❯').trim().to_string())
        .filter(|line| !line.is_empty() && !line.starts_with('/'))
        .last()
}

fn extract_real_message(snapshot: &PtySnapshot) -> Option<String> {
    extract_last_assistant_message(snapshot).or_else(|| extract_last_user_message(snapshot))
}

fn wait_for_post_selection_sync(snapshot: &PtySnapshot) -> bool {
    let left_picker =
        !snapshot.screen.contains("Resume Session") && !snapshot.screen.contains("Search…");
    let cross_directory = contains_any(
        snapshot,
        &[
            "This conversation is from a different directory.",
            "To resume, run:",
            "Command copied to clipboard",
        ],
    );
    left_picker && (extract_real_message(snapshot).is_some() || cross_directory)
}

async fn wait_for_post_selection(
    runtime: &mut ScenarioRuntime,
    timeout: Duration,
) -> Result<PtySnapshot> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = runtime.claude_cli_mut()?.snapshot()?;
        if wait_for_post_selection_sync(&snapshot) {
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "selection never produced post-picker output\ncurrent screen:\n{}\n\ntranscript tail:\n{}",
                snapshot.screen,
                transcript_tail(&snapshot.transcript, 4000)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn contains_any(snapshot: &PtySnapshot, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| snapshot.screen.contains(needle) || snapshot.transcript.contains(needle))
}

fn snapshot_copy(snapshot: &PtySnapshot) -> PtySnapshot {
    PtySnapshot {
        screen: snapshot.screen.clone(),
        transcript: snapshot.transcript.clone(),
        transcript_debug: snapshot.transcript_debug.clone(),
    }
}

fn transcript_tail(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
