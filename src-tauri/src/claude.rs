use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use walkdir::WalkDir;

mod cli;

use cli::fetch_cli_usage_limits;

// Minimum cache TTL — hard floor so the API isn't hammered. Anthropic
// rate-limits /api/oauth/usage aggressively (we've measured 429s at ~30
// hits/hour). At 60s floor, worst-case is 60 hits/hour from this one
// client — still under the bucket with margin for `claude.exe` etc sharing
// the same OAuth token.
const MIN_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const DISK_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(20 * 60);
// On 429, refuse to retry for this long. The endpoint's window is opaque so
// we just back off hard regardless of user refresh setting.
const RATE_LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(900);
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DiskCache {
    fetched_at_unix: i64,
    five_hour_percent: f64,
    five_hour_resets_at: Option<DateTime<Utc>>,
    weekly_percent: f64,
    weekly_resets_at: Option<DateTime<Utc>>,
}

struct CacheEntry {
    fetched_at: Instant,
    value: ClaudeLiveLimits,
    /// Instant before which we will not attempt another HTTP call. Set when
    /// the server returns 429, cleared on successful fetch.
    cooldown_until: Option<Instant>,
    /// Last fetch error message, for UI surfacing.
    last_error: Option<String>,
}

fn cache() -> &'static Mutex<Option<CacheEntry>> {
    static C: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();
    C.get_or_init(|| {
        // Seed from disk on first access.
        let seed = read_disk_cache().map(|d| CacheEntry {
            // Mark as "old enough to refresh" so first call still tries fresh.
            fetched_at: Instant::now() - std::time::Duration::from_secs(3600),
            value: ClaudeLiveLimits {
                five_hour_percent: d.five_hour_percent,
                five_hour_resets_at: d.five_hour_resets_at,
                weekly_percent: d.weekly_percent,
                weekly_resets_at: d.weekly_resets_at,
                sub_quotas: Vec::new(),
                extra_usage: None,
            },
            cooldown_until: None,
            last_error: None,
        });
        Mutex::new(seed)
    })
}

fn disk_cache_path() -> Option<std::path::PathBuf> {
    let mut p = dirs::cache_dir()?;
    p.push("tally");
    let _ = std::fs::create_dir_all(&p);
    p.push("claude-usage-cache.json");
    Some(p)
}

fn legacy_disk_cache_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();

    if let Some(mut p) = dirs::cache_dir() {
        p.push("usage-widget");
        p.push("claude-usage-cache.json");
        paths.push(p);
    }

    if let Some(mut local) = dirs::data_local_dir() {
        local.push("usage-widget");
        local.push("claude-usage-cache.json");
        paths.push(local);
    }

    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let mut packages = std::path::PathBuf::from(local_app_data);
        packages.push("Packages");
        if let Ok(entries) = std::fs::read_dir(packages) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.to_ascii_lowercase().contains("claude") {
                    let mut p = entry.path();
                    p.push("LocalCache");
                    p.push("Local");
                    p.push("usage-widget");
                    p.push("claude-usage-cache.json");
                    paths.push(p);
                }
            }
        }
    }

    paths
}

fn read_disk_cache() -> Option<DiskCache> {
    let mut paths = Vec::new();
    if let Some(path) = disk_cache_path() {
        paths.push(path);
    }
    paths.extend(legacy_disk_cache_paths());

    for path in paths {
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Ok(cache) = serde_json::from_str::<DiskCache>(&s) {
            let age_secs = Utc::now().timestamp().saturating_sub(cache.fetched_at_unix);
            if age_secs as u64 > DISK_CACHE_MAX_AGE.as_secs() {
                continue;
            }
            return Some(cache);
        }
    }
    None
}

fn write_disk_cache(value: &ClaudeLiveLimits) {
    if let Some(path) = disk_cache_path() {
        let d = DiskCache {
            fetched_at_unix: Utc::now().timestamp(),
            five_hour_percent: value.five_hour_percent,
            five_hour_resets_at: value.five_hour_resets_at,
            weekly_percent: value.weekly_percent,
            weekly_resets_at: value.weekly_resets_at,
        };
        if let Ok(s) = serde_json::to_string(&d) {
            let _ = std::fs::write(path, s);
        }
    }
}

