use super::{ClaudeLimitSource, ClaudeLiveLimits, SubQuota};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Utc, Weekday};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use std::io::{Read, Write};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration as StdDuration, Instant};

const CLAUDE_CLI_USAGE_TIMEOUT: StdDuration = StdDuration::from_secs(24);
const CLAUDE_CLI_USAGE_RETRY_TIMEOUT: StdDuration = StdDuration::from_secs(60);

pub(super) fn fetch_cli_usage_limits() -> Result<ClaudeLiveLimits> {
    match fetch_cli_usage_limits_once(CLAUDE_CLI_USAGE_TIMEOUT) {
        Ok(limits) => Ok(limits),
        Err(err) if should_retry_cli_probe(&err) => {
            eprintln!("[tally] Claude CLI /usage retrying with extended timeout ({err})");
            fetch_cli_usage_limits_once(CLAUDE_CLI_USAGE_RETRY_TIMEOUT)
        }
        Err(err) => Err(err),
    }
}

fn fetch_cli_usage_limits_once(timeout: StdDuration) -> Result<ClaudeLiveLimits> {
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
    let mut cmd = {
        let mut c = CommandBuilder::new("claude");
        c.args(["--allowed-tools", ""]);
        c
    };

    if let Some(home) = dirs::home_dir() {
        cmd.cwd(home);
    }
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
    cmd.env_remove("ANTHROPIC_BASE_URL");
    cmd.env_remove("ANTHROPIC_MODEL");
    cmd.env_remove("ANTHROPIC_SMALL_FAST_MODEL");

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

    let mut output = String::new();
    let mut answered_dsr_count = 0;
    let mut trust_answered = false;
    let mut trust_answered_at: Option<Instant> = None;
    let startup_started = Instant::now();
    while startup_started.elapsed() < StdDuration::from_secs(10) {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&chunk);
        }
        answer_terminal_position_queries(&output, &mut writer, &mut answered_dsr_count)?;
        if !trust_answered && terminal_needs_trust_confirm(&output) {
            writer.write_all(b"\r")?;
            writer.flush()?;
            trust_answered = true;
            trust_answered_at = Some(Instant::now());
        }
        if terminal_ready_for_command(&output)
            || trust_answered_at
                .map(|t| t.elapsed() >= StdDuration::from_millis(2200))
                .unwrap_or(false)
            || (output.is_empty() && startup_started.elapsed() >= StdDuration::from_secs(3))
        {
            break;
        }
        std::thread::sleep(StdDuration::from_millis(120));
    }
    output.clear();
    writer.write_all(b"/usage\r")?;
    writer.flush()?;

    let started = Instant::now();
    let mut first_relevant_at: Option<Instant> = None;
    let mut last_enter_at = Instant::now();
    while started.elapsed() < timeout {
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&chunk);
        }
        answer_terminal_position_queries(&output, &mut writer, &mut answered_dsr_count)?;
        let lower = output.to_lowercase();
        let normalized = lower
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>();
        if normalized.contains("showplanusagelimits") || normalized.contains("showplan") {
            let _ = writer.write_all(b"\r");
            let _ = writer.flush();
        }
        if last_enter_at.elapsed() >= StdDuration::from_millis(800) {
            let _ = writer.write_all(b"\r");
            let _ = writer.flush();
            last_enter_at = Instant::now();
        }
        if usage_output_ready(&output) {
            first_relevant_at.get_or_insert_with(Instant::now);
            if first_relevant_at
                .map(|t| t.elapsed() >= StdDuration::from_millis(2000))
                .unwrap_or(false)
            {
                break;
            }
        }
        std::thread::sleep(StdDuration::from_millis(150));
    }

    let _ = writer.write_all(b"\x03");
    let _ = writer.flush();
    // Capture PID before kill so we can also reap the descendant process tree
    // on Windows. The PTY child is cmd.exe → claude.exe → (helpers). Without
    // a tree-kill, child.kill() only terminates cmd.exe and leaks claude.exe
    // every probe — that's a real process leak on long-running Tally sessions.
    let pty_pid = child.process_id();
    let _ = child.kill();
    #[cfg(windows)]
    if let Some(pid) = pty_pid {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    let _ = pty_pid;
    while let Ok(chunk) = rx.try_recv() {
        output.push_str(&chunk);
    }

    if std::env::var_os("TALLY_CLAUDE_DEBUG_CLI_OUTPUT").is_some() {
        dump_cli_output_tail(&output);
    }

    parse_cli_usage_limits(&output).inspect_err(|_| {
        if std::env::var_os("TALLY_CLAUDE_DEBUG_CLI_OUTPUT").is_some() {
            dump_cli_output_tail(&output);
        }
    })
}

