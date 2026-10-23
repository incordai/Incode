//! Who did the work: the agent named on every attempt, the spend charged to
//! it, the progress count, and what is written to memory under its id.

use super::tests::{never_cancelled, passing_task, BrokenJournal, NamedWorker, SpyJournal};
use super::*;
use crate::ledger::{Task, TaskState};

/// A reporter that records what it was told.
#[derive(Default)]
struct SpyProgress {
    seen: Vec<(usize, usize)>,
}

impl Progress for SpyProgress {
    fn advanced(&mut self, done: usize, total: usize) {
        self.seen.push((done, total));
    }
}

/// **The denominator is every task in the plan, not the ones still open.**
/// A total that shrank as work finished would show a bar that never moved:
/// 1/1, then 1/1 again.
#[test]
fn progress_counts_against_the_whole_plan() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            passing_task("t-1"),
            passing_task("t-2"),
            passing_task("t-3"),
        ],
    };
    let mut worker = NamedWorker {
        agent: crate::identity::AgentId::new("some-backend", "grunt"),
        served_by: Vec::new(),
        fail: false,
    };
    let mut watcher = SpyProgress::default();

    let _ = run_watched(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut watcher,
        &mut NoReplan,
        // This test watches PROGRESS, not goals, and the stop reason is
        // discarded below — so the honest no-op is the right one here.
        &mut NoGoals,
        &NoLanes,
    );

    assert_eq!(
        watcher.seen,
        vec![(1, 3), (2, 3), (3, 3)],
        "the total must stay at three while the count climbs"
    );
}

/// A run with nobody watching behaves exactly as before — every existing
/// caller passes no reporter.
#[test]
fn an_unwatched_run_is_unchanged() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1")],
    };
    let mut worker = NamedWorker {
        agent: crate::identity::AgentId::new("some-backend", "grunt"),
        served_by: Vec::new(),
        fail: false,
    };
    let outcome = run(
        &mut ledger,
        &mut worker,
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
    );
    assert_eq!(outcome.completed, vec!["t-1".to_string()]);
}

fn worker(backend: &str, models: &[&str], fail: bool) -> NamedWorker {
    NamedWorker {
        agent: crate::identity::AgentId::new(backend, "grunt"),
        served_by: models.iter().map(|m| (*m).to_string()).collect(),
        fail,
    }
}

fn fingerprint() -> String {
    crate::identity::AgentId::new("some-backend", "grunt").fingerprint()
}

/// **The ledger names who did the work.** Without this a finished task says
/// what happened and never who — which makes "which worker keeps producing
/// work the check rejects?" unanswerable from the record.
#[test]
fn a_finished_task_records_the_agent_that_did_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "work")
            .with_verify(["cargo", "--version"])
            .with_tier("grunt")],
    };
    let mut w = worker("some-backend", &["model-a"], false);
    let mut budget = Budget::default();

    let outcome = run(
        &mut ledger,
        &mut w,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );

    assert_eq!(outcome.completed, vec!["t-1".to_string()]);
    let worked_by = ledger.get("t-1").expect("task").worked_by.clone();
    assert!(
        worked_by.contains("some-backend:grunt"),
        "the ledger must name the worker, got: {worked_by}"
    );
    assert!(
        worked_by.contains(&fingerprint()),
        "and carry the fingerprint a query matches on, got: {worked_by}"
    );
    assert!(
        worked_by.contains("model-a"),
        "and say what it actually ran on, got: {worked_by}"
    );
}

/// **A failed attempt has an author too.** Recording only successes would
/// hide exactly the pattern this record exists to expose.
#[test]
fn a_failed_attempt_still_names_who_made_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "work")
            .with_verify(["cargo", "--version"])
            .with_tier("grunt")],
    };
    let mut w = worker("some-backend", &[], true);
    let mut budget = Budget::default();

    let _ = run(
        &mut ledger,
        &mut w,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );

    let task = ledger.get("t-1").expect("task").clone();
    assert_eq!(task.state, TaskState::Abandoned);
    assert!(
        task.worked_by.contains("some-backend:grunt"),
        "a failure with no author is the record worth having, got: {}",
        task.worked_by
    );
    assert!(
        task.worked_by.contains("model not reported"),
        "and it must not invent a model it never learned, got: {}",
        task.worked_by
    );
}

/// **A model change does not change the worker.** Two tasks served by two
/// different models are still one agent in the record — the case a fallback
/// produces, and the reason the model cannot be the identity.
#[test]
fn a_model_change_between_tasks_does_not_change_the_worker() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "work")
            .with_verify(["cargo", "--version"])
            .with_tier("grunt")],
    };
    let mut budget = Budget::default();

    let mut first = worker("some-backend", &["model-a"], false);
    let _ = run(
        &mut ledger,
        &mut first,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );
    let one = ledger.get("t-1").expect("t-1").worked_by.clone();

    let mut ledger2 = Ledger {
        version: 1,
        tasks: vec![Task::new("t-2", "r-2", "work")
            .with_verify(["cargo", "--version"])
            .with_tier("grunt")],
    };
    // Same agent, different model — what a fallback leaves behind.
    let mut after = worker("some-backend", &["model-b"], false);
    let _ = run(
        &mut ledger2,
        &mut after,
        dir.path(),
        &mut budget,
        &never_cancelled(),
    );
    let two = ledger2.get("t-2").expect("t-2").worked_by.clone();

    assert!(
        one.contains("model-a") && two.contains("model-b"),
        "{one} / {two}"
    );
    assert!(
        one.contains(&fingerprint()) && two.contains(&fingerprint()),
        "one worker, one identity: {one} / {two}"
    );
}

