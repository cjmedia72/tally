use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration as StdDuration;
use walkdir::WalkDir;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
/// CREATE_NO_WINDOW (0x08000000) — suppresses the console flash when a GUI
/// app spawns a console subprocess on Windows. Without this, every refresh
/// pops a black cmd.exe window for ~50ms.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Helper: build a Command with the no-window flag set on Windows.
fn quiet_command(program: &impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

// =====================================================================
// LIVE rate limits via ChatGPT/Codex OAuth first, then `codex app-server`.
// This mirrors CodexBar's app strategy: direct chatgpt.com usage API when
// auth.json has OAuth credentials, CLI RPC as fallback.
// =====================================================================

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexAuthTokens>,
    last_refresh: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexAuthTokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexRefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthUsageResponse {
    rate_limit: Option<OAuthRateLimit>,
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthRateLimit {
    primary_window: Option<OAuthRateLimitWindow>,
    secondary_window: Option<OAuthRateLimitWindow>,
}

#[derive(Debug, Deserialize)]
struct OAuthRateLimitWindow {
    used_percent: i64,
    reset_at: Option<i64>,
}

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

/// True if Codex is authenticated. OAuth can work even when the CLI binary
/// fallback is unavailable.
pub fn is_available() -> bool {
    if codex_auth_path().map(|p| p.exists()).unwrap_or(false) {
        return true;
    }
    locate_codex().exists()
}

fn codex_home_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("CODEX_HOME") {
        let s = raw.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    let mut home = dirs::home_dir()?;
    home.push(".codex");
    Some(home)
}

fn codex_auth_path() -> Option<PathBuf> {
    let mut p = codex_home_dir()?;
    p.push("auth.json");
    Some(p)
}

fn read_codex_auth_file() -> Result<(PathBuf, CodexAuthFile)> {
    let path = codex_auth_path().ok_or_else(|| anyhow!("no codex home dir"))?;
    let file = File::open(&path).map_err(|e| anyhow!("read {}: {e}", path.display()))?;
    let auth: CodexAuthFile =
        serde_json::from_reader(file).map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
    Ok((path, auth))
}

fn read_codex_auth() -> Result<CodexAuthTokens> {
    let (path, auth) = read_codex_auth_file()?;
    let tokens = auth
        .tokens
        .ok_or_else(|| anyhow!("codex auth missing tokens"))?;
    let stale = auth
        .last_refresh
        .map(|ts| Utc::now().signed_duration_since(ts) > Duration::days(8))
        .unwrap_or(true);
    if stale {
        if let Some(refresh_token) = tokens.refresh_token.as_deref() {
            if !refresh_token.trim().is_empty() {
                return refresh_codex_auth(&path, &tokens);
            }
        }
    }
    Ok(tokens)
}

fn refresh_codex_auth(
    path: &std::path::Path,
    current: &CodexAuthTokens,
) -> Result<CodexAuthTokens> {
    let refresh_token = current
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("codex auth missing refresh_token"))?;
    let resp = ureq::post("https://auth.openai.com/oauth/token")
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .send_json(serde_json::json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "scope": "openid profile email"
        }))
        .map_err(|e| anyhow!("refresh codex oauth token: {e}"))?;
    let body: CodexRefreshResponse = resp
        .into_json()
        .map_err(|e| anyhow!("decode codex oauth refresh: {e}"))?;
    let updated = CodexAuthTokens {
        access_token: body.access_token.or_else(|| current.access_token.clone()),
        refresh_token: body.refresh_token.or_else(|| current.refresh_token.clone()),
        id_token: body.id_token.or_else(|| current.id_token.clone()),
        account_id: current.account_id.clone(),
    };
    save_codex_auth_tokens(path, &updated)?;
    Ok(updated)
}

