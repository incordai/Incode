//! Lanes: several ready tasks at once, each in its own checkout — and still
//! nothing is done until the check passes in the merged tree. Also the
//! research artifact, a task left running by a crash, and what lanes spend.

use super::tests::{
    never_cancelled, passing_task, run_scheduling, AllGoalsReached, FailingWorker, LyingWorker,
};
use super::*;
use crate::ledger::{Task, TaskState};

/// A pool that supplies a worker writing a file in whatever tree it gets.
struct FileWritingLanes {
    /// Every workspace a lane was opened in, so the test can prove they
    /// differed rather than trusting that they did.
    seen: std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>,
}

impl LaneWorkers for FileWritingLanes {
    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn TaskWorker + Send>> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(workspace.to_path_buf());
        Some(Box::new(LaneScribe {
            workspace: workspace.to_path_buf(),
        }))
    }
}

struct LaneScribe {
    workspace: std::path::PathBuf,
}

impl TaskWorker for LaneScribe {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        std::fs::write(
            self.workspace.join(format!("{}.txt", task.id)),
            format!("{} was here\n", task.id),
        )
        .map_err(|e| e.to_string())?;
        Ok(format!("{} done", task.id))
    }
}

/// A repository with one commit, or `None` where git is unusable.
fn a_repo() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
    let dir = tempfile::tempdir().ok()?;
    let root = dir.path().to_path_buf();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    git(&["init"])?;
    git(&["config", "user.email", "t@example.invalid"])?;
    git(&["config", "user.name", "incode test"])?;
    std::fs::write(root.join("base.txt"), "base\n").ok()?;
    git(&["add", "-A"])?;
    git(&["commit", "-m", "base"])?;
    Some((dir, root))
}

/// **THE LOOP REALLY USES LANES, AND STILL OBEYS ITS OWN LAW.**
///
/// Two ready tasks, two lanes. What must be true afterwards: each agent
/// worked in a DIFFERENT directory, both files reached the main tree via
/// a merge, and both tasks are `Done` — which they can only be because
/// `verify` ran in the main tree and passed. The lanes produced reports;
/// the runner still decided.
#[test]
fn two_ready_tasks_are_worked_in_parallel_and_both_land() {
    let _env = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((_guard, root)) = a_repo() else {
        return;
    };
    std::env::set_var(crate::parallel::LANES_ENV, "2");

    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let lanes = FileWritingLanes {
        seen: std::sync::Arc::clone(&seen),
    };

    let outcome = run_watched(
        &mut ledger,
        // The SEQUENTIAL worker must never be asked: every task was
        // already worked by a lane.
        &mut NeverWorker,
        &root,
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &lanes,
    );

    std::env::remove_var(crate::parallel::LANES_ENV);

    assert_eq!(outcome.stop, StopReason::LedgerEmpty, "{:?}", outcome.stop);
    assert_eq!(outcome.completed.len(), 2, "both tasks must finish");

    // Two lanes, two DIFFERENT trees, and neither is the main one.
    let opened = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(opened.len(), 2, "opened: {opened:?}");
    assert_ne!(opened[0], opened[1], "both lanes got the same directory");
    assert!(!opened.contains(&root), "a lane worked in the main tree");

    // AND THE WORK LANDED. This is the merge, proved from the outside.
    assert!(
        root.join("t-1.txt").exists(),
        "lane one's work never merged"
    );
    assert!(
        root.join("t-2.txt").exists(),
        "lane two's work never merged"
    );

    for task in &ledger.tasks {
        assert_eq!(task.state, TaskState::Done, "`{}` is not done", task.id);
    }
}

/// Refuses to work, so a test can prove it was never asked to.
struct NeverWorker;

impl TaskWorker for NeverWorker {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        panic!(
            "the sequential worker ran `{}`; a lane should have",
            task.id
        )
    }
}

/// Every lane rewrites the same line of the same file, so the second
/// merge must conflict.
struct CollidingLanes;

impl LaneWorkers for CollidingLanes {
    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn TaskWorker + Send>> {
        Some(Box::new(Collider {
            workspace: workspace.to_path_buf(),
        }))
    }
}

struct Collider {
    workspace: std::path::PathBuf,
}

