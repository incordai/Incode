//! A ceiling that actually stops a run.
//!
//! Spend was measured and never bounded: the usage tracker counted every token
//! and nothing ever said "no". That is fine while a person is watching each
//! turn, and unacceptable the moment the harness is allowed to work a queue on
//! its own — an unattended loop with no ceiling is a bill with no upper bound.
//!
//! # Why tokens rather than money
//!
//! The obvious ceiling is a dollar figure, and it would be a fabrication here.
//! Cost estimation matches model names against a three-tier table and prices
//! anything it does not recognise at the default tier — so with a fleet of
//! arbitrary models from the provider registry, most spend would be costed at a
//! rate nobody chose. A limit built on that reads as precise and is not.
//!
//! Tokens are *measured*. They are what the provider counts and what the bill is
//! computed from, so a token ceiling bounds the real quantity rather than a
//! guess about it. A currency ceiling becomes honest the day the rate table
//! covers every model the fleet can reach; until then this is the truthful unit.
//!
//! # The rule this module exists to obey
//!
//! **Bound the action, never the measurement.** The ceiling stops *requests*.
//! What was spent is always counted in full, including the request that crossed
//! the line and every subagent's attempts — a total that stops rising because a
//! cap was hit would hide exactly the overrun the cap exists to reveal.

use crate::runtime::usage::TokenUsage;

/// Environment variable naming the ceiling, in total tokens.
pub const BUDGET_ENV: &str = "INCORD_BUDGET_TOKENS";

/// A token ceiling for a run, and what has been spent against it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Budget {
    /// `None` means no ceiling — the behaviour before this existed.
    ceiling: Option<u64>,
    spent: u64,
}

impl Budget {
    /// Read the ceiling from the environment. Absent or unparseable means none.
    ///
    /// A malformed value is deliberately *not* treated as zero: an operator who
    /// typos a number should get the previous behaviour, not a harness that
    /// refuses to make a single request and looks broken.
    #[must_use]
    pub fn from_env() -> Self {
        let ceiling = std::env::var(BUDGET_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|&c| c > 0);
        Self { ceiling, spent: 0 }
    }

    #[must_use]
    pub fn with_ceiling(ceiling: u64) -> Self {
        Self {
            ceiling: Some(ceiling),
            spent: 0,
        }
    }

    /// Count what a request cost.
    ///
    /// Saturating rather than wrapping: a total that silently restarts near zero
    /// after an enormous run would report a spent budget as a fresh one.
    pub fn record(&mut self, usage: TokenUsage) {
        self.spent = self.spent.saturating_add(u64::from(usage.total_tokens()));
    }

    #[must_use]
    pub fn spent(&self) -> u64 {
        self.spent
    }

    #[must_use]
    pub fn ceiling(&self) -> Option<u64> {
        self.ceiling
    }

    #[must_use]
    pub fn has_ceiling(&self) -> bool {
        self.ceiling.is_some()
    }

    /// Tokens left, or `None` when there is no ceiling.
    #[must_use]
    pub fn remaining(&self) -> Option<u64> {
        self.ceiling.map(|c| c.saturating_sub(self.spent))
    }

    /// Whether the ceiling has been reached.
    ///
    /// Asked *before* a request rather than after, so the stop lands before the
    /// money is gone rather than just after it.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        match self.ceiling {
            Some(ceiling) => self.spent >= ceiling,
            None => false,
        }
    }

    /// What to tell the operator when a run stops on its ceiling.
    #[must_use]
    pub fn exhausted_message(&self) -> String {
        match self.ceiling {
            Some(ceiling) => format!(
                "Stopped: the token budget is spent ({} of {} used). \
                 Raise {BUDGET_ENV} or start a new run.",
                self.spent, ceiling
            ),
            None => "Stopped: the token budget is spent.".to_string(),
        }
    }

    /// A line for the operator, so a ceiling is visible before it bites.
    #[must_use]
    pub fn summary_line(&self) -> Option<String> {
        self.ceiling
            .map(|ceiling| format!("Budget: {} tokens for this run.", ceiling))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(total: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: total,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    /// No ceiling must behave exactly as before this module existed.
    #[test]
    fn without_a_ceiling_nothing_is_ever_exhausted() {
        let mut budget = Budget::default();
        budget.record(usage(1_000_000));
        assert!(!budget.is_exhausted());
        assert_eq!(budget.remaining(), None);
        assert_eq!(budget.summary_line(), None);
        assert_eq!(budget.spent(), 1_000_000);
    }

    #[test]
    fn a_ceiling_is_reached_when_spend_meets_it() {
        let mut budget = Budget::with_ceiling(100);
        budget.record(usage(99));
        assert!(!budget.is_exhausted(), "99 of 100 is not exhausted");
        assert_eq!(budget.remaining(), Some(1));

        budget.record(usage(1));
        assert!(budget.is_exhausted(), "reaching the ceiling exhausts it");
        assert_eq!(budget.remaining(), Some(0));
    }

    /// The cap bounds requests, not the counter. A total that stopped rising at
    /// the ceiling would hide how far past it a run actually went — which is
    /// precisely the number an operator needs after an overrun.
    #[test]
    fn spend_keeps_being_counted_past_the_ceiling() {
        let mut budget = Budget::with_ceiling(100);
        budget.record(usage(250));
        assert_eq!(
            budget.spent(),
            250,
            "the measurement must not be clamped to the cap"
        );
        assert_eq!(budget.remaining(), Some(0), "but nothing is left to spend");
        assert!(budget.is_exhausted());
    }

    /// A run long enough to overflow would otherwise report a spent budget as a
    /// fresh one.
    #[test]
    fn an_enormous_total_saturates_rather_than_wrapping_to_zero() {
        let mut budget = Budget::with_ceiling(10);
        budget.spent = u64::MAX - 1;
        budget.record(usage(1_000));
        assert_eq!(budget.spent(), u64::MAX);
        assert!(budget.is_exhausted());
    }

    #[test]
    fn the_ceiling_is_read_from_the_environment() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(BUDGET_ENV, "5000");
        let budget = Budget::from_env();
        std::env::remove_var(BUDGET_ENV);

        assert_eq!(budget.ceiling(), Some(5000));
        assert!(budget.summary_line().is_some());
    }

    /// A typo must not produce a harness that refuses every request and looks
    /// broken. It falls back to no ceiling, which is the previous behaviour.
    #[test]
    fn a_malformed_or_zero_ceiling_means_no_ceiling_rather_than_zero() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for bad in ["not-a-number", "0", "", "-5"] {
            std::env::set_var(BUDGET_ENV, bad);
            let budget = Budget::from_env();
            assert!(
                !budget.has_ceiling(),
                "`{bad}` must not become a ceiling; got {:?}",
                budget.ceiling()
            );
            assert!(!budget.is_exhausted());
        }
        std::env::remove_var(BUDGET_ENV);
    }

    #[test]
    fn the_stop_message_says_what_was_spent_and_how_to_continue() {
        let mut budget = Budget::with_ceiling(100);
        budget.record(usage(120));
        let message = budget.exhausted_message();
        assert!(message.contains("120"), "got: {message}");
        assert!(message.contains("100"), "got: {message}");
        assert!(message.contains(BUDGET_ENV), "got: {message}");
    }
}
