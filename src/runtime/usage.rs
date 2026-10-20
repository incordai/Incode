//! Token usage tracking and cost estimation.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TokenUsage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub cache_read_input_tokens: u32,
}

impl TokenUsage {
    /// Fold another report for the SAME response into this one.
    ///
    /// A streaming response reports usage twice and neither report is
    /// complete: `message_start` carries the input and cache counts with
    /// output still at zero, and `message_delta` carries the final cumulative
    /// output with input absent. Keeping only the most recent one therefore
    /// threw away the input count and under-reported both tokens and cost.
    ///
    /// Both counts are cumulative for the response, so the larger value is the
    /// current one for each field — which also makes this independent of the
    /// order the events arrive in.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.max(other.input_tokens),
            output_tokens: self.output_tokens.max(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .max(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .max(other.cache_read_input_tokens),
        }
    }

    #[must_use]
    pub fn total_tokens(self) -> u32 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }

    /// Dollars this turn would cost, or `None` when no rate is known for the
    /// model.
    ///
    /// # Why an unknown model gets NO figure
    ///
    /// This used to fall through to a mid-tier default for anything it did not
    /// recognise. That was invisible while one vendor's models were the only
    /// ones the harness could reach; with a second agent it means a
    /// `gemini-…`-family turn is quietly priced at another vendor's rate and
    /// reported as a dollar amount. A made-up number in a cost line is worse
    /// than no number: the operator cannot tell it is made up.
    ///
    /// The rates are one vendor's published per-million prices, in
    /// [`PRICES`], matched by name. Anything else is unpriced until an
    /// operator supplies a rate — which does not exist yet, and is the honest
    /// next step rather than a table of guesses for every vendor.
    ///
    /// Cache writes and cache reads are priced at their own rates: on a
    /// resumed agent session they are most of the input, and leaving them out
    /// reported a cache-heavy turn at a fraction of its bill.
    #[must_use]
    pub fn estimate_cost_usd(self, model: Option<&str>) -> Option<f64> {
        let price = price_for(model?)?;
        let per = |tokens: u32, rate: f64| f64::from(tokens) / 1_000_000.0 * rate;
        Some(
            per(self.input_tokens, price.input)
                + per(self.output_tokens, price.output)
                + per(self.cache_creation_input_tokens, price.cache_write)
                + per(self.cache_read_input_tokens, price.cache_read),
        )
    }

    /// Add another, separate spend to this one (not the same response — see
    /// [`Self::merge`] for that).
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }

    /// Human-readable summary line.
    #[must_use]
    pub fn summary_line(self, model: Option<&str>) -> String {
        // An unpriced model says so rather than showing a figure nobody can
        // check. See [`Self::estimate_cost_usd`].
        let cost = match self.estimate_cost_usd(model) {
            Some(usd) => format!("${usd:.4}"),
            None => "unpriced".to_string(),
        };
        format!(
            "tokens: {} total (in={} out={} cache_w={} cache_r={}) est={cost}",
            self.total_tokens(),
            self.input_tokens,
            self.output_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
        )
    }
}

// ---------------------------------------------------------------------------
// Prices
// ---------------------------------------------------------------------------

/// Dollars per million tokens for one model family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    /// Writing the prompt cache (the 5-minute tier).
    pub cache_write: f64,
    /// Reading from the prompt cache.
    pub cache_read: f64,
}

const fn price(input: f64, output: f64, cache_write: f64, cache_read: f64) -> ModelPrice {
    ModelPrice {
        input,
        output,
        cache_write,
        cache_read,
    }
}

/// THE PRICE TABLE. Update it here; nothing else holds a rate.
///
/// Matched as a substring of the model id, lower-cased with `.` and `_` read
/// as `-` (so `claude-opus-5-5`, `opus-5.5` and `us.anthropic.claude-opus-5-5`
/// all match `opus-5-5`). ORDERED MOST SPECIFIC FIRST: `opus-5` would
/// otherwise claim `opus-5-5`, and the bare family names are a last resort.
///
/// Anthropic first-party list prices per million tokens, as published
/// 2026-10. A bare alias (`opus`, `sonnet`, `haiku`) names whichever model
/// the agent resolves it to, which this cannot see — it is priced at the
/// family's older, higher rate so the estimate errs high rather than low.
pub const PRICES: &[(&str, ModelPrice)] = &[
    ("fable", price(10.0, 50.0, 12.5, 0.25)),
    ("mythos", price(10.0, 50.0, 12.5, 0.25)),
    ("opus-5-5", price(4.0, 20.0, 5.0, 0.20)),
    ("opus-5", price(5.0, 25.0, 6.25, 0.50)),
    ("opus-4-8", price(5.0, 25.0, 6.25, 0.50)),
    ("opus-4-7", price(5.0, 25.0, 6.25, 0.50)),
    ("opus-4-6", price(5.0, 25.0, 6.25, 0.50)),
    ("opus-4-5", price(5.0, 25.0, 6.25, 0.50)),
    ("opus", price(15.0, 75.0, 18.75, 1.50)),
    ("sonnet-5", price(2.0, 10.0, 2.5, 0.20)),
    ("sonnet", price(3.0, 15.0, 3.75, 0.30)),
    ("haiku-5", price(0.10, 0.50, 0.125, 0.01)),
    ("haiku", price(1.0, 5.0, 1.25, 0.10)),
];

