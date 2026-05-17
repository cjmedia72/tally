use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration as StdDuration;
use walkdir::WalkDir;

// =====================================================================
// LIVE rate limits via `codex app-server` JSON-RPC.
// Matches what the Codex Desktop popup shows. Same auth pool.
// Method: account/rateLimits/read
// =====================================================================

#[derive(Debug, Deserialize)]
struct RpcResponse {
    id: Option<u32>,
    result: Option<RateLimitsResult>,
}

#[derive(Debug, Deserialize)]
struct RateLimitsResult {
    #[serde(rename = "rateLimits")]
    rate_limits: Option<RateLimitsPayload>,
}

#[derive(Debug, Deserialize)]
struct RateLimitsPayload {
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
    #[serde(rename = "planType")]
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RateLimitWindow {
    #[serde(rename = "usedPercent")]
    used_percent: i64,
    #[serde(rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(rename = "windowDurationMins")]
    window_duration_mins: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct CodexPeriodStats {
    pub tokens: CodexTokens,
    pub requests: u64,
    pub cost: f64,
}

#[derive(Debug, Default, Clone)]
pub struct CodexStats {
    pub today: CodexPeriodStats,
    pub d1: CodexPeriodStats,
    pub d7: CodexPeriodStats,
    pub d14: CodexPeriodStats,
    pub d30: CodexPeriodStats,
    pub mtd: CodexPeriodStats,
    pub rate_limits: CodexRateLimits,
    pub plan_label: String,
    pub plan_label_raw: String,
    pub last_event_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexTokens {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning: u64,
}

#[derive(Debug, Default, Clone)]
pub struct CodexRateLimits {
    pub primary_used_percent: f64,
    pub primary_resets_at: Option<DateTime<Utc>>,
    pub secondary_used_percent: f64,
    pub secondary_resets_at: Option<DateTime<Utc>>,
}

/// True if Codex CLI is installed AND authenticated (auth.json exists).
pub fn is_available() -> bool {
    let codex_path = locate_codex();
    if !codex_path.exists() {
        return false;
    }
    if let Some(mut auth) = dirs::home_dir() {
        auth.push(".codex");
        auth.push("auth.json");
        if !auth.exists() {
            return false;
        }
    }
    true
}

/// Locate the codex executable. Windows doesn't auto-resolve .cmd/.exe shims
/// when `Command::new("codex")` is invoked from a child process, so we have
/// to find it explicitly. Order: PATH lookup via `where`, npm-global vendor
/// path, then fall back to plain "codex" and hope.
fn locate_codex() -> PathBuf {
    // 1. Native Win32 vendor binary (npm-global install of @openai/codex)
    if let Some(mut p) = dirs::data_dir() {
        // dirs::data_dir() = %APPDATA% on Windows
        p.push("npm");
        p.push("node_modules");
        p.push("@openai");
        p.push("codex");
        p.push("node_modules");
        p.push("@openai");
        p.push("codex-win32-x64");
        p.push("vendor");
        p.push("x86_64-pc-windows-msvc");
        p.push("codex");
        p.push("codex.exe");
        if p.exists() {
            return p;
        }
    }
    // 2. PATH lookup via where (Windows) / which (Unix)
    let lookup_cmd = if cfg!(windows) { "where" } else { "which" };
    for name in &["codex.cmd", "codex.exe", "codex"] {
        if let Ok(out) = Command::new(lookup_cmd).arg(name).output() {
            if out.status.success() {
                if let Some(first) = String::from_utf8_lossy(&out.stdout).lines().next() {
                    let p = PathBuf::from(first.trim());
                    if p.exists() {
                        return p;
                    }
                }
            }
        }
    }
    // 3. Last resort
    PathBuf::from(if cfg!(windows) { "codex.cmd" } else { "codex" })
}

/// Spawn `codex app-server`, send initialize + account/rateLimits/read,
/// wait actively for the id=2 response via a reader thread + channel.
/// Holds stdin open until we have the answer (mimics the working manual
/// pipe sequence where a trailing sleep keeps stdin alive while codex
/// reaches out to OpenAI's API for the live rate-limit state).
///
/// Returns: (rate_limits, plan_label_human, plan_type_raw, fetched_at)
pub fn fetch_live_rate_limits() -> Result<(CodexRateLimits, String, String, DateTime<Utc>)> {
    let codex_path = locate_codex();
    let mut child = Command::new(&codex_path)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("spawn {} failed: {}", codex_path.display(), e))?;

    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

    // Reader thread — pushes every line into a channel until id=2 lands
    // (or EOF). Main thread waits with a generous timeout.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(|r| r.ok()) {
            let contains_id2 = line.contains("\"id\":2");
            if tx.send(line).is_err() {
                break;
            }
            if contains_id2 {
                break;
            }
        }
    });

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"tally","version":"0.1.0"}}}"#;
    let initd = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
    let read = r#"{"jsonrpc":"2.0","id":2,"method":"account/rateLimits/read","params":{}}"#;
    writeln!(stdin, "{init}")?;
    writeln!(stdin, "{initd}")?;
    writeln!(stdin, "{read}")?;
    stdin.flush()?;
    // Hold stdin alive — do NOT drop until we have the answer or time out.

    let deadline = std::time::Instant::now() + StdDuration::from_secs(8);
    let mut found: Option<RateLimitsPayload> = None;
    let mut diag = String::new();
    let mut lines_seen = 0usize;
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                lines_seen += 1;
                if diag.len() < 400 {
                    diag.push_str(&line);
                    diag.push('\n');
                }
                if !line.contains("\"id\":2") {
                    continue;
                }
                let resp: RpcResponse = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if resp.id == Some(2) {
                    if let Some(r) = resp.result {
                        if let Some(rl) = r.rate_limits {
                            found = Some(rl);
                            break;
                        }
                    }
                }
            }
            Err(_) => break, // timeout or channel closed
        }
    }

    // Clean up: close stdin, kill child, reap.
    drop(stdin);
    let pid = child.id();
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
    }
    #[cfg(unix)]
    {
        let _ = child.kill();
    }
    let _ = child.wait();

    let payload = found.ok_or_else(|| {
        anyhow!(
            "no rateLimits response from codex app-server ({} lines, head: {:?})",
            lines_seen,
            &diag.chars().take(300).collect::<String>()
        )
    })?;
    let mut rl = CodexRateLimits::default();
    if let Some(p) = payload.primary {
        rl.primary_used_percent = p.used_percent as f64;
        rl.primary_resets_at = p
            .resets_at
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
    }
    if let Some(s) = payload.secondary {
        rl.secondary_used_percent = s.used_percent as f64;
        rl.secondary_resets_at = s
            .resets_at
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
    }
    let plan_type_raw = payload.plan_type.clone().unwrap_or_default();
    let plan_label = match payload.plan_type.as_deref() {
        Some("prolite") => "PRO 5×".to_string(),
        Some("pro") => "PRO".to_string(),
        Some("team") => "TEAM".to_string(),
        Some("plus") => "PLUS".to_string(),
        Some(other) => other.to_uppercase(),
        None => "PRO 5×".to_string(),
    };
    Ok((rl, plan_label, plan_type_raw, Utc::now()))
}