impl TaskWorker for Collider {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        std::fs::write(
            self.workspace.join("base.txt"),
            format!("rewritten by {}\n", task.id),
        )
        .map_err(|e| e.to_string())?;
        // The agent is perfectly happy. It has no idea another lane
        // touched the same line — that is the whole point of isolation,
        // and the whole reason the merge has to be the judge.
        Ok(format!("{} done", task.id))
    }
}

/// **A LANE THAT DID NOT MERGE MUST NOT BE CALLED DONE.**
///
/// The agent reported success and meant it: in its own tree the work is
/// finished. But its change is on a branch, not in the tree `verify` is
/// about to run over — so believing the report would prove a tree that
/// does not contain the change and mark the task complete on the strength
/// of it. That is the fabrication the whole verify design exists to stop,
/// arriving through a new door.
///
/// One task lands, the other conflicts. The conflicted one must be open,
/// and the run must not report plain success.
#[test]
fn a_lane_that_could_not_merge_is_not_recorded_as_finished() {
    let _env = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((_guard, root)) = a_repo() else {
        return;
    };
    std::env::set_var(crate::parallel::LANES_ENV, "2");

    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };

    let outcome = run_watched(
        &mut ledger,
        // Attempt two runs sequentially, and must also fail, so the task
        // settles rather than looping.
        &mut FailingWorker,
        &root,
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &CollidingLanes,
    );

    std::env::remove_var(crate::parallel::LANES_ENV);

    // Exactly one lane's work reached the tree.
    let landed = std::fs::read_to_string(root.join("base.txt")).expect("read");
    assert!(
        landed.contains("rewritten by"),
        "no lane landed at all: {landed}"
    );
    assert!(
        !landed.contains("<<<<<<<"),
        "conflict markers were left in the main tree: {landed}"
    );

    // AND THE ONE THAT DID NOT IS NOT DONE.
    let done: Vec<&str> = ledger
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Done)
        .map(|t| t.id.as_str())
        .collect();
    assert_eq!(
        done.len(),
        1,
        "a task whose work never merged was marked done: {done:?}"
    );

    assert_ne!(
        outcome.stop,
        StopReason::LedgerEmpty,
        "a run that could not land half its work reported plain success"
    );
}

/// The `incode` binary cargo just built, beside this test's own exe.
///
/// `cargo test` puts unit tests in `…/debug/deps/` and binaries in
/// `…/debug/`, so the sibling of the parent is where it lands. `None`
/// when only the library was built.
fn built_binary() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe
        .parent()?
        .parent()?
        .join(crate::verify::SELF_PROGRAM)
        .with_extension(std::env::consts::EXE_EXTENSION);
    candidate.is_file().then_some(candidate)
}

/// Writes whatever it was told to deliver, and says so.
struct DeliveringWorker {
    workspace: std::path::PathBuf,
    /// What to write. Short enough to fail the check, or long enough to
    /// pass it — the test picks.
    body: String,
}

