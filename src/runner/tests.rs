//! The runner's law end to end, re-planning, and the fixtures the other test
//! modules (`lane_tests`, `owner_tests`, `attribution_tests`) share.

use super::*;
use crate::ledger::{Task, TaskState};

/// A worker that always claims success without doing anything.
///
/// This is the adversary the design exists for: it is confident, it is
/// wrong, and nothing it says may move a task to `Done`.
pub(super) struct LyingWorker {
    pub(super) calls: usize,
}

impl TaskWorker for LyingWorker {
    fn work(&mut self, _task: &Task) -> Result<String, String> {
        self.calls += 1;
        Ok("I have completed the task and verified it works perfectly.".to_string())
    }
}

/// A worker whose attempt fails outright.
/// A replanner that offers a fresh plan a fixed number of times, and
/// records everything it was told — including the failures it was notified
/// of but never asked to re-plan.
struct ScriptedReplanner {
    offers_left: u8,
    replans_asked: u8,
    failures_seen: Vec<String>,
}

impl ScriptedReplanner {
    fn offering(times: u8) -> Self {
        Self {
            offers_left: times,
            replans_asked: 0,
            failures_seen: Vec::new(),
        }
    }
}

impl Replanner for ScriptedReplanner {
    fn failed(&mut self, roadmap_id: &str, _why: &str) {
        self.failures_seen.push(roadmap_id.to_string());
    }

    fn replan(&mut self, roadmap_id: &str, _why: &str) -> Vec<Task> {
        self.replans_asked += 1;
        if self.offers_left == 0 {
            return Vec::new();
        }
        self.offers_left -= 1;
        vec![Task::new(
            format!("{roadmap_id}.retry{}", self.replans_asked),
            roadmap_id,
            "a different approach",
        )
        .with_verify(["true"])]
    }
}

fn one_failing_item() -> Ledger {
    Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "the thing").with_verify(["true"])],
    }
}

fn run_replanning(
    ledger: &mut Ledger,
    replanner: &mut dyn Replanner,
    dir: &std::path::Path,
) -> RunOutcome {
    run_watched(
        ledger,
        &mut FailingWorker,
        dir,
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        replanner,
        &mut AllGoalsReached,
        &NoLanes,
    )
}

/// **THE FEATURE.** A goal whose plan failed used to end the run. Now a
/// different plan is tried, so the loop keeps working toward the goal
/// instead of stopping at the first approach that did not work.
///
/// Swap the replanner for `NoReplan` and this fails: the run stops at the
/// first exhausted task, which is exactly the old behaviour.
#[test]
fn a_failed_item_is_planned_again_instead_of_ending_the_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = one_failing_item();
    let mut replanner = ScriptedReplanner::offering(1);

    let outcome = run_replanning(&mut ledger, &mut replanner, dir.path());

    assert_eq!(
        replanner.replans_asked, 2,
        "asked once for the original plan and once for the retry, then the \
             offer ran out"
    );
    assert!(
        ledger.tasks.iter().any(|t| t.id.contains("retry")),
        "the fresh plan must actually reach the ledger: {:?}",
        ledger.tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    assert!(
        matches!(outcome.stop, StopReason::TaskFailedTwice { .. }),
        "and it still stops honestly once nothing more is offered: {:?}",
        outcome.stop
    );
}

/// **THE BOUND, which is what makes the loop safe to leave running.** A
/// replanner that always has another idea must NOT be allowed to loop
/// forever on an operator's subscription.
///
/// Raise `MAX_REPLANS` and this fails on the count — deliberately, because
/// the number is the safety property.
#[test]
fn a_replanner_that_never_runs_out_is_still_stopped_by_the_bound() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = one_failing_item();
    let mut replanner = ScriptedReplanner::offering(u8::MAX);

    let outcome = run_replanning(&mut ledger, &mut replanner, dir.path());

    assert_eq!(
        replanner.replans_asked, MAX_REPLANS,
        "the bound decides, not the planner's willingness"
    );
    assert!(matches!(outcome.stop, StopReason::TaskFailedTwice { .. }));
}

/// **EVERY failure is written down, including the last one.** The failure
/// that spends the bound is the one no re-plan is asked for, and it is the
/// most worth remembering — so `failed` is notified separately from
/// `replan`.
#[test]
fn the_failure_that_spends_the_bound_is_still_reported_for_learning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = one_failing_item();
    let mut replanner = ScriptedReplanner::offering(u8::MAX);

    let _ = run_replanning(&mut ledger, &mut replanner, dir.path());

    assert_eq!(
        replanner.failures_seen.len(),
        usize::from(MAX_REPLANS) + 1,
        "one more failure than re-plans: the last one ends the run and is \
             still learned from: {:?}",
        replanner.failures_seen
    );
    assert!(replanner.failures_seen.iter().all(|id| id == "r-1"));
}