fn save_codex_auth_tokens(path: &std::path::Path, tokens: &CodexAuthTokens) -> Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let mut json: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if !json.is_object() {
        json = serde_json::json!({});
    }
    let mut token_obj = serde_json::Map::new();
    if let Some(value) = tokens.access_token.as_ref() {
        token_obj.insert(
            "access_token".to_string(),
            serde_json::Value::String(value.clone()),
        );
    }
    if let Some(value) = tokens.refresh_token.as_ref() {
        token_obj.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(value.clone()),
        );
    }
    if let Some(value) = tokens.id_token.as_ref() {
        token_obj.insert(
            "id_token".to_string(),
            serde_json::Value::String(value.clone()),
        );
    }
    if let Some(value) = tokens.account_id.as_ref() {
        token_obj.insert(
            "account_id".to_string(),
            serde_json::Value::String(value.clone()),
        );
    }
    json["tokens"] = serde_json::Value::Object(token_obj);
    json["last_refresh"] = serde_json::Value::String(Utc::now().to_rfc3339());
    let body = serde_json::to_string_pretty(&json)?;
    std::fs::write(path, body).map_err(|e| anyhow!("write {}: {e}", path.display()))
}

fn plan_label(plan_type: Option<&str>) -> String {
    match plan_type {
        Some("prolite") => "PRO 5x".to_string(),
        Some("pro") => "PRO".to_string(),
        Some("team") => "TEAM".to_string(),
        Some("plus") => "PLUS".to_string(),
        Some(other) => other.to_uppercase(),
        None => "PRO 5x".to_string(),
    }
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
        if let Ok(out) = quiet_command(&lookup_cmd).arg(name).output() {
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

fn fetch_oauth_rate_limits() -> Result<(CodexRateLimits, String, String, DateTime<Utc>)> {
    let tokens = read_codex_auth()?;
    let access_token = tokens
        .access_token
        .ok_or_else(|| anyhow!("codex auth missing access_token"))?;
    let mut req = ureq::get("https://chatgpt.com/backend-api/wham/usage")
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Accept", "application/json")
        .set("User-Agent", "Tally")
        .timeout(std::time::Duration::from_secs(10));
    if let Some(account_id) = tokens.account_id {
        if !account_id.trim().is_empty() {
            req = req.set("ChatGPT-Account-Id", account_id.trim());
        }
    }
    let resp = req
        .call()
        .map_err(|e| anyhow!("call chatgpt wham usage: {e}"))?;
    let body: OAuthUsageResponse = resp
        .into_json()
        .map_err(|e| anyhow!("decode chatgpt wham usage: {e}"))?;

    let mut rl = CodexRateLimits::default();
    if let Some(primary) = body
        .rate_limit
        .as_ref()
        .and_then(|r| r.primary_window.as_ref())
    {
        rl.primary_used_percent = primary.used_percent as f64;
        rl.primary_resets_at = primary
            .reset_at
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
    }
    if let Some(secondary) = body
        .rate_limit
        .as_ref()
        .and_then(|r| r.secondary_window.as_ref())
    {
        rl.secondary_used_percent = secondary.used_percent as f64;
        rl.secondary_resets_at = secondary
            .reset_at
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single());
    }
    let raw = body.plan_type.unwrap_or_default();
    let label = plan_label(if raw.is_empty() {
        None
    } else {
        Some(raw.as_str())
    });
    Ok((rl, label, raw, Utc::now()))
}

/// Returns: (rate_limits, plan_label_human, plan_type_raw, fetched_at)
pub fn fetch_live_rate_limits() -> Result<(CodexRateLimits, String, String, DateTime<Utc>)> {
    match fetch_oauth_rate_limits() {
        Ok(result) => return Ok(result),
        Err(e) => eprintln!("[tally] codex oauth fetch failed; falling back to RPC: {e}"),
    }
    fetch_rpc_rate_limits()
}

/// Spawn `codex app-server`, send initialize + account/rateLimits/read,
/// wait actively for the id=2 response via a reader thread + channel.
/// Holds stdin open until we have the answer (mimics the working manual
/// pipe sequence where a trailing sleep keeps stdin alive while codex
/// reaches out to OpenAI's API for the live rate-limit state).
fn fetch_rpc_rate_limits() -> Result<(CodexRateLimits, String, String, DateTime<Utc>)> {
    let codex_path = locate_codex();
    let mut child = quiet_command(&codex_path)
        .args(["-s", "read-only", "-a", "untrusted"])
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

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"tally","version":"0.1.14"}}}"#;
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
        let _ = quiet_command(&"taskkill")
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
        rl.primary_resets_at = p.resets_at.and_then(|ts| Utc.timestamp_opt(ts, 0).single());
    }
    if let Some(s) = payload.secondary {
        rl.secondary_used_percent = s.used_percent as f64;
        rl.secondary_resets_at = s.resets_at.and_then(|ts| Utc.timestamp_opt(ts, 0).single());
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
    /// Per-turn delta — the tokens consumed in just this turn.
    /// We use this for time-bucketing so a long-running session's
    /// activity gets attributed to the right day/hour.
    #[serde(default)]
    last_token_usage: Option<CodexTokenUsageRaw>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    files: u64,
    bytes: u64,
    newest_mtime_ns: u128,
    local_year: i32,
    local_ordinal: u32,
}