impl TaskWorker for DeliveringWorker {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        if let Some(path) = &task.deliverable {
            std::fs::write(
                crate::findings::resolve(&self.workspace, path),
                self.body.clone(),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok("looked into it".to_string())
    }
}

/// **A RESEARCH TASK IS PROVED BY ITS ARTIFACT, END TO END.**
///
/// Running the real check as a real subprocess, because that is the part
/// that could be wrong in a way no unit test sees: the ledger writes an
/// argv, `verify` resolves the program name to this binary, the binary
/// parses the flag and reads the file. Every link is somewhere else.
///
/// Both directions, in one test. An agent that wrote nothing worth
/// reading must NOT finish — that is the failure the old fold was a
/// response to, and a check that only ever passes is the same bug with
/// more machinery.
#[test]
fn a_research_task_is_done_only_when_its_findings_are_real() {
    // This test process is not `incode` — it is the test harness, which
    // is exactly the case `SELF_BIN_ENV` exists for. Skipped rather than
    // failed where the binary has not been built: the check being
    // exercised is a real subprocess, and there is nothing honest to
    // assert without one.
    let Some(binary) = built_binary() else {
        return;
    };
    std::env::set_var(crate::verify::SELF_BIN_ENV, &binary);

    for (body, should_finish) in [
        ("Done.".to_string(), false),
        ("x".repeat(crate::findings::MIN_CHARS + 1), true),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        crate::findings::prepare(dir.path());

        let mut task = Task::new("r-1.1", "r-1", "find the writers")
            .with_tier("explore")
            .delivering_findings(crate::findings::path_for("r-1.1"));
        task.state = TaskState::Ready;
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![task],
        };

        let mut worker = DeliveringWorker {
            workspace: dir.path().to_path_buf(),
            body,
        };
        let outcome = run(
            &mut ledger,
            &mut worker,
            dir.path(),
            &mut Budget::default(),
            &never_cancelled(),
        );

        let finished = ledger.tasks[0].state == TaskState::Done;
        assert_eq!(
            finished, should_finish,
            "state {:?}, note {:?}, stop {:?}",
            ledger.tasks[0].state, ledger.tasks[0].note, outcome.stop
        );
        if should_finish {
            // Proved by the command, not by the agent saying so.
            assert!(
                ledger.tasks[0]
                    .evidence
                    .as_deref()
                    .is_some_and(|e| e.contains("r-1.1.md")),
                "{:?}",
                ledger.tasks[0].evidence
            );
        }
    }
}

/// **THE LAST ATTEMPT SHOULD KNOW IT IS THE LAST.**
///
/// `Task::attempts_left` was computed and nobody asked it, so an agent on
/// its final try worked exactly as it had on its first — reaching for the
/// better change rather than the one it was sure of, on the attempt where
/// failing means the task is set aside for a person.
///
/// And NOT on the first attempt: telling an agent it has two tries invites
/// it to spend one.
#[test]
fn only_the_final_attempt_is_told_that_it_is_final() {
    let mut task = Task::new("t-1", "r-1", "fix it").with_verify(["true"]);

    assert!(
        !task_instruction(&task).contains("LAST attempt"),
        "a first attempt must not be told it is the last"
    );

    task.attempts = crate::ledger::MAX_ATTEMPTS - 1;
    let final_try = task_instruction(&task);
    assert!(
        final_try.contains("LAST attempt"),
        "the final attempt was not told: {final_try}"
    );
    assert!(
        final_try.contains("smallest change"),
        "saying it is the last is only useful with what to do about it: {final_try}"
    );
}

/// **A TASK LEFT `Running` MUST NOT BE REPORTED AS FINISHED.**
///
/// `next_ready` finds `Ready`, `waiting_ids` finds `Blocked` and
/// `Pending`, and nothing looked for `Running`. Nothing reclaims it
/// either — there is no lease — so a run killed mid-task leaves that on
/// disk, and the next run walked past it into the goals check and could
/// announce that every task was finished.
///
/// That is the same fabrication `NothingRunnable` was added to stop,
/// arriving through the one state neither query covered.
#[test]
fn a_task_left_running_by_a_crash_is_not_called_finished() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut stranded = passing_task("t-1");
    // Exactly what a killed run leaves behind: claimed, never settled.
    stranded.state = TaskState::Running;
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![stranded],
    };

    // Goals that PASS, deliberately. With the check removed this run
    // reports `LedgerEmpty` — "every task is finished" — which is the
    // exact fabrication being guarded against. Without them the control
    // would stop at `GoalsUnchecked` and the test would prove a near-miss
    // instead of the thing it claims.
    let outcome = run_watched(
        &mut ledger,
        &mut NeverWorker,
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &NoLanes,
    );

    assert_eq!(
        outcome.stop,
        StopReason::Stranded {
            ids: vec!["t-1".to_string()]
        },
        "a task still marked in progress was reported as {:?}",
        outcome.stop
    );

    // AND THE MESSAGE SAYS WHAT TO DO. "Stranded" is not a thing an
    // operator can act on without being told where the state lives.
    let said = outcome.stop.message();
    assert!(said.contains("t-1"), "{said}");
    assert!(
        said.contains("ledger.json"),
        "the operator is not told where to fix it: {said}"
    );
}

/// Fails like [`FailingWorker`], but keeps every task it was handed.
struct RecordingWorker {
    seen: Vec<Task>,
}

