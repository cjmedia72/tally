use anyhow::Result;
use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;

use crate::{claude, codex, plans};

#[derive(Debug, Serialize)]
pub struct UsageSnapshot {
    pub claude: Option<BrandSnapshot>,
    pub codex: Option<BrandSnapshot>,
    pub claude_available: bool,
    pub codex_available: bool,
    pub roi: RoiBlock,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct BrandSnapshot {
    pub name: String,
    pub tier: String,
    pub color: String,
    pub five_hour: WindowState,
    pub weekly: WindowState,
    pub periods: std::collections::HashMap<String, PeriodView>,
    pub last_event_at: Option<DateTime<Utc>>,
    /// Additional weekly quotas (Sonnet / Opus / Cowork / Claude Design).
    /// Only populated for Claude. Empty array for Codex.
    pub sub_quotas: Vec<SubQuotaView>,
    /// Anthropic extra-usage $ tracker. Null when not enabled.
    pub extra_usage: Option<ExtraUsageView>,
}

#[derive(Debug, Serialize)]
pub struct SubQuotaView {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    /// 7-day $ cost attributed to this sub-quota (when computable from JSONL).
    /// 0.0 if no per-entrypoint match exists.
    pub cost_7d: f64,
}

#[derive(Debug, Serialize)]
pub struct ExtraUsageView {
    pub enabled: bool,
    pub used: f64,
    pub limit: f64,
    pub used_percent: f64,
    pub currency: String,
}

#[derive(Debug, Serialize, Default)]
pub struct PeriodView {
    pub tokens: TokenBreakdown,
    pub requests: u64,
    pub api_equiv: f64,
}

#[derive(Debug, Serialize)]
pub struct WindowState {
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub resets_label: String,
}

#[derive(Debug, Serialize, Default)]
pub struct TokenBreakdown {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cached_input: u64,
    pub reasoning: u64,
}

#[derive(Debug, Serialize)]
pub struct RoiBlock {
    /// Per-period combined api equivalent (Claude + Codex). Keys:
    /// "today", "1d", "7d", "14d", "30d", "mtd"
    pub period_api_equiv: std::collections::HashMap<String, f64>,
    pub today_api_equiv: f64, // legacy alias for today
    pub mtd_api_equiv: f64,   // always MTD (the right side cell)
    pub subscriptions: f64,
    pub claude_monthly: f64,
    pub codex_monthly: f64,
    pub leverage: f64,
    pub saved_mtd: f64,
    pub mtd_days_elapsed: u32,
}

pub fn build(refresh_ms: u64) -> Result<UsageSnapshot> {
    let claude_available = claude::is_available();
    let codex_available = codex::is_available();

    let claude_stats = if claude_available { claude::collect(refresh_ms)? } else { claude::ClaudeStats::default() };
    let codex_stats  = if codex_available  { codex::collect()?  } else { codex::CodexStats::default() };

    // Auto-detect plans from API. Fall back to sensible defaults if calls fail.
    let claude_plan = if claude_available {
        claude::fetch_plan_tier()
            .map(|tier| plans::claude_plan(&tier))
            .unwrap_or_else(|_| plans::claude_plan("default_claude_max_5x"))
    } else {
        plans::PlanInfo { label: String::new(), monthly_cost: 0.0 }
    };
    let codex_plan = if codex_available {
        plans::codex_plan(
            if codex_stats.plan_label_raw.is_empty() {
                "prolite"
            } else {
                &codex_stats.plan_label_raw
            }
        )
    } else {
        plans::PlanInfo { label: String::new(), monthly_cost: 0.0 }
    };

    // Per-model accurate costs — accumulated during JSONL walk using each
    // message/session's actual model rate, not a blended fallback.
    let claude_today_cost = claude_stats.today.cost;
    let claude_mtd_cost   = claude_stats.mtd.cost;
    let codex_today_cost  = codex_stats.today.cost;
    let codex_mtd_cost    = codex_stats.mtd.cost;

    let today_api_equiv = claude_today_cost + codex_today_cost;
    let mtd_api_equiv = claude_mtd_cost + codex_mtd_cost;
    // Subscriptions sourced from auto-detected plans, not hardcoded.
    let subscriptions = claude_plan.monthly_cost + codex_plan.monthly_cost;
    let leverage = if subscriptions > 0.0 {
        mtd_api_equiv / subscriptions
    } else {
        0.0
    };
    let saved_mtd = (mtd_api_equiv - subscriptions).max(0.0);

    let now = Utc::now();

    // Build per-period views for both brands.
    let claude_periods = {
        let mut m = std::collections::HashMap::new();
        for (key, p) in [
            ("today", &claude_stats.today),
            ("1d",    &claude_stats.d1),
            ("7d",    &claude_stats.d7),
            ("14d",   &claude_stats.d14),
            ("30d",   &claude_stats.d30),
            ("mtd",   &claude_stats.mtd),
        ] {
            m.insert(key.to_string(), PeriodView {
                tokens: TokenBreakdown {
                    input: p.tokens.input,
                    output: p.tokens.output,
                    cache_read: p.tokens.cache_read,
                    cache_write: p.tokens.cache_write,
                    ..Default::default()
                },
                requests: p.requests,
                api_equiv: p.cost,
            });
        }
        m
    };
    let codex_periods = {
        let mut m = std::collections::HashMap::new();
        for (key, p) in [
            ("today", &codex_stats.today),
            ("1d",    &codex_stats.d1),
            ("7d",    &codex_stats.d7),
            ("14d",   &codex_stats.d14),
            ("30d",   &codex_stats.d30),
            ("mtd",   &codex_stats.mtd),
        ] {
            m.insert(key.to_string(), PeriodView {
                tokens: TokenBreakdown {
                    input: p.tokens.input,
                    cached_input: p.tokens.cached_input,
                    output: p.tokens.output,
                    reasoning: p.tokens.reasoning,
                    ..Default::default()
                },
                requests: p.requests,
                api_equiv: p.cost,
            });
        }
        m
    };

    let claude_snap = if claude_available { Some(BrandSnapshot {
        name: "Claude Code".to_string(),
        tier: claude_plan.label.clone(),
        color: "#D97757".to_string(),
        five_hour: build_window(
            claude_stats.five_hour_percent,
            claude_stats.next_5h_reset,
            short_time,
        ),
        weekly: build_window(
            claude_stats.weekly_percent,
            claude_stats.next_weekly_reset,
            short_date_time,
        ),
        periods: claude_periods,
        last_event_at: claude_stats.last_event_at,
        sub_quotas: {
            let mut views: Vec<SubQuotaView> = claude_stats.sub_quotas.iter().map(|q| {
                let label_lc = q.label.to_lowercase();
                let cost = if label_lc.contains("cowork") {
                    claude_stats.cowork_cost_7d
                } else {
                    claude_stats.cost_by_entrypoint_7d.iter()
                        .filter(|(ep, _)| ep.to_lowercase().contains(&label_lc))
                        .map(|(_, c)| *c).sum::<f64>()
                };
                SubQuotaView {
                    label: q.label.clone(),
                    used_percent: q.utilization,
                    resets_at: q.resets_at,
                    cost_7d: cost,
                }
            }).collect();
            // Synthesize a Cowork row if we have local Cowork session $ but the
            // API returned null for seven_day_cowork (older usage outside the 7d
            // window, or API didn't include it). Use 0% utilization in that case.
            let has_cowork_row = views.iter().any(|v| v.label.eq_ignore_ascii_case("cowork"));
            if !has_cowork_row && claude_stats.cowork_cost_7d > 0.0 {
                views.push(SubQuotaView {
                    label: "Cowork".to_string(),
                    used_percent: 0.0,
                    resets_at: None,
                    cost_7d: claude_stats.cowork_cost_7d,
                });
            }
            views
        },
        extra_usage: claude_stats.extra_usage.as_ref().map(|e| ExtraUsageView {
            enabled: e.enabled,
            used: e.used,
            limit: e.limit,
            used_percent: e.utilization,
            currency: e.currency.clone(),
        }),
    }) } else { None };

    let codex_snap = if codex_available { Some(BrandSnapshot {
        name: "Codex".to_string(),
        tier: codex_plan.label.clone(),
        color: "#10A37F".to_string(),
        five_hour: build_window(
            codex_stats.rate_limits.primary_used_percent,
            codex_stats.rate_limits.primary_resets_at,
            short_time,
        ),
        weekly: build_window(
            codex_stats.rate_limits.secondary_used_percent,
            codex_stats.rate_limits.secondary_resets_at,
            short_date_time,
        ),
        periods: codex_periods,
        last_event_at: codex_stats.last_event_at,
        sub_quotas: Vec::new(),
        extra_usage: None,
    }) } else { None };

    // Combined api equivalents per period (Claude + Codex)
    let combined_period = |key: &str| -> f64 {
        let c = claude_stats_period_cost(&claude_stats, key);
        let x = codex_stats_period_cost(&codex_stats, key);
        c + x
    };
    let mut period_api_equiv = std::collections::HashMap::new();
    for k in ["today", "1d", "7d", "14d", "30d", "mtd"] {
        period_api_equiv.insert(k.to_string(), combined_period(k));
    }

    let roi = RoiBlock {
        period_api_equiv,
        today_api_equiv,
        mtd_api_equiv,
        subscriptions,
        claude_monthly: claude_plan.monthly_cost,
        codex_monthly: codex_plan.monthly_cost,
        leverage,
        saved_mtd,
        mtd_days_elapsed: now.day(),
    };

    Ok(UsageSnapshot {
        claude: claude_snap,
        codex: codex_snap,
        claude_available,
        codex_available,
        roi,
        updated_at: now,
    })
}

fn claude_stats_period_cost(s: &claude::ClaudeStats, key: &str) -> f64 {
    match key {
        "today" => s.today.cost,
        "1d"    => s.d1.cost,
        "7d"    => s.d7.cost,
        "14d"   => s.d14.cost,
        "30d"   => s.d30.cost,
        "mtd"   => s.mtd.cost,
        _ => 0.0,
    }
}

fn codex_stats_period_cost(s: &codex::CodexStats, key: &str) -> f64 {
    match key {
        "today" => s.today.cost,
        "1d"    => s.d1.cost,
        "7d"    => s.d7.cost,
        "14d"   => s.d14.cost,
        "30d"   => s.d30.cost,
        "mtd"   => s.mtd.cost,
        _ => 0.0,
    }
}

fn short_time(dt: DateTime<Utc>) -> String {
    let local: DateTime<chrono::Local> = dt.into();
    local.format("%-I:%M %p").to_string()
}

fn short_date_time(dt: DateTime<Utc>) -> String {
    let local: DateTime<chrono::Local> = dt.into();
    local.format("%a %-I:%M %p").to_string()
}

/// Build a WindowState, normalizing past resets_at labels.
///
/// Claude/Codex rate-limit APIs return a `resets_at` that points at the END
/// of the *current* active window. Once that timestamp passes with no new
/// activity, the server keeps returning the same (now-stale) timestamp until
/// the user sends a new message that starts a fresh window. A stale or absent
/// timestamp should not erase a valid utilization percentage; it only means we
/// cannot show an exact reset clock.
fn build_window(
    raw_percent: f64,
    resets_at: Option<DateTime<Utc>>,
    fmt: fn(DateTime<Utc>) -> String,
) -> WindowState {
    let now = Utc::now();
    let used_percent = raw_percent.clamp(0.0, 100.0);
    match resets_at {
        Some(t) if t > now => WindowState {
            used_percent,
            resets_at: Some(t),
            resets_label: fmt(t),
        },
        _ => WindowState {
            used_percent,
            resets_at: None,
            resets_label: if used_percent > 0.0 {
                "Reset pending".to_string()
            } else {
                "Ready".to_string()
            },
        },
    }
}