impl FileSignature {
    fn new(local_now: chrono::DateTime<Local>) -> Self {
        Self {
            files: 0,
            bytes: 0,
            newest_mtime_ns: 0,
            local_year: local_now.year(),
            local_ordinal: local_now.ordinal(),
        }
    }

    fn observe_file(&mut self, len: u64, modified: std::time::SystemTime) {
        if let Ok(delta) = modified.duration_since(std::time::UNIX_EPOCH) {
            self.newest_mtime_ns = self.newest_mtime_ns.max(delta.as_nanos());
        }
        self.files += 1;
        self.bytes = self.bytes.saturating_add(len);
    }
}

#[derive(Debug, Default, Clone)]
struct LocalTokenStats {
    today: CodexPeriodStats,
    d1: CodexPeriodStats,
    d7: CodexPeriodStats,
    d14: CodexPeriodStats,
    d30: CodexPeriodStats,
    mtd: CodexPeriodStats,
}

fn token_cache() -> &'static Mutex<Option<(FileSignature, LocalTokenStats)>> {
    static CACHE: OnceLock<Mutex<Option<(FileSignature, LocalTokenStats)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
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
    let mut sessions_dir = codex_home_dir().ok_or_else(|| anyhow!("no codex home dir"))?;
    sessions_dir.push("sessions");
    let mut archived_dir = sessions_dir.clone();
    archived_dir.pop();
    archived_dir.push("archived_sessions");

    let now = Utc::now();
    let cutoff_30d = now - Duration::days(30);
    let cutoff_14d = now - Duration::days(14);
    let cutoff_7d = now - Duration::days(7);
    let cutoff_1d = now - Duration::days(1);
    let now_local = Local::now();
    let today_start = Local
        .from_local_datetime(&now_local.date_naive().and_hms_opt(0, 0, 0).unwrap())
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

    // Per-event ledger: each token_count event's last_token_usage attributed
    // to THAT event's timestamp. Avoids the long-session attribution bug.
    let mut events: Vec<(DateTime<Utc>, PathBuf, CodexTokenUsageRaw)> = Vec::new();
    let mut session_model: HashMap<PathBuf, String> = HashMap::new();

    let mut roots = Vec::new();
    if sessions_dir.exists() {
        roots.push(sessions_dir);
    }
    if archived_dir.exists() {
        roots.push(archived_dir);
    }
    if roots.is_empty() {
        return Ok(stats);
    }

    let mut jsonl_files: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let mut signature = FileSignature::new(now_local);
    for entry in roots
        .iter()
        .flat_map(|r| WalkDir::new(r).into_iter())
        .filter_map(|e| e.ok())
    {
        let path = entry.path().to_path_buf();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Ok(mtime) = meta.modified() {
            let mt: DateTime<Utc> = mtime.into();
            if mt < cutoff_30d {
                continue;
            }
            signature.observe_file(meta.len(), mtime);
        } else {
            signature.observe_file(meta.len(), std::time::UNIX_EPOCH);
        }
        jsonl_files.push((path, meta));
    }

    if let Some((cached_sig, cached)) = token_cache().lock().unwrap().as_ref() {
        if *cached_sig == signature {
            apply_local_token_stats(&mut stats, cached);
            return Ok(stats);
        }
    }

    for (path, _meta) in jsonl_files {
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
            // Per-event attribution: use info.last_token_usage if present
            // (the delta for just this turn). Skip events without it — the
            // first event in a session always has info=null.
            if let Some(info) = payload.info {
                if let Some(delta) = info.last_token_usage {
                    if delta.input_tokens > 0
                        || delta.output_tokens > 0
                        || delta.reasoning_output_tokens > 0
                    {
                        events.push((ts, path.clone(), delta));
                    }
                }
            }
        }
    }

    // Each event = one turn. Attribute its delta tokens + cost to the
    // buckets matching the event's own timestamp.
    let empty = String::new();
    let mut daily_rows: BTreeMap<NaiveDate, crate::history::DailyUsage> = BTreeMap::new();
    for (ts, path, delta) in &events {
        let model = session_model.get(path).unwrap_or(&empty);
        let cost = crate::pricing::codex_turn_cost(
            model,
            delta.input_tokens,
            delta.cached_input_tokens,
            delta.output_tokens,
            delta.reasoning_output_tokens,
        );
        let local_ts: DateTime<Local> = (*ts).into();
        let daily = daily_rows.entry(local_ts.date_naive()).or_default();
        daily.tokens.input += delta.input_tokens;
        daily.tokens.cached_input += delta.cached_input_tokens;
        daily.tokens.output += delta.output_tokens;
        daily.tokens.reasoning += delta.reasoning_output_tokens;
        daily.requests += 1;
        daily.api_equiv += cost;

        let accrue = |p: &mut CodexPeriodStats| {
            p.tokens.input += delta.input_tokens;
            p.tokens.cached_input += delta.cached_input_tokens;
            p.tokens.output += delta.output_tokens;
            p.tokens.reasoning += delta.reasoning_output_tokens;
            p.cost += cost;
            p.requests += 1;
        };
        if *ts >= today_start {
            accrue(&mut stats.today);
        }
        if *ts >= cutoff_1d {
            accrue(&mut stats.d1);
        }
        if *ts >= cutoff_7d {
            accrue(&mut stats.d7);
        }
        if *ts >= cutoff_14d {
            accrue(&mut stats.d14);
        }
        if *ts >= cutoff_30d {
            accrue(&mut stats.d30);
        }
        if *ts >= mtd_start {
            accrue(&mut stats.mtd);
        }
    }

    if let Err(e) = crate::history::upsert_daily_usage("codex", &daily_rows) {
        eprintln!("[tally] codex daily history backfill failed: {e}");
    }

    *token_cache().lock().unwrap() = Some((signature, local_token_stats_from(&stats)));
    Ok(stats)
}

