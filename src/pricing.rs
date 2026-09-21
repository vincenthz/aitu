//! Model prices, in USD per million tokens.
//!
//! Built-in rates are a convenience, not an authority: a model released after
//! this binary was built will not be in the table. Rather than silently
//! pricing it at zero, an unknown model is tracked as *unpriced* and reported
//! as such, and can be filled in from the config file.

use serde::Deserialize;
use std::collections::BTreeMap;

use crate::usage::{Speed, Tokens};

const PER_MILLION: f64 = 1_000_000.0;

/// Anthropic's standard cache multipliers, relative to the input rate.
const CACHE_WRITE_5M: f64 = 1.25;
const CACHE_WRITE_1H: f64 = 2.0;
const CACHE_READ: f64 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

impl Price {
    /// Rates derived from the input price with Anthropic's standard cache
    /// multipliers.
    const fn anthropic(input: f64, output: f64) -> Self {
        Self {
            input,
            cache_write_5m: input * CACHE_WRITE_5M,
            cache_write_1h: input * CACHE_WRITE_1H,
            cache_read: input * CACHE_READ,
            output,
        }
    }

    /// As `anthropic`, but with a cache read rate that is not 0.1x input.
    const fn anthropic_read(input: f64, output: f64, cache_read: f64) -> Self {
        Self {
            cache_read,
            ..Self::anthropic(input, output)
        }
    }

    /// Older OpenAI models bill cache writes at the input rate. Models with
    /// a cache-write premium override those fields in the built-in table.
    const fn openai(input: f64, cache_read: f64, output: f64) -> Self {
        Self {
            input,
            cache_write_5m: input,
            cache_write_1h: input,
            cache_read,
            output,
        }
    }

    pub fn cost(&self, tokens: &Tokens) -> f64 {
        (tokens.input as f64 * self.input
            + tokens.cache_write_5m as f64 * self.cache_write_5m
            + tokens.cache_write_1h as f64 * self.cache_write_1h
            + tokens.cache_read as f64 * self.cache_read
            + tokens.output as f64 * self.output)
            / PER_MILLION
    }
}

/// A price entry from the config file. Only `input` and `output` are required;
/// the cache rates default to Anthropic's multipliers.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceOverride {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: Option<f64>,
}

impl PriceOverride {
    fn resolve(&self) -> Price {
        Price {
            input: self.input,
            cache_write_5m: self.cache_write_5m.unwrap_or(self.input * CACHE_WRITE_5M),
            cache_write_1h: self.cache_write_1h.unwrap_or(self.input * CACHE_WRITE_1H),
            cache_read: self.cache_read.unwrap_or(self.input * CACHE_READ),
            output: self.output,
        }
    }
}

#[derive(Debug, Default)]
pub struct Prices {
    overrides: BTreeMap<String, Price>,
}

impl Prices {
    pub fn new(overrides: &[(String, PriceOverride)]) -> Self {
        Self {
            overrides: overrides
                .iter()
                .map(|(model, price)| (normalize(model), price.resolve()))
                .collect(),
        }
    }

    pub fn override_count(&self) -> usize {
        self.overrides.len()
    }

    /// `None` means "no price known" - never assume zero.
    pub fn lookup(&self, model: &str, speed: Speed) -> Option<Price> {
        let key = normalize(model);
        if let Some(price) = self.overrides.get(&key) {
            return Some(*price);
        }
        builtin(&key, speed)
    }

    /// Price each call before aggregation: Astra's long-context tier applies
    /// to the entire request when its prompt (including cache) exceeds 272K.
    /// Config overrides remain fixed rates and replace all built-in pricing.
    pub fn cost(&self, model: &str, speed: Speed, tokens: &Tokens) -> Option<f64> {
        let key = normalize(model);
        let mut price = self.lookup(&key, speed)?;
        if key == "gpt-6-astra"
            && !self.overrides.contains_key(&key)
            && tokens.billed_input() + tokens.cache_read > 272_000
        {
            price.input *= 2.0;
            price.cache_read *= 2.0;
            price.cache_write_5m *= 2.0;
            price.cache_write_1h *= 2.0;
            price.output *= 1.5;
        }
        Some(price.cost(tokens))
    }
}

/// Reduce a logged model string to a lookup key: drop any provider prefix
/// (`openai/gpt-6`), lowercase, and drop a trailing date snapshot
/// (`claude-haiku-4-5-20251001`).
fn normalize(model: &str) -> String {
    let bare = model.rsplit('/').next().unwrap_or(model).trim();
    let lower = bare.to_ascii_lowercase().replace('_', "-");
    strip_snapshot(&lower)
}

