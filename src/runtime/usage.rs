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
    /// The rates below are one vendor's published per-million prices and are
    /// matched by name. Anything else is unpriced until an operator supplies a
    /// rate — which does not exist yet, and is the honest next step rather than
    /// a table of guesses for every vendor.
    #[must_use]
    pub fn estimate_cost_usd(self, model: Option<&str>) -> Option<f64> {
        let (input_rate, output_rate) = Self::rate_for(model?)?;
        let input_cost = f64::from(self.input_tokens) / 1_000_000.0 * input_rate;
        let output_cost = f64::from(self.output_tokens) / 1_000_000.0 * output_rate;
        Some(input_cost + output_cost)
    }

    /// The per-million (input, output) rate for a model this harness can price.
    fn rate_for(model: &str) -> Option<(f64, f64)> {
        let id = model.to_ascii_lowercase();
        // Ordered most specific first: several ids contain more than one of
        // these words, and the first match must be the model's own tier.
        for (needle, rates) in [
            ("haiku", (1.0, 5.0)),
            ("opus", (15.0, 75.0)),
            ("sonnet", (3.0, 15.0)),
        ] {
            if id.contains(needle) {
                return Some(rates);
            }
        }
        None
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
}