fn dump_cli_output_tail(output: &str) {
    let clean = strip_ansi(output);
    let tail = clean
        .chars()
        .rev()
        .take(1800)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    eprintln!("[tally] Claude CLI /usage raw tail:\n{tail}");
}

fn answer_terminal_position_queries(
    output: &str,
    writer: &mut Box<dyn Write + Send>,
    answered_dsr_count: &mut usize,
) -> Result<()> {
    while output.matches("\x1b[6n").count() > *answered_dsr_count {
        writer.write_all(b"\x1b[1;1R")?;
        writer.flush()?;
        *answered_dsr_count += 1;
    }
    Ok(())
}

fn terminal_needs_trust_confirm(output: &str) -> bool {
    let normalized = normalize_label(&strip_ansi(output));
    normalized.contains("quicksafetycheck")
        || normalized.contains("doyoutrustthefiles")
        || normalized.contains("yesitrustthisfolder")
}

fn terminal_ready_for_command(output: &str) -> bool {
    let normalized = normalize_label(&strip_ansi(output));
    normalized.contains("forshortcuts")
        || normalized.contains("welcomeback")
        || normalized.contains("tryhowdoilog")
}

fn should_retry_cli_probe(err: &anyhow::Error) -> bool {
    let message = err.to_string().to_lowercase();
    message.contains("timed out")
        || message.contains("no output")
        || message.contains("still loading usage")
        || message.contains("startup output")
}