// =====================================================================
// LIVE Claude rate limits via /api/oauth/usage
// Same endpoint claude.ai's dashboard and Claude Code CLI use internally.
// Returns server-computed utilization — no calibration, no estimation.
// =====================================================================

#[derive(Debug, Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    seven_day_oauth_apps: Option<UsageWindow>,
    seven_day_opus: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
    seven_day_design: Option<UsageWindow>,
    seven_day_claude_design: Option<UsageWindow>,
    claude_design: Option<UsageWindow>,
    design: Option<UsageWindow>,
    seven_day_routines: Option<UsageWindow>,
    seven_day_claude_routines: Option<UsageWindow>,
    claude_routines: Option<UsageWindow>,
    routines: Option<UsageWindow>,
    routine: Option<UsageWindow>,
    seven_day_cowork: Option<UsageWindow>,
    seven_day_omelette: Option<UsageWindow>,
    extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Deserialize, Clone)]
struct UsageWindow {
    utilization: f64,
    resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Clone)]
struct ExtraUsage {
    is_enabled: Option<bool>,
    monthly_limit: Option<f64>,
    used_credits: Option<f64>,
    utilization: Option<f64>,
    currency: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    organization: Option<ProfileOrg>,
}

#[derive(Debug, Deserialize)]
struct ProfileOrg {
    rate_limit_tier: Option<String>,
}

/// Fetch the user's Claude subscription tier identifier from /api/oauth/profile.
/// Cached aggressively (24h) since it changes rarely.
pub fn fetch_plan_tier() -> Result<String> {
    // Disk cache: ~/tally/claude-plan.json with 24h TTL
    let cache_path = disk_cache_path().map(|mut p| {
        p.set_file_name("claude-plan.json");
        p
    });
    if let Some(p) = &cache_path {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&s) {
                let ts = entry.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
                let age = Utc::now().timestamp() - ts;
                if age < 86_400 {
                    if let Some(t) = entry.get("tier").and_then(|v| v.as_str()) {
                        return Ok(t.to_string());
                    }
                }
            }
        }
    }
    let token = read_oauth_token()?;
    let resp = ureq::get("https://api.anthropic.com/api/oauth/profile")
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(std::time::Duration::from_secs(8))
        .call()
        .map_err(|e| anyhow!("call /api/oauth/profile: {e}"))?;
    let body: ProfileResponse = resp
        .into_json()
        .map_err(|e| anyhow!("decode profile response: {e}"))?;
    let tier = body
        .organization
        .and_then(|o| o.rate_limit_tier)
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(p) = cache_path {
        let payload = serde_json::json!({ "ts": Utc::now().timestamp(), "tier": tier });
        let _ = std::fs::write(p, payload.to_string());
    }
    Ok(tier)
}

#[derive(Debug, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OauthBlock,
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    access_token: String,
}

fn read_oauth_token() -> Result<String> {
    let mut path = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    path.push(".claude");
    path.push(".credentials.json");
    let file = File::open(&path).map_err(|e| anyhow!("read {}: {e}", path.display()))?;
    let creds: Credentials =
        serde_json::from_reader(file).map_err(|e| anyhow!("parse credentials: {e}"))?;
    Ok(creds.claude_ai_oauth.access_token)
}

/// True if Claude Code CLI is installed AND authenticated (has OAuth token).
pub fn is_available() -> bool {
    read_oauth_token().is_ok()
}

