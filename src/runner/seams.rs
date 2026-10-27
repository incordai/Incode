//! The seams the loop works through: who does the work, who is asked, who is
//! told, and who plans again.
//!
//! Each is a trait so the loop's scheduling and its proof discipline can be
//! exercised without a model, a service or a person — and each default says
//! honestly that it does nothing.

use super::Handoff;
use crate::ledger::{Ledger, Task};

/// What actually did the work for one task.
///
/// A trait rather than a hardwired call to the agent loop, so the runner's
/// scheduling and its proof discipline can be exercised without a model — and
/// so the thing that does the work stays swappable.
pub trait TaskWorker {
    /// Attempt one task. The returned text is the worker's REPORT, which is a
    /// claim: it is recorded, and it never decides whether the task is done.
    fn work(&mut self, task: &Task) -> Result<String, String>;

    /// Guard rules the agent that just worked will NOT apply.
    ///
    /// Read by the runner after each attempt so the gap reaches the LEDGER, not
    /// only a trace line. It was carried in the worker's report first, and that
    /// was wrong twice over: the ledger note keeps only the report's first line,
    /// so the gap never landed; and the report also becomes the handoff note to
    /// later agents, where a guard notice crowds out the finding.
    ///
    /// Defaulted to nothing, for a worker with no notion of enforcement.
    fn unenforced(&self) -> &[String] {
        &[]
    }

    /// Every agent handover during the last attempt: who failed, why, who took
    /// over.
    ///
    /// Read on EVERY outcome, not just a passing one. A run where the first
    /// agent could not do the work is exactly the run whose record has to say
    /// so, and reconstructing it afterwards from the attribution line is not
    /// possible — that names only whoever finished.
    fn handovers(&self) -> &[String] {
        &[]
    }

    /// The run is over: drop any conversation still held open.
    ///
    /// An agent CLI's conversation is cached on the vendor's side and resumed
    /// by id. The worker starts a fresh one per task, which leaves the LAST
    /// task's alive with nothing to clear it — so this is what makes "the
    /// context is gone once the work is accepted" true of a whole run.
    ///
    /// Defaulted to nothing, for a worker that holds no conversation.
    fn release(&mut self) {}

    /// What the tasks BEFORE this one reported.
    ///
    /// # Why a task needs this at all
    ///
    /// Every task runs as a fresh agent session — nothing on the ship path sets
    /// a session id, so `resume` is always absent and task two inherits no
    /// conversation from task one. The plan is split by role precisely so that
    /// an `explore` step can find something an `implement` step then uses, and
    /// without this the finding died with the process that made it: the second
    /// agent re-derived from scratch what the first already knew, or worse,
    /// derived something different.
    ///
    /// Defaulted to ignoring the briefing, so a worker with no notion of prior
    /// work — a script, a stub, a test — is unaffected.
    fn brief(&mut self, _handoffs: &[Handoff]) {}

    /// What the operator's `user_prompt_submit` hooks printed for the next
    /// task's instruction, each an `<operator-hook>` block, to ride its turn.
    /// Replaced on every call; given by `cli::run_memory::Remembered` just
    /// before each `work`.
    ///
    /// Defaulted to ignoring it, for a worker with no agent turn to add it to.
    fn operator_context(&mut self, _blocks: &[String]) {}

    /// The last `work`'s tool calls and what each returned, taken — for the
    /// memory engine's transcript, whose recorder learns from them
    /// (`MemoryHooks::tools_ran`). Read by `cli::run_memory::Remembered` after
    /// each `work`, whether or not it succeeded.
    ///
    /// Defaulted to none, for a worker that runs no tools.
    fn take_steps(&mut self) -> crate::hooks::memory::Said {
        crate::hooks::memory::Said::default()
    }

    /// Who did the last attempt, and what they actually ran on.
    ///
    /// Defaulted to the harness itself: a worker that does not delegate is still
    /// an author, and a ledger entry with no author reads as "nobody did this".
    fn attribution(&self, task: &Task) -> crate::identity::Attribution {
        crate::identity::Attribution::new(crate::identity::AgentId::in_process(
            task.role().unwrap_or_default(),
        ))
    }