pub(crate) fn claude_cli_available() -> bool {
    std::process::Command::new("claude")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Detects a Claude Code update banner/prompt in captured CLI output. Used
/// only to produce a clearer error message when a probe yields no usage data —
/// never to drive keystrokes. Matches the normalized (lowercase-alphanumeric)
/// forms of the phrasings Claude Code uses for update notices.
fn looks_like_update_prompt(clean: &str) -> bool {
    let n = normalize_label(clean);
    n.contains("updateavailable")
        || n.contains("newversionofclaudecode")
        || n.contains("anewversionisavailable")
        || (n.contains("npminstall") && n.contains("claudecode"))
        || n.contains("runclaudeupdate")
}

fn usage_output_ready(text: &str) -> bool {
    let clean = strip_ansi(text);
    let normalized: String = clean
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    (usage_capture_has_session_value(&normalized) && clean.contains('%'))
        || (usage_output_looks_relevant(&clean)
            && clean.to_lowercase().contains("failed to load usage data"))
}

fn parse_cli_usage_limits(raw: &str) -> Result<ClaudeLiveLimits> {
    let clean = strip_ansi(raw);
    if clean.trim().is_empty() {
        return Err(anyhow!("Claude CLI /usage probe timed out with no output"));
    }
    if is_usage_still_loading(&clean) {
        return Err(anyhow!("Claude CLI /usage is still loading usage data"));
    }
    if normalize_label(&clean).contains("failedtoloadusagedata") {
        return Err(anyhow!("Claude CLI could not load usage data"));
    }
    if !usage_output_looks_relevant(&clean) {
        // Surface a specific, actionable error when the probe is blocked by an
        // update banner rather than a vague "startup output". We do NOT inject
        // keystrokes to dismiss it (the existing periodic-Enter loop already
        // clears non-blocking banners, and speculative key-injection into an
        // unverified Claude prompt risks corrupting a working capture).
        if looks_like_update_prompt(&clean) {
            return Err(anyhow!(
                "Claude CLI /usage blocked by an update prompt — run `claude` to update, then retry"
            ));
        }
        return Err(anyhow!("Claude CLI /usage looked like startup output"));
    }
    let panel = trim_to_latest_usage_panel(&clean).unwrap_or(clean.as_str());
    let lines: Vec<&str> = panel.lines().collect();
    let normalized: Vec<String> = lines.iter().map(|l| normalize_label(l)).collect();

    let session = extract_percent_after_compact("currentsession", panel)
        .or_else(|| extract_percent_after("currentsession", &lines, &normalized));
    let weekly = extract_percent_after_compact("currentweekallmodels", panel)
        .or_else(|| extract_percent_after("currentweekallmodels", &lines, &normalized))
        .or_else(|| extract_percent_after_compact("weeklylimits", panel))
        .or_else(|| extract_percent_after("weeklylimits", &lines, &normalized));
    let sonnet = extract_percent_after_compact("currentweeksonnetonly", panel)
        .or_else(|| extract_percent_after("currentweeksonnetonly", &lines, &normalized))
        .or_else(|| extract_percent_after_compact("currentweeksonnet", panel))
        .or_else(|| extract_percent_after("currentweeksonnet", &lines, &normalized))
        .or_else(|| extract_percent_after_compact("sonnetonly", panel))
        .or_else(|| extract_percent_after("sonnetonly", &lines, &normalized));

    let session = session
        .ok_or_else(|| anyhow!("Claude CLI /usage parse failed: missing Current session"))?;
    let session_reset = extract_reset_after_compact("currentsession", panel)
        .or_else(|| extract_reset_after("currentsession", &lines, &normalized));
    let weekly_reset = extract_reset_after_compact("currentweekallmodels", panel)
        .or_else(|| extract_reset_after("currentweekallmodels", &lines, &normalized))
        .or_else(|| extract_reset_after_compact("weeklylimits", panel))
        .or_else(|| extract_reset_after("weeklylimits", &lines, &normalized));
    let sonnet_reset = extract_reset_after_compact("currentweeksonnetonly", panel)
        .or_else(|| extract_reset_after("currentweeksonnetonly", &lines, &normalized))
        .or_else(|| extract_reset_after_compact("currentweeksonnet", panel))
        .or_else(|| extract_reset_after("currentweeksonnet", &lines, &normalized))
        .or_else(|| extract_reset_after_compact("sonnetonly", panel))
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
        source: ClaudeLimitSource::Cli,
        fetched_at: Utc::now(),
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
    if let Some(idx) = lower.rfind("settings:") {
        let tail = &text[idx..];
        let tail_lower = tail.to_lowercase();
        if tail_lower.contains("usage")
            && (tail.contains('%')
                || tail_lower.contains("loading usage")
                || tail_lower.contains("loadingusage")
                || tail_lower.contains("current session")
                || tail_lower.contains("current week"))
        {
            return Some(tail);
        }
    }
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

fn usage_output_looks_relevant(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    normalized.contains("currentsession")
        || normalized.contains("currentweek")
        || normalized.contains("loadingusage")
        || normalized.contains("failedtoloadusagedata")
}

fn is_usage_still_loading(text: &str) -> bool {
    let normalized: String = strip_ansi(text)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    normalized.contains("loadingusage")
        && !usage_capture_has_session_value(&normalized)
        && all_usage_percents(text).is_empty()
}

fn usage_capture_has_session_value(normalized: &str) -> bool {
    normalized.contains("currentsession")
        && (normalized.contains("used")
            || normalized.contains("left")
            || normalized.contains("remaining")
            || normalized.contains("available"))
}

fn all_usage_percents(text: &str) -> Vec<f64> {
    let normalized: String = strip_ansi(text)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let has_usage_windows =
        normalized.contains("currentsession") || normalized.contains("currentweek");
    let loading_only = normalized.contains("loadingusage") && !has_usage_windows;
    let has_usage_keywords = normalized.contains("used")
        || normalized.contains("left")
        || normalized.contains("remaining")
        || normalized.contains("available");
    if loading_only || !has_usage_keywords {
        return Vec::new();
    }
    strip_ansi(text)
        .lines()
        .filter_map(percent_from_line)
        .collect()
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

fn extract_percent_after_compact(label: &str, panel: &str) -> Option<f64> {
    let compact = compact_usage_text(panel);
    let tail = compact_tail_after(&compact, label)?;
    static PCT: OnceLock<Regex> = OnceLock::new();
    let re = PCT.get_or_init(|| Regex::new(r"([0-9]{1,3}(?:\.[0-9]+)?)%([a-z]{0,24})").unwrap());
    for caps in re.captures_iter(tail) {
        let raw = caps.get(1)?.as_str().parse::<f64>().ok()?.clamp(0.0, 100.0);
        let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        if suffix.starts_with("used")
            || suffix.starts_with("spent")
            || suffix.starts_with("consumed")
        {
            return Some(raw);
        }
        if suffix.starts_with("left")
            || suffix.starts_with("remaining")
            || suffix.starts_with("available")
        {
            return Some(100.0 - raw);
        }
    }
    None
}

fn compact_usage_text(text: &str) -> String {
    strip_ansi(text)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '%' | ':' | ','))
        .collect()
}

fn compact_tail_after<'a>(compact: &'a str, label: &str) -> Option<&'a str> {
    let label_idx = compact.find(label)?;
    let start = label_idx + label.len();
    let boundaries = [
        "currentsession",
        "currentweekallmodels",
        "currentweeksonnetonly",
        "currentweeksonnet",
        "sonnetonly",
        "whatscontributing",
        "usagecredits",
    ];
    let end = boundaries
        .iter()
        .filter_map(|boundary| {
            compact[start..]
                .find(boundary)
                .map(|idx| start + idx)
                .filter(|idx| *idx > start)
        })
        .min()
        .unwrap_or(compact.len());
    Some(&compact[start..end])
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

