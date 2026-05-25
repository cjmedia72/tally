use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

use super::api::{live_limits_from_usage_response, UsageResponse};
use super::cache::disk_cache_path;
use super::types::{ClaudeLimitSource, FetchOutcome};

// Anthropic's public Claude Code OAuth client. Same id baked into the
// Claude Code CLI and Claude Desktop. Used for the refresh-token grant
// against Anthropic's console-side token endpoint.
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
// Refresh proactively when within this many seconds of expiry so the next
// HTTP call always carries a fresh token (no wasted 401 round-trip).
const TOKEN_REFRESH_MARGIN_SECS: i64 = 60;

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    organization: Option<ProfileOrg>,
}

#[derive(Debug, Deserialize)]
struct ProfileOrg {
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OauthBlock,
    /// Capture-and-roundtrip any other top-level fields the Claude CLI may
    /// write (so refreshing doesn't strip them).
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OauthBlock {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    /// Preserves `scopes`, `subscriptionType`, `rateLimitTier`, etc.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn credentials_path() -> Result<PathBuf> {
    let mut p = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    p.push(".claude");
    p.push(".credentials.json");
    Ok(p)
}

fn read_credentials() -> Result<(PathBuf, CredentialsFile)> {
    let path = credentials_path()?;
    let file = File::open(&path).map_err(|e| anyhow!("read {}: {e}", path.display()))?;
    let creds: CredentialsFile =
        serde_json::from_reader(file).map_err(|e| anyhow!("parse credentials: {e}"))?;
    Ok((path, creds))
}

fn save_credentials(path: &Path, creds: &CredentialsFile) -> Result<()> {
    let body = serde_json::to_string_pretty(creds)?;
    std::fs::write(path, body).map_err(|e| anyhow!("write {}: {e}", path.display()))
}

fn is_token_expired(block: &OauthBlock) -> bool {
    match block.expires_at {
        Some(exp_ms) => {
            let now_ms = Utc::now().timestamp_millis();
            now_ms + (TOKEN_REFRESH_MARGIN_SECS * 1000) >= exp_ms
        }
        // No expiry info — assume valid until something 401s (legacy creds).
        None => false,
    }
}

fn post_refresh(refresh_token: &str) -> Result<RefreshResponse> {
    let resp = ureq::post(CLAUDE_OAUTH_TOKEN_URL)
        .set("Content-Type", "application/json")
        .set("User-Agent", &format!("claude-code/{}", claude_code_version()))
        .timeout(std::time::Duration::from_secs(15))
        .send_json(serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_OAUTH_CLIENT_ID,
        }))
        .map_err(|e| anyhow!("refresh claude oauth token: {e}"))?;
    resp.into_json::<RefreshResponse>()
        .map_err(|e| anyhow!("decode claude oauth refresh: {e}"))
}

/// Returns a non-expired access token, refreshing + persisting the credentials
/// file if needed. Mirrors the rotation Claude Code CLI itself performs, so a
/// user without the CLI installed (or one whose CLI hasn't run in a while)
/// still gets auto-refresh on long-running Tally sessions.
fn ensure_fresh_access_token() -> Result<String> {
    let (path, mut creds) = read_credentials()?;
    if !is_token_expired(&creds.claude_ai_oauth) {
        return Ok(creds.claude_ai_oauth.access_token.clone());
    }
    let refresh = creds
        .claude_ai_oauth
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow!("claude oauth token expired and no refresh_token to rotate"))?;
    let resp = post_refresh(&refresh)?;
    if let Some(at) = resp.access_token {
        creds.claude_ai_oauth.access_token = at;
    }
    if let Some(rt) = resp.refresh_token {
        creds.claude_ai_oauth.refresh_token = Some(rt);
    }
    if let Some(exp_in) = resp.expires_in {
        creds.claude_ai_oauth.expires_at = Some(Utc::now().timestamp_millis() + exp_in * 1000);
    }
    save_credentials(&path, &creds)?;
    Ok(creds.claude_ai_oauth.access_token)
}

pub(crate) fn read_oauth_token() -> Result<String> {
    ensure_fresh_access_token()
}

/// Fetch the user's Claude subscription tier identifier from /api/oauth/profile.
/// Cached aggressively (24h) since it changes rarely.
pub fn fetch_plan_tier() -> Result<String> {
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

pub(crate) fn http_fetch_live_limits() -> FetchOutcome {
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
                "Anthropic /api/oauth/usage returned 429 - backing off".to_string(),
            );
        }
        // 401 here means our (just-refreshed-if-needed) token was rejected.
        // Most likely cause: refresh_token revoked server-side. Bubble up
        // so the caller falls through to CLI parsing.
        Err(e) => return FetchOutcome::Other(anyhow!("call /api/oauth/usage: {e}")),
    };
    let body: UsageResponse = match resp.into_json() {
        Ok(b) => b,
        Err(e) => return FetchOutcome::Other(anyhow!("decode usage response: {e}")),
    };

    FetchOutcome::Ok(live_limits_from_usage_response(
        body,
        ClaudeLimitSource::Oauth,
    ))
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