// Tiny std-only "wait with timeout" via try_wait loop
trait WaitTimeout {
    fn wait_timeout(&mut self, dur: StdDuration) -> Result<()>;
}
impl WaitTimeout for std::process::Child {
    fn wait_timeout(&mut self, dur: StdDuration) -> Result<()> {
        let start = std::time::Instant::now();
        while start.elapsed() < dur {
            match self.try_wait()? {
                Some(_) => return Ok(()),
                None => std::thread::sleep(StdDuration::from_millis(50)),
            }
        }
        let _ = self.kill();
        let _ = self.wait();
        Ok(())
    }
}

// =====================================================================
// Token totals (for ROI math): still aggregated from JSONL session files.
// =====================================================================

#[derive(Debug, Deserialize)]
struct CodexLine {
    #[serde(default)]
    timestamp: Option<DateTime<Utc>>,
    /// Outer event type — distinguishes "turn_context" / "event_msg" /
    /// "session_meta" etc. The model field lives inside payload for
    /// turn_context events; payload.type only exists on event_msg.
    #[serde(default, rename = "type")]
    event_type: Option<String>,
    #[serde(default)]
    payload: Option<CodexPayload>,
}

#[derive(Debug, Deserialize)]
struct CodexPayload {
    #[serde(default, rename = "type")]
    payload_type: Option<String>,
    #[serde(default)]
    info: Option<CodexTokenInfo>,
    #[serde(default)]
    model: Option<String>, // present on turn_context payloads
}

