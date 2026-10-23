//! A claimed task, worked once and then settled by its check.
//!
//! # The one law
//!
//! The only edge into `Done` is [`LedgerLoop::passed`], and the only way there
//! is a [`crate::verify::Verified`] from a command that exited zero. The
//! worker's report — however confident, from whichever model — is recorded as
//! a claim beside the evidence, never in place of it.

use std::path::Path;

use super::ledger_loop::LedgerLoop;
use super::{note_from, Handoff, Replanner, StopReason, MAX_REPLANS};
use crate::ledger::{Ledger, Task, TaskState};
use crate::verify;

/// What happened to the agents during one attempt — handovers, and guard rules
/// not enforced — recorded on every outcome by [`annotate`].
///
/// Built by [`LedgerLoop::attempt`]; read by `passed` and `failed`.
struct AttemptFacts {
    handovers: Vec<String>,
    gaps: Vec<String>,
}

impl LedgerLoop<'_> {
    /// Work `task` once — or take the report a lane already made for it — then
    /// let the check decide: `None` to go round again, or why the run stops.
    ///
    /// Called by `LedgerLoop::round`, once the task is claimed and counted.
    pub(super) fn attempt(&mut self, task: Task) -> Option<StopReason> {
        // Told what earlier tasks found BEFORE the attempt, not after: a fresh
        // session has no memory of them, and the point is to spend the earlier
        // work rather than repeat it.
        // A LANE ALREADY DID THIS ONE.
        //
        // Consumed, never reused: a second attempt at the same task must
        // actually re-run the agent. Handing back the report that failed the
        // first time would turn the attempt limit into a way of recording the
        // same failure twice.
        let attempt = match self.lane_reports.ready.remove(&task.id) {
            Some(from_lane) => from_lane,
            None => {
                self.crew.worker.brief(&self.handoffs);
                self.crew.worker.work(&task)
            }
        };
        // Counted whether the attempt succeeded or failed: a failed round trip
        // was still paid for, and a ceiling that ignores failures bounds the
        // wrong number.
        let spent_here = self.crew.worker.spent_since_last_call();
        self.budget.record(spent_here);
        // Recorded for the attempt, not only for the success: knowing which
        // worker keeps failing a task is the point of writing it down at all.
        let attribution = self.crew.worker.attribution(&task);
        // WHO SPENT IT, not just how much. The ceiling answers "is there
        // budget left"; an operator running four models on three
        // subscriptions is asking a different question — which one is
        // burning it — and the total cannot answer that.
        // Summed as totals rather than folded as `TokenUsage`: `merge` exists
        // to reconcile the TWO reports of one streaming response and takes the
        // larger of each field, which across separate turns would keep the
        // biggest turn instead of adding them up.
        *self
            .by_agent
            .entry(attribution.agent.label())
            .or_insert(0u64) += u64::from(spent_here.total_tokens());
        // Taken BEFORE any branch, so every outcome below records the same
        // facts about the attempt. Read once: the worker clears both on its
        // next task.
        let facts = AttemptFacts {
            handovers: self.crew.worker.handovers().to_vec(),
            gaps: self.crew.worker.unenforced().to_vec(),
        };
        if let Some(entry) = self.ledger.get_mut_public(&task.id) {
            entry.worked_by = attribution.line();
        }
        let report = match attempt {
            Ok(report) => report,
            // The attempt itself failed. That is a failed attempt, not a
            // verdict on the code.
            Err(reason) => return self.failed(task, reason, &facts),
        };

        // The only edge into `Done`. The worker's report is not consulted here;
        // it is kept beside the evidence, below.
        match verify::verify(&task.verify, self.workspace) {
            Ok(proof) => {
                self.passed(&task, &proof, &report, &attribution, &facts);
                None
            }
            Err(why) => self.failed(task, why, &facts),
        }
    }

    /// Record a task its check passed: done, with the evidence, and the
    /// worker's report beside it as a claim.
    ///
    /// Called by [`Self::attempt`], only with a `Verified` in hand.
    fn passed(
        &mut self,
        task: &Task,
        proof: &verify::Verified,
        report: &str,
        attribution: &crate::identity::Attribution,
        facts: &AttemptFacts,
    ) {
        let _ = self.ledger.complete(&task.id, proof);
        // Recorded as a CLAIM, explicitly labelled, so reading the
        // ledger later cannot mistake it for what proved the task.
        if let Some(entry) = self.ledger.tasks.iter_mut().find(|t| t.id == task.id) {
            entry.note = format!("verified. worker claimed: {}", first_line(report));
        }
        // The same annotation as every other outcome — a guard that was
        // off and an agent that was replaced are facts about the
        // attempt, and a passing task is not exempt from saying so.
        annotate(self.ledger, &task.id, &facts.handovers, &facts.gaps);
        // Written down under the agent's name, beside the evidence that
        // closed it. A failure here is reported and never fatal: the
        // work is done and proved whether or not the note lands.
        if let Err(e) = self
            .crew
            .journal
            .record(attribution, task, &proof.evidence().summary())
        {
            eprintln!("could not record `{}` in memory: {e}", task.id);
        }
        self.completed.push(task.id.clone());
        // Only a task that PASSED hands anything on. A failed attempt's
        // account of itself is exactly the account not worth spreading
        // to the tasks that follow it.
        self.handoffs.push(Handoff {
            from: task.id.clone(),
            role: task.role().unwrap_or("unrouted").to_string(),
            note: note_from(report),
        });
        // Counted against every task in the plan, not against the ones
        // still open: a denominator that shrank as work finished would
        // show a bar that never advanced.
        self.crew.progress.advanced(
            self.ledger
                .tasks
                .iter()
                .filter(|t| t.state == TaskState::Done)
                .count(),
            self.ledger.tasks.len(),
        );
        let _ = self.ledger.save(self.workspace);
    }

    /// Record a failed attempt — the agent's, or the check's — and, once the
    /// task has used its attempts, learn from it and plan again or stop.
    ///
    /// Called by [`Self::attempt`] from BOTH failure exits: the attempt that
    /// failed and the check that failed.
    fn failed(&mut self, task: Task, why: String, facts: &AttemptFacts) -> Option<StopReason> {
        let outcome = self.ledger.record_failure(&task.id, &why);
        annotate(self.ledger, &task.id, &facts.handovers, &facts.gaps);
        let _ = self.ledger.save(self.workspace);
        if !matches!(outcome, Ok(TaskState::Abandoned)) {
            return None;
        }
        if learn_and_replan(
            self.ledger,
            self.workspace,
            self.crew.replanner,
            &mut self.replans,
            &task.roadmap_id,
            &why,
        ) {
            return None;
        }
        Some(StopReason::TaskFailedTwice { id: task.id, why })
    }
}