fn extract_reset_after_compact(label: &str, panel: &str) -> Option<DateTime<Utc>> {
    let compact = compact_usage_text(panel);
    let tail = compact_tail_after(&compact, label)?;

    static MONTH_DAY: OnceLock<Regex> = OnceLock::new();
    let month_day = MONTH_DAY.get_or_init(|| {
        Regex::new(
            r"(?:resets?|reses)(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*(\d{1,2}),?(\d{1,2})(?::(\d{2}))?(am|pm)",
        )
        .unwrap()
    });
    if let Some(caps) = month_day.captures(tail) {
        let month = month_from_text(caps.get(1)?.as_str())?;
        let day = caps.get(2)?.as_str().parse::<u32>().ok()?;
        let hour_raw = caps.get(3)?.as_str().parse::<u32>().ok()?;
        let minute = caps
            .get(4)
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        return local_month_day_time_to_utc(month, day, hour_raw, minute, caps.get(5)?.as_str());
    }

    static WEEKDAY: OnceLock<Regex> = OnceLock::new();
    let weekday = WEEKDAY.get_or_init(|| {
        Regex::new(
            r"(?:resets?|reses)(mon|tue|wed|thu|fri|sat|sun)[a-z]*(\d{1,2})(?::(\d{2}))?(am|pm)",
        )
        .unwrap()
    });
    if let Some(caps) = weekday.captures(tail) {
        let line = format!(
            "Resets {} {}:{} {}",
            caps.get(1)?.as_str(),
            caps.get(2)?.as_str(),
            caps.get(3).map(|m| m.as_str()).unwrap_or("00"),
            caps.get(4)?.as_str()
        );
        return parse_weekday_reset(&line);
    }

    static TIME_ONLY: OnceLock<Regex> = OnceLock::new();
    let time_only = TIME_ONLY
        .get_or_init(|| Regex::new(r"(?:resets?|reses)(\d{1,2})(?::(\d{2}))?(am|pm)").unwrap());
    if let Some(caps) = time_only.captures(tail) {
        let hour_raw = caps.get(1)?.as_str().parse::<u32>().ok()?;
        let minute = caps
            .get(2)
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        return local_time_to_next_utc(hour_raw, minute, caps.get(3)?.as_str());
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
            r"(?i)resets?\s+in\s+(?:(\d+)\s*d(?:ay)?s?\s*)?(?:(\d+)\s*h(?:r|our)?s?\s*)?(?:(\d+)\s*m(?:in(?:ute)?s?)?)?",
        )
        .unwrap()
    });
    if let Some(caps) = rel.captures(line) {
        let days = caps
            .get(1)
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .unwrap_or(0);
        let hours = caps
            .get(2)
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .unwrap_or(0);
        let mins = caps
            .get(3)
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .unwrap_or(0);
        if days > 0 || hours > 0 || mins > 0 {
            return Some(
                now + Duration::days(days) + Duration::hours(hours) + Duration::minutes(mins),
            );
        }
    }

    parse_weekday_reset(line)
}