    /// What the last attempt cost, so the run's ceiling bounds real spend
    /// rather than a number nobody updates.
    ///
    /// Defaulted to nothing, because a worker that spends no tokens — a script,
    /// a stub, a test — should not have to say so.
    fn spent_since_last_call(&mut self) -> crate::runtime::usage::TokenUsage {
        crate::runtime::usage::TokenUsage::default()
    }
}

/// How the runner reaches a person when it hits something a machine must not
/// settle.
///
/// A trait so the runner's behaviour on each verdict is testable without a
/// service, a phone or a person — the three things that make the escalation path
/// the least exercised code in any autonomous loop, and therefore the one that
/// fails when it finally runs.
pub trait OwnerChannel {
    /// Raise the question and wait for the answer.
    ///
    /// An error means the question could not be put or could not be answered.
    /// It must NEVER be read as approval: a channel that is down is a reason to
    /// stop, not a reason to proceed unsupervised.
    fn decide(
        &mut self,
        title: &str,
        detail: &str,
        project: &str,
    ) -> Result<crate::owner::Verdict, String>;
}

/// The real channel: a card in the owner's queue, answered by them or by the
/// proxy acting as them.
pub struct MemoryOwner {
    /// How long to wait for one answer before reporting it still pending.
    pub seconds: u64,
    /// How loudly to ask. The service clamps this to 1..=5 and uses it to decide
    /// whether a closed phone is buzzed.
    pub severity: u8,
}

impl Default for MemoryOwner {
    fn default() -> Self {
        Self {
            seconds: crate::owner::WAIT_SECONDS,
            // Mid-scale: a stuck runner is worth a person's attention and is not
            // an emergency.
            severity: 3,
        }
    }
}

impl OwnerChannel for MemoryOwner {
    fn decide(
        &mut self,
        title: &str,
        detail: &str,
        project: &str,
    ) -> Result<crate::owner::Verdict, String> {
        // A decision the owner already made is not asked again. Only settled
        // answers count — approve and dismiss — and only for this exact question
        // on this project; the rest are answers about one attempt, not standing
        // policy. See `owner::past_verdict`.
        //
        // A history that cannot be read is not an answer either: fall through and
        // ask, rather than treating an unreachable service as "nothing was ever
        // decided", which would be indistinguishable from a fresh question.
        if let Ok(Some(settled)) = crate::owner::past_verdict(title, project) {
            return Ok(settled);
        }
        crate::owner::ask_and_wait(title, detail, project, self.severity, self.seconds)
    }
}

/// How many times one roadmap item may be planned again after failing.
///
/// Two, and then the run stops for a person — the same bound and the same reason
/// as [`crate::ledger::MAX_ATTEMPTS`]. A third plan for a goal that has defeated
/// two is not persistence, it is an unattended process spending an operator's
/// subscription on an approach nobody has looked at.
pub const MAX_REPLANS: u8 = 2;

/// A different approach to a roadmap item whose first plan failed.
///
/// # Wired to
///
/// - **Implemented by** `crate::cli`, which calls [`crate::plan::split`].
/// - **Called by** `learn_and_replan` (`attempt.rs`) when a task exhausts its
///   attempts.
/// - **Sibling to** [`OwnerChannel`]: both are seams that let this module stay
///   testable without a model or a network, and both must fail CLOSED.
///
/// # An empty plan is the correct way to give up
///
/// Returning no tasks stops the run exactly as it stopped before this trait
/// existed. A planner with no better idea must say so rather than return the
/// same plan again, and a caller that cannot reach a model returns empty rather
/// than guessing — a re-plan that is really "we could not ask" would spend the
/// bound on nothing.
pub trait Replanner {
    /// Told about EVERY exhausted task, whether or not a re-plan follows.
    ///
    /// Separate from [`Self::replan`] because the last failure — the one that
    /// spends the bound — is the most worth writing down, and it is exactly the
    /// one no re-plan is asked for. A default no-op body means an implementation
    /// that does not learn is not forced to pretend to.
    fn failed(&mut self, _roadmap_id: &str, _why: &str) {}

