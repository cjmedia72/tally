use super::{ClaudeLiveLimits, SubQuota};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use std::io::{Read, Write};
use std::sync::{mpsc, OnceLock};
use std::time::Instant;

const CLAUDE_CLI_USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(18);

pub(super) fn fetch_cli_usage_limits() -> Result<ClaudeLiveLimits> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 42,
        cols: 140,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    #[cfg(windows)]
    let mut cmd = {
        let mut c = CommandBuilder::new("cmd.exe");
        c.args(["/C", "claude"]);
        c
    };

    #[cfg(not(windows))]
    let mut cmd = CommandBuilder::new("claude");

    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut writer = pair.master.take_writer()?;
    let mut reader = pair.master.try_clone_reader()?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(String::from_utf8_lossy(&buf[..n]).to_string())
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    std::thread::sleep(std::time::Duration::from_millis(900));
    writer.write_all(b"/usage\r")?;
    writer.flush()?;

    let started = Instant::now();
    let mut output = String::new();
    let mut first_relevant_at: Option<Instant> = None;
    while started.elapsed() < CLAUDE_CLI_USAGE_TIMEOUT {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&chunk);
        }
        if usage_output_ready(&output) {
            first_relevant_at.get_or_insert_with(Instant::now);
            if first_relevant_at
                .map(|t| t.elapsed() >= std::time::Duration::from_millis(1400))
                .unwrap_or(false)
            {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }

    let _ = writer.write_all(b"\x03");
    let _ = writer.flush();
    let _ = child.kill();
    while let Ok(chunk) = rx.try_recv() {
        output.push_str(&chunk);
    }

    parse_cli_usage_limits(&output)
}

fn usage_output_ready(text: &str) -> bool {
    let clean = strip_ansi(text);
    let normalized: String = clean
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    (normalized.contains("currentsession") || normalized.contains("currentweek"))
        && clean.contains('%')
        && (clean.to_lowercase().contains("used")
            || clean.to_lowercase().contains("left")
            || clean.to_lowercase().contains("remaining")
            || clean.to_lowercase().contains("available"))
        && !normalized.contains("loadingusage")
}

fn parse_cli_usage_limits(raw: &str) -> Result<ClaudeLiveLimits> {
    let clean = strip_ansi(raw);
    let panel = trim_to_latest_usage_panel(&clean).unwrap_or(clean.as_str());
    let lines: Vec<&str> = panel.lines().collect();
    let normalized: Vec<String> = lines.iter().map(|l| normalize_label(l)).collect();

    let session = extract_percent_after("currentsession", &lines, &normalized);
    let weekly = extract_percent_after("currentweekallmodels", &lines, &normalized)
        .or_else(|| extract_percent_after("weeklylimits", &lines, &normalized));
    let sonnet = extract_percent_after("currentweeksonnetonly", &lines, &normalized)
        .or_else(|| extract_percent_after("currentweeksonnet", &lines, &normalized))
        .or_else(|| extract_percent_after("sonnetonly", &lines, &normalized));

    let session = session
        .ok_or_else(|| anyhow!("Claude CLI /usage parse failed: missing Current session"))?;
    let session_reset = extract_reset_after("currentsession", &lines, &normalized);
    let weekly_reset = extract_reset_after("currentweekallmodels", &lines, &normalized)
        .or_else(|| extract_reset_after("weeklylimits", &lines, &normalized));
    let sonnet_reset = extract_reset_after("currentweeksonnetonly", &lines, &normalized)
        .or_else(|| extract_reset_after("currentweeksonnet", &lines, &normalized))
        .or_else(|| extract_reset_after("sonnetonly", &lines, &normalized));

    let mut sub_quotas = Vec::new();
    if let Some(sonnet_percent) = sonnet {
        sub_quotas.push(SubQuota {
            label: "Sonnet only".to_string(),
            utilization: sonnet_percent,
            resets_at: sonnet_reset,
        });
    }

    Ok(ClaudeLiveLimits {
        five_hour_percent: session,
        five_hour_resets_at: session_reset,
        weekly_percent: weekly.unwrap_or(0.0),
        weekly_resets_at: weekly_reset,
        sub_quotas,
        extra_usage: None,
    })
}

fn strip_ansi(text: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap())
        .replace_all(text, "")
        .to_string()
}

fn trim_to_latest_usage_panel(text: &str) -> Option<&str> {
    let lower = text.to_lowercase();
    if let Some(idx) = lower.rfind("plan usage limits") {
        return Some(&text[idx..]);
    }
    if let Some(idx) = lower.rfind("current session") {
        return Some(&text[idx..]);
    }
    if let Some(idx) = lower.rfind("usage limits") {
        return Some(&text[idx..]);
    }
    None
}

