//! The loop itself: one round per task, until a round names a reason to stop.
//!
//! The state a run carries from round to round lives on [`LedgerLoop`], and
//! each round is a few named steps. Two of them continue in their own files:
//! a task nobody can check goes to the owner (`escalation`), and a claimed
//! task is worked and then proved or failed (`attempt`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::lanes::{run_lanes_if_useful, LaneReports, LaneRound};
use super::stop::ranked;
use super::{
    GoalVerdict, Handoff, LaneWorkers, OwnerChannel, Progress, Replanner, RoadmapGoals, RunOutcome,
    StopReason, TaskWorker, WorkJournal,
};
use crate::budget::Budget;
use crate::ledger::Ledger;

/// Every seam a run works through, beside the ledger, the workspace, the
/// budget and the cancel.
///
/// # Wired to
///
/// - **Built by** [`super::run_watched`], from its own arguments.
/// - **Handed to** [`work_the_ledger`], which keeps it on the [`LedgerLoop`].
pub(super) struct Crew<'r> {
    pub(super) worker: &'r mut dyn TaskWorker,
    pub(super) owner: &'r mut dyn OwnerChannel,
    pub(super) journal: &'r mut dyn WorkJournal,
    pub(super) progress: &'r mut dyn Progress,
    pub(super) replanner: &'r mut dyn Replanner,
    pub(super) goals: &'r mut dyn RoadmapGoals,
    pub(super) lanes: &'r dyn LaneWorkers,
}

/// [`super::run_watched`]'s loop, with nothing after it.
///
/// Split so the release there happens on every exit. The body returns from
/// half a dozen places — a budget ceiling, a cancel, a question for the owner
/// — and a cleanup written at each of them is a cleanup that will be missing
/// from the seventh.
///
/// Called by [`super::run_watched`], once per run.
pub(super) fn work_the_ledger<'r>(
    ledger: &'r mut Ledger,
    workspace: &'r Path,
    budget: &'r mut Budget,
    cancelled: &'r dyn Fn() -> bool,
    crew: Crew<'r>,
) -> RunOutcome {
    let mut run = LedgerLoop::start(ledger, workspace, budget, cancelled, crew);
    loop {
        if let Some(stop) = run.round() {
            return run.finish(stop);
        }
    }
}

/// What one run carries from round to round.
///
/// # Wired to
///
/// - **Built and driven by** [`work_the_ledger`], one [`Self::round`] at a
///   time, until a round names a [`StopReason`].
/// - **Continued in** `super::escalation` (`escalate`, for a task nobody can
///   check) and `super::attempt` (`attempt`, for a task claimed and worked).
pub(super) struct LedgerLoop<'r> {
    pub(super) ledger: &'r mut Ledger,
    pub(super) workspace: &'r Path,
    pub(super) budget: &'r mut Budget,
    cancelled: &'r dyn Fn() -> bool,
    pub(super) crew: Crew<'r>,
    /// Every task this run finished, in the order it finished.
    pub(super) completed: Vec<String>,
    /// What finished tasks have reported, in the order they finished. Grows only
    /// on a PASS — see where `attempt.rs` pushes it after `ledger.complete`.
    pub(super) handoffs: Vec<Handoff>,
    pub(super) attempted: usize,
    /// Per task, per run. Bounds the ask-answer-ask cycle described on
    /// [`super::MAX_ESCALATIONS`].
    pub(super) escalations: HashMap<String, u8>,
    /// Per ROADMAP ITEM, not per task: a re-plan invents new task ids, so a
    /// counter keyed on the task would reset itself every time it fired and the
    /// bound would never be reached.
    pub(super) replans: HashMap<String, u8>,
    pub(super) lane_reports: LaneReports,
    /// Tokens per agent, so "which one is burning the budget" is answerable.
    /// Keyed by the AGENT's label, not the model's: a fallback changes what
    /// answered without changing who asked, and a spend split across the two
    /// names would make one worker look like two.
    pub(super) by_agent: BTreeMap<String, u64>,
    /// Asked once. Detecting per pick would shell out to git on every task to
    /// learn a fact about the workspace that cannot change mid-run.
    isolation: crate::parallel::Isolation,
    /// The forge and base branch lanes are opened onto as pull requests, or
    /// `None` to merge them here. Decided once, in [`review_for`].
    review: Option<(&'r dyn crate::tools::pull_request::Forge, String)>,
}

