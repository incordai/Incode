//! A task nobody can check, put to the owner — and what each answer may do.
//!
//! What a verdict may do is bounded: it decides whether to keep trying, never
//! whether the work is correct. No answer on this channel can produce a
//! [`crate::verify::Verified`], so no answer can move a task to `Done`.

use super::ledger_loop::LedgerLoop;
use super::StopReason;
use crate::ledger::Task;

/// How many times one task may be put back to the owner in a single run.
///
/// Observed, not theorised: a `change` verdict returns a task to the queue, and a
/// task that is still unverifiable comes straight back round to be asked about
/// again. Two redirections is the same patience the attempt counter gives, and
/// without a bound here the runner spins between the ledger and the owner forever
/// — it hung a test run doing exactly that before this existed.
pub const MAX_ESCALATIONS: u8 = 2;

/// The ledger note for a verdict: the verb, who applied it, and their reason.
///
/// Who decided is kept because the proxy answering alone and the owner answering
/// are different events, and a note saying only "approved" hides which happened.
fn verdict_note(verdict: &Result<crate::owner::Verdict, String>) -> String {
    match verdict {
        Ok(v) => v.summary(),
        Err(e) => format!("could not ask: {e}"),
    }
}

/// The question a task with no check is blocked on.
///
/// Called by [`LedgerLoop::escalate`], every time the task comes round.
fn unverifiable_question(task: &Task) -> String {
    let mut question = format!(
        "`{}` has no check, so nothing can prove it finished. Give it a \
         verify command, or decide it yourself.",
        task.id
    );
    // Carry the last answer forward. A task redirected by the owner and
    // then blocked again would otherwise lose the instruction it was
    // redirected WITH — the one thing the next attempt is meant to
    // follow, overwritten by the question that produced it.
    if !task.note.trim().is_empty() {
        question.push_str(&format!(" Last answer: {}", task.note.trim()));
    }
    question
}

impl LedgerLoop<'_> {
    /// Block a task nobody can check, ask the owner about it, and act on the
    /// answer: `None` to go round again, or why the run stops.
    ///
    /// Called by `LedgerLoop::round` for a picked task whose
    /// `can_be_verified` is false. Never completes it.
    pub(super) fn escalate(&mut self, task: Task) -> Option<StopReason> {
        let question = unverifiable_question(&task);
        let _ = self.ledger.block(&task.id, &question);
        let _ = self.ledger.save(self.workspace);

        let asked = self.escalations.entry(task.id.clone()).or_insert(0);
        *asked = asked.saturating_add(1);
        if *asked > MAX_ESCALATIONS {
            // Asked twice, redirected twice, still unanswerable by a machine.
            // Continuing would be the runner and the owner talking past each
            // other in a loop, which is worse than stopping.
            let why = format!(
                "{question} Asked {MAX_ESCALATIONS} times already and it still \
                 has no check, so this needs a person rather than another round."
            );
            let _ = self.ledger.block(&task.id, &why);
            let _ = self.ledger.save(self.workspace);
            return Some(StopReason::NeedsOwner {
                id: task.id,
                question: why,
            });
        }

        // What each answer is allowed to mean, spelled out in the card,
        // because a verdict is only as good as the question it answers. In
        // particular APPROVE does not mean "it is finished" — nothing on this
        // channel can say that.
        let detail = format!(
            "{question}\n\nApprove = set it aside as not machine-verifiable \
             (NOT done — nothing checked it). Change = reply with the command \
             that proves it, and it will be retried. Dismiss = drop the task.",
        );
        let verdict = self.crew.owner.decide(
            &format!("Runner blocked on `{}`", task.id),
            &detail,
            &self.workspace.display().to_string(),
        );
        self.apply_verdict(task, question, verdict)
    }

    /// Do what the owner's answer allows, and nothing more.
    ///
    /// Called by [`Self::escalate`], once per answer.
    fn apply_verdict(
        &mut self,
        task: Task,
        question: String,
        verdict: Result<crate::owner::Verdict, String>,
    ) -> Option<StopReason> {
        match verdict {
            // Set aside, never done. The one edge into `Done` takes proof and
            // this is not proof; it is permission to stop asking.
            Ok(crate::owner::Verdict::Approved { .. }) => {
                let note = verdict_note(&verdict);
                let _ = self.ledger.set_aside(&task.id, note);
                let _ = self.ledger.save(self.workspace);
                None
            }
            Ok(crate::owner::Verdict::Change { ref note, .. }) => {
                let instruction = note.clone();
                let record = verdict_note(&verdict);
                let cmd = crate::verify::parse_command(&instruction);
                if !cmd.is_empty() {
                    let _ = self.ledger.set_verify(&task.id, cmd);
                    self.escalations.remove(&task.id);
                }
                let _ = self.ledger.unblock(&task.id, record, instruction);
                let _ = self.ledger.save(self.workspace);
                None
            }
            // Parked, and answered. Nothing to retry and nothing to drop —
            // but "not this one" is not "not anything", so the rest of the
            // queue carries on. It stays `Blocked`, which is what keeps it
            // out of the queue AND what makes the run report it at the end:
            // a skip nobody is told about is the failure this arm risks.
            Ok(crate::owner::Verdict::Held { .. }) => {
                let why = verdict_note(&verdict);
                let _ = self.ledger.block(&task.id, &why);
                let _ = self.ledger.save(self.workspace);
                None
            }
            Ok(crate::owner::Verdict::Dismissed { .. }) => {
                let note = verdict_note(&verdict);
                let _ = self.ledger.cancel(&task.id, note);
                let _ = self.ledger.save(self.workspace);
                None
            }
            // Unanswered, still being read, or the channel failed. All three
            // stop and leave the task `Blocked` — silence is not consent, a
            // request for more detail is not an answer (the service does not
            // even consume the token for it), and a channel that is down is
            // the least safe moment to assume anything.
            Ok(crate::owner::Verdict::Pending | crate::owner::Verdict::Info { .. }) | Err(_) => {
                let why = match &verdict {
                    Err(e) => format!("{question} (could not ask: {e})"),
                    _ => question,
                };
                let _ = self.ledger.block(&task.id, &why);
                let _ = self.ledger.save(self.workspace);
                Some(StopReason::NeedsOwner {
                    id: task.id,
                    question: why,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **THE LAST ANSWER RIDES THE NEXT QUESTION.** A task the owner
    /// redirected and that came back still unverifiable must not lose the
    /// instruction it was redirected with — and a task never answered must not
    /// grow an empty "Last answer".
    #[test]
    fn the_question_carries_the_last_answer_forward_and_only_when_there_is_one() {
        let mut task = Task::new("t-1", "r-1", "tidy the docs");

        let first = unverifiable_question(&task);
        assert!(first.starts_with("`t-1` has no check"), "{first}");
        assert!(!first.contains("Last answer"), "{first}");

        task.note = "  use the docs check  ".to_string();
        let again = unverifiable_question(&task);
        assert!(
            again.ends_with(" Last answer: use the docs check"),
            "{again}"
        );
    }

    /// A channel that could not be asked is recorded as exactly that, never as
    /// an answer.
    #[test]
    fn a_channel_that_failed_is_noted_as_not_asked() {
        let note = verdict_note(&Err("the store is unreachable".to_string()));
        assert_eq!(note, "could not ask: the store is unreachable");
    }
}
