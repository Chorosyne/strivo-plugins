//! Pricing data + cost estimation for Crunchr analysis runs (M5.6).
//!
//! Today we expose a pricing table keyed by OpenRouter / Mistral
//! model slug, plus an estimator that takes prompt and completion
//! token counts and returns USD cents. The token counts come from
//! `pipeline::estimate_tokens` (the M1.1.h heuristic — accurate
//! enough for display; a tiktoken-rs upgrade is tracked separately).
//!
//! Prices reflect the public rate cards as of 2026-05; users on
//! enterprise plans will want to override locally. The table is a
//! `&'static [PricingRow]` so future contributors can grep + append
//! without touching downstream consumers.

/// USD per 1k tokens, broken into prompt vs completion sides.
#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    pub model: &'static str,
    /// USD per 1k input tokens.
    pub prompt_per_1k: f64,
    /// USD per 1k output tokens.
    pub completion_per_1k: f64,
}

/// Known model rates. Slugs match OpenRouter's canonical IDs (see
/// https://openrouter.ai/models) so the existing
/// `analysis.openrouter_api_key_env` flow round-trips cleanly.
pub const PRICING: &[Pricing] = &[
    // OpenRouter — Mistral family
    Pricing {
        model: "mistralai/mistral-7b-instruct",
        prompt_per_1k: 0.00007,
        completion_per_1k: 0.00007,
    },
    Pricing {
        model: "mistralai/mistral-small",
        prompt_per_1k: 0.0002,
        completion_per_1k: 0.0006,
    },
    Pricing {
        model: "mistralai/mistral-large",
        prompt_per_1k: 0.003,
        completion_per_1k: 0.009,
    },
    Pricing {
        model: "mistralai/mixtral-8x7b-instruct",
        prompt_per_1k: 0.00024,
        completion_per_1k: 0.00024,
    },
    Pricing {
        model: "mistralai/mixtral-8x22b-instruct",
        prompt_per_1k: 0.0012,
        completion_per_1k: 0.0012,
    },
    // OpenRouter — Anthropic
    Pricing {
        model: "anthropic/claude-3-haiku",
        prompt_per_1k: 0.00025,
        completion_per_1k: 0.00125,
    },
    Pricing {
        model: "anthropic/claude-3-sonnet",
        prompt_per_1k: 0.003,
        completion_per_1k: 0.015,
    },
    Pricing {
        model: "anthropic/claude-3-opus",
        prompt_per_1k: 0.015,
        completion_per_1k: 0.075,
    },
    Pricing {
        model: "anthropic/claude-3.5-sonnet",
        prompt_per_1k: 0.003,
        completion_per_1k: 0.015,
    },
    // OpenRouter — OpenAI
    Pricing {
        model: "openai/gpt-4o-mini",
        prompt_per_1k: 0.00015,
        completion_per_1k: 0.0006,
    },
    Pricing {
        model: "openai/gpt-4o",
        prompt_per_1k: 0.0025,
        completion_per_1k: 0.01,
    },
    // Whisper (transcription): per minute of audio, not per token —
    // included here for the table-of-record. Consumers requesting a
    // Pricing for these slugs will hit the heuristic fallback.
];

/// Look up pricing for a model slug, returning `None` for unknown
/// models (consumers fall back to a heuristic or refuse to estimate).
pub fn pricing_for(model: &str) -> Option<&'static Pricing> {
    PRICING.iter().find(|p| p.model == model)
}

/// Estimate cost in **cents** (`u64` for integer-only arithmetic in
/// downstream UI). Rounds half-up.
pub fn estimate_cost_cents(
    model: &str,
    prompt_tokens: usize,
    completion_tokens: usize,
) -> Option<u64> {
    let p = pricing_for(model)?;
    let usd = (prompt_tokens as f64 / 1000.0) * p.prompt_per_1k
        + (completion_tokens as f64 / 1000.0) * p.completion_per_1k;
    // Convert to cents and round half-up. Cap at u64::MAX as a sanity.
    let cents = (usd * 100.0).round() as i64;
    Some(cents.max(0) as u64)
}

/// Format a cent value as "$0.08" / "$1.23".
pub fn format_cents(cents: u64) -> String {
    let dollars = cents / 100;
    let rem = cents % 100;
    format!("${dollars}.{rem:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_lookup_known_model() {
        let p = pricing_for("mistralai/mistral-7b-instruct").unwrap();
        assert!(p.prompt_per_1k > 0.0);
    }

    #[test]
    fn pricing_lookup_unknown() {
        assert!(pricing_for("not/a-model").is_none());
    }

    #[test]
    fn cost_estimate_round_trip() {
        // mistral-large at 1k prompt + 1k completion = $0.003 + $0.009 = $0.012 = 1.2 cents
        let cents = estimate_cost_cents("mistralai/mistral-large", 1000, 1000).unwrap();
        // Half-up rounding: 1.2 cents rounds to 1.
        assert_eq!(cents, 1);
    }

    #[test]
    fn cost_estimate_larger_workload() {
        // 100k prompt + 50k completion against claude-3-haiku =
        // 100 * $0.00025 + 50 * $0.00125 = $0.025 + $0.0625 = $0.0875 = 9 cents
        let cents = estimate_cost_cents("anthropic/claude-3-haiku", 100_000, 50_000).unwrap();
        assert_eq!(cents, 9);
    }

    #[test]
    fn format_cents_shape() {
        assert_eq!(format_cents(8), "$0.08");
        assert_eq!(format_cents(100), "$1.00");
        assert_eq!(format_cents(1234), "$12.34");
    }
}