fn strip_snapshot(model: &str) -> String {
    match model.rsplit_once('-') {
        Some((head, tail))
            if tail.len() == 8
                && tail.starts_with("20")
                && tail.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head.to_string()
        }
        _ => model.to_string(),
    }
}

/// Matching is exact against the key, never a substring test: a substring
/// match on a name like `opus-4` silently captures every future `opus-4x`.
fn builtin(model: &str, speed: Speed) -> Option<Price> {
    // Claude Opus 5 fast mode is billed at a premium over standard.
    if speed == Speed::Fast {
        return match model {
            "claude-opus-5" | "claude-opus-4-8" => Some(Price::anthropic(10.00, 50.00)),
            _ => builtin(model, Speed::Standard),
        };
    }

    let price = match model {
        // Anthropic - https://claude.com/pricing (input/output; cache rates
        // are 1.25x / 2x / 0.1x of input unless noted).
        // Verified 2026-09-21:
        // https://platform.claude.com/docs/en/models/fable-5-1/overview
        "claude-fable-5-1" | "claude-mythos-5-1" => Price::anthropic_read(10.00, 50.00, 0.25),
        "claude-fable-5" | "claude-mythos-5" => Price::anthropic(10.00, 50.00),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => Price::anthropic(5.00, 25.00),
        "claude-opus-4-1" | "claude-opus-4-0" | "claude-opus-4" => Price::anthropic(15.00, 75.00),
        "claude-sonnet-5" => Price::anthropic(2.00, 10.00),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-sonnet-4-0" | "claude-sonnet-4" => {
            Price::anthropic(3.00, 15.00)
        }
        "claude-haiku-4-5" => Price::anthropic(1.00, 5.00),

        // Verified 2026-09-21 (long-context multipliers applied in Prices::cost):
        // https://developers.openai.com/api/docs/models/gpt-6-astra
        "gpt-6-astra" => Price {
            cache_write_5m: 12.50,
            cache_write_1h: 12.50,
            ..Price::openai(10.00, 1.00, 50.00)
        },

        // Older OpenAI rates are carried over from prior tooling and are NOT
        // verified against OpenAI's published pricing - override them in the
        // config file if the dollar figures matter to you.
        "gpt-5.5" => Price::openai(5.00, 0.50, 30.00),
        "gpt-5.4" => Price::openai(2.50, 0.25, 15.00),
        "gpt-5.4-mini" => Price::openai(0.75, 0.075, 4.50),
        "gpt-5.4-nano" => Price::openai(0.20, 0.02, 1.25),
        "gpt-5.3-codex" | "gpt-5.2" => Price::openai(1.75, 0.175, 14.00),
        "gpt-5.1" => Price::openai(1.25, 0.125, 10.00),
        "gpt-5-mini" => Price::openai(0.25, 0.025, 2.00),

        _ => return None,
    };
    Some(price)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> Tokens {
        Tokens {
            input: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            output: 1_000_000,
        }
    }

    #[test]
    fn prices_the_two_cache_write_ttls_differently() {
        let price = Prices::default()
            .lookup("claude-opus-5", Speed::Standard)
            .unwrap();

        assert_eq!(price.cache_write_5m, 6.25);
        assert_eq!(price.cache_write_1h, 10.00);
        assert_eq!(price.cache_read, 0.50);
        // 5.00 + 0.50 + 6.25 + 10.00 + 25.00
        assert_eq!(format!("{:.2}", price.cost(&tokens())), "46.75");
    }

    #[test]
    fn current_models_are_priced() {
        let prices = Prices::default();
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "claude-fable-5-1",
            "gpt-6-astra",
        ] {
            assert!(
                prices.lookup(model, Speed::Standard).is_some(),
                "{model} is unpriced"
            );
        }
    }

    #[test]
    fn fable_5_1_has_a_cheaper_cache_read_than_the_standard_multiplier() {
        let price = Prices::default()
            .lookup("claude-fable-5-1", Speed::Standard)
            .unwrap();

        assert_eq!(price.cache_read, 0.25);
        assert_eq!(price.input, 10.00);
        assert_eq!(price.cache_write_5m, 12.50);
        assert_eq!(price.cache_write_1h, 20.00);
        assert_eq!(price.output, 50.00);
        assert_eq!(price.cost(&tokens()), 92.75);
    }

    #[test]
    fn astra_prices_uncached_cached_and_cache_write_tokens() {
        let tokens = Tokens {
            input: 10_000,
            cache_read: 100_000,
            cache_write_5m: 20_000,
            output: 1_000,
            ..Tokens::default()
        };
        let cost = Prices::default()
            .cost("openai/gpt-6-astra", Speed::Standard, &tokens)
            .unwrap();

        // $0.10 input + $0.10 read + $0.25 write + $0.05 output.
        assert!((cost - 0.50).abs() < 1e-10);
    }

    #[test]
    fn astra_long_context_tier_counts_all_prompt_buckets_but_not_output() {
        let prices = Prices::default();
        let mut tokens = Tokens {
            input: 2_000,
            cache_read: 250_000,
            cache_write_5m: 20_000,
            output: 10_000,
            ..Tokens::default()
        };
        let standard = prices
            .cost("gpt-6-astra", Speed::Standard, &tokens)
            .unwrap();
        assert!((standard - 1.02).abs() < 1e-10);

        tokens.cache_read += 1;
        let long = prices
            .cost("openai/gpt-6-astra", Speed::Standard, &tokens)
            .unwrap();
        assert!((long - 1.790002).abs() < 1e-10);
    }

    #[test]
    fn fast_mode_costs_more_than_standard() {
        let prices = Prices::default();
        let standard = prices.lookup("claude-opus-5", Speed::Standard).unwrap();
        let fast = prices.lookup("claude-opus-5", Speed::Fast).unwrap();

        assert_eq!(standard.input, 5.00);
        assert_eq!(fast.input, 10.00);
        assert_eq!(fast.output, 50.00);
    }

    #[test]
    fn fast_mode_falls_back_to_standard_for_models_without_a_fast_rate() {
        let prices = Prices::default();

        assert_eq!(
            prices.lookup("claude-sonnet-5", Speed::Fast),
            prices.lookup("claude-sonnet-5", Speed::Standard),
        );
    }

    #[test]
    fn unknown_models_are_unpriced_rather_than_free() {
        assert_eq!(
            Prices::default().lookup("some-model-from-the-future", Speed::Standard),
            None
        );
    }

    #[test]
    fn normalizes_provider_prefix_and_date_snapshot() {
        assert_eq!(normalize("openai/gpt-5.4"), "gpt-5.4");
        assert_eq!(normalize("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(normalize("  Claude-Opus-5 "), "claude-opus-5");
        // Not a date, so not stripped.
        assert_eq!(normalize("gpt-5-mini"), "gpt-5-mini");
    }

    #[test]
    fn does_not_match_a_longer_model_name_by_prefix() {
        let prices = Prices::default();

        assert!(prices.lookup("claude-opus-4", Speed::Standard).is_some());
        assert!(prices.lookup("claude-opus-4-9", Speed::Standard).is_none());
    }

    #[test]
    fn config_overrides_beat_the_builtin_table() {
        let prices = Prices::new(&[(
            String::from("claude-opus-5"),
            PriceOverride {
                input: 1.0,
                output: 2.0,
                cache_write_5m: None,
                cache_write_1h: None,
                cache_read: None,
            },
        )]);
        let price = prices.lookup("claude-opus-5", Speed::Standard).unwrap();

        assert_eq!(price.input, 1.0);
        assert_eq!(price.cache_write_5m, 1.25);
        assert_eq!(price.cache_read, 0.1);
    }

    #[test]
    fn overrides_can_price_a_model_the_table_has_never_heard_of() {
        let prices = Prices::new(&[(
            String::from("openai/some-new-model"),
            PriceOverride {
                input: 3.0,
                output: 12.0,
                cache_write_5m: Some(3.0),
                cache_write_1h: Some(3.0),
                cache_read: Some(0.3),
            },
        )]);

        assert!(prices.lookup("some-new-model", Speed::Standard).is_some());
    }

    #[test]
    fn astra_override_replaces_long_context_pricing_too() {
        let prices = Prices::new(&[(
            String::from("openai/gpt-6-astra"),
            PriceOverride {
                input: 3.0,
                output: 12.0,
                cache_write_5m: Some(3.0),
                cache_write_1h: Some(3.0),
                cache_read: Some(0.3),
            },
        )]);

        assert_eq!(
            prices.cost("gpt-6-astra", Speed::Standard, &tokens()),
            Some(21.3)
        );
    }
}
