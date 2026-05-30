use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeLimitSource {
    Oauth,
    Cli,
    Web,
    #[default]
    Cache,
}

impl ClaudeLimitSource {
    pub fn label(self) -> &'static str {
        match self {
            ClaudeLimitSource::Oauth => "OAuth",
            ClaudeLimitSource::Cli => "CLI",
            ClaudeLimitSource::Web => "Web",
            ClaudeLimitSource::Cache => "Cache",
        }
    }
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

#[derive(Debug, Clone)]
pub struct ClaudeLiveLimits {
    pub source: ClaudeLimitSource,
    pub fetched_at: DateTime<Utc>,
    pub five_hour_percent: f64,
    pub five_hour_resets_at: Option<DateTime<Utc>>,
    pub weekly_percent: f64,
    pub weekly_resets_at: Option<DateTime<Utc>>,
    pub sub_quotas: Vec<SubQuota>,
    pub extra_usage: Option<ExtraUsageInfo>,
}

impl Default for ClaudeLiveLimits {
    fn default() -> Self {
        Self {
            source: ClaudeLimitSource::Cache,
            fetched_at: Utc::now(),
            five_hour_percent: 0.0,
            five_hour_resets_at: None,
            weekly_percent: 0.0,
            weekly_resets_at: None,
            sub_quotas: Vec::new(),
            extra_usage: None,
        }
    }
}

/// Result of a single live-limit fetch attempt. This keeps 429 distinct from
/// normal errors so the cache can apply a longer cooldown only when required.
pub(crate) enum FetchOutcome {
    Ok(ClaudeLiveLimits),
    RateLimited(String),
    Other(anyhow::Error),
}

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
    pub limit_source: Option<ClaudeLimitSource>,
    pub limit_fetched_at: Option<DateTime<Utc>>,
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

#[derive(Debug, Default, Clone, Copy)]
pub struct TokenWindow {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_limit_source_labels_match_ui_copy() {
        assert_eq!(ClaudeLimitSource::Oauth.label(), "OAuth");
        assert_eq!(ClaudeLimitSource::Cli.label(), "CLI");
        assert_eq!(ClaudeLimitSource::Web.label(), "Web");
        assert_eq!(ClaudeLimitSource::Cache.label(), "Cache");
    }
}