/// The rate for a model this harness can price, or `None`.
#[must_use]
pub fn price_for(model: &str) -> Option<ModelPrice> {
    let id: String = model
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '.' || c == '_' { '-' } else { c })
        .collect();
    PRICES
        .iter()
        .find(|(needle, _)| id.contains(needle))
        .map(|(_, p)| *p)
}

/// `/cost`: tokens and estimated dollars, one block per model, then a total.
///
/// An unpriced model says so, and the total then says it covers only the
/// priced part — a sum that silently left one model out would read as whole.
#[must_use]
pub fn cost_report(by_model: &[(String, TokenUsage)]) -> Vec<String> {
    if by_model.is_empty() {
        return vec!["No tokens reported yet this session.".to_string()];
    }
    let mut lines = Vec::new();
    let mut total = 0.0;
    let mut unpriced = 0;
    for (model, used) in by_model {
        let cost = match used.estimate_cost_usd(Some(model)) {
            Some(usd) => {
                total += usd;
                format!("~${usd:.4}")
            }
            None => {
                unpriced += 1;
                "unpriced".to_string()
            }
        };
        lines.push(format!("{model}: {cost}"));
        lines.push(format!(
            "  input {}  output {}  cache write {}  cache read {}",
            used.input_tokens,
            used.output_tokens,
            used.cache_creation_input_tokens,
            used.cache_read_input_tokens
        ));
    }
    let note = if unpriced > 0 {
        format!(" ({unpriced} model(s) unpriced, not included)")
    } else {
        String::new()
    };
    lines.push(format!("Total: ~${total:.4}{note}"));
    lines.push(
        "Estimate from list prices (src/runtime/usage.rs PRICES); a subscription is billed differently."
            .to_string(),
    );
    lines
}

// ---------------------------------------------------------------------------
// UsageTracker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTracker {
    latest_turn: TokenUsage,
    cumulative: TokenUsage,
    turns: u32,
}

