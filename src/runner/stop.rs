//! Why a run ended, and what it did on the way.
//!
//! A run that stops without saying why is indistinguishable from one that
//! crashed, so every exit carries a [`StopReason`] with a message an operator
//! can act on.

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Nothing left to pick up **and every roadmap goal proved reached**. The
    /// success case, and the only one that is not a problem.
    LedgerEmpty,
    /// Every task ran, and the thing the owner asked for still is not there.
    ///
    /// Distinct from [`Self::LedgerEmpty`] for the same reason
    /// [`Self::NothingRunnable`] is: "every task finished" and "the goal is
    /// reached" are different facts. A plan can be completed in full and still
    /// not deliver the item it was written for — that is what a wrong plan
    /// looks like from the outside, and it is invisible if the queue draining
    /// is reported as success.
    ///
    /// Each entry is one roadmap item and why it is not proven.
    GoalsUnmet { unreached: Vec<String> },
    /// Every task ran, and the goals could not be READ, so nothing was proved.
    ///
    /// Kept apart from [`Self::GoalsUnmet`] because the two send an operator to
    /// opposite places: one means the code does not do what was asked, the
    /// other means the service holding the roadmap is down. Collapsing them
    /// would have people debugging code to fix a stopped process.
    ///
    /// It is also NOT [`Self::LedgerEmpty`]. "I could not check" is not "it
    /// passed", and the moment that distinction is dropped the gate certifies
    /// silently in exactly the conditions where nobody is watching.
    GoalsUnchecked { why: String },
    /// The token ceiling was reached.
    ///
    /// Carries the CEILING as well as the spend, because "24,000 used" is not
    /// something an operator can act on — they need to know what it was
    /// measured against and which variable raises it. That text already
    /// existed, in [`crate::budget::Budget::exhausted_message`], and nothing
    /// called it while this variant printed a worse version of the same
    /// sentence.
    BudgetSpent {
        spent: u64,
        /// `None` when the run had no ceiling, which is a different sentence.
        ceiling: Option<u64>,
    },
    /// A task used its attempts. The run halts rather than skipping it: an
    /// unattended loop that steps over its failures reports success while
    /// leaving the work undone.
    TaskFailedTwice { id: String, why: String },
    /// Something needs a person.
    NeedsOwner { id: String, question: String },
    /// The queue drained, but work is still open and none of it can be picked
    /// up — every remaining task is waiting on somebody or something.
    ///
    /// Distinct from [`Self::LedgerEmpty`] on purpose. "Nothing left to run" and
    /// "everything is finished" are different facts, and reporting the second
    /// when the first is true is how held work disappears quietly.
    ///
    /// `dead` is the subset that can never become ready at all, with what holds
    /// each one — a task waiting on an abandoned dependency is not waiting, it
    /// is stuck, and the two need different words.
    /// Work is open that is neither ready nor waiting on anyone.
    ///
    /// # The hole this closes
    ///
    /// `next_ready` finds `Ready`; `waiting_ids` finds `Blocked` and
    /// `Pending`. `Running` is in neither — and nothing reclaims it. A run
    /// killed mid-task leaves that state on disk, and the next run found no
    /// ready task, no waiting task, went on to the goals check and could
    /// report `LedgerEmpty`: *every task is finished*, with one still marked
    /// in progress.
    ///
    /// Not folded into [`Self::NothingRunnable`], whose message says the
    /// tasks are "waiting on somebody". These are waiting on nobody. A person
    /// told to go and find who is holding it would be looking for someone who
    /// does not exist.
    Stranded { ids: Vec<String> },
    NothingRunnable {
        waiting: Vec<String>,
        dead: Vec<(String, Vec<String>)>,
    },
    /// The user asked it to stop.
    Cancelled,
}

