//! The final testing, the owner channel and what each verdict may do, and
//! the run's other stops: budget, cancel, failed attempts, and what an agent
//! is told.

use super::tests::{
    never_cancelled, passing_task, run_scheduling, FailingWorker, LyingWorker, OneGoalUnmet,
};
use super::*;
use crate::ledger::{Task, TaskState};

/// **A FINISHED PLAN IS NOT A REACHED GOAL.**
///
/// The whole point of the final testing. Every task passes its own check,
/// the queue drains, the ledger has no open work — and the roadmap item
/// those tasks were planned for is still not proved. That is what a wrong
/// plan looks like from outside, and before this gate existed the run
/// reported "Every task in the ledger is finished" and stopped there.
///
/// Note what is NOT asserted away: `completed` is still three and the
/// ledger is still clean. The tasks really did finish. The claim being
/// refused is the one that was never checked.
#[test]
fn a_drained_queue_whose_goal_is_unproved_is_not_reported_as_success() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };
    let mut worker = LyingWorker { calls: 0 };

    let outcome = run_watched(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut OneGoalUnmet,
        &NoLanes,
    );

    assert_eq!(
        outcome.completed.len(),
        2,
        "the tasks did finish — that is not what is in dispute"
    );
    assert!(!ledger.has_open_work(), "the queue really did drain");

    match &outcome.stop {
            StopReason::GoalsUnmet { unreached } => {
                assert_eq!(unreached.len(), 1);
                assert!(unreached[0].contains("R1"), "got: {unreached:?}");
            }
            other => panic!("a plan that finished without reaching its goal must not report success, got: {other:?}"),
        }

    // And the operator is told which of the two facts is true.
    let said = outcome.stop.message();
    assert!(
        said.contains("The plan finished; the goal did not."),
        "got: {said}"
    );
}

/// **"COULD NOT CHECK" IS NOT "PASSED", AND NOT "FAILED" EITHER.**
///
/// The roadmap lives in the brief, which is fetched over HTTP. When that
/// service is down the honest answer is that nothing was proved — and the
/// dangerous mistake would be reporting the drained queue as success,
/// which happens precisely when nobody is watching. The second mistake
/// would be reporting it as a failed goal, sending someone to debug code
/// to fix a stopped process.
#[test]
fn goals_that_could_not_be_read_are_reported_as_unchecked() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let mut worker = LyingWorker { calls: 0 };

    // `NoGoals` is the no-roadmap-source case, which is the same shape as
    // an unreachable brief and needs no service to exercise.
    let outcome = run_watched(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut NoGoals,
        &NoLanes,
    );

    assert!(
        matches!(outcome.stop, StopReason::GoalsUnchecked { .. }),
        "unchecked must be its own answer, got: {:?}",
        outcome.stop
    );
    assert_ne!(outcome.stop, StopReason::LedgerEmpty);

    let said = outcome.stop.message();
    assert!(
        said.contains("NOT checked"),
        "an operator must not read this as a pass: {said}"
    );
}

/// A task nobody can check is a decision for a person. The run stops and
/// says so rather than completing it or skipping past it.
#[test]
fn a_task_with_no_check_stops_the_run_for_the_owner() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "tidy the docs")],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();

    let outcome = run(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );

    match outcome.stop {
        StopReason::NeedsOwner { id, question } => {
            assert_eq!(id, "t-1");
            assert!(question.contains("no check"), "got: {question}");
        }
        other => panic!("expected the owner to be asked, got {other:?}"),
    }
    assert_eq!(worker.calls, 0, "it must not be attempted at all");
    assert_eq!(ledger.get("t-1").expect("task").state, TaskState::Blocked);
}

/// An owner channel that answers with whatever it was told to, and remembers
/// what it was asked. The verdict path is the least-exercised code in any
/// autonomous loop; this is what makes it exercisable at all.
struct ScriptedOwner {
    answer: Result<crate::owner::Verdict, String>,
    asked: Vec<String>,
}