fn local_token_stats_from(stats: &CodexStats) -> LocalTokenStats {
    LocalTokenStats {
        today: stats.today.clone(),
        d1: stats.d1.clone(),
        d7: stats.d7.clone(),
        d14: stats.d14.clone(),
        d30: stats.d30.clone(),
        mtd: stats.mtd.clone(),
    }
}

fn apply_local_token_stats(stats: &mut CodexStats, local: &LocalTokenStats) {
    stats.today = local.today.clone();
    stats.d1 = local.d1.clone();
    stats.d7 = local.d7.clone();
    stats.d14 = local.d14.clone();
    stats.d30 = local.d30.clone();
    stats.mtd = local.mtd.clone();
}

#[cfg(test)]
mod local_cache_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn file_signature_changes_on_file_or_day_changes() {
        let day = Local.with_ymd_and_hms(2026, 5, 27, 8, 0, 0).unwrap();
        let next_day = Local.with_ymd_and_hms(2026, 5, 28, 8, 0, 0).unwrap();
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);

        let mut base = FileSignature::new(day);
        base.observe_file(42, mtime);

        let mut same = FileSignature::new(day);
        same.observe_file(42, mtime);
        assert_eq!(base, same);

        let mut size_changed = FileSignature::new(day);
        size_changed.observe_file(43, mtime);
        assert_ne!(base, size_changed);

        let mut day_changed = FileSignature::new(next_day);
        day_changed.observe_file(42, mtime);
        assert_ne!(base, day_changed);
    }

    #[test]
    fn cached_local_token_stats_do_not_overwrite_live_limit_fields() {
        let mut stats = CodexStats {
            plan_label: "PRO 5x".to_string(),
            plan_label_raw: "prolite".to_string(),
            last_event_at: Some(Utc::now()),
            ..Default::default()
        };
        stats.rate_limits.primary_used_percent = 12.0;
        let local = LocalTokenStats {
            today: CodexPeriodStats {
                requests: 7,
                ..Default::default()
            },
            ..Default::default()
        };

        apply_local_token_stats(&mut stats, &local);

        assert_eq!(stats.today.requests, 7);
        assert_eq!(stats.rate_limits.primary_used_percent, 12.0);
        assert_eq!(stats.plan_label_raw, "prolite");
        assert!(stats.last_event_at.is_some());
    }
}