impl<'r> LedgerLoop<'r> {
    /// Everything settled before the first task is picked: how lanes may run,
    /// which reviews were merged since the last run, and where lanes land.
    ///
    /// Called by [`work_the_ledger`], once per run.
    fn start(
        ledger: &'r mut Ledger,
        workspace: &'r Path,
        budget: &'r mut Budget,
        cancelled: &'r dyn Fn() -> bool,
        crew: Crew<'r>,
    ) -> Self {
        let mut completed = Vec::new();
        let isolation = crate::parallel::detect_isolation(workspace);

        // ASKED FOR FOUR, GOT ONE, AND TOLD WHY.
        //
        // `Isolation::None` carries the reason and `why_not` returns it, and
        // nothing asked: an operator who set INCODE_LANES=4 and got a sequential
        // run had no way to learn that the workspace is not a git repository.
        // The run looks identical to one that simply had nothing to parallelise.
        //
        // The rule for WHEN to say it lives in `parallel::shortfall_note`, which
        // is testable. What is left here is the print.
        if let Some(note) =
            crate::parallel::shortfall_note(crate::parallel::requested_lanes(), &isolation)
        {
            eprintln!("{note}");
        }

        // PULL REQUESTS SETTLED FIRST: a task whose review was merged since the
        // last run is checked and finished before anything is picked, so its
        // dependants become ready in this run rather than the next.
        if let Some(forge) = crew.lanes.forge() {
            let (notes, done) = crate::reviews::settle(ledger, workspace, forge);
            for note in notes {
                eprintln!("{note}");
            }
            completed.extend(done);
        }

        let review = review_for(&isolation, crew.lanes);
        Self {
            ledger,
            workspace,
            budget,
            cancelled,
            crew,
            completed,
            handoffs: Vec::new(),
            attempted: 0,
            escalations: HashMap::new(),
            replans: HashMap::new(),
            lane_reports: LaneReports::default(),
            by_agent: BTreeMap::new(),
            isolation,
            review,
        }
    }

    /// One turn of the loop: `None` to go round again, or why the run stops.
    ///
    /// Called by [`work_the_ledger`] until it returns a reason.
    fn round(&mut self) -> Option<StopReason> {
        self.crew.progress.ledger(self.ledger);
        if (self.cancelled)() {
            return Some(StopReason::Cancelled);
        }
        // Checked before the work rather than after, so the stop lands before
        // the spend rather than just after it.
        if self.budget.is_exhausted() {
            return Some(StopReason::BudgetSpent {
                spent: self.budget.spent(),
                ceiling: self.budget.ceiling(),
            });
        }

        self.ledger.refresh_ready();

        // RUN A BATCH OF LANES, if there is more than one task ready and the
        // workspace can isolate them. Done here, before a task is picked, so
        // everything below — picking, verifying, recording, escalating — is
        // the same code it has always been, reading reports that happen to
        // have arrived together.
        self.run_lanes();

        let Some(task) = self.ledger.next_ready().cloned() else {
            return Some(self.drained());
        };

        // A task nobody can check cannot be finished by a machine. That is a
        // decision for a person, not a chore to mark done, so it goes to the
        // owner rather than being completed on the worker's word.
        if !task.can_be_verified() {
            return self.escalate(task);
        }

        if self.ledger.start(&task.id).is_err() {
            // Something changed the ledger underneath us. Re-reading is the
            // honest response; looping on a stale view is not.
            return None;
        }
        self.crew.progress.ledger(self.ledger);
        self.attempted += 1;
        self.attempt(task)
    }