    /// A fresh plan for `roadmap_id`, knowing why the last one failed.
    ///
    /// Reading what this project has already got wrong belongs to the
    /// IMPLEMENTATION, not to this loop. The runner has never known about models
    /// or the network and must not learn here — and a loop that called the
    /// memory service directly would make every test of it either a network test
    /// or a writer into the operator's real store. See [`crate::lessons`].
    fn replan(&mut self, roadmap_id: &str, why: &str) -> Vec<Task>;
}

/// No second opinion. The default, and today's behaviour exactly.
pub struct NoReplan;

impl Replanner for NoReplan {
    fn replan(&mut self, _roadmap_id: &str, _why: &str) -> Vec<Task> {
        Vec::new()
    }
}

/// Nobody to ask.
///
/// Not a stub for tests — a real mode. A runner started where no owner can be
/// reached should stop at the first question rather than guess, and saying so in
/// the type is better than passing a null channel that silently approves.
pub struct NoOwner;

impl OwnerChannel for NoOwner {
    fn decide(
        &mut self,
        _title: &str,
        _detail: &str,
        _project: &str,
    ) -> Result<crate::owner::Verdict, String> {
        Err("no owner channel is configured, so nobody can answer this".to_string())
    }
}

/// Told how the run is going, as it goes.
///
/// A trait for the same reason [`OwnerChannel`] and [`WorkJournal`] are: the
/// runner must be testable without a terminal, and progress reported only in
/// production is progress nobody has watched.
pub trait Progress {
    /// Called when the count of finished tasks changes.
    fn advanced(&mut self, done: usize, total: usize);

    /// The whole ledger as it stands: at the top of every round of the loop
    /// — so after each task finished, failed or was set aside — and when a
    /// task is claimed, so the one running now can be named. Told, never
    /// asked: nothing it does reaches the ledger.
    fn ledger(&mut self, _ledger: &Ledger) {}

    /// Who follows the run's workers as they work — each parallel lane, with
    /// its own conversation — when anyone does. See [`crate::watch`].
    fn watch(&self) -> Option<std::sync::Arc<dyn crate::watch::Watch>> {
        None
    }
}

/// Nobody watching.
///
/// The default. A run with no reporter behaves exactly as it did before one
/// existed — which is what every existing caller and test wants.
pub struct Unwatched;

impl Progress for Unwatched {
    fn advanced(&mut self, _done: usize, _total: usize) {}
}

/// Where finished work is written down, under the name of whoever did it.
///
/// A trait for the same reason [`OwnerChannel`] is one: the runner's behaviour
/// must be exercisable without a service, and a recording path that only ever
/// runs in production is a recording path nobody has run.
pub trait WorkJournal {
    /// Record one finished task. An error is reported, never fatal — losing the
    /// note is bad; losing the finished work because the note could not be
    /// written would be worse.
    fn record(
        &mut self,
        attribution: &crate::identity::Attribution,
        task: &Task,
        evidence: &str,
    ) -> Result<(), String>;
}

/// Nothing written down.
///
/// The default, and honest about it: a run with no journal simply has no memory
/// of itself beyond the ledger on disk.
pub struct NoJournal;

impl WorkJournal for NoJournal {
    fn record(
        &mut self,
        _attribution: &crate::identity::Attribution,
        _task: &Task,
        _evidence: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// The real journal: the shared memory every agent on this machine reads.
///
/// Saved under the AGENT's fingerprint, not the model's — see
/// [`crate::identity`] for why the model cannot be the identity.
pub struct MemoryJournal;

impl WorkJournal for MemoryJournal {
    fn record(
        &mut self,
        attribution: &crate::identity::Attribution,
        task: &Task,
        evidence: &str,
    ) -> Result<(), String> {
        let (text, subject) =
            crate::knowledge::work_note(attribution, &task.id, &task.title, evidence);
        // The folder the run works in, which `work_note`'s subject is named
        // from too: the engine files a lesson by the project its `cwd` is in.
        let cwd = std::env::current_dir()
            .map_err(|e| format!("the run's folder could not be read: {e}"))?;
        crate::knowledge::remember(&crate::knowledge::Lesson {
            subject: &subject,
            ..crate::knowledge::Lesson::new("fact", &text, &cwd)
        })
        .map(|_| ())
    }
}