/// **Memory is written under the agent, beside the proof.**
#[test]
fn finished_work_is_written_to_memory_under_the_agent_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "add the thing")
            .with_verify(["cargo", "--version"])
            .with_tier("grunt")],
    };
    let mut w = worker("some-backend", &["model-a"], false);
    let mut budget = Budget::default();
    let mut journal = SpyJournal::default();

    let _ = run_with_journal(
        &mut ledger,
        &mut w,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut NoOwner,
        &mut journal,
    );

    assert_eq!(journal.written.len(), 1, "one finished task, one note");
    let (text, subject) = journal.written[0].clone();
    assert!(
        subject.contains(&fingerprint()),
        "the subject must scope by agent so the history is a lookup: {subject}"
    );
    assert!(
        text.contains("t-1") && text.contains("add the thing"),
        "got: {text}"
    );
    assert!(
        text.contains("cargo --version") && text.contains("passed"),
        "the note must carry the evidence that closed it: {text}"
    );
}

/// **Unproved work is never written down as finished.** The journal sits on
/// the far side of the one edge into `Done`, not beside it.
#[test]
fn unverified_work_is_never_journalled() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            Task::new("t-1", "r-1", "work").with_verify(["cargo", "--definitely-not-a-real-flag"])
        ],
    };
    let mut w = worker("some-backend", &["model-a"], false);
    let mut budget = Budget::default();
    let mut journal = SpyJournal::default();

    let _ = run_with_journal(
        &mut ledger,
        &mut w,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut NoOwner,
        &mut journal,
    );

    assert!(
        journal.written.is_empty(),
        "memory must not learn that unproved work was finished: {:?}",
        journal.written
    );
}

/// **A store that is down does not lose the work.** The note records the
/// finish; it is never a condition of it.
#[test]
fn a_store_that_is_down_does_not_undo_finished_work() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![Task::new("t-1", "r-1", "work").with_verify(["cargo", "--version"])],
    };
    let mut w = worker("some-backend", &["model-a"], false);
    let mut budget = Budget::default();

    let outcome = run_with_journal(
        &mut ledger,
        &mut w,
        dir.path(),
        &mut budget,
        &never_cancelled(),
        &mut NoOwner,
        &mut BrokenJournal,
    );

    assert_eq!(outcome.completed, vec!["t-1".to_string()]);
    assert_eq!(ledger.get("t-1").expect("task").state, TaskState::Done);
}

/// Works each task as the agent its id names, spending what that agent spends.
#[derive(Default)]
struct AgentPerTask {
    last: String,
}

impl AgentPerTask {
    /// `(agent, tokens)` for a task id.
    fn agent_for(id: &str) -> (&'static str, u32) {
        match id {
            "t-1" => ("beta", 1_000),
            "t-2" => ("alpha", 1_000),
            _ => ("idle", 0),
        }
    }
}

impl TaskWorker for AgentPerTask {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        self.last = task.id.clone();
        Ok("did it".to_string())
    }
    fn spent_since_last_call(&mut self) -> crate::runtime::usage::TokenUsage {
        crate::runtime::usage::TokenUsage {
            input_tokens: Self::agent_for(&self.last).1,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }
    fn attribution(&self, task: &Task) -> crate::identity::Attribution {
        crate::identity::Attribution::new(crate::identity::AgentId::new(
            Self::agent_for(&task.id).0,
            "grunt",
        ))
    }
}

/// **HEAVIEST FIRST, A TIE BY NAME, AND NOBODY WHO SPENT NOTHING.** The spend
/// table answers "which one is burning the budget": an agent that spent
/// nothing is not an answer, and an order that changed between runs would make
/// two reports of the same run disagree.
#[test]
fn spend_is_ranked_heaviest_first_ties_by_name_and_idle_agents_left_out() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![
            passing_task("t-1"),
            passing_task("t-2"),
            passing_task("t-3"),
        ],
    };

    let outcome = run(
        &mut ledger,
        &mut AgentPerTask::default(),
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
    );

    let label = |name: &str| crate::identity::AgentId::new(name, "grunt").label();
    assert_eq!(outcome.completed.len(), 3, "{:?}", outcome.stop);
    assert_eq!(
        outcome.spend_by_agent,
        vec![(label("alpha"), 1_000), (label("beta"), 1_000)]
    );
}