impl TaskWorker for RecordingWorker {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        self.seen.push(task.clone());
        Err("the provider was unavailable".to_string())
    }
}

/// **A CONFLICT MUST NOT THROW THE WORK AWAY.**
///
/// The lane wrote a real change and committed it. The merge failed only
/// because another lane had moved the same lines — the change is not
/// wrong, the tree is different. If the retry is handed a bare task it
/// starts from an empty tree and writes the whole thing again, so the
/// same work is paid for twice and the nearly-right version is lost.
///
/// What must be true: the branch still exists in the repository, and the
/// task the retry is given names it.
#[test]
fn a_conflicted_lane_tells_the_retry_where_its_work_is() {
    let _env = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((_guard, root)) = a_repo() else {
        return;
    };
    std::env::set_var(crate::parallel::LANES_ENV, "2");

    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };
    let mut retry = RecordingWorker { seen: Vec::new() };

    let _ = run_watched(
        &mut ledger,
        &mut retry,
        &root,
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &CollidingLanes,
    );

    std::env::remove_var(crate::parallel::LANES_ENV);

    // The retry of the conflicted task, whichever one lost the race.
    let retried = retry
        .seen
        .iter()
        .find(|t| t.unmerged_branch.is_some())
        .unwrap_or_else(|| {
            panic!(
                "no retried task was told where its unmerged work is: {:?}",
                retry
                    .seen
                    .iter()
                    .map(|t| (t.id.clone(), t.unmerged_branch.clone()))
                    .collect::<Vec<_>>()
            )
        });
    let branch = retried
        .unmerged_branch
        .clone()
        .expect("filtered on Some above");

    // NAMED A BRANCH THAT REALLY EXISTS. A name nobody can check out is
    // worse than none: it sends the next attempt after work that is not
    // there and it looks, from the ledger, as though nothing was lost.
    let exists = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .output()
        .expect("git rev-parse");
    assert!(
        exists.status.success(),
        "the task names branch `{branch}`, which the repository does not have"
    );

    // AND THE PROMPT SAYS IT. The field is only useful if it reaches the
    // agent's instructions.
    let told = task_instruction(retried);
    assert!(
        told.contains(&branch),
        "the retry's instruction never names `{branch}`: {told}"
    );
}

/// **THE TOTAL CANNOT SAY WHICH AGENT SPENT IT.**
///
/// The ceiling answers "is there budget left". An operator running
/// several models across several subscriptions is asking which one is
/// burning it, and that question has no answer in a single number.
#[test]
fn spend_is_attributed_to_the_agent_that_did_the_work() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };

    let outcome = run_scheduling(
        &mut ledger,
        &mut SpendingWorker { calls: 0 },
        dir.path(),
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
    );

    assert_eq!(
        outcome.spend_by_agent.len(),
        1,
        "{:?}",
        outcome.spend_by_agent
    );
    let (agent, spent) = &outcome.spend_by_agent[0];
    assert!(agent.contains("thrifty"), "the agent is unnamed: {agent}");
    assert_eq!(*spent, 2_000, "two tasks at 1000 each");

    let report = outcome.spend_report().expect("something was spent");
    assert!(report.contains("2000"), "got: {report}");
    assert!(
        report.contains("100%"),
        "one agent spent all of it: {report}"
    );
}

/// A run that spent nothing prints no table.
#[test]
fn a_run_that_spent_nothing_has_nothing_to_report() {
    let outcome = RunOutcome {
        stop: StopReason::LedgerEmpty,
        completed: Vec::new(),
        attempted: 0,
        spend_by_agent: Vec::new(),
    };
    assert_eq!(outcome.spend_report(), None);
}

/// Reports a fixed cost per task, under a nameable identity.
struct SpendingWorker {
    calls: usize,
}