impl UsageTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, usage: TokenUsage) {
        self.latest_turn = usage;
        self.cumulative.input_tokens += usage.input_tokens;
        self.cumulative.output_tokens += usage.output_tokens;
        self.cumulative.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        self.cumulative.cache_read_input_tokens += usage.cache_read_input_tokens;
        self.turns += 1;
    }

    #[must_use]
    pub fn current_turn_usage(&self) -> TokenUsage {
        self.latest_turn
    }

    #[must_use]
    pub fn cumulative_usage(&self) -> TokenUsage {
        self.cumulative
    }

    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_cumulative_usage() {
        let mut tracker = UsageTracker::new();
        tracker.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 1,
        });
        tracker.record(TokenUsage {
            input_tokens: 20,
            output_tokens: 6,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 2,
        });

        assert_eq!(tracker.turns(), 2);
        assert_eq!(tracker.current_turn_usage().input_tokens, 20);
        assert_eq!(tracker.cumulative_usage().output_tokens, 10);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 48);
    }

    /// The two usage reports a streaming response actually produces, folded
    /// the way the runtime folds them.
    ///
    /// Keeping only the most recent dropped the input and cache counts
    /// entirely — a prompt-cached turn costing thousands of input tokens was
    /// billed as if it had none.
    #[test]
    fn the_two_halves_of_a_streamed_usage_report_combine() {
        // What `message_start` carries: input and cache, output not yet known.
        let from_start = TokenUsage {
            input_tokens: 2048,
            output_tokens: 1,
            cache_creation_input_tokens: 512,
            cache_read_input_tokens: 1024,
        };
        // What `message_delta` carries: the final output only.
        let from_delta = TokenUsage {
            input_tokens: 0,
            output_tokens: 347,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };

        let merged = from_start.merge(from_delta);

        assert_eq!(
            merged.input_tokens, 2048,
            "the input count from message_start must survive the later report"
        );
        assert_eq!(
            merged.output_tokens, 347,
            "the final output count must replace the placeholder"
        );
        assert_eq!(merged.cache_creation_input_tokens, 512);
        assert_eq!(merged.cache_read_input_tokens, 1024);
        assert_eq!(merged.total_tokens(), 2048 + 347 + 512 + 1024);

        // Order must not matter: events can be folded either way round.
        assert_eq!(from_delta.merge(from_start), merged);
    }

    /// Cost is what the user actually sees. With the input count lost, a
    /// cache-heavy turn was reported at a small fraction of its real price.
    #[test]
    fn dropping_the_input_report_would_understate_cost() {
        let from_start = TokenUsage {
            input_tokens: 100_000,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let from_delta = TokenUsage {
            input_tokens: 0,
            output_tokens: 500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };

        let merged_cost = from_start
            .merge(from_delta)
            .estimate_cost_usd(Some("opus"))
            .expect("this model has a known rate");
        let last_wins_cost = from_delta
            .estimate_cost_usd(Some("opus"))
            .expect("this model has a known rate");

        assert!(
            merged_cost > last_wins_cost * 10.0,
            "merging must recover the input cost; merged {merged_cost} vs {last_wins_cost}"
        );
    }

    /// **A model with no known rate gets NO figure, not another vendor's.**
    ///
    /// This fell through to a mid-tier default for anything unrecognised. With
    /// one vendor reachable that was invisible; with a second agent it prices a
    /// `gemini-…` turn at another vendor's rate and prints it as dollars. A
    /// number the operator cannot tell is invented is worse than no number.
    #[test]
    fn an_unpriced_model_is_reported_as_unpriced() {
        let used = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };

        assert_eq!(
            used.estimate_cost_usd(Some("gemini-3.1-pro-high")),
            None,
            "another vendor's model must not be priced at this one's rates"
        );
        assert_eq!(used.estimate_cost_usd(None), None, "no model, no figure");
        assert!(
            used.summary_line(Some("gemini-3.1-pro-high"))
                .contains("unpriced"),
            "the line must say it cannot price this, not show a made-up total: {}",
            used.summary_line(Some("gemini-3.1-pro-high"))
        );

        // Control: a model that IS priced still shows a figure, so the
        // assertions above are not passing because pricing broke entirely.
        assert!(used.estimate_cost_usd(Some("opus")).is_some());
        assert!(used.summary_line(Some("opus")).contains('$'));
    }

    /// Cache tokens are priced at their own rates, and the most specific
    /// family wins over its prefix.
    #[test]
    fn cache_tokens_are_priced_and_the_specific_model_wins() {
        let used = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
        };
        let opus55 = used.estimate_cost_usd(Some("claude-opus-5-5")).unwrap();
        assert!(
            (opus55 - (4.0 + 20.0 + 5.0 + 0.20)).abs() < 1e-9,
            "{opus55}"
        );
        let opus5 = used.estimate_cost_usd(Some("claude-opus-5")).unwrap();
        assert!((opus5 - (5.0 + 25.0 + 6.25 + 0.5)).abs() < 1e-9, "{opus5}");
        // Dots and provider prefixes are the same model.
        assert_eq!(
            price_for("us.anthropic.claude-sonnet-5.5"),
            price_for("claude-sonnet-5-5")
        );
        assert_eq!(price_for("claude-haiku-5-5").unwrap().input, 0.10);
        assert_eq!(price_for("claude-haiku-4-5").unwrap().input, 1.0);
    }

    #[test]
    fn the_cost_report_totals_priced_models_and_names_unpriced_ones() {
        let used = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let lines = cost_report(&[
            ("claude-sonnet-5-5".to_string(), used),
            ("gemini-3".to_string(), used),
        ]);
        let text = lines.join("\n");
        assert!(text.contains("claude-sonnet-5-5: ~$2.0000"), "{text}");
        assert!(text.contains("gemini-3: unpriced"), "{text}");
        assert!(
            text.contains("Total: ~$2.0000 (1 model(s) unpriced"),
            "{text}"
        );
        assert_eq!(cost_report(&[]).len(), 1);
    }

    #[test]
    fn adding_spends_sums_every_field() {
        let a = TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 4,
        };
        assert_eq!(a.plus(a).total_tokens(), 20);
    }
}