#[derive(Debug, Clone, Default)]
pub struct SubQuota {
    pub label: String,
    pub utilization: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct ExtraUsageInfo {
    pub enabled: bool,
    pub used: f64,
    pub limit: f64,
    pub utilization: f64,
    pub currency: String,
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeLiveLimits {
    pub five_hour_percent: f64,
    pub five_hour_resets_at: Option<DateTime<Utc>>,
    pub weekly_percent: f64,
    pub weekly_resets_at: Option<DateTime<Utc>>,
    pub sub_quotas: Vec<SubQuota>,
    pub extra_usage: Option<ExtraUsageInfo>,
}

/// Result of a single HTTP fetch attempt. Distinguishes rate-limit from
/// other errors so the caller can apply a longer backoff specifically for 429.
enum FetchOutcome {
    Ok(ClaudeLiveLimits),
    RateLimited(String),
    Other(anyhow::Error),
}

fn http_fetch_live_limits() -> FetchOutcome {
    let token = match read_oauth_token() {
        Ok(t) => t,
        Err(e) => return FetchOutcome::Other(e),
    };
    let user_agent = format!("claude-code/{}", claude_code_version());
    let result = ureq::get("https://api.anthropic.com/api/oauth/usage")
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", &user_agent)
        .timeout(std::time::Duration::from_secs(8))
        .call();
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(429, _)) => {
            return FetchOutcome::RateLimited(
                "Anthropic /api/oauth/usage returned 429 — backing off".to_string(),
            );
        }
        Err(e) => {
            return FetchOutcome::Other(anyhow!("call /api/oauth/usage: {e}"));
        }
    };
    let body: UsageResponse = match resp.into_json() {
        Ok(b) => b,
        Err(e) => return FetchOutcome::Other(anyhow!("decode usage response: {e}")),
    };

    // Sub-quotas: include any that are present in the response (non-null).
    // Anthropic returns these whether you've used the feature or not — we
    // surface them so the widget can render whatever's relevant.
    let mut sub_quotas = Vec::new();
    let push_q = |out: &mut Vec<SubQuota>, label: &str, w: &Option<UsageWindow>| {
        if let Some(w) = w {
            out.push(SubQuota {
                label: label.to_string(),
                utilization: w.utilization,
                resets_at: w.resets_at,
            });
        }
    };
    push_q(&mut sub_quotas, "Sonnet", &body.seven_day_sonnet);
    push_q(&mut sub_quotas, "Opus", &body.seven_day_opus);

    let design = body
        .seven_day_design
        .as_ref()
        .or(body.seven_day_claude_design.as_ref())
        .or(body.claude_design.as_ref())
        .or(body.design.as_ref())
        .or(body.seven_day_omelette.as_ref())
        .cloned();
    let routines = body
        .seven_day_routines
        .as_ref()
        .or(body.seven_day_claude_routines.as_ref())
        .or(body.claude_routines.as_ref())
        .or(body.routines.as_ref())
        .or(body.routine.as_ref())
        .or(body.seven_day_cowork.as_ref())
        .cloned();
    push_q(&mut sub_quotas, "Claude Design", &design);
    push_q(&mut sub_quotas, "Claude Routines", &routines);

    let extra_usage = body.extra_usage.map(|e| ExtraUsageInfo {
        enabled: e.is_enabled.unwrap_or(false),
        used: e.used_credits.unwrap_or(0.0),
        limit: e.monthly_limit.unwrap_or(0.0),
        utilization: e.utilization.unwrap_or(0.0),
        currency: e.currency.unwrap_or_else(|| "USD".to_string()),
    });

    let session_window = body.five_hour.as_ref().or(body.seven_day.as_ref());
    let weekly_window = body
        .seven_day
        .as_ref()
        .or(body.seven_day_oauth_apps.as_ref());

    FetchOutcome::Ok(ClaudeLiveLimits {
        five_hour_percent: session_window.map(|w| w.utilization).unwrap_or(0.0),
        five_hour_resets_at: session_window.and_then(|w| w.resets_at),
        weekly_percent: weekly_window.map(|w| w.utilization).unwrap_or(0.0),
        weekly_resets_at: weekly_window.and_then(|w| w.resets_at),
        sub_quotas,
        extra_usage,
    })
}

fn claude_code_version() -> String {
    let output = std::process::Command::new("claude")
        .arg("--version")
        .output();
    if let Ok(output) = output {
        let raw = String::from_utf8_lossy(&output.stdout);
        if let Some(first) = raw.split_whitespace().next() {
            if !first.trim().is_empty() {
                return first.trim().to_string();
            }
        }
    }
    "2.1.0".to_string()
}