fn parse_weekday_reset(line: &str) -> Option<DateTime<Utc>> {
    static ABS: OnceLock<Regex> = OnceLock::new();
    let abs = ABS.get_or_init(|| {
        Regex::new(r"(?i)resets?\s+([a-z]{3,9})\s+(\d{1,2})(?::(\d{2}))?\s*(am|pm)").unwrap()
    });
    let caps = abs.captures(line)?;
    let weekday = weekday_from_text(caps.get(1)?.as_str())?;
    let hour_raw = caps.get(2)?.as_str().parse::<u32>().ok()?;
    let minute = caps
        .get(3)
        .and_then(|m| m.as_str().parse::<u32>().ok())
        .unwrap_or(0);
    let meridiem = caps.get(4)?.as_str().to_ascii_lowercase();
    if hour_raw == 0 || hour_raw > 12 || minute > 59 {
        return None;
    }
    let hour = match meridiem.as_str() {
        "am" if hour_raw == 12 => 0,
        "am" => hour_raw,
        "pm" if hour_raw == 12 => 12,
        "pm" => hour_raw + 12,
        _ => return None,
    };
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let now = Local::now();
    let today = now.date_naive();
    let current = now.weekday().num_days_from_monday() as i64;
    let target = weekday.num_days_from_monday() as i64;
    let mut days_ahead = (target - current).rem_euclid(7);
    let mut candidate_date = today + Duration::days(days_ahead);
    let mut candidate = Local
        .from_local_datetime(&candidate_date.and_time(time))
        .single()?;
    if candidate <= now {
        days_ahead += 7;
        candidate_date = today + Duration::days(days_ahead);
        candidate = Local
            .from_local_datetime(&candidate_date.and_time(time))
            .single()?;
    }
    Some(candidate.with_timezone(&Utc))
}

fn local_month_day_time_to_utc(
    month: u32,
    day: u32,
    hour_raw: u32,
    minute: u32,
    meridiem: &str,
) -> Option<DateTime<Utc>> {
    let hour = hour_from_meridiem(hour_raw, meridiem)?;
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let now = Local::now();
    let mut year = now.year();
    let mut date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let mut candidate = Local.from_local_datetime(&date.and_time(time)).single()?;
    if candidate <= now {
        year += 1;
        date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
        candidate = Local.from_local_datetime(&date.and_time(time)).single()?;
    }
    Some(candidate.with_timezone(&Utc))
}

fn local_time_to_next_utc(hour_raw: u32, minute: u32, meridiem: &str) -> Option<DateTime<Utc>> {
    let hour = hour_from_meridiem(hour_raw, meridiem)?;
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let now = Local::now();
    let today = now.date_naive();
    let mut candidate = Local.from_local_datetime(&today.and_time(time)).single()?;
    if candidate <= now {
        candidate = Local
            .from_local_datetime(&(today + Duration::days(1)).and_time(time))
            .single()?;
    }
    Some(candidate.with_timezone(&Utc))
}