    /// Run a batch of lanes when that is possible and worth it, and pass on
    /// what they said about their merges.
    ///
    /// Called by [`Self::round`], before a task is picked.
    fn run_lanes(&mut self) {
        let round = LaneRound {
            workspace: self.workspace,
            isolation: &self.isolation,
            lanes: self.crew.lanes,
            cancelled: self.cancelled,
            review: self
                .review
                .as_ref()
                .map(|(forge, base)| (*forge, base.as_str())),
        };
        run_lanes_if_useful(&round, self.ledger, &mut self.lane_reports, self.budget);
        for note in self.lane_reports.notes.drain(..) {
            crate::trace!("lanes", "{note}");
        }
    }

    /// Why the run stops when nothing is ready to pick up.
    ///
    /// Nothing to pick up is not the same as everything being finished.
    /// A task parked by the owner, or one waiting on a dependency that
    /// was set aside, leaves the queue unrunnable with work still open —
    /// and reporting that as "every task is finished" is exactly how held
    /// work vanishes without anyone noticing.
    ///
    /// Called by [`Self::round`] when `next_ready` finds nothing.
    fn drained(&mut self) -> StopReason {
        let waiting = self.ledger.waiting_ids();
        let dead = self.ledger.dead_dependencies();
        if !waiting.is_empty() {
            return StopReason::NothingRunnable { waiting, dead };
        }
        // NOTHING READY, NOTHING WAITING, AND STILL WORK OPEN.
        //
        // Only `Running` can be here. Nothing reclaims it — there is no
        // lease — so a run killed mid-task leaves that state on disk and
        // the next run walked straight past it into the goals check,
        // where it could report `LedgerEmpty`: every task is finished,
        // with one still marked in progress. `Ledger::has_open_work` is
        // the question that catches it, and nothing had ever asked it.
        if self.ledger.has_open_work() {
            return StopReason::Stranded {
                ids: self
                    .ledger
                    .tasks
                    .iter()
                    .filter(|t| t.state.is_open())
                    .map(|t| t.id.clone())
                    .collect(),
            };
        }

        // THE FINAL TESTING, and it belongs to the orchestrator.
        //
        // Until this ran, a drained queue was reported as success. Every
        // task can pass its own check and leave the roadmap item undone —
        // a task's check proves a change compiles, a goal's check proves
        // the thing the owner asked for exists — so the loop was able to
        // finish a whole plan, announce it, and be wrong.
        //
        // Costly on purpose: one command per roadmap item. That is why it
        // is here, once, at the end, and not on every turn.
        match self.crew.goals.verdict(self.workspace, self.ledger) {
            GoalVerdict::AllReached => StopReason::LedgerEmpty,
            GoalVerdict::Unmet(unreached) => StopReason::GoalsUnmet { unreached },
            GoalVerdict::Unchecked(why) => StopReason::GoalsUnchecked { why },
        }
    }

    /// What the run did, stopped for `stop`.
    ///
    /// Called by [`work_the_ledger`], on every exit of the loop.
    fn finish(self, stop: StopReason) -> RunOutcome {
        RunOutcome {
            stop,
            completed: self.completed,
            attempted: self.attempted,
            spend_by_agent: ranked(&self.by_agent),
        }
    }
}

/// Where lanes are opened as pull requests, when the operator asked for that
/// and lanes will run: the forge, and the branch they are opened onto.
///
/// OPENED FOR REVIEW, when the operator asked for that and lanes will run.
/// Decided once, here, so a run that cannot open pull requests says why
/// before any lane works — and merges as it always has.
///
/// Called by [`LedgerLoop::start`], once per run.
fn review_for<'r>(
    isolation: &crate::parallel::Isolation,
    lanes: &'r dyn LaneWorkers,
) -> Option<(&'r dyn crate::tools::pull_request::Forge, String)> {
    let review_base = match (isolation, lanes.forge()) {
        (crate::parallel::Isolation::Worktrees { repo_root }, Some(forge))
            if lanes.open_for_review() && crate::parallel::requested_lanes() > 1 =>
        {
            match crate::parallel::review_base(repo_root, forge) {
                Ok(base) => Some(base),
                Err(why) => {
                    eprintln!("Lanes will be merged here, not opened as pull requests: {why}.");
                    None
                }
            }
        }
        _ => None,
    };
    match (lanes.forge(), review_base) {
        (Some(forge), Some(base)) => Some((forge, base)),
        _ => None,
    }
}