/// Returns the live limits with caching tied to the user's UI refresh
/// interval. The backend cache TTL is `max(refresh_ms, MIN_CACHE_TTL)` so
/// each UI poll receives data at most that old, while still protecting the
/// API from extreme settings.
///
/// **Caching policy:**
/// - Normal: serve cached value for `refresh_ms` (floor 30s), then fetch.
/// - 429 received: refuse to retry for 15 minutes (`RATE_LIMIT_BACKOFF`),
///   regardless of user setting — the server is telling us to back off.
pub fn fetch_live_limits(refresh_ms: u64) -> Result<ClaudeLiveLimits> {
    let ttl = std::time::Duration::from_millis(refresh_ms).max(MIN_CACHE_TTL);
    let now_inst = Instant::now();
    {
        let guard = cache().lock().unwrap();
        if let Some(entry) = guard.as_ref() {
            // Honor active 429 cooldown: serve cached value, do NOT hit the API.
            if let Some(until) = entry.cooldown_until {
                if now_inst < until {
                    return Ok(entry.value.clone());
                }
            }
            if entry.fetched_at.elapsed() < ttl {
                return Ok(entry.value.clone());
            }
        }
    }
    match fetch_cli_usage_limits()
        .map(FetchOutcome::Ok)
        .unwrap_or_else(|e| {
            eprintln!("[tally] claude CLI /usage failed ({e}); falling back to OAuth usage");
            http_fetch_live_limits()
        }) {
        FetchOutcome::Ok(fresh) => {
            let mut guard = cache().lock().unwrap();
            *guard = Some(CacheEntry {
                fetched_at: Instant::now(),
                value: fresh.clone(),
                cooldown_until: None,
                last_error: None,
            });
            drop(guard);
            write_disk_cache(&fresh);
            Ok(fresh)
        }
        FetchOutcome::RateLimited(msg) => {
            eprintln!("[tally] {msg} — cooldown {}s", RATE_LIMIT_BACKOFF.as_secs());
            let mut guard = cache().lock().unwrap();
            if let Some(entry) = guard.as_mut() {
                entry.fetched_at = Instant::now();
                entry.cooldown_until = Some(Instant::now() + RATE_LIMIT_BACKOFF);
                entry.last_error = Some(msg);
                Ok(entry.value.clone())
            } else {
                Err(anyhow!(msg))
            }
        }
        FetchOutcome::Other(e) => {
            let mut guard = cache().lock().unwrap();
            if let Some(entry) = guard.as_mut() {
                eprintln!("[tally] claude live fetch failed ({e}); using cached value");
                entry.fetched_at = Instant::now();
                entry.last_error = Some(e.to_string());
                Ok(entry.value.clone())
            } else {
                Err(e)
            }
        }
    }
}

// =====================================================================
// Token totals (today + MTD) for $ / ROI math — still parsed from JSONL.
// =====================================================================

#[derive(Debug, Default, Clone)]
pub struct PeriodStats {
    pub tokens: TokenWindow,
    pub requests: u64,
    pub cost: f64,
}

#[derive(Debug, Default, Clone)]
pub struct ClaudeStats {
    pub today: PeriodStats,
    pub d1: PeriodStats,
    pub d7: PeriodStats,
    pub d14: PeriodStats,
    pub d30: PeriodStats,
    pub mtd: PeriodStats,
    pub five_hour_percent: f64,
    pub weekly_percent: f64,
    pub next_5h_reset: Option<DateTime<Utc>>,
    pub next_weekly_reset: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub sub_quotas: Vec<SubQuota>,
    pub extra_usage: Option<ExtraUsageInfo>,
    pub cost_by_entrypoint_7d: std::collections::HashMap<String, f64>,
    /// $ specifically attributed to Cowork sessions (via cliSessionId match
    /// against Claude local-agent-mode-sessions indexes).
    pub cowork_cost_7d: f64,
    pub cowork_cost_today: f64,
    pub cowork_cost_mtd: f64,
}

