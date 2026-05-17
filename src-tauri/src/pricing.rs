// Per-model pricing in USD per 1M tokens. Public list prices, May 2026.
// Update when vendor pricing shifts. Cache-read is typically 10% of input;
// cache-write is 125% (5min ephemeral) — using the standard ratio.

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

/// Look up Claude pricing by model name. Matches substrings so versioned
/// models like "claude-sonnet-4-6" map to the Sonnet rate. Unknown models
/// fall back to Sonnet (middle of the pricing range) so we never wildly
/// over- or under-report.
pub fn claude_rate(model: &str) -> ModelRate {
    let m = model.to_lowercase();
    if m.contains("opus") {
        // Opus 4 / 4.5 / 4.7 family
        rate(15.00, 75.00)
    } else if m.contains("haiku") {
        // Haiku 4 / 4.5 family
        rate(0.80, 4.00)
    } else if m.contains("sonnet") {
        // Sonnet 4 / 4.5 / 4.6 family
        rate(3.00, 15.00)
    } else {
        // Unknown — use Sonnet as the safest middle estimate
        rate(3.00, 15.00)
    }
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
        CodexRate { input: 1.25, cached_input: 0.125, output: 10.00 }
    } else if m == "gpt-5" || m.starts_with("gpt-5-") && !m.contains("5.") && !m.contains("5-5") {
        // Plain gpt-5 (not 5.x variants)
        CodexRate { input: 1.25, cached_input: 0.125, output: 10.00 }
    } else if m.contains("gpt-5.4") || m.contains("gpt-5-4") {
        // gpt-5.4 — between 5 and 5.5; treat as gpt-5.5 family by default
        CodexRate { input: 5.00, cached_input: 0.50, output: 30.00 }
    } else if m.contains("gpt-5.5") || m.contains("gpt-5-5") {
        // GPT-5.5 standard public pricing
        CodexRate { input: 5.00, cached_input: 0.50, output: 30.00 }
    } else if m.contains("o3") || m.contains("o4") {
        // Reasoning-heavy older models
        CodexRate { input: 5.00, cached_input: 0.50, output: 20.00 }
    } else {
        // Unknown / future model — default to gpt-5.5 (current Codex CLI default)
        CodexRate { input: 5.00, cached_input: 0.50, output: 30.00 }
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