impl StopReason {
    /// What to tell the operator, in words that say what to do next.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::LedgerEmpty => {
                "Every task in the ledger is finished, and every roadmap goal is proved."
                    .to_string()
            }
            Self::GoalsUnmet { unreached } => {
                let mut out = format!(
                    "Stopped: every task ran, but {} roadmap item(s) are NOT proved reached. \
                     The plan finished; the goal did not.",
                    unreached.len()
                );
                for line in unreached {
                    out.push_str(&format!("\n  {line}"));
                }
                out
            }
            Self::GoalsUnchecked { why } => format!(
                "Every task in the ledger is finished. The roadmap goals were NOT checked, so \
                 nothing here proves the work is done: {why}"
            ),
            // The budget writes its own message. Two places phrasing the same
            // stop is how one of them ends up telling an operator less than
            // the other — which is what happened: this arm said how much was
            // used, and never what the limit was or how to raise it.
            Self::BudgetSpent { spent, ceiling } => {
                crate::budget::Budget::exhausted_text(*spent, *ceiling)
            }
            Self::TaskFailedTwice { id, why } => {
                format!("Stopped: `{id}` failed twice and was set aside. Last failure: {why}")
            }
            Self::NeedsOwner { id, question } => {
                format!("Stopped: `{id}` needs a decision. {question}")
            }
            Self::NothingRunnable { waiting, dead } => {
                let mut out = format!(
                    "Stopped: nothing left to run, but {} task(s) are still waiting on somebody: {}. They are NOT finished.",
                    waiting.len(),
                    waiting.join(", ")
                );
                // Named separately because these are not waiting on anyone: they
                // wait on something that will never arrive, and a person reading
                // "waiting" would leave them there indefinitely.
                for (id, blockers) in dead {
                    out.push_str(&format!(
                        "\n  `{id}` can NEVER become ready — it depends on {}.",
                        blockers.join(", ")
                    ));
                }
                out
            }
            Self::Stranded { ids } => format!(
                "Stopped: {} task(s) are marked in progress with nothing running them: {}. \
                 They are NOT finished. A run that was killed or crashed mid-task leaves \
                 this behind, and nothing reclaims it — set each one back to `ready` in \
                 `.incode/ledger.json`, or cancel it, then run again.",
                ids.len(),
                ids.join(", ")
            ),
            Self::Cancelled => "Stopped: cancelled.".to_string(),
        }
    }
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub stop: StopReason,
    pub completed: Vec<String>,
    pub attempted: usize,
    /// Tokens spent, per agent, highest first.
    ///
    /// The ceiling answers "is there budget left". An operator running several
    /// models across several subscriptions is asking a different question —
    /// WHICH one is spending it — and a single total cannot answer that. Kept
    /// on the outcome so the answer survives the run rather than scrolling
    /// past in a trace line.
    pub spend_by_agent: Vec<(String, u64)>,
}

impl RunOutcome {
    /// What each agent cost, in the shape a person reads.
    ///
    /// `None` when nothing was spent — a run that did no work should not
    /// print an empty table.
    #[must_use]
    pub fn spend_report(&self) -> Option<String> {
        if self.spend_by_agent.is_empty() {
            return None;
        }
        let total: u64 = self.spend_by_agent.iter().map(|(_, n)| n).sum();
        let mut out = format!(
            "Tokens spent: {total} across {} agent(s)",
            self.spend_by_agent.len()
        );
        for (agent, spent) in &self.spend_by_agent {
            let share = (spent * 100).checked_div(total).unwrap_or(0);
            out.push_str(&format!("\n  {agent}  {spent} ({share}%)"));
        }
        Some(out)
    }
}

/// Spend per agent, heaviest first.
///
/// Ordered by cost rather than by name: the question this answers is "which
/// one is burning the budget", and an alphabetical list makes the reader do
/// the sorting.
///
/// Called by `LedgerLoop::finish`, on every exit of the loop.
pub(super) fn ranked(by_agent: &std::collections::BTreeMap<String, u64>) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = by_agent
        .iter()
        .filter(|(_, spent)| **spent > 0)
        .map(|(agent, spent)| (agent.clone(), *spent))
        .collect();
    // Name breaks a tie, so the order is stable between runs.
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **HEAVIEST FIRST, A TIE BY NAME, AND NOBODY WHO SPENT NOTHING.** The
    /// table answers "which one is burning the budget": an agent that spent
    /// nothing is not an answer, and an order that changed between runs would
    /// make two reports of the same run disagree.
    #[test]
    fn spend_is_ranked_heaviest_first_with_ties_broken_by_name() {
        let by_agent = std::collections::BTreeMap::from([
            ("b-agent".to_string(), 500u64),
            ("idle".to_string(), 0),
            ("a-agent".to_string(), 500),
            ("heavy".to_string(), 9_000),
        ]);
        assert_eq!(
            ranked(&by_agent),
            vec![
                ("heavy".to_string(), 9_000),
                ("a-agent".to_string(), 500),
                ("b-agent".to_string(), 500),
            ]
        );
    }
}