fn hour_from_meridiem(hour_raw: u32, meridiem: &str) -> Option<u32> {
    if hour_raw == 0 || hour_raw > 12 {
        return None;
    }
    match meridiem {
        "am" if hour_raw == 12 => Some(0),
        "am" => Some(hour_raw),
        "pm" if hour_raw == 12 => Some(12),
        "pm" => Some(hour_raw + 12),
        _ => None,
    }
}

fn month_from_text(text: &str) -> Option<u32> {
    match &text.to_ascii_lowercase()[..3.min(text.len())] {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

fn weekday_from_text(text: &str) -> Option<Weekday> {
    match &text.to_ascii_lowercase()[..3.min(text.len())] {
        "mon" => Some(Weekday::Mon),
        "tue" => Some(Weekday::Tue),
        "wed" => Some(Weekday::Wed),
        "thu" => Some(Weekday::Thu),
        "fri" => Some(Weekday::Fri),
        "sat" => Some(Weekday::Sat),
        "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_prompt_detector_matches_known_phrasings() {
        assert!(looks_like_update_prompt("Update available! 2.1.0 -> 2.2.0"));
        assert!(looks_like_update_prompt(
            "A new version of Claude Code is ready"
        ));
        assert!(looks_like_update_prompt(
            "Run npm install -g @anthropic-ai/claude-code to update"
        ));
        // Real usage output must NOT trip the detector (no false positives that
        // would mask a genuine parse failure).
        assert!(!looks_like_update_prompt(
            "Current session 42% used, resets 3:00pm"
        ));
        assert!(!looks_like_update_prompt(""));
    }

    #[test]
    fn update_prompt_yields_actionable_error() {
        let err = parse_cli_usage_limits("Update available! Run claude update").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("update prompt"), "got: {msg}");
        assert!(msg.contains("claude"), "got: {msg}");
    }

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
        assert_eq!(limits.sub_quotas[0].utilization, 2.0);
        assert!(limits.weekly_resets_at.is_some());
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

        assert!(err.contains("still loading usage"));
    }

    #[test]
    fn trims_to_latest_settings_usage_panel() {
        let limits = parse_cli_usage_limits(include_str!(
            "../../tests/fixtures/claude_usage/settings_panel_with_status_noise.txt"
        ))
        .unwrap();

        assert_eq!(limits.five_hour_percent, 12.0);
        assert_eq!(limits.weekly_percent, 22.0);
        assert_eq!(limits.sub_quotas[0].utilization, 0.0);
    }

    #[test]
    fn parses_compact_tui_capture_without_reusing_session_percent() {
        let limits = parse_cli_usage_limits(include_str!(
            "../../tests/fixtures/claude_usage/compact_tui_capture.txt"
        ))
        .unwrap();

        assert_eq!(limits.five_hour_percent, 14.0);
        assert_eq!(limits.weekly_percent, 38.0);
        assert_eq!(limits.sub_quotas[0].utilization, 0.0);
        assert!(limits.five_hour_resets_at.is_some());
        assert!(limits.weekly_resets_at.is_some());
    }

    #[test]
    fn rejects_startup_output_as_retryable() {
        let err = parse_cli_usage_limits(include_str!(
            "../../tests/fixtures/claude_usage/startup_output.txt"
        ))
        .unwrap_err()
        .to_string();

        assert!(err.contains("startup output"));
        assert!(should_retry_cli_probe(&anyhow!(err)));
    }

    #[test]
    fn rejects_failed_to_load_usage_data_without_guessing() {
        let err = parse_cli_usage_limits(include_str!(
            "../../tests/fixtures/claude_usage/failed_to_load.txt"
        ))
        .unwrap_err()
        .to_string();

        assert!(err.contains("could not load usage data"));
        assert!(!should_retry_cli_probe(&anyhow!(err)));
    }

    #[test]
    fn parses_weekday_reset_time_from_claude_cli() {
        let reset = reset_from_line("Resets Sun 8:00 PM").unwrap();

        assert!(reset > Utc::now());
        assert!(reset <= Utc::now() + Duration::days(7));
    }
}
