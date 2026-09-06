//! Model pricing.
//!
//! Verbatim port of the bundled rate table in
//! `Vendored/CostUsage/CostUsagePricing.swift:68-207` (Codex) and `:246-420` (Claude).
//! Rates are USD **per token**, exactly as upstream stores them.
//!
//! Semantics upstream documents and this port keeps:
//! - `cache_read` `None` → cached input is billed at the normal input rate (no discount).
//! - `cache_write` `None` → cache writes are billed as uncached input.
//! - `threshold_tokens` → a request whose input exceeds it is billed entirely at the
//!   above-threshold rates (long-context pricing applies to the whole request).
//! - An unknown model resolves to `None`: upstream refuses to invent a price, and so do we,
//!   because a wrong number is worse than an honest gap.

/// Per-token rates for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub threshold_tokens: Option<u64>,
    pub input_above: Option<f64>,
    pub output_above: Option<f64>,
    pub cache_read_above: Option<f64>,
    pub cache_write_above: Option<f64>,
    /// Upstream's `displayLabel`, e.g. "Research Preview" for the free Spark model.
    pub label: Option<&'static str>,
}

impl Rates {
    const fn simple(input: f64, output: f64, cache_read: Option<f64>) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write: None,
            threshold_tokens: None,
            input_above: None,
            output_above: None,
            cache_read_above: None,
            cache_write_above: None,
            label: None,
        }
    }

    const fn tiered(
        input: f64,
        output: f64,
        cache_read: f64,
        cache_write: Option<f64>,
        threshold: u64,
        input_above: f64,
        output_above: f64,
        cache_read_above: f64,
        cache_write_above: Option<f64>,
    ) -> Self {
        Self {
            input,
            output,
            cache_read: Some(cache_read),
            cache_write,
            threshold_tokens: Some(threshold),
            input_above: Some(input_above),
            output_above: Some(output_above),
            cache_read_above: Some(cache_read_above),
            cache_write_above,
            label: None,
        }
    }

    /// Rates for a request of `input_tokens` total input, applying long-context tiers.
    fn tier_for(&self, input_tokens: u64) -> (f64, f64, Option<f64>, Option<f64>) {
        let above = self
            .threshold_tokens
            .is_some_and(|threshold| input_tokens > threshold);
        if above {
            (
                self.input_above.unwrap_or(self.input),
                self.output_above.unwrap_or(self.output),
                self.cache_read_above.or(self.cache_read),
                self.cache_write_above.or(self.cache_write),
            )
        } else {
            (self.input, self.output, self.cache_read, self.cache_write)
        }
    }

    /// Cost in USD for one request's token counts.
    ///
    /// `uncached_input` excludes cache reads and writes; each cache bucket falls back to the
    /// input rate when the model has no separate rate, mirroring upstream.
    pub fn cost(&self, tokens: RequestTokens) -> f64 {
        let total_input = tokens.uncached_input + tokens.cache_read + tokens.cache_write;
        let (input, output, cache_read, cache_write) = self.tier_for(total_input);

        tokens.uncached_input as f64 * input
            + tokens.cache_read as f64 * cache_read.unwrap_or(input)
            + tokens.cache_write as f64 * cache_write.unwrap_or(input)
            + tokens.output as f64 * output
    }
}

/// Token counts for a single request, already split into billing buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestTokens {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Includes reasoning tokens; upstream bills reasoning as output.
    pub output: u64,
}

