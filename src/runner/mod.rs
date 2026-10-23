//! Working the ledger without being asked each time.
//!
//! Everything else in this crate happens inside one turn that a person started.
//! This is the loop across turns: take the next open task, get it done, prove
//! it, record what happened, repeat — until there is a stated reason to stop.
//!
//! # The one law
//!
//! **The runner decides what to do next. It never decides that something is
//! done.** Choosing the next task is scheduling, and a bad choice wastes a turn.
//! Declaring work complete is a claim about the world, and a wrong one is
//! absorbed silently into everything that follows. So the only edge into `Done`
//! runs through [`crate::verify`], and the worker's report — however confident,
//! from whichever model — is recorded as a claim beside the evidence, never in
//! place of it.
//!
//! # Stopping
//!
//! A run that stops without saying why is indistinguishable from one that
//! crashed, so every exit carries a [`StopReason`].
//!
//! # Layout
//!
//! - `ledger_loop` — the loop and the state it carries between rounds.
//! - `escalation` — a task nobody can check, put to the owner.
//! - `attempt` — a claimed task worked, then proved or failed by its check.
//! - `lanes` — several ready tasks at once, each in its own checkout.
//! - `goals` — the final testing, once the queue has drained.
//! - `seams` — the traits the loop works through, and their honest defaults.
//! - `briefing` — what an agent is told before it works a task.
//! - `stop` — why a run ended, and what it did.

use std::path::Path;

use crate::budget::Budget;
use crate::ledger::Ledger;

mod attempt;
mod briefing;
mod escalation;
mod goals;
mod lanes;
mod ledger_loop;
mod seams;
mod stop;

pub use briefing::{
    handoff_block, note_from, task_instruction, Handoff, MAX_HANDOFFS_SHOWN, MAX_NOTE_CHARS,
};
pub use escalation::MAX_ESCALATIONS;
pub use goals::{BriefGoals, GoalVerdict, NoGoals, RoadmapGoals};
pub use lanes::{LaneWorkers, NoLanes};
pub use seams::{
    MemoryJournal, MemoryOwner, NoJournal, NoOwner, NoReplan, OwnerChannel, Progress, Replanner,
    TaskWorker, Unwatched, WorkJournal, MAX_REPLANS,
};
pub use stop::{RunOutcome, StopReason};

/// Work the ledger until there is a reason to stop.
///
/// `cancelled` is polled between tasks rather than passed as a flag, so a Ctrl-C
/// lands at the next boundary instead of at the end of the queue.
pub fn run(
    ledger: &mut Ledger,
    worker: &mut dyn TaskWorker,
    workspace: &Path,
    budget: &mut Budget,
    cancelled: &dyn Fn() -> bool,
) -> RunOutcome {
    run_with_owner(ledger, worker, workspace, budget, cancelled, &mut NoOwner)
}

/// Work the ledger, asking a person when it reaches something a machine must not
/// settle.
///
/// The difference from [`run`] is one thing and it is the important one: a
/// question that used to halt the run now goes to the owner and comes back as a
/// verdict. What a verdict may do is bounded — it decides whether to keep trying,
/// never whether the work is correct. No answer on this channel can produce a
/// [`crate::verify::Verified`], so no answer can move a task to `Done`.
pub fn run_with_owner(
    ledger: &mut Ledger,
    worker: &mut dyn TaskWorker,
    workspace: &Path,
    budget: &mut Budget,
    cancelled: &dyn Fn() -> bool,
    owner: &mut dyn OwnerChannel,
) -> RunOutcome {
    run_with_journal(
        ledger,
        worker,
        workspace,
        budget,
        cancelled,
        owner,
        &mut NoJournal,
    )
}

/// Work the ledger, asking a person when needed, and write down who did what.
///
/// The journal is the only addition, and it is deliberately last and separate:
/// it cannot influence scheduling, cannot influence verification, and cannot
/// stop a run. It only records.
pub fn run_with_journal(
    ledger: &mut Ledger,
    worker: &mut dyn TaskWorker,
    workspace: &Path,
    budget: &mut Budget,
    cancelled: &dyn Fn() -> bool,
    owner: &mut dyn OwnerChannel,
    journal: &mut dyn WorkJournal,
) -> RunOutcome {
    run_watched(
        ledger,
        worker,
        workspace,
        budget,
        cancelled,
        owner,
        journal,
        &mut Unwatched,
        &mut NoReplan,
        // No roadmap source at this layer, and it reports that rather than
        // certifying — see [`NoGoals`]. The production path goes through
        // `run_watched` with [`BriefGoals`].
        &mut NoGoals,
        &NoLanes,
    )
}

/// Work the ledger with someone watching the count.
///
/// The reporter is deliberately last and, like the journal, cannot influence
/// anything: it is told what happened and decides nothing.
#[allow(clippy::too_many_arguments)]
pub fn run_watched(
    ledger: &mut Ledger,
    worker: &mut dyn TaskWorker,
    workspace: &Path,
    budget: &mut Budget,
    cancelled: &dyn Fn() -> bool,
    owner: &mut dyn OwnerChannel,
    journal: &mut dyn WorkJournal,
    progress: &mut dyn Progress,
    replanner: &mut dyn Replanner,
    goals: &mut dyn RoadmapGoals,
    lanes: &dyn LaneWorkers,
) -> RunOutcome {
    // BEFORE the first task, because a research task's deliverable lives in a
    // directory that may not exist yet and the agent is judged on whether it
    // wrote the file. Best effort: if this fails the agent can still create
    // the parent itself, and if it cannot, the check says so by name.
    crate::findings::prepare(workspace);

    let outcome = ledger_loop::work_the_ledger(
        ledger,
        workspace,
        budget,
        cancelled,
        ledger_loop::Crew {
            worker: &mut *worker,
            owner,
            journal,
            progress,
            replanner,
            goals,
            lanes,
        },
    );

    // THE LAST CONVERSATION IS STILL RESUMABLE UNTIL THIS.
    //
    // Every task after the first starts a fresh one (see
    // `AgentBackendWorker::work`), which leaves exactly one at the end of a
    // run — the one nothing will ever clear, because there is no next task.
    // Dropping it here is what makes "the context is gone once the work is
    // accepted" true of the whole run and not just of its middle.
    //
    // On EVERY exit, not only the successful one. A run that stopped for the
    // owner is precisely the one that sits idle longest before anyone returns
    // to it.
    worker.release();
    outcome
}

/// Questions agents left each other that nothing answered, for the operator.
///
/// # Why an unanswered question has to be shown
///
/// An ask does not block — it cannot, because in a sequential run the only
/// other participant is the agent doing the waiting. So an unanswered question
/// costs nothing at the moment it is asked and the agent carries on with a
/// guess. That is the right behaviour, and it has one condition: somebody sees
/// the question afterwards. A channel whose unanswered questions sit in a file
/// nobody opens is not a channel — it is a place guesses go to look like
/// decisions.
#[must_use]
pub fn unanswered_questions(workspace: &Path) -> Vec<String> {
    crate::team::Channel::in_workspace(workspace)
        .open_questions()
        .iter()
        .map(crate::team::Message::line)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests — declared last: the wiring check reads a file only up to its first
// column-zero `#[cfg(test)]`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod attribution_tests;
#[cfg(test)]
mod lane_tests;
#[cfg(test)]
mod owner_tests;
#[cfg(test)]
mod tests;