#[derive(Debug, Deserialize, Default)]
struct CodexTokenInfo {
    #[serde(default)]
    total_token_usage: Option<CodexTokenUsageRaw>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct CodexTokenUsageRaw {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

pub fn collect() -> Result<CodexStats> {
    let mut stats = CodexStats::default();

    // 1. Live rate limits via JSON-RPC (the authoritative source)
    match fetch_live_rate_limits() {
        Ok((rl, plan_label, plan_type_raw, ts)) => {
            stats.rate_limits = rl;
            stats.plan_label = plan_label;
            stats.plan_label_raw = plan_type_raw;
            stats.last_event_at = Some(ts);
        }
        Err(e) => {
            eprintln!("[tally] codex live fetch failed: {e}");
            stats.plan_label = "PRO 5×".to_string();
            stats.plan_label_raw = String::new();
        }
    }

    // 2. Token totals (today / MTD) from JSONL session aggregation
    let mut sessions_dir = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    sessions_dir.push(".codex");
    sessions_dir.push("sessions");
    if !sessions_dir.exists() {
        return Ok(stats);
    }

    let now = Utc::now();
    let cutoff_30d = now - Duration::days(30);
    let cutoff_14d = now - Duration::days(14);
    let cutoff_7d  = now - Duration::days(7);
    let cutoff_1d  = now - Duration::days(1);
    let now_local = Local::now();
    let today_start = Local
        .from_local_datetime(
            &now_local.date_naive().and_hms_opt(0, 0, 0).unwrap(),
        )
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);
    let mtd_start = Local
        .from_local_datetime(
            &chrono::NaiveDate::from_ymd_opt(now_local.year(), now_local.month(), 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);

    let mut final_per_session: HashMap<PathBuf, (DateTime<Utc>, CodexTokenUsageRaw)> = HashMap::new();
    let mut session_request_count: HashMap<PathBuf, u64> = HashMap::new();
    let mut session_model: HashMap<PathBuf, String> = HashMap::new();

    for entry in WalkDir::new(&sessions_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path().to_path_buf();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                let mt: DateTime<Utc> = mtime.into();
                if mt < cutoff_30d {
                    continue;
                }
            }
        }
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        for line in reader.lines().map_while(|r| r.ok()) {
            let parsed: CodexLine = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let ts = match parsed.timestamp {
                Some(t) => t,
                None => continue,
            };
            let payload = match parsed.payload {
                Some(p) => p,
                None => continue,
            };
            // Codex JSONL puts the discriminator on the OUTER `type` field
            // for turn_context / session_meta, and on payload.type for
            // event_msg wrappers. Check both.
            if parsed.event_type.as_deref() == Some("turn_context") {
                if let Some(m) = payload.model {
                    session_model.entry(path.clone()).or_insert(m);
                }
                continue;
            }
            if payload.payload_type.as_deref() != Some("token_count") {
                continue;
            }
            if ts >= today_start {
                *session_request_count.entry(path.clone()).or_insert(0) += 1;
            }
            if let Some(info) = payload.info {
                if let Some(total) = info.total_token_usage {
                    let take = final_per_session
                        .get(&path)
                        .map(|(prev_ts, _)| ts > *prev_ts)
                        .unwrap_or(true);
                    if take {
                        final_per_session.insert(path.clone(), (ts, total));
                    }
                }
            }
        }
    }

    for (path, (ts, totals)) in &final_per_session {
        let empty = String::new();
        let model = session_model.get(path).unwrap_or(&empty);
        let session_cost = crate::pricing::codex_turn_cost(
            model,
            totals.input_tokens,
            totals.cached_input_tokens,
            totals.output_tokens,
            totals.reasoning_output_tokens,
        );
        let mut accrue = |p: &mut CodexPeriodStats| {
            p.tokens.input += totals.input_tokens;
            p.tokens.cached_input += totals.cached_input_tokens;
            p.tokens.output += totals.output_tokens;
            p.tokens.reasoning += totals.reasoning_output_tokens;
            p.cost += session_cost;
        };
        if *ts >= today_start { accrue(&mut stats.today); }
        if *ts >= cutoff_1d   { accrue(&mut stats.d1); }
        if *ts >= cutoff_7d   { accrue(&mut stats.d7); }
        if *ts >= cutoff_14d  { accrue(&mut stats.d14); }
        if *ts >= cutoff_30d  { accrue(&mut stats.d30); }
        if *ts >= mtd_start   { accrue(&mut stats.mtd); }
    }
    // Attribute request counts to the same periods using session's last ts
    for (path, count) in &session_request_count {
        let session_ts = final_per_session.get(path).map(|(t, _)| *t);
        if let Some(ts) = session_ts {
            if ts >= today_start { stats.today.requests += count; }
            if ts >= cutoff_1d   { stats.d1.requests += count; }
            if ts >= cutoff_7d   { stats.d7.requests += count; }
            if ts >= cutoff_14d  { stats.d14.requests += count; }
            if ts >= cutoff_30d  { stats.d30.requests += count; }
            if ts >= mtd_start   { stats.mtd.requests += count; }
        }
    }

    Ok(stats)
}