impl ScriptedOwner {
    fn saying(answer: crate::owner::Verdict) -> Self {
        Self {
            answer: Ok(answer),
            asked: Vec::new(),
        }
    }
}

impl OwnerChannel for ScriptedOwner {
    fn decide(
        &mut self,
        title: &str,
        detail: &str,
        _project: &str,
    ) -> Result<crate::owner::Verdict, String> {
        self.asked.push(format!("{title} {detail}"));
        self.answer.clone()
    }
}

fn unverifiable_ledger() -> Ledger {
    Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "tidy the docs")],
    }
}

/// **The law, at the escalation.** An APPROVAL is not proof. The owner may
/// say "stop asking about this"; nothing they can say marks it done, because
/// `complete` takes a `Verified` and no verdict can produce one.
#[test]
fn an_owner_approval_sets_a_task_aside_and_never_completes_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Approved {
        by: "owner".to_string(),
        reason: "docs only".to_string(),
    });

    let outcome = run_scheduling(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    assert!(
        matches!(outcome.stop, StopReason::LedgerEmpty),
        "the queue drains rather than halting, got: {:?}",
        outcome.stop
    );
    let task = ledger.get("t-1").expect("task");
    assert_eq!(
        task.state,
        TaskState::Abandoned,
        "approved-without-a-check is SET ASIDE, never Done"
    );
    assert!(
        task.evidence.is_none(),
        "nothing checked it, so no evidence"
    );
    assert!(
        outcome.completed.is_empty(),
        "an approval must not appear as completed work"
    );
    assert!(task.note.contains("owner"), "who decided: {}", task.note);
}

/// A CHANGE is an instruction: put it back in the queue with the note, so the
/// next attempt is the one the owner asked for.
#[test]
fn an_owner_change_returns_the_task_to_the_queue_with_the_instruction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Change {
        note: "use the docs check".to_string(),
        by: "owner".to_string(),
        reason: String::new(),
    });

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    let task = ledger.get("t-1").expect("task");
    assert!(
        task.note.contains("use the docs check"),
        "the instruction has to survive to the retry: {}",
        task.note
    );
    // Still unverifiable, so it comes back round and is asked about again
    // rather than being run blind — the correct loop, not a bug.
    assert!(
        matches!(
            outcome.stop,
            StopReason::LedgerEmpty | StopReason::NeedsOwner { .. }
        ),
        "got: {:?}",
        outcome.stop
    );
}

/// When the owner replies to an unverifiable task with a command, the task
/// adopts that verify command, escapes the unverified block, runs, and completes.
#[test]
fn an_owner_change_providing_verify_command_unblocks_and_verifies_task() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Change {
        note: "cargo --version".to_string(),
        by: "owner".to_string(),
        reason: String::new(),
    });

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    let task = ledger.get("t-1").expect("task");
    assert_eq!(
        task.verify,
        vec!["cargo".to_string(), "--version".to_string()]
    );
    assert_eq!(task.state, TaskState::Done);
    assert_eq!(outcome.completed, vec!["t-1".to_string()]);
}

/// A DISMISSAL drops it. `Cancelled` is distinct from `Abandoned`: one is the
/// owner saying no, the other is the machine running out of attempts.
#[test]
fn an_owner_dismissal_cancels_the_task() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Dismissed {
        by: "proxy".to_string(),
        reason: "outside the brief".to_string(),
    });

    run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    let task = ledger.get("t-1").expect("task");
    assert_eq!(task.state, TaskState::Cancelled);
    assert!(
        task.note.contains("proxy") && task.note.contains("outside the brief"),
        "the record says who refused and why: {}",
        task.note
    );
}

/// **Silence is not consent.** No answer inside the wait leaves the task
/// blocked and stops the run. Both earlier loops failed the other way — an
/// unanswered question auto-answered and reported as the owner's approval.
#[test]
fn an_unanswered_question_stops_the_run_and_approves_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Pending);

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    assert!(
        matches!(outcome.stop, StopReason::NeedsOwner { .. }),
        "got: {:?}",
        outcome.stop
    );
    assert_eq!(ledger.get("t-1").expect("task").state, TaskState::Blocked);
    assert_eq!(worker.calls, 0);
}