/// Claude stores local app data in different roots depending on install mode.
/// The packaged Windows app keeps its Roaming profile under LocalCache, while
/// CLI installs typically use `%APPDATA%\Claude`.
fn claude_config_roots() -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    if let Some(mut p) = dirs::config_dir() {
        p.push("Claude");
        if seen.insert(p.clone()) {
            roots.push(p);
        }
    }

    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let packages = std::path::PathBuf::from(local_app_data).join("Packages");
        if let Ok(entries) = std::fs::read_dir(packages) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if !name.contains("claude") {
                    continue;
                }
                let p = entry
                    .path()
                    .join("LocalCache")
                    .join("Roaming")
                    .join("Claude");
                if seen.insert(p.clone()) {
                    roots.push(p);
                }
            }
        }
    }

    roots
}

fn cowork_session_roots() -> Vec<std::path::PathBuf> {
    claude_config_roots()
        .into_iter()
        .map(|root| root.join("local-agent-mode-sessions"))
        .filter(|root| root.exists())
        .collect()
}

/// Discover Cowork session IDs by walking the local-agent-mode-sessions tree
/// and extracting `cliSessionId` from each `local_*.json` index file.
fn discover_cowork_session_ids() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for root in cowork_session_roots() {
        for entry in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if !(name.starts_with("local_") && name.ends_with(".json")) {
                continue;
            }
            let body = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Minimal field-only parse to avoid pulling in unrelated nested types.
            #[derive(Deserialize)]
            struct CoworkIdx {
                #[serde(rename = "cliSessionId")]
                cli_session_id: Option<String>,
            }
            if let Ok(idx) = serde_json::from_str::<CoworkIdx>(&body) {
                if let Some(sid) = idx.cli_session_id {
                    set.insert(sid);
                }
            }
        }
    }
    set
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TokenWindow {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Deserialize)]
struct ClaudeLine {
    #[serde(default)]
    timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    message: Option<ClaudeMessage>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    usage: Option<ClaudeUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

pub fn collect(refresh_ms: u64) -> Result<ClaudeStats> {
    let mut stats = ClaudeStats::default();

    // 1. Live rate-limit utilization from /api/oauth/usage
    match fetch_live_limits(refresh_ms) {
        Ok(live) => {
            stats.five_hour_percent = live.five_hour_percent;
            stats.weekly_percent = live.weekly_percent;
            stats.next_5h_reset = live.five_hour_resets_at;
            stats.next_weekly_reset = live.weekly_resets_at;
            stats.sub_quotas = live.sub_quotas;
            stats.extra_usage = live.extra_usage;
            stats.last_event_at = Some(Utc::now());
        }
        Err(e) => {
            eprintln!("[tally] claude live fetch failed: {e}");
        }
    }

    // 2. Token totals from JSONL for cost / ROI math
    let mut projects_dir = match dirs::home_dir() {
        Some(d) => d,
        None => return Ok(stats),
    };
    projects_dir.push(".claude");
    projects_dir.push("projects");

    // Cowork-tagged session IDs (linked via local-agent-mode-sessions index).
    let cowork_ids = discover_cowork_session_ids();

    // Build list of directories to walk for JSONL events.
    // 1. Regular Claude Code sessions: ~/.claude/projects/
    // 2. Cowork sessions: each has its OWN .claude/projects/ inside
    //    each Claude local-agent-mode-sessions root.
    let mut walk_roots: Vec<std::path::PathBuf> = Vec::new();
    if projects_dir.exists() {
        walk_roots.push(projects_dir.clone());
    }
    for cw_root in cowork_session_roots() {
        // Find every nested .claude/projects/ subtree.
        for entry in WalkDir::new(&cw_root)
            .max_depth(10)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.is_dir()
                && p.ends_with("projects")
                && p.parent()
                    .map(|pp| pp.ends_with(".claude"))
                    .unwrap_or(false)
            {
                walk_roots.push(p.to_path_buf());
            }
        }
    }
    if walk_roots.is_empty() {
        return Ok(stats);
    }

