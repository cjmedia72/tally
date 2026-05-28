// Per-model pricing in USD per 1M tokens.
// Source: https://platform.claude.com/docs/en/about-claude/pricing (verified 2026-05-25).
// Cache-read is 10% of base input; cache-write (5-min ephemeral, the default)
// is 125% of base input. Tally doesn't distinguish 5m vs 1h cache writes since
// JSONL doesn't carry that bit — 5m is by far the more common default.
//
// Anthropic dropped Opus pricing dramatically with Opus 4.5: the 4.5/4.6/4.7/4.8
// generation runs at $5/$25 vs the deprecated Opus 4/4.1 at $15/$75. Haiku
// pricing went UP slightly with 4.5 ($1/$5) vs retired 3.5 ($0.80/$4).

#[derive(Debug, Clone, Copy)]
pub struct ModelRate {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

const fn rate(input: f64, output: f64) -> ModelRate {
    ModelRate {
        input,
        output,
        cache_read: input * 0.10,
        cache_write: input * 1.25,
    }
}

/// Look up Claude pricing by model name. Version-aware so the post-Opus-4.5
/// price drop is reflected and Haiku generations are billed correctly.
///
/// Recognized SKU patterns (case-insensitive, substring match):
/// - `claude-opus-4-5-*` through `opus-4-9-*` and `opus-5-*` → new Opus
/// - `claude-opus-4-1-*`, `opus-4-DATE`, bare `opus`, `claude-3-opus`
///   → legacy Opus (deprecated $15/$75)
/// - `claude-haiku-4-5-*` and forward → new Haiku
/// - `claude-3-5-haiku-*` or bare `haiku` → retired Haiku 3.5
/// - any `sonnet` (4 / 4.5 / 4.6) → unchanged $3/$15
/// - unknown → Sonnet rate (safest middle estimate)
pub fn claude_rate(model: &str) -> ModelRate {
    let m = model.to_lowercase();
    if m.contains("opus") {
        if has_modern_minor(&m, "opus") {
            // Opus 4.5 / 4.6 / 4.7 / 4.8 + future ≥4.5
            return rate(5.00, 25.00);
        }
        // Opus 4 (deprecated), Opus 4.1 (deprecated), Claude 3 Opus — legacy
        return rate(15.00, 75.00);
    }
    if m.contains("haiku") {
        if has_modern_minor(&m, "haiku") {
            // Haiku 4.5 + future ≥4.5
            return rate(1.00, 5.00);
        }
        // Haiku 3.5 (retired except Bedrock/Vertex), older Haiku
        return rate(0.80, 4.00);
    }
    if m.contains("sonnet") {
        // Sonnet 4 (deprecated), 4.5, 4.6 — all share standard Sonnet pricing
        return rate(3.00, 15.00);
    }
    // Unknown — default to Sonnet (middle of the range, won't wildly over-
    // or under-report on a new model we haven't seen yet).
    rate(3.00, 15.00)
}

/// Detects whether the model id contains a `<family>-X-Y` segment where the
/// generation has hit Anthropic's "modern" pricing threshold (X=4 AND Y≥5,
/// or X≥5). Supports both dash (`4-5`) and dot (`4.5`) separators.
///
/// Examples for family=`opus`:
/// - `claude-opus-4-5-20250929`  → true  (Opus 4.5)
/// - `claude-opus-4-7-20260301`  → true  (Opus 4.7)
/// - `claude-opus-5-0-20270101`  → true  (future Opus 5.x)
/// - `claude-opus-4-1-20250805`  → false (Opus 4.1, legacy)
/// - `claude-opus-4-20250514`    → false (Opus 4, legacy)
/// - `claude-3-opus-20240229`    → false (Opus 3, legacy)
fn has_modern_minor(m: &str, family: &str) -> bool {
    // X≥5 (next major generation — opus-5, haiku-5, ...)
    for sep in ['-', '.'] {
        let needle = format!("{family}{sep}5");
        if m.contains(&needle) {
            return true;
        }
    }
    // 4.5 through 4.9 (current generation modern pricing)
    for minor in 5..=9 {
        for sep in ['-', '.'] {
            let needle = format!("{family}-4{sep}{minor}");
            if m.contains(&needle) {
                return true;
            }
            let needle2 = format!("{family}.4{sep}{minor}");
            if m.contains(&needle2) {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone, Copy)]
pub struct CodexRate {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64, // reasoning charged at this rate too
}

/// Look up Codex/OpenAI pricing by model name. Public list prices May 2026.
/// Default is gpt-5.5 (what Codex CLI runs today). Cached input is the
/// vendor's discounted rate for content read from prompt cache.
pub fn codex_rate(model: &str) -> CodexRate {
    let m = model.to_lowercase();
    if m.contains("gpt-5-codex") {
        // gpt-5-codex SKU — same family as base gpt-5
        CodexRate {
            input: 1.25,
            cached_input: 0.125,
            output: 10.00,
        }
    } else if m == "gpt-5" || m.starts_with("gpt-5-") && !m.contains("5.") && !m.contains("5-5") {
        // Plain gpt-5 (not 5.x variants)
        CodexRate {
            input: 1.25,
            cached_input: 0.125,
            output: 10.00,
        }
    } else if m.contains("gpt-5.4") || m.contains("gpt-5-4") {
        // gpt-5.4 — between 5 and 5.5; treat as gpt-5.5 family by default
        CodexRate {
            input: 5.00,
            cached_input: 0.50,
            output: 30.00,
        }
    } else if m.contains("gpt-5.5") || m.contains("gpt-5-5") {
        // GPT-5.5 standard public pricing
        CodexRate {
            input: 5.00,
            cached_input: 0.50,
            output: 30.00,
        }
    } else if m.contains("o3") || m.contains("o4") {
        // Reasoning-heavy older models
        CodexRate {
            input: 5.00,
            cached_input: 0.50,
            output: 20.00,
        }
    } else {
        // Unknown / future model — default to gpt-5.5 (current Codex CLI default)
        CodexRate {
            input: 5.00,
            cached_input: 0.50,
            output: 30.00,
        }
    }
}

/// Cost for a single Claude message's token usage.
pub fn claude_message_cost(
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    let r = claude_rate(model);
    (input as f64 / 1_000_000.0) * r.input
        + (output as f64 / 1_000_000.0) * r.output
        + (cache_read as f64 / 1_000_000.0) * r.cache_read
        + (cache_write as f64 / 1_000_000.0) * r.cache_write
}

/// Cost for a Codex turn's token usage.
pub fn codex_turn_cost(
    model: &str,
    input: u64,
    cached_input: u64,
    output: u64,
    reasoning: u64,
) -> f64 {
    let r = codex_rate(model);
    let non_cached_input = input.saturating_sub(cached_input);
    (non_cached_input as f64 / 1_000_000.0) * r.input
        + (cached_input as f64 / 1_000_000.0) * r.cached_input
        + ((output + reasoning) as f64 / 1_000_000.0) * r.output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected ~{b}, got {a}");
    }

    // --- Opus generation routing ---
    #[test] fn opus_45_is_new_pricing() { approx(claude_rate("claude-opus-4-5-20250929").input, 5.00); }
    #[test] fn opus_46_is_new_pricing() { approx(claude_rate("claude-opus-4-6-20251115").input, 5.00); }
    #[test] fn opus_47_is_new_pricing() { approx(claude_rate("claude-opus-4-7-20260301").input, 5.00); }
    #[test] fn opus_48_is_new_pricing() { approx(claude_rate("claude-opus-4-8-20260501").input, 5.00); }
    #[test] fn opus_41_is_legacy_pricing() { approx(claude_rate("claude-opus-4-1-20250805").input, 15.00); }
    #[test] fn opus_4_is_legacy_pricing() { approx(claude_rate("claude-opus-4-20250514").input, 15.00); }
    #[test] fn opus_3_is_legacy_pricing() { approx(claude_rate("claude-3-opus-20240229").input, 15.00); }
    #[test] fn opus_45_output_25() { approx(claude_rate("claude-opus-4-5").output, 25.00); }
    #[test] fn opus_41_output_75() { approx(claude_rate("claude-opus-4-1").output, 75.00); }

    // --- Haiku generation routing ---
    #[test] fn haiku_45_is_new_pricing() { approx(claude_rate("claude-haiku-4-5-20251001").input, 1.00); }
    #[test] fn haiku_45_output_5() { approx(claude_rate("claude-haiku-4-5").output, 5.00); }
    #[test] fn haiku_35_is_legacy_pricing() { approx(claude_rate("claude-3-5-haiku-20241022").input, 0.80); }
    #[test] fn haiku_35_output_4() { approx(claude_rate("claude-3-5-haiku-20241022").output, 4.00); }

    // --- Sonnet routing (unchanged across 4 / 4.5 / 4.6) ---
    #[test] fn sonnet_45() { approx(claude_rate("claude-sonnet-4-5-20250929").input, 3.00); }
    #[test] fn sonnet_46() { approx(claude_rate("claude-sonnet-4-6").input, 3.00); }
    #[test] fn sonnet_4_deprecated() { approx(claude_rate("claude-sonnet-4-20250514").input, 3.00); }

    // --- Cache multipliers ---
    #[test] fn cache_read_is_10pct() { let r = claude_rate("claude-opus-4-5"); approx(r.cache_read, 0.5); }
    #[test] fn cache_write_is_125pct() { let r = claude_rate("claude-opus-4-5"); approx(r.cache_write, 6.25); }

    // --- Composite cost calculation: 1M input + 100k output on Opus 4.5 ---
    #[test]
    fn opus_45_composite_cost() {
        // 1M input @ $5 = $5; 100k output @ $25 = $2.50; total = $7.50
        let cost = claude_message_cost("claude-opus-4-5", 1_000_000, 100_000, 0, 0);
        approx(cost, 7.50);
    }

    // --- Composite cost on legacy Opus 4.1 (3× higher — the bug we just fixed) ---
    #[test]
    fn opus_41_composite_cost() {
        // 1M input @ $15 = $15; 100k output @ $75 = $7.50; total = $22.50
        let cost = claude_message_cost("claude-opus-4-1", 1_000_000, 100_000, 0, 0);
        approx(cost, 22.50);
    }

    // --- Unknown model falls back safely to Sonnet rate ---
    #[test]
    fn unknown_falls_back_to_sonnet() {
        approx(claude_rate("claude-future-model-20281001").input, 3.00);
    }

    // --- Future Opus 5.x routes to modern pricing ---
    #[test]
    fn opus_5_is_modern() {
        approx(claude_rate("claude-opus-5-0-20270101").input, 5.00);
    }
}