fn normalize_label(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn extract_percent_after(label: &str, lines: &[&str], normalized: &[String]) -> Option<f64> {
    for (idx, line) in normalized.iter().enumerate() {
        if line.contains(label) {
            for candidate in lines.iter().skip(idx).take(14) {
                let candidate_norm = normalize_label(candidate);
                if candidate_norm.starts_with("current") && !candidate_norm.contains(label) {
                    break;
                }
                if let Some(pct) = percent_from_line(candidate) {
                    return Some(pct);
                }
            }
        }
    }
    None
}

fn percent_from_line(line: &str) -> Option<f64> {
    let lower = line.to_lowercase();
    if lower.contains('|')
        && ["opus", "sonnet", "haiku", "default"]
            .iter()
            .any(|token| lower.contains(token))
    {
        return None;
    }
    static PCT: OnceLock<Regex> = OnceLock::new();
    let re = PCT.get_or_init(|| Regex::new(r"([0-9]{1,3}(?:\.[0-9]+)?)\s*%").unwrap());
    let caps = re.captures(line)?;
    let raw = caps.get(1)?.as_str().parse::<f64>().ok()?.clamp(0.0, 100.0);
    if ["used", "spent", "consumed"]
        .iter()
        .any(|token| lower.contains(token))
    {
        Some(raw)
    } else if ["left", "remaining", "available"]
        .iter()
        .any(|token| lower.contains(token))
    {
        Some(100.0 - raw)
    } else {
        None
    }
}

fn extract_reset_after(
    label: &str,
    lines: &[&str],
    normalized: &[String],
) -> Option<DateTime<Utc>> {
    for (idx, line) in normalized.iter().enumerate() {
        if line.contains(label) {
            for candidate in lines.iter().skip(idx).take(14) {
                let candidate_norm = normalize_label(candidate);
                if candidate_norm.starts_with("current") && !candidate_norm.contains(label) {
                    break;
                }
                if let Some(reset) = reset_from_line(candidate) {
                    return Some(reset);
                }
            }
        }
    }
    None
}

fn reset_from_line(line: &str) -> Option<DateTime<Utc>> {
    let lower = line.to_lowercase();
    if !lower.contains("reset") {
        return None;
    }
    let now = Utc::now();
    if lower.contains("less than a minute") {
        return Some(now + Duration::minutes(1));
    }

    static REL: OnceLock<Regex> = OnceLock::new();
    let rel = REL.get_or_init(|| {
        Regex::new(
            r"(?i)resets?\s+in\s+(?:(\d+)\s*h(?:r|our)?s?\s*)?(?:(\d+)\s*m(?:in(?:ute)?s?)?)?",
        )
        .unwrap()
    });
    if let Some(caps) = rel.captures(line) {
        let hours = caps
            .get(1)
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .unwrap_or(0);
        let mins = caps
            .get(2)
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .unwrap_or(0);
        if hours > 0 || mins > 0 {
            return Some(now + Duration::hours(hours) + Duration::minutes(mins));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_session_without_sonnet_overwrite() {
        let limits =
            parse_cli_usage_limits(include_str!("../../tests/fixtures/claude_usage/maxed.txt"))
                .unwrap();

        assert_eq!(limits.five_hour_percent, 100.0);
        assert_eq!(limits.weekly_percent, 44.0);
        assert_eq!(limits.sub_quotas.len(), 1);
        assert_eq!(limits.sub_quotas[0].label, "Sonnet only");
        assert_eq!(limits.sub_quotas[0].utilization, 0.0);
        assert!(limits.five_hour_resets_at.is_some());
        assert!(limits.weekly_resets_at.is_some());
    }

    #[test]
    fn parses_remaining_as_used_percent() {
        let limits = parse_cli_usage_limits(include_str!(
            "../../tests/fixtures/claude_usage/remaining.txt"
        ))
        .unwrap();

        assert_eq!(limits.five_hour_percent, 18.0);
        assert_eq!(limits.weekly_percent, 9.0);
    }

    #[test]
    fn trims_terminal_noise_and_ignores_model_table_percents() {
        let limits = parse_cli_usage_limits(include_str!(
            "../../tests/fixtures/claude_usage/ansi_noise.txt"
        ))
        .unwrap();

        assert_eq!(limits.five_hour_percent, 12.0);
        assert_eq!(limits.weekly_percent, 31.0);
        assert_eq!(limits.sub_quotas[0].utilization, 5.0);
    }

    #[test]
    fn rejects_empty_or_loading_output() {
        let err = parse_cli_usage_limits(include_str!(
            "../../tests/fixtures/claude_usage/loading.txt"
        ))
        .unwrap_err()
        .to_string();

        assert!(err.contains("missing Current session"));
    }
}