/// A planner with no better idea gives up by saying nothing, and the run
/// stops exactly as it did before any of this existed.
#[test]
fn a_replanner_with_no_better_idea_stops_the_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = one_failing_item();
    let mut replanner = ScriptedReplanner::offering(0);

    let outcome = run_replanning(&mut ledger, &mut replanner, dir.path());

    assert_eq!(
        replanner.replans_asked,
        1,
        "asked once, told nothing. stop={:?} tasks={:?}",
        outcome.stop,
        ledger
            .tasks
            .iter()
            .map(|t| (t.id.clone(), t.state, t.attempts))
            .collect::<Vec<_>>()
    );
    assert!(matches!(outcome.stop, StopReason::TaskFailedTwice { .. }));
}

/// **A RE-PLAN MUST NEVER DISCARD VERIFIED WORK.** A loop that could delete
/// its own evidence would be able to launder a failure into a fresh start,
/// which is the one thing the completion law exists to prevent. Cancelled
/// tasks survive too: the owner dismissed those, and a machine must not
/// reopen a person's decision.
#[test]
fn replanning_keeps_finished_and_cancelled_work_and_replaces_only_the_rest() {
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            {
                let mut done = Task::new("r-1.1", "r-1", "already proven");
                done.state = TaskState::Done;
                done
            },
            {
                let mut cancelled = Task::new("r-1.2", "r-1", "owner dismissed this");
                cancelled.state = TaskState::Cancelled;
                cancelled
            },
            Task::new("r-1.3", "r-1", "still open"),
            Task::new("r-2.1", "r-2", "another item entirely"),
        ],
    };

    let replaced = ledger.replan_item("r-1", vec![Task::new("r-1.9", "r-1", "fresh")]);

    assert_eq!(replaced, 1, "only the open task was replaced");
    let ids: Vec<&str> = ledger.tasks.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"r-1.1"), "verified work survives: {ids:?}");
    assert!(
        ids.contains(&"r-1.2"),
        "the owner's decision survives: {ids:?}"
    );
    assert!(ids.contains(&"r-1.9"), "the fresh plan is added: {ids:?}");
    assert!(ids.contains(&"r-2.1"), "another item is untouched: {ids:?}");
    assert!(
        !ids.contains(&"r-1.3"),
        "the failed approach is gone: {ids:?}"
    );
}

pub(super) struct FailingWorker;

impl TaskWorker for FailingWorker {
    fn work(&mut self, _task: &Task) -> Result<String, String> {
        Err("the provider was unavailable".to_string())
    }
}

/// A worker that names itself, so what the runner records can be checked.
pub struct NamedWorker {
    pub agent: crate::identity::AgentId,
    pub served_by: Vec<String>,
    pub fail: bool,
}

impl TaskWorker for NamedWorker {
    fn work(&mut self, _task: &Task) -> Result<String, String> {
        if self.fail {
            return Err("the provider was unavailable".to_string());
        }
        Ok("done".to_string())
    }

    fn attribution(&self, _task: &Task) -> crate::identity::Attribution {
        crate::identity::Attribution::new(self.agent.clone()).served_by(self.served_by.clone())
    }
}

/// A journal that keeps what it was told instead of writing it anywhere.
#[derive(Default)]
pub struct SpyJournal {
    pub written: Vec<(String, String)>,
}

impl WorkJournal for SpyJournal {
    fn record(
        &mut self,
        attribution: &crate::identity::Attribution,
        task: &Task,
        evidence: &str,
    ) -> Result<(), String> {
        let (text, subject) =
            crate::knowledge::work_note(attribution, &task.id, &task.title, evidence);
        self.written.push((text, subject));
        Ok(())
    }
}

/// A journal that cannot write — the store is down.
pub struct BrokenJournal;

impl WorkJournal for BrokenJournal {
    fn record(
        &mut self,
        _attribution: &crate::identity::Attribution,
        _task: &Task,
        _evidence: &str,
    ) -> Result<(), String> {
        Err("the memory service is unreachable".to_string())
    }
}

pub fn never_cancelled() -> impl Fn() -> bool {
    || false
}

/// A roadmap whose every item is already proved.
///
/// For the tests about SCHEDULING — which task is picked, how attempts are
/// counted, when the owner is asked. Those have nothing to say about goals,
/// and the real gate would drag an HTTP call into each of them and make
/// them pass or fail on whether the memory service happened to be running.
///
/// Deliberately lives here and not beside [`NoGoals`]: a shipped type that
/// answers "everything passed" without looking is one wrong import away
/// from disabling the gate in production.
pub(super) struct AllGoalsReached;

impl RoadmapGoals for AllGoalsReached {
    fn verdict(&mut self, _workspace: &Path, _ledger: &Ledger) -> GoalVerdict {
        GoalVerdict::AllReached
    }
}

/// A roadmap with one item nothing proved.
pub(super) struct OneGoalUnmet;

impl RoadmapGoals for OneGoalUnmet {
    fn verdict(&mut self, _workspace: &Path, _ledger: &Ledger) -> GoalVerdict {
        GoalVerdict::Unmet(vec!["R1 checkout — no check proved it".to_string()])
    }
}

