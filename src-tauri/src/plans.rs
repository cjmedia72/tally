// Map vendor-provided plan identifiers → display label + monthly cost (USD).
// Costs are public list prices as of May 2026. Update when vendors change tiers.

#[derive(Debug, Clone)]
pub struct PlanInfo {
    pub label: String,
    pub monthly_cost: f64,
}

/// Anthropic `rate_limit_tier` from /api/oauth/profile
pub fn claude_plan(rate_limit_tier: &str) -> PlanInfo {
    match rate_limit_tier {
        "default_claude_pro" => plan("PRO · $20", 20.0),
        "default_claude_max_5x" => plan("MAX 5× · $100", 100.0),
        "default_claude_max_20x" => plan("MAX 20× · $200", 200.0),
        "default_claude_team" => plan("TEAM · $30", 30.0),
        "default_claude_team_premium" => plan("TEAM PRO · $150", 150.0),
        "default_claude_enterprise" => plan("ENTERPRISE", 0.0),
        // Friendly fallback for unknown tiers — show the raw id so it's visible
        other if other.contains("max") => plan("MAX · ?", 100.0),
        other if other.contains("pro") => plan("PRO · ?", 20.0),
        _ => plan("CLAUDE · ?", 0.0),
    }
}

/// Codex/OpenAI `plan_type` from app-server JSON-RPC rate-limits response.
/// `prolite` is OpenAI's internal id for the 5× tier (publicly "Codex Pro 5×").
pub fn codex_plan(plan_type: &str) -> PlanInfo {
    match plan_type {
        "free" => plan("FREE", 0.0),
        "plus" => plan("PLUS · $20", 20.0),
        "prolite" => plan("PRO 5× · $100", 100.0),
        "pro" => plan("PRO · $200", 200.0),
        "team" => plan("TEAM", 25.0),
        "enterprise" => plan("ENTERPRISE", 0.0),
        other => plan(&other.to_uppercase(), 0.0),
    }
}

fn plan(label: &str, cost: f64) -> PlanInfo {
    PlanInfo {
        label: label.to_string(),
        monthly_cost: cost,
    }
}