/// **A channel that is down is the least safe moment to assume anything.**
/// It stops, and says the asking failed rather than reporting the question as
/// merely unanswered.
#[test]
fn a_broken_owner_channel_stops_the_run_rather_than_proceeding() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner {
        answer: Err("the store is unreachable".to_string()),
        asked: Vec::new(),
    };

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    match outcome.stop {
        StopReason::NeedsOwner { question, .. } => {
            assert!(question.contains("could not ask"), "got: {question}");
        }
        other => panic!("a dead channel must stop the run, got {other:?}"),
    }
    assert_eq!(ledger.get("t-1").expect("task").state, TaskState::Blocked);
}

/// The card must say what each answer will do. A verdict is only as good as
/// the question it answered, and "approve" against an unstated question is
/// how permission to continue becomes a claim that work is finished.
#[test]
fn the_card_says_what_each_answer_will_mean() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Pending);

    run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    let card = owner.asked.first().expect("a card was raised");
    assert!(card.contains("t-1"), "which task: {card}");
    assert!(
        card.contains("NOT done"),
        "approve must not read as done: {card}"
    );
    assert!(card.contains("Change ="), "got: {card}");
    assert!(card.contains("Dismiss ="), "got: {card}");
}

/// **An owner who keeps redirecting must not spin the runner.** A `change`
/// puts the task back, and a task that is still unverifiable comes straight
/// back round — this looped forever and hung a test run before
/// [`MAX_ESCALATIONS`] existed, which is why the bound is on the ACTION
/// (how many times it may be asked) and not on anything the owner says.
#[test]
fn an_owner_who_only_ever_redirects_cannot_loop_the_runner() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Change {
        note: "try again".to_string(),
        by: "owner".to_string(),
        reason: String::new(),
    });

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    match outcome.stop {
        StopReason::NeedsOwner { question, .. } => {
            assert!(question.contains("needs a person"), "got: {question}");
        }
        other => panic!("it must stop rather than spin, got {other:?}"),
    }
    assert_eq!(
        owner.asked.len(),
        usize::from(MAX_ESCALATIONS),
        "it asks up to the bound and then stops asking"
    );
    assert_eq!(ledger.get("t-1").expect("task").state, TaskState::Blocked);
}

/// **THE LAST ANSWER RIDES THE NEXT QUESTION.** A task redirected by the
/// owner and blocked again carries the answer it was redirected with into the
/// next card — and the first card, asked before any answer, carries none.
#[test]
fn a_redirected_task_carries_the_last_answer_into_the_next_card() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let answer = crate::owner::Verdict::Change {
        note: "try again".to_string(),
        by: "owner".to_string(),
        reason: String::new(),
    };
    let recorded = answer.summary();
    let mut owner = ScriptedOwner::saying(answer);

    let _ = run_with_owner(
        &mut ledger,
        &mut LyingWorker { calls: 0 },
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
        &mut owner,
    );

    assert_eq!(owner.asked.len(), 2, "{:?}", owner.asked);
    assert!(
        !owner.asked[0].contains("Last answer:"),
        "{}",
        owner.asked[0]
    );
    assert!(
        owner.asked[1].contains(&format!("Last answer: {}", recorded.trim())),
        "{}",
        owner.asked[1]
    );
}