impl TaskWorker for SpendingWorker {
    fn work(&mut self, _task: &Task) -> Result<String, String> {
        self.calls += 1;
        Ok("did it".to_string())
    }
    fn spent_since_last_call(&mut self) -> crate::runtime::usage::TokenUsage {
        crate::runtime::usage::TokenUsage {
            input_tokens: 1_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }
    fn attribution(&self, task: &Task) -> crate::identity::Attribution {
        crate::identity::Attribution::new(crate::identity::AgentId::new(
            "thrifty",
            task.role().unwrap_or("grunt"),
        ))
    }
}

/// A lane that burns tokens and reports it, like a real agent.
struct ExpensiveLanes;

impl LaneWorkers for ExpensiveLanes {
    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn TaskWorker + Send>> {
        Some(Box::new(Spender {
            workspace: workspace.to_path_buf(),
        }))
    }
}

struct Spender {
    workspace: std::path::PathBuf,
}

impl TaskWorker for Spender {
    fn work(&mut self, task: &Task) -> Result<String, String> {
        std::fs::write(self.workspace.join(format!("{}.txt", task.id)), "x\n")
            .map_err(|e| e.to_string())?;
        Ok("done".to_string())
    }
    fn spent_since_last_call(&mut self) -> crate::runtime::usage::TokenUsage {
        crate::runtime::usage::TokenUsage {
            input_tokens: 10_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }
}

/// **WHAT A LANE SPENDS MUST COUNT AGAINST THE CEILING.**
///
/// The runner bounds a run by recording what ITS OWN worker spent. A lane
/// is a different worker, on a different thread, that the runner never
/// touches — so without carrying the usage back, every token a lane spends
/// is invisible and the ceiling silently stops applying to precisely the
/// mode that spends fastest.
///
/// A budget that is not enforced is worse than no budget: somebody set it
/// and believes in it.
#[test]
fn what_a_lane_spends_is_charged_to_the_run() {
    let _env = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((_guard, root)) = a_repo() else {
        return;
    };
    std::env::set_var(crate::parallel::LANES_ENV, "2");

    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };
    let mut budget = Budget::with_ceiling(1_000_000);

    let _ = run_watched(
        &mut ledger,
        &mut NeverWorker,
        &root,
        &mut budget,
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &ExpensiveLanes,
    );

    std::env::remove_var(crate::parallel::LANES_ENV);

    assert!(
        budget.spent() >= 20_000,
        "two lanes spent 10k each and the run was charged {}",
        budget.spent()
    );
}

/// **ONE LANE CHANGES NOTHING.** The default, and the guarantee that
/// nobody's run alters shape because they upgraded.
#[test]
fn a_single_lane_never_creates_a_worktree() {
    let _env = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((_guard, root)) = a_repo() else {
        return;
    };
    std::env::remove_var(crate::parallel::LANES_ENV);

    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let lanes = FileWritingLanes {
        seen: std::sync::Arc::clone(&seen),
    };

    let outcome = run_watched(
        &mut ledger,
        &mut LyingWorker { calls: 0 },
        &root,
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &lanes,
    );

    assert_eq!(outcome.stop, StopReason::LedgerEmpty);
    assert!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a lane was opened for a run that asked for one"
    );
}

/// A pool that says up front it cannot supply workers — and records it if it
/// is asked for one anyway.
struct DecliningLanes {
    seen: std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>,
}

impl LaneWorkers for DecliningLanes {
    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn TaskWorker + Send>> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(workspace.to_path_buf());
        None
    }

    fn available(&self) -> bool {
        false
    }
}

/// **A POOL THAT CANNOT SERVE LEAVES THE RUN AS IT WAS.** Two lanes asked
/// for, two tasks ready, a repository — and a pool that says no. The run
/// works both tasks sequentially, and no lane is ever opened for a batch that
/// would never run.
#[test]
fn a_pool_that_cannot_supply_workers_leaves_the_run_sequential() {
    let _env = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((_guard, root)) = a_repo() else {
        return;
    };
    std::env::set_var(crate::parallel::LANES_ENV, "2");

    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let lanes = DecliningLanes {
        seen: std::sync::Arc::clone(&seen),
    };
    let mut worker = LyingWorker { calls: 0 };

    let outcome = run_watched(
        &mut ledger,
        &mut worker,
        &root,
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &lanes,
    );

    std::env::remove_var(crate::parallel::LANES_ENV);

    assert_eq!(outcome.stop, StopReason::LedgerEmpty, "{:?}", outcome.stop);
    assert_eq!(worker.calls, 2, "both tasks were worked here, in turn");
    assert!(
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "a pool that declined was asked for a worker anyway"
    );
}