/// `CostUsagePricing.swift:68-207`.
const CODEX: &[(&str, Rates)] = &[
    ("gpt-5", Rates::simple(1.25e-6, 1e-5, Some(1.25e-7))),
    ("gpt-5-codex", Rates::simple(1.25e-6, 1e-5, Some(1.25e-7))),
    ("gpt-5-mini", Rates::simple(2.5e-7, 2e-6, Some(2.5e-8))),
    ("gpt-5-nano", Rates::simple(5e-8, 4e-7, Some(5e-9))),
    ("gpt-5-pro", Rates::simple(1.5e-5, 1.2e-4, None)),
    ("gpt-5.1", Rates::simple(1.25e-6, 1e-5, Some(1.25e-7))),
    ("gpt-5.1-codex", Rates::simple(1.25e-6, 1e-5, Some(1.25e-7))),
    (
        "gpt-5.1-codex-max",
        Rates::simple(1.25e-6, 1e-5, Some(1.25e-7)),
    ),
    (
        "gpt-5.1-codex-mini",
        Rates::simple(2.5e-7, 2e-6, Some(2.5e-8)),
    ),
    ("gpt-5.2", Rates::simple(1.75e-6, 1.4e-5, Some(1.75e-7))),
    (
        "gpt-5.2-codex",
        Rates::simple(1.75e-6, 1.4e-5, Some(1.75e-7)),
    ),
    ("gpt-5.2-pro", Rates::simple(2.1e-5, 1.68e-4, None)),
    (
        "gpt-5.3-codex",
        Rates::simple(1.75e-6, 1.4e-5, Some(1.75e-7)),
    ),
    (
        "gpt-5.3-codex-spark",
        Rates {
            label: Some("Research Preview"),
            ..Rates::simple(0.0, 0.0, Some(0.0))
        },
    ),
    (
        "gpt-5.4",
        Rates::tiered(
            2.5e-6, 1.5e-5, 2.5e-7, None, 272_000, 5e-6, 2.25e-5, 5e-7, None,
        ),
    ),
    ("gpt-5.4-mini", Rates::simple(7.5e-7, 4.5e-6, Some(7.5e-8))),
    ("gpt-5.4-nano", Rates::simple(2e-7, 1.25e-6, Some(2e-8))),
    ("gpt-5.4-pro", Rates::simple(3e-5, 1.8e-4, None)),
    (
        "gpt-5.5",
        Rates::tiered(5e-6, 3e-5, 5e-7, None, 272_000, 1e-5, 4.5e-5, 1e-6, None),
    ),
    ("gpt-5.5-pro", Rates::simple(3e-5, 1.8e-4, None)),
    (
        "gpt-5.6-sol",
        Rates::tiered(
            5e-6,
            3e-5,
            5e-7,
            Some(6.25e-6),
            272_000,
            1e-5,
            4.5e-5,
            1e-6,
            Some(1.25e-5),
        ),
    ),
    (
        "gpt-5.6-terra",
        Rates::tiered(
            2e-6,
            1.2e-5,
            2e-7,
            Some(2.5e-6),
            272_000,
            4e-6,
            1.8e-5,
            4e-7,
            Some(5e-6),
        ),
    ),
    (
        "gpt-5.6-luna",
        Rates::tiered(
            2e-7,
            1.2e-6,
            2e-8,
            Some(2.5e-7),
            272_000,
            4e-7,
            2.4e-6,
            4e-8,
            Some(5e-7),
        ),
    ),
];

/// `CostUsagePricing.swift:246-420`. Claude bills cache creation separately from input.
const CLAUDE: &[(&str, Rates)] = &[
    (
        "claude-fable-5",
        Rates {
            cache_write: Some(1.25e-5),
            ..Rates::simple(1e-5, 5e-5, Some(1e-6))
        },
    ),
    (
        "claude-haiku-4-5",
        Rates {
            cache_write: Some(1.25e-6),
            ..Rates::simple(1e-6, 5e-6, Some(1e-7))
        },
    ),
    (
        "claude-opus-4-5",
        Rates {
            cache_write: Some(6.25e-6),
            ..Rates::simple(5e-6, 2.5e-5, Some(5e-7))
        },
    ),
    (
        "claude-opus-4-6",
        Rates {
            cache_write: Some(6.25e-6),
            ..Rates::simple(5e-6, 2.5e-5, Some(5e-7))
        },
    ),
    (
        "claude-opus-4-7",
        Rates {
            cache_write: Some(6.25e-6),
            ..Rates::simple(5e-6, 2.5e-5, Some(5e-7))
        },
    ),
    (
        "claude-opus-4-8",
        Rates {
            cache_write: Some(6.25e-6),
            ..Rates::simple(5e-6, 2.5e-5, Some(5e-7))
        },
    ),
    (
        "claude-sonnet-4-5",
        Rates::tiered(
            3e-6,
            1.5e-5,
            3e-7,
            Some(3.75e-6),
            200_000,
            6e-6,
            2.25e-5,
            6e-7,
            Some(7.5e-6),
        ),
    ),
    (
        "claude-sonnet-4-6",
        Rates {
            cache_write: Some(3.75e-6),
            ..Rates::simple(3e-6, 1.5e-5, Some(3e-7))
        },
    ),
];

/// Strips the date suffix and provider route prefix upstream also ignores, e.g.
/// `claude-sonnet-4-5-20250929` → `claude-sonnet-4-5`, `openai/gpt-5.6-sol` → `gpt-5.6-sol`.
pub fn normalize(model: &str) -> String {
    let mut name = model.trim().to_lowercase();
    if let Some((_, tail)) = name.rsplit_once('/') {
        name = tail.to_string();
    }
    // Trailing `-YYYYMMDD` release stamp.
    let parts: Vec<&str> = name.split('-').collect();
    if let Some(last) = parts.last() {
        if last.len() == 8 && last.chars().all(|c| c.is_ascii_digit()) {
            name = parts[..parts.len() - 1].join("-");
        }
    }
    name
}