/// Add what happened to the agents onto a task's ledger note.
///
/// # Why on every outcome, not only a pass
///
/// These two facts — a guard the agent could not enforce, and a handover from
/// one agent to another — belong to the ATTEMPT, not to its verdict. Recording
/// them only when a task passed put them exactly where they are least needed: a
/// task that was abandoned while the guard was off, or after two agents failed
/// it in turn, said nothing about either. That is the run an operator opens the
/// ledger to understand.
fn annotate(ledger: &mut Ledger, task_id: &str, handovers: &[String], gaps: &[String]) {
    if handovers.is_empty() && gaps.is_empty() {
        return;
    }
    if let Some(entry) = ledger.tasks.iter_mut().find(|t| t.id == task_id) {
        for handover in handovers {
            entry.note.push_str(&format!(" | HANDOVER: {handover}"));
        }
        for gap in gaps {
            entry.note.push_str(&format!(" | NOT ENFORCED: {gap}"));
        }
    }
}

/// Learn from an exhausted task and, if the bound allows, put a different plan
/// in the ledger.
///
/// # Wired to
///
/// **Both** failure exits of the loop in [`run_watched`] — the one where the
/// ATTEMPT failed and the one where the CHECK failed. They are separate arms
/// several hundred lines apart, and the first cut of this feature wired only
/// the second: the task abandoned, the run stopped, and nothing was ever
/// learned or re-planned. A test caught it; a reader would not have.
///
/// Returns `true` when a fresh plan is now in the ledger and the loop should
/// carry on, `false` when the run should stop.
fn learn_and_replan(
    ledger: &mut Ledger,
    workspace: &Path,
    replanner: &mut dyn Replanner,
    replans: &mut std::collections::HashMap<String, u8>,
    roadmap_id: &str,
    why: &str,
) -> bool {
    // TOLD FIRST, always — so the failure that spends the bound, the one no
    // re-plan is asked for, is still written down.
    replanner.failed(roadmap_id, why);

    let used = *replans.get(roadmap_id).unwrap_or(&0);
    if used >= MAX_REPLANS {
        crate::trace!("plan", "{roadmap_id}: re-plans spent");
        return false;
    }
    let fresh = replanner.replan(roadmap_id, why);
    if fresh.is_empty() {
        crate::trace!("plan", "{roadmap_id}: no different plan offered");
        return false;
    }
    replans.insert(roadmap_id.to_string(), used + 1);
    let replaced = ledger.replan_item(roadmap_id, fresh);
    let _ = ledger.save(workspace);
    crate::trace!(
        "plan",
        "{roadmap_id}: re-planned after failure, {replaced} unfinished task(s) replaced ({} of {MAX_REPLANS})",
        used + 1
    );
    true
}

/// The first line of a report, so a note stays a note.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}