/// **The owner's instruction has to reach the agent.** `change` exists so the
/// person can tell the coding agent what to do differently; an instruction
/// that lands only in the ledger makes the whole exchange theatre — the owner
/// answers, the runner records it, and the agent tries the same thing again
/// knowing nothing. This asserts the words are in what the agent is told.
#[test]
fn the_owners_instruction_reaches_the_agent_and_not_only_the_log() {
    let mut task = Task::new("t-1", "r-1", "tidy the docs");
    task.state = TaskState::Blocked;
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![task],
    };

    ledger
        .unblock(
            "t-1",
            "change by owner",
            "use the docs check, not cargo test",
        )
        .expect("a blocked task can be answered");

    let task = ledger.get("t-1").expect("task");
    assert_eq!(task.state, TaskState::Ready);
    let told = task_instruction(task);
    assert!(
        told.contains("use the docs check, not cargo test"),
        "the agent must be told what the owner said: {told}"
    );
    assert!(
        told.contains("Follow it"),
        "and told that it is an instruction: {told}"
    );
}

/// The record and the instruction are different things. A failure message fed
/// back as guidance would be worse than sending nothing at all.
#[test]
fn a_failure_note_is_never_handed_to_the_agent_as_an_instruction() {
    let task = Task::new("t-1", "r-1", "tidy the docs");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![task],
    };
    ledger
        .record_failure("t-1", "the check failed with exit 101")
        .expect("recorded");

    let task = ledger.get("t-1").expect("task");
    let told = task_instruction(task);
    assert!(
        !told.contains("exit 101"),
        "a failure note is a record, not guidance: {told}"
    );
}

/// `hold` parks ONE task; the rest of the queue carries on. "Not this one"
/// is not "not anything", and halting the whole run on a parked item wastes
/// every other task that was ready to go.
#[test]
fn a_held_task_is_skipped_and_the_rest_of_the_queue_still_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            Task::new("t-held", "r-1", "tidy the docs"),
            passing_task("t-next"),
        ],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Held {
        by: "owner".to_string(),
        reason: "after the release".to_string(),
    });

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    assert_eq!(
        ledger.get("t-held").expect("task").state,
        TaskState::Blocked,
        "parked, not dropped and not finished"
    );
    assert_eq!(
        ledger.get("t-next").expect("task").state,
        TaskState::Done,
        "the task behind it must not be punished for the hold"
    );
    assert_eq!(outcome.completed, vec!["t-next".to_string()]);
    assert_eq!(worker.calls, 1, "only the runnable task was attempted");
}

/// **A skip nobody is told about is worse than a halt.** A run that ends with
/// work still parked must NOT report that everything is finished — that is
/// how held work vanishes. This was already wrong before `hold` existed:
/// anything left `Blocked` produced "Every task in the ledger is finished."
#[test]
fn a_run_that_ends_with_parked_work_never_claims_everything_is_finished() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            Task::new("t-held", "r-1", "tidy the docs"),
            passing_task("t-next"),
        ],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Held {
        by: "owner".to_string(),
        reason: "after the release".to_string(),
    });

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    match &outcome.stop {
        StopReason::NothingRunnable { waiting, dead } => {
            assert_eq!(waiting, &vec!["t-held".to_string()]);
            assert!(dead.is_empty(), "nothing is stuck here, only parked");
        }
        other => panic!("parked work must be reported, got {other:?}"),
    }
    let said = outcome.stop.message();
    assert!(said.contains("t-held"), "name it: {said}");
    assert!(
        said.contains("NOT finished"),
        "and say plainly that it is not done: {said}"
    );
    assert!(
        !said.contains("Every task in the ledger is finished"),
        "the success sentence must not appear: {said}"
    );
}

/// `info` is read-only and does not even consume the token in the service, so
/// the question is still open. It must never read as a decision.
#[test]
fn a_request_for_more_detail_is_not_a_decision() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Info {
        by: "owner".to_string(),
        reason: "what does this task touch?".to_string(),
    });

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    assert!(
        matches!(outcome.stop, StopReason::NeedsOwner { .. }),
        "got: {:?}",
        outcome.stop
    );
    let task = ledger.get("t-1").expect("task");
    assert_eq!(task.state, TaskState::Blocked);
    assert_ne!(
        task.state,
        TaskState::Abandoned,
        "asking for detail is not approval"
    );
    assert_eq!(worker.calls, 0);
}