/// Rates for a model, or `None` when the model is unknown.
pub fn rates_for(model: &str) -> Option<Rates> {
    let name = normalize(model);
    let table = if name.starts_with("claude") {
        CLAUDE
    } else {
        CODEX
    };
    if let Some((_, rates)) = table.iter().find(|(key, _)| *key == name) {
        return Some(*rates);
    }
    // Suffixed variants (`gpt-5.6-sol-high`, `claude-opus-4-6-thinking`): longest prefix wins.
    table
        .iter()
        .filter(|(key, _)| name.starts_with(key))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, rates)| *rates)
}

/// Convenience: cost in USD for one request, `None` when the model is unpriced.
pub fn cost_for(model: &str, tokens: RequestTokens) -> Option<f64> {
    rates_for(model).map(|rates| rates.cost(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_resolve_including_dated_and_prefixed_names() {
        assert!(rates_for("gpt-5.6-sol").is_some());
        assert!(
            rates_for("openai/gpt-5.6-sol").is_some(),
            "route prefix is stripped"
        );
        assert!(
            rates_for("claude-sonnet-4-5-20250929").is_some(),
            "release stamp is stripped"
        );
        assert!(
            rates_for("gpt-5.6-sol-high").is_some(),
            "variant suffix falls back"
        );
        assert_eq!(rates_for("totally-made-up-model"), None);
    }

    #[test]
    fn prefix_match_prefers_the_longest_key() {
        // `gpt-5.1-codex-mini` must not resolve to `gpt-5.1` or `gpt-5.1-codex`.
        let mini = rates_for("gpt-5.1-codex-mini-2026").unwrap();
        assert_eq!(mini.input, 2.5e-7);
    }

    #[test]
    fn codex_cost_splits_cached_and_uncached_input() {
        // gpt-5.6-sol: input 5e-6, cache read 5e-7, cache write 6.25e-6, output 3e-5.
        let cost = cost_for(
            "gpt-5.6-sol",
            RequestTokens {
                uncached_input: 1_000,
                cache_read: 10_000,
                cache_write: 100,
                output: 500,
            },
        )
        .unwrap();
        let expected = 1_000.0 * 5e-6 + 10_000.0 * 5e-7 + 100.0 * 6.25e-6 + 500.0 * 3e-5;
        assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");
    }

    #[test]
    fn missing_cache_rates_fall_back_to_the_input_rate() {
        // gpt-5-pro has no cache read rate: cached input is billed as input.
        let rates = rates_for("gpt-5-pro").unwrap();
        assert_eq!(rates.cache_read, None);
        let cost = rates.cost(RequestTokens {
            uncached_input: 0,
            cache_read: 1_000,
            cache_write: 0,
            output: 0,
        });
        assert!((cost - 1_000.0 * 1.5e-5).abs() < 1e-12);
    }

    #[test]
    fn long_context_requests_use_the_above_threshold_tier() {
        let rates = rates_for("gpt-5.6-sol").unwrap();
        assert_eq!(rates.threshold_tokens, Some(272_000));

        let below = rates.cost(RequestTokens {
            uncached_input: 100_000,
            output: 1_000,
            ..RequestTokens::default()
        });
        let above = rates.cost(RequestTokens {
            uncached_input: 300_000,
            output: 1_000,
            ..RequestTokens::default()
        });
        // 3x the input tokens but 6x the input cost, plus the pricier output rate.
        let below_input = 100_000.0 * 5e-6;
        let above_input = 300_000.0 * 1e-5;
        assert!((below - (below_input + 1_000.0 * 3e-5)).abs() < 1e-9);
        assert!((above - (above_input + 1_000.0 * 4.5e-5)).abs() < 1e-9);
    }

    #[test]
    fn claude_cache_creation_has_its_own_rate() {
        let rates = rates_for("claude-opus-4-6").unwrap();
        let cost = rates.cost(RequestTokens {
            uncached_input: 1_000,
            cache_read: 2_000,
            cache_write: 3_000,
            output: 400,
        });
        let expected = 1_000.0 * 5e-6 + 2_000.0 * 5e-7 + 3_000.0 * 6.25e-6 + 400.0 * 2.5e-5;
        assert!((cost - expected).abs() < 1e-12);
    }

    #[test]
    fn research_preview_models_are_free_and_labelled() {
        let rates = rates_for("gpt-5.3-codex-spark").unwrap();
        assert_eq!(rates.label, Some("Research Preview"));
        assert_eq!(
            rates.cost(RequestTokens {
                uncached_input: 1_000_000,
                output: 1_000_000,
                ..RequestTokens::default()
            }),
            0.0
        );
    }

    #[test]
    fn normalization_is_case_and_whitespace_insensitive() {
        assert_eq!(normalize("  GPT-5.6-Sol  "), "gpt-5.6-sol");
        assert_eq!(
            normalize("anthropic/claude-opus-4-6-20260205"),
            "claude-opus-4-6"
        );
    }
}
