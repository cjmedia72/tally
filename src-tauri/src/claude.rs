use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Utc};
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use walkdir::WalkDir;

// Cache the last successful /api/oauth/usage result so we don't hammer the
// endpoint (it 429s if called too often). Persisted to disk so restarts
// don't reset to 0%. Survives transient failures and rate limits.
const USAGE_CACHE_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60);

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
}

fn cache() -> &'static Mutex<Option<CacheEntry>> {
    static C: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();
    C.get_or_init(|| {
        // Seed from disk on first access.
        let seed = read_disk_cache().map(|d| CacheEntry {
            // Mark as "old enough to refresh" so first call still tries fresh.
            fetched_at: Instant::now() - USAGE_CACHE_MIN_AGE - std::time::Duration::from_secs(1),
            value: ClaudeLiveLimits {
                five_hour_percent: d.five_hour_percent,
                five_hour_resets_at: d.five_hour_resets_at,
                weekly_percent: d.weekly_percent,
                weekly_resets_at: d.weekly_resets_at,
                sub_quotas: Vec::new(),
                extra_usage: None,
            },
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

fn read_disk_cache() -> Option<DiskCache> {
    let path = disk_cache_path()?;
    let s = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&s).ok()
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
    seven_day_opus: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
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
    // Disk cache: ~/usage-widget/claude-plan.txt with 24h TTL
    let cache_path = disk_cache_path().map(|mut p| { p.set_file_name("claude-plan.json"); p });
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
    let creds: Credentials = serde_json::from_reader(file)
        .map_err(|e| anyhow!("parse credentials: {e}"))?;
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

fn http_fetch_live_limits() -> Result<ClaudeLiveLimits> {
    let token = read_oauth_token()?;
    let resp = ureq::get("https://api.anthropic.com/api/oauth/usage")
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(std::time::Duration::from_secs(8))
        .call()
        .map_err(|e| anyhow!("call /api/oauth/usage: {e}"))?;
    let body: UsageResponse = resp
        .into_json()
        .map_err(|e| anyhow!("decode usage response: {e}"))?;

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
    push_q(&mut sub_quotas, "Sonnet",       &body.seven_day_sonnet);
    push_q(&mut sub_quotas, "Opus",         &body.seven_day_opus);
    push_q(&mut sub_quotas, "Cowork",       &body.seven_day_cowork);
    push_q(&mut sub_quotas, "Claude Design",&body.seven_day_omelette);

    let extra_usage = body.extra_usage.map(|e| ExtraUsageInfo {
        enabled: e.is_enabled.unwrap_or(false),
        used: e.used_credits.unwrap_or(0.0),
        limit: e.monthly_limit.unwrap_or(0.0),
        utilization: e.utilization.unwrap_or(0.0),
        currency: e.currency.unwrap_or_else(|| "USD".to_string()),
    });

    Ok(ClaudeLiveLimits {
        five_hour_percent: body.five_hour.as_ref().map(|w| w.utilization).unwrap_or(0.0),
        five_hour_resets_at: body.five_hour.as_ref().and_then(|w| w.resets_at),
        weekly_percent: body.seven_day.as_ref().map(|w| w.utilization).unwrap_or(0.0),
        weekly_resets_at: body.seven_day.as_ref().and_then(|w| w.resets_at),
        sub_quotas,
        extra_usage,
    })
}

/// Returns the live limits, using a 25-second in-process cache to avoid
/// hammering /api/oauth/usage (which 429s under load). On fetch failure,
/// falls back to the cached value if any.
pub fn fetch_live_limits() -> Result<ClaudeLiveLimits> {
    {
        let guard = cache().lock().unwrap();
        if let Some(entry) = guard.as_ref() {
            if entry.fetched_at.elapsed() < USAGE_CACHE_MIN_AGE {
                return Ok(entry.value.clone());
            }
        }
    }
    match http_fetch_live_limits() {
        Ok(fresh) => {
            let mut guard = cache().lock().unwrap();
            *guard = Some(CacheEntry {
                fetched_at: Instant::now(),
                value: fresh.clone(),
            });
            drop(guard);
            write_disk_cache(&fresh);
            Ok(fresh)
        }
        Err(e) => {
            // Fall back to last-known cached value if we have one.
            let guard = cache().lock().unwrap();
            if let Some(entry) = guard.as_ref() {
                eprintln!("[tally] claude live fetch failed ({e}); using cached value");
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
    /// $ + tokens specifically attributed to Cowork sessions (via cliSessionId
    /// match against `~/AppData/Roaming/Claude/local-agent-mode-sessions/`).
    pub cowork_cost_7d: f64,
    pub cowork_cost_today: f64,
    pub cowork_cost_mtd: f64,
}

/// Discover Cowork session IDs by walking the local-agent-mode-sessions tree
/// and extracting `cliSessionId` from each `local_*.json` index file.
fn discover_cowork_session_ids() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let mut root = match dirs::config_dir() {
        // config_dir() = %APPDATA%\Roaming on Windows
        Some(p) => p,
        None => return set,
    };
    root.push("Claude");
    root.push("local-agent-mode-sessions");
    if !root.exists() {
        return set;
    }
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

pub fn collect() -> Result<ClaudeStats> {
    let mut stats = ClaudeStats::default();

    // 1. Live rate-limit utilization from /api/oauth/usage
    match fetch_live_limits() {
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
    if !projects_dir.exists() {
        return Ok(stats);
    }

    // Cowork-tagged session IDs (linked via local-agent-mode-sessions index).
    let cowork_ids = discover_cowork_session_ids();

    // Build list of directories to walk for JSONL events.
    // 1. Regular Claude Code sessions: ~/.claude/projects/
    // 2. Cowork sessions: each has its OWN .claude/projects/ inside
    //    ~/AppData/Roaming/Claude/local-agent-mode-sessions/{...}/local_*/.claude/projects/
    let mut walk_roots: Vec<std::path::PathBuf> = vec![projects_dir.clone()];
    if let Some(mut cw_root) = dirs::config_dir() {
        cw_root.push("Claude");
        cw_root.push("local-agent-mode-sessions");
        if cw_root.exists() {
            // Find every nested .claude/projects/ subtree
            for entry in WalkDir::new(&cw_root).max_depth(6).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.is_dir() && p.ends_with("projects")
                    && p.parent().map(|pp| pp.ends_with(".claude")).unwrap_or(false)
                {
                    walk_roots.push(p.to_path_buf());
                }
            }
        }
    }

    let now = Utc::now();
    // Rolling periods — exact-minute rollers from now
    let cutoff_30d = now - Duration::days(30);
    let cutoff_14d = now - Duration::days(14);
    let cutoff_7d  = now - Duration::days(7);
    let cutoff_1d  = now - Duration::days(1);
    // Calendar-anchored periods — use LOCAL time so "today" and "MTD" match
    // the user's wall clock, then convert to UTC for event comparison.
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

    for entry in walk_roots.iter().flat_map(|r| WalkDir::new(r).into_iter()).filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        // Detect Cowork session by path containment OR by file-stem matching
        // a known cowork cliSessionId.
        let is_cowork_path = path.components().any(|c| {
            c.as_os_str().to_string_lossy().contains("local-agent-mode-sessions")
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
            // Per-message cost using the actual model that ran (not a blend).
            let msg_cost = crate::pricing::claude_message_cost(
                &model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
            );
            let mut accrue = |p: &mut PeriodStats| {
                add(&mut p.tokens, &usage);
                p.requests += 1;
                p.cost += msg_cost;
            };
            if ts >= today_start { accrue(&mut stats.today); }
            if ts >= cutoff_1d   { accrue(&mut stats.d1); }
            if ts >= cutoff_7d   { accrue(&mut stats.d7); }
            if ts >= cutoff_14d  { accrue(&mut stats.d14); }
            if ts >= cutoff_30d  { accrue(&mut stats.d30); }
            if ts >= mtd_start   { accrue(&mut stats.mtd); }

            // Per-entrypoint $ over last 7 days (matches the API's weekly windows).
            if ts >= cutoff_7d {
                let ep = parsed.entrypoint.unwrap_or_else(|| "unknown".to_string());
                *stats.cost_by_entrypoint_7d.entry(ep).or_insert(0.0) += msg_cost;
            }
            // Cowork attribution: file lives under local-agent-mode-sessions
            // OR matches a known cliSessionId from the cowork index.
            if is_cowork_session {
                if ts >= cutoff_7d   { stats.cowork_cost_7d += msg_cost; }
                if ts >= today_start { stats.cowork_cost_today += msg_cost; }
                if ts >= mtd_start   { stats.cowork_cost_mtd += msg_cost; }
            }
        }
    }
    Ok(stats)
}

fn add(w: &mut TokenWindow, u: &ClaudeUsage) {
    w.input += u.input_tokens;
    w.output += u.output_tokens;
    w.cache_read += u.cache_read_input_tokens;
    w.cache_write += u.cache_creation_input_tokens;
}