/// [`run_with_owner`] with the roadmap goals already proved.
///
/// For the tests whose subject is SCHEDULING — which task is picked, when
/// the owner is asked, what the ledger looks like afterwards. Those assert
/// [`StopReason::LedgerEmpty`] to mean "the queue drained clean", and the
/// goal gate is a separate concern with its own tests; without this they
/// would all be re-asserting it by accident.
pub(super) fn run_scheduling(
    ledger: &mut Ledger,
    worker: &mut dyn TaskWorker,
    workspace: &Path,
    budget: &mut Budget,
    cancelled: &dyn Fn() -> bool,
    owner: &mut dyn OwnerChannel,
) -> RunOutcome {
    run_watched(
        ledger,
        worker,
        workspace,
        budget,
        cancelled,
        owner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &NoLanes,
    )
}

/// A task whose check passes, using a real command.
pub fn passing_task(id: &str) -> Task {
    Task::new(id, format!("r-{id}"), "work").with_verify(["cargo", "--version"])
}

/// A task whose check really fails.
fn failing_task(id: &str) -> Task {
    Task::new(id, format!("r-{id}"), "work").with_verify(["cargo", "--definitely-not-a-real-flag"])
}

/// **The law, end to end.** A worker that claims success cannot complete a
/// task whose check fails — the exact failure both previous loops had.
#[test]
fn a_confident_worker_cannot_complete_work_the_check_rejects() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![failing_task("t-1")],
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

    assert!(
        matches!(outcome.stop, StopReason::TaskFailedTwice { .. }),
        "it must stop on the failure, got: {:?}",
        outcome.stop
    );
    assert!(outcome.completed.is_empty(), "nothing may be marked done");
    assert_eq!(
        ledger.get("t-1").expect("task").state,
        TaskState::Abandoned,
        "the task is set aside, not completed on the worker's word"
    );
    assert!(
        ledger.get("t-1").expect("task").evidence.is_none(),
        "a task that never passed must carry no evidence"
    );
}

/// The same worker, the same confident report — but a check that really
/// passes. Now the task completes, and the report is kept as a CLAIM beside
/// the evidence rather than as the reason.
#[test]
fn a_passing_check_completes_the_task_and_labels_the_report_as_a_claim() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();

    let outcome = run_scheduling(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut NoOwner,
    );

    assert_eq!(outcome.stop, StopReason::LedgerEmpty);
    assert_eq!(outcome.completed, vec!["t-1".to_string()]);

    let task = ledger.get("t-1").expect("task");
    assert_eq!(task.state, TaskState::Done);
    assert!(
        task.evidence
            .as_deref()
            .is_some_and(|e| e.contains("passed")),
        "the evidence is what the command said; got {:?}",
        task.evidence
    );
    assert!(
        task.note.contains("worker claimed:"),
        "the report is kept, explicitly as a claim; got: {}",
        task.note
    );
}

/// Several tasks in order, each proven before the next is picked up.
#[test]
fn the_run_works_the_ledger_until_it_is_empty() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            passing_task("t-1"),
            passing_task("t-2"),
            passing_task("t-3"),
        ],
    };
    let mut worker = LyingWorker { calls: 0 };
    let mut budget = Budget::default();

    let outcome = run_scheduling(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut NoOwner,
    );

    assert_eq!(outcome.stop, StopReason::LedgerEmpty);
    assert_eq!(outcome.completed.len(), 3);
    assert_eq!(worker.calls, 3, "each task was actually worked");
    assert!(!ledger.has_open_work());
}

/// Records whether the run let go of the conversation at the end.
#[derive(Default)]
struct ReleasingWorker {
    released: bool,
}

impl TaskWorker for ReleasingWorker {
    fn work(&mut self, _task: &Task) -> Result<String, String> {
        Ok("did the thing".to_string())
    }
    fn release(&mut self) {
        self.released = true;
    }
}

/// **THE CONVERSATION IS DROPPED ON EVERY EXIT, NOT JUST THE GOOD ONE.**
///
/// A run that stopped for the owner, or on a cancel, is exactly the one
/// that sits idle longest before anyone comes back to it — so it is the
/// one whose conversation must not be left resumable. A cleanup written
/// only on the success path is missing from the paths that need it most.
#[test]
fn a_run_lets_go_of_its_conversation_however_it_ends() {
    let dir = tempfile::tempdir().expect("temp dir");

    // Exit 1: the queue drains.
    let mut drained = ReleasingWorker::default();
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let _ = run_scheduling(
        &mut ledger,
        &mut drained,
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
    );
    assert!(drained.released, "a drained run kept its conversation open");

    // Exit 2: cancelled before a single task runs — the early return.
    let mut cancelled_worker = ReleasingWorker::default();
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let outcome = run_scheduling(
        &mut ledger,
        &mut cancelled_worker,
        dir.path(),
        &mut Budget::default(),
        &|| true,
        &mut NoOwner,
    );
    assert_eq!(outcome.stop, StopReason::Cancelled);
    assert!(
        cancelled_worker.released,
        "a cancelled run kept its conversation open"
    );
}