/// **Stuck is not waiting.** A task whose dependency was set aside can never
/// become ready — no person is coming to unblock it, because nothing is
/// pending on anyone. Reporting it in the same words as a parked task leaves
/// it in the ledger forever, looking like it is about to be answered.
#[test]
fn a_task_whose_dependency_can_never_land_is_reported_as_stuck() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            // Unverifiable, so the owner is asked and approves it aside.
            Task::new("t-first", "r-1", "tidy the docs"),
            Task::new("t-second", "r-2", "build on the docs")
                .with_dependencies(["t-first"])
                .with_verify(["cargo", "--version"]),
        ],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();
    let mut owner = ScriptedOwner::saying(crate::owner::Verdict::Approved {
        by: "owner".to_string(),
        reason: "not machine-checkable".to_string(),
    });

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut owner,
    );

    assert_eq!(
        ledger.get("t-first").expect("task").state,
        TaskState::Abandoned
    );
    match &outcome.stop {
        StopReason::NothingRunnable { waiting, dead } => {
            assert_eq!(waiting, &vec!["t-second".to_string()]);
            assert_eq!(dead.len(), 1, "the stuck task must be named: {dead:?}");
            assert_eq!(dead[0].0, "t-second");
            assert!(dead[0].1[0].contains("abandoned"), "got: {:?}", dead[0].1);
        }
        other => panic!("a stuck queue must say so, got {other:?}"),
    }
    let said = outcome.stop.message();
    assert!(
        said.contains("can NEVER become ready"),
        "the words must differ from ordinary waiting: {said}"
    );
}

/// A dependency nobody ever added is the same dead end, and the commoner one
/// — a roadmap that names an item the ledger does not carry.
#[test]
fn a_dependency_that_does_not_exist_is_a_dead_end_too() {
    let ledger = Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "work").with_dependencies(["t-ghost"])],
    };
    let dead = ledger.dead_dependencies();
    assert_eq!(dead.len(), 1);
    assert!(
        dead[0].1[0].contains("no such task"),
        "got: {:?}",
        dead[0].1
    );
}

/// A dependency that is merely unfinished is NOT dead. Without this the
/// check would flag every ordinary queue on its first pass.
#[test]
fn an_unfinished_dependency_is_not_reported_as_dead() {
    let ledger = Ledger {
        version: 1,
        tasks: vec![
            Task::new("t-first", "r-1", "first"),
            Task::new("t-second", "r-2", "second").with_dependencies(["t-first"]),
        ],
    };
    assert!(ledger.dead_dependencies().is_empty());
}

/// With nobody to ask, the runner stops at the first question instead of
/// guessing — the mode a headless run without a console is in.
#[test]
fn with_no_owner_channel_the_run_stops_at_the_first_question() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = unverifiable_ledger();
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();

    let outcome = run_with_owner(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut NoOwner,
    );

    assert!(matches!(outcome.stop, StopReason::NeedsOwner { .. }));
    assert_eq!(ledger.get("t-1").expect("task").state, TaskState::Blocked);
}

/// The ceiling stops the run before the next task, not after the queue.
#[test]
fn a_spent_budget_stops_the_run_before_the_next_task() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::with_ceiling(10);
    budget.record(crate::runtime::usage::TokenUsage {
        input_tokens: 50,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    });

    let outcome = run(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );

    assert_eq!(
        outcome.stop,
        StopReason::BudgetSpent {
            spent: 50,
            ceiling: Some(10)
        }
    );
    assert_eq!(worker.calls, 0, "no task may be started with nothing left");

    // AND THE MESSAGE IS ACTIONABLE. "50 used" is not something anyone can
    // do anything with: it needs the limit it was measured against and the
    // name of the variable that raises it. Both existed in
    // `Budget::exhausted_message` and nothing called it.
    let said = outcome.stop.message();
    assert!(said.contains("50 of 10"), "{said}");
    assert!(
        said.contains(crate::budget::BUDGET_ENV),
        "the operator is not told how to raise it: {said}"
    );
}