/// A forge that knows the default branch and refuses everything else,
/// keeping a note of whatever it was asked to do.
#[derive(Default)]
struct DeskForge {
    asked: std::sync::Mutex<Vec<&'static str>>,
}

impl DeskForge {
    fn refuse<T>(&self, what: &'static str) -> Result<T, String> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(what);
        Err(format!("{what} is not part of this test"))
    }
}

impl crate::tools::pull_request::Forge for DeskForge {
    fn default_branch(&self, _repo: &Path) -> Result<String, String> {
        Ok("main".to_string())
    }
    fn open_pr(&self, _repo: &Path, _branch: &str) -> Result<Option<String>, String> {
        self.refuse("open_pr")
    }
    fn create(
        &self,
        _repo: &Path,
        _new: &crate::tools::pull_request::NewPr,
    ) -> Result<String, String> {
        self.refuse("create")
    }
    fn pr_status(
        &self,
        _repo: &Path,
        _branch: &str,
    ) -> Result<Option<crate::tools::pull_request::PrStatus>, String> {
        self.refuse("pr_status")
    }
    fn merge(
        &self,
        _repo: &Path,
        _number: u64,
        _method: crate::tools::pull_request::MergeMethod,
        _head: &str,
    ) -> Result<String, String> {
        self.refuse("merge")
    }
    fn review(
        &self,
        _repo: &Path,
        _pr: &str,
    ) -> Result<crate::tools::pull_request::Review, String> {
        self.refuse("review")
    }
}

/// Lanes asked to be opened as pull requests on [`DeskForge`].
struct ReviewedLanes {
    forge: DeskForge,
}

impl LaneWorkers for ReviewedLanes {
    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn TaskWorker + Send>> {
        Some(Box::new(LaneScribe {
            workspace: workspace.to_path_buf(),
        }))
    }

    fn forge(&self) -> Option<&dyn crate::tools::pull_request::Forge> {
        Some(&self.forge)
    }

    fn open_for_review(&self) -> bool {
        true
    }
}

/// **A BRANCH AHEAD OF ITS REMOTE IS MERGED HERE, NOT OPENED FOR REVIEW.**
/// Pull requests were asked for and the branch is on origin — but HEAD has a
/// commit origin lacks, which every pull request would carry. So the batch is
/// merged here: both tasks land and are proved, and the forge is never asked
/// to open anything.
#[test]
fn lanes_asked_for_review_on_a_branch_ahead_of_origin_are_merged_here() {
    let _env = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some((_guard, root)) = a_repo() else {
        return;
    };
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // On origin as it was, then one commit ahead of it.
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).expect("branch");
    git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/repo.git",
    ])
    .expect("remote");
    git(&[
        "update-ref",
        &format!("refs/remotes/origin/{branch}"),
        "HEAD",
    ])
    .expect("origin ref");
    std::fs::write(root.join("ahead.txt"), "not pushed\n").expect("write");
    git(&["add", "-A"]).expect("add");
    git(&["commit", "-m", "ahead of origin"]).expect("commit");
    std::env::set_var(crate::parallel::LANES_ENV, "2");

    let mut ledger = Ledger {
        version: 1,
        tasks: vec![passing_task("t-1"), passing_task("t-2")],
    };
    let lanes = ReviewedLanes {
        forge: DeskForge::default(),
    };

    let outcome = run_watched(
        &mut ledger,
        &mut NeverWorker,
        &root,
        &mut Budget::default(),
        &never_cancelled(),
        &mut NoOwner,
        &mut NoJournal,
        &mut Unwatched,
        &mut NoReplan,
        &mut AllGoalsReached,
        &lanes,
    );

    std::env::remove_var(crate::parallel::LANES_ENV);

    assert_eq!(outcome.stop, StopReason::LedgerEmpty, "{:?}", outcome.stop);
    assert!(
        root.join("t-1.txt").exists(),
        "lane one was not merged here"
    );
    assert!(
        root.join("t-2.txt").exists(),
        "lane two was not merged here"
    );
    assert!(
        lanes
            .forge
            .asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "the forge was asked to open a pull request for unpushed work"
    );
}