    let now = Utc::now();
    // Local-derivation: every message timestamp with billable usage. Used
    // after the walk to compute the current 5h window without depending on
    // /api/oauth/usage (which 429s, can return stale resets_at, etc).
    let mut all_message_times: Vec<DateTime<Utc>> = Vec::new();
    // Rolling periods — exact-minute rollers from now
    let cutoff_30d = now - Duration::days(30);
    let cutoff_14d = now - Duration::days(14);
    let cutoff_7d = now - Duration::days(7);
    let cutoff_1d = now - Duration::days(1);
    // Calendar-anchored periods — use LOCAL time so "today" and "MTD" match
    // the user's wall clock, then convert to UTC for event comparison.
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

    for entry in walk_roots
        .iter()
        .flat_map(|r| WalkDir::new(r).into_iter())
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        // Detect Cowork session by path containment OR by file-stem matching
        // a known cowork cliSessionId.
        let is_cowork_path = path.components().any(|c| {
            c.as_os_str()
                .to_string_lossy()
                .contains("local-agent-mode-sessions")
        });
        let path_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let is_cowork_session = is_cowork_path || cowork_ids.contains(path_stem);
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                let mt: DateTime<Utc> = mtime.into();
                if mt < cutoff_30d {
                    continue;
                }
            }
        }
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        for line in reader.lines().map_while(|r| r.ok()) {
            let parsed: ClaudeLine = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let is_cowork_event = is_cowork_session
                || parsed
                    .session_id
                    .as_deref()
                    .map(|sid| cowork_ids.contains(sid))
                    .unwrap_or(false);
            let ts = match parsed.timestamp {
                Some(t) => t,
                None => continue,
            };
            let msg = match parsed.message {
                Some(m) => m,
                None => continue,
            };
            let model = msg.model.unwrap_or_default();
            let usage = match msg.usage {
                Some(u) => u,
                None => continue,
            };
            // Capture for local 5h-window derivation. Only messages with
            // usage data — those represent billable API calls that count
            // toward the 5-hour budget.
            all_message_times.push(ts);
            // Per-message cost using the actual model that ran (not a blend).
            let msg_cost = crate::pricing::claude_message_cost(
                &model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
            );
            let accrue = |p: &mut PeriodStats| {
                add(&mut p.tokens, &usage);
                p.requests += 1;
                p.cost += msg_cost;
            };
            if ts >= today_start {
                accrue(&mut stats.today);
            }
            if ts >= cutoff_1d {
                accrue(&mut stats.d1);
            }
            if ts >= cutoff_7d {
                accrue(&mut stats.d7);
            }
            if ts >= cutoff_14d {
                accrue(&mut stats.d14);
            }
            if ts >= cutoff_30d {
                accrue(&mut stats.d30);
            }
            if ts >= mtd_start {
                accrue(&mut stats.mtd);
            }

            // Per-entrypoint $ over last 7 days (matches the API's weekly windows).
            if ts >= cutoff_7d {
                let ep = parsed.entrypoint.unwrap_or_else(|| "unknown".to_string());
                *stats.cost_by_entrypoint_7d.entry(ep).or_insert(0.0) += msg_cost;
            }
            // Cowork attribution: file lives under local-agent-mode-sessions
            // OR the event sessionId matches a known cliSessionId from the cowork index.
            if is_cowork_event {
                if ts >= cutoff_7d {
                    stats.cowork_cost_7d += msg_cost;
                }
                if ts >= today_start {
                    stats.cowork_cost_today += msg_cost;
                }
                if ts >= mtd_start {
                    stats.cowork_cost_mtd += msg_cost;
                }
            }
        }
    }

    // Limits/reset times come from the same provider payload as the usage
    // percentage (OAuth/Web/CLI in the CodexBar model). Local JSONL is kept
    // for cost/ROI only; do not infer a reset clock from message timestamps.
    if !all_message_times.is_empty() {
        all_message_times.sort();
        stats.last_event_at = Some(*all_message_times.last().unwrap());
    }

    Ok(stats)
}

fn add(w: &mut TokenWindow, u: &ClaudeUsage) {
    w.input += u.input_tokens;
    w.output += u.output_tokens;
    w.cache_read += u.cache_read_input_tokens;
    w.cache_write += u.cache_creation_input_tokens;
}