#[test]
fn a_cancel_stops_the_run_at_the_next_task_boundary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();

    let outcome = run(&mut ledger, &mut worker, dir.path(), &mut budget, &|| true);

    assert_eq!(outcome.stop, StopReason::Cancelled);
    assert_eq!(worker.calls, 0);
}

/// A worker that cannot even attempt the task is a failed attempt, not a
/// verdict on the code — and two of them stop the run.
#[test]
fn a_worker_that_cannot_run_counts_as_a_failed_attempt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let mut budget = Budget::default();

    let outcome = run(
        &mut ledger,
        &mut FailingWorker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );

    match outcome.stop {
        StopReason::TaskFailedTwice { id, why } => {
            assert_eq!(id, "t-1");
            assert!(why.contains("unavailable"), "got: {why}");
        }
        other => panic!("expected two failures to stop it, got {other:?}"),
    }
    assert_eq!(outcome.attempted, 2, "it tried twice before giving up");
}

/// Every stop must be explainable to a person.
#[test]
fn every_stop_reason_says_what_happened() {
    for reason in [
        StopReason::LedgerEmpty,
        StopReason::BudgetSpent {
            spent: 5,
            ceiling: Some(4),
        },
        StopReason::TaskFailedTwice {
            id: "t-1".to_string(),
            why: "the check failed".to_string(),
        },
        StopReason::NeedsOwner {
            id: "t-2".to_string(),
            question: "which schema?".to_string(),
        },
        StopReason::NothingRunnable {
            waiting: vec!["t-3".to_string()],
            dead: vec![("t-4".to_string(), vec!["t-9 (cancelled)".to_string()])],
        },
        StopReason::Cancelled,
    ] {
        let message = reason.message();
        assert!(!message.is_empty(), "{reason:?} must explain itself");
    }
}

/// The ledger on disk must reflect what happened, or a restart repeats work
/// that was already proven.
#[test]
fn progress_survives_on_disk_for_the_next_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();

    run(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );

    let reloaded = Ledger::load(dir.path()).expect("the run wrote a ledger");
    assert_eq!(
        reloaded.get("t-1").expect("task").state,
        TaskState::Done,
        "a restart must not redo proven work"
    );
}

/// **The frozen shapes must reach the agent that has to honour them.** A
/// contract named only in a plan file is one the coder never sees.
#[test]
fn the_contracts_a_task_builds_against_reach_the_agent() {
    let task =
        Task::new("t-1", "r-1", "the users endpoint").with_contracts(["User", "POST /api/users"]);
    let told = task_instruction(&task);
    assert!(told.contains("User"), "got: {told}");
    assert!(told.contains("POST /api/users"), "got: {told}");
    assert!(
        told.contains("exact names"),
        "and told that the names are the point: {told}"
    );
}

/// A task with no contracts must not carry an empty banner about them.
#[test]
fn a_task_with_no_contracts_says_nothing_about_contracts() {
    let told = task_instruction(&Task::new("t-1", "r-1", "tidy the docs"));
    assert!(!told.contains("frozen interfaces"), "got: {told}");
}

/// **Routing is a plan decision.** The tier is set where the whole roadmap
/// was visible, not guessed at run time with the least information anyone
/// will ever have about the task.
#[test]
fn a_planned_tier_names_the_role_the_task_runs_on() {
    assert_eq!(
        Task::new("t-1", "r-1", "work").with_tier("grunt").role(),
        Some("grunt")
    );
    assert_eq!(
        Task::new("t-1", "r-1", "work").with_tier("REVIEW").role(),
        Some("review"),
        "a tier written in another case is the same tier"
    );
}

/// A typo in a plan costs money, never correctness: an unknown tier falls
/// back to the session model rather than to whatever sorts first.
#[test]
fn an_unknown_tier_falls_back_rather_than_guessing() {
    assert_eq!(
        Task::new("t-1", "r-1", "work").with_tier("chepa").role(),
        None
    );
    assert_eq!(Task::new("t-1", "r-1", "work").role(), None);
}
