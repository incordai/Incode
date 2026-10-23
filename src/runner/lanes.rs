//! Several ready tasks at once, each in its own checkout.
//!
//! Lanes only produce REPORTS. Each is then consumed by the ordinary loop
//! exactly as if the agent had just spoken, and the check runs against the
//! merged tree — see [`LaneReports`] for why.

use std::path::Path;

use super::TaskWorker;
use crate::budget::Budget;
use crate::ledger::{Ledger, Task, TaskState};

/// Supplies a worker per lane, each pointed at its own checkout.
///
/// # Why a factory and not the worker the runner already has
///
/// Lanes run at the same time, so each needs its OWN worker — a different
/// agent process, working in a different directory. The runner holds one
/// `&mut dyn TaskWorker`, which is exactly one thing and cannot be in two
/// places.
///
/// It is a trait for the same reason [`OwnerChannel`] and [`RoadmapGoals`]
/// are: the real one builds an agent CLI backend, and a loop whose parallel
/// path can only be exercised by spawning real agents is a path nobody tests.
/// `Sync` because the factory is called from every lane's thread at once. It
/// is the one thing all the lanes share, so it is the one thing that has to be
/// safe to share.
pub trait LaneWorkers: Sync {
    /// A worker that will work in `workspace`.
    ///
    /// `None` means lanes are not available — and that is the DEFAULT, not a
    /// failure. The run proceeds exactly as it always has.
    fn spawn_for(&self, workspace: &Path) -> Option<Box<dyn TaskWorker + Send>>;

    /// Whether this pool can supply workers at all.
    ///
    /// Asked BEFORE any worktree is created, so a pool that cannot serve does
    /// not leave a directory per task on disk for a batch that never ran.
    /// Separate from [`Self::spawn_for`] because asking "could you?" must not
    /// build one — an implementation that counts what it hands out would
    /// count a worker nobody ever used.
    fn available(&self) -> bool {
        true
    }

    /// Where pull requests are opened and read, when there is a forge.
    ///
    /// Read at the start of every run whenever a task is waiting on one
    /// ([`crate::reviews::settle`]), so a review opened under `INCODE_LANES_AS=pr` is
    /// still settled by a run without it. `None`: no forge, nothing settled.
    fn forge(&self) -> Option<&dyn crate::tools::pull_request::Forge> {
        None
    }

    /// Whether a lane whose check passed is opened as a pull request on
    /// [`Self::forge`] instead of merged here.
    fn open_for_review(&self) -> bool {
        false
    }
}

/// No lanes: one agent, one tree, today's behaviour.
///
/// The default, and deliberately so. Parallelism moves where an agent's files
/// live, and that must never start happening because somebody upgraded.
pub struct NoLanes;

impl LaneWorkers for NoLanes {
    fn spawn_for(&self, _workspace: &Path) -> Option<Box<dyn TaskWorker + Send>> {
        None
    }

    fn available(&self) -> bool {
        false
    }
}

/// Reports produced by a batch of lanes, waiting to be settled one at a time.
///
/// # Why the results are cached rather than settled in the batch
///
/// The runner's law — attempts, escalation, the owner channel, and the single
/// edge into `Done` through [`crate::verify`] — is written once, in the
/// sequential loop below. Re-implementing it for the parallel path would mean
/// two copies of the rule that decides whether work is finished, and the one
/// that gets fixed would not always be the one that ran.
///
/// So lanes only produce REPORTS. Each is then consumed by the ordinary loop
/// exactly as if the agent had just spoken, and the check runs against the
/// merged tree — which is the only tree that matters.
#[derive(Default)]
pub(super) struct LaneReports {
    pub(super) ready: std::collections::HashMap<String, Result<String, String>>,
    /// What to tell the operator about each merge, in order.
    pub(super) notes: Vec<String>,
}

/// What one round of the loop hands the lanes: where they run, who supplies
/// their workers, how to stop, and where passing work lands.
///
/// # Wired to
///
/// - **Built by** `LedgerLoop::run_lanes`, once per round of the loop.
/// - **Read by** [`run_lanes_if_useful`] and the steps it calls.
pub(super) struct LaneRound<'a> {
    pub(super) workspace: &'a Path,
    pub(super) isolation: &'a crate::parallel::Isolation,
    pub(super) lanes: &'a dyn LaneWorkers,
    pub(super) cancelled: &'a dyn Fn() -> bool,
    /// The forge and base branch a passing lane is opened onto as a pull
    /// request, or `None` to merge here.
    pub(super) review: Option<(&'a dyn crate::tools::pull_request::Forge, &'a str)>,
}

/// Work several ready tasks at once, if that is both possible and worth it.
///
/// Does nothing — and that is the ordinary case — when any of these is true:
/// reports from a previous batch are still unconsumed, the operator asked for
/// one lane, the workspace is not a git repository, fewer than two tasks are
/// ready, or the pool declines to supply workers. Each leaves the sequential
/// loop exactly as it was.
///
/// Called by `LedgerLoop::run_lanes`, before each task is picked.
pub(super) fn run_lanes_if_useful(
    round: &LaneRound<'_>,
    ledger: &mut Ledger,
    reports: &mut LaneReports,
    budget: &mut Budget,
) {
    let Some((repo_root, batch)) = batch_worth_running(round, ledger, reports) else {
        return;
    };

    // Each lane's check runs in its own checkout, in the folder matching this
    // workspace, before its work may be merged (`INCODE_LANE_CHECK=0` turns
    // that gate off; the check after the merge runs either way).
    let check_in = crate::parallel::lane_check_wanted()
        .then(|| crate::parallel::workspace_prefix(round.workspace));
    let landing = landing_for(round.review, repo_root, &mut reports.notes);
    let outcomes = crate::parallel::run_batch(
        repo_root,
        &batch,
        check_in.as_deref(),
        &landing,
        |lane_workspace| {
            round.lanes.spawn_for(lane_workspace).unwrap_or_else(|| {
                // The probe above said yes. A pool that then changes its mind
                // must not take the batch down with it: this lane reports a
                // failure and the task goes back to the queue.
                Box::new(UnavailableWorker)
            })
        },
    );

    for outcome in outcomes {
        settle_lane(outcome, round.workspace, ledger, reports, budget);
    }
}

/// The repository and the tasks for a batch, or `None` when a batch is not
/// possible or not worth it — every reason listed on [`run_lanes_if_useful`].
///
/// Called by [`run_lanes_if_useful`], before anything is created on disk.
fn batch_worth_running<'a>(
    round: &LaneRound<'a>,
    ledger: &Ledger,
    reports: &LaneReports,
) -> Option<(&'a Path, Vec<Task>)> {
    if !reports.ready.is_empty() || (round.cancelled)() {
        return None;
    }
    let crate::parallel::Isolation::Worktrees { repo_root } = round.isolation else {
        return None;
    };

    let ready = ledger
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Ready)
        .count();
    let width =
        crate::parallel::lanes_for(crate::parallel::requested_lanes(), round.isolation, ready);
    if width < 2 {
        return None;
    }

    let batch = crate::parallel::batch(ledger, width);
    if batch.len() < 2 {
        return None;
    }

    // ASKED BEFORE ANY WORKTREE IS CREATED. A pool that cannot supply workers
    // would otherwise leave a directory per task on disk for a batch that
    // never ran.
    if !round.lanes.available() {
        return None;
    }
    Some((repo_root.as_path(), batch))
}

/// Where this batch's passing work goes: opened for review, or merged here.
///
/// Opened for review rather than merged, when the run was started for that —
/// unless the branch has commits the remote lacks, which every pull request
/// would then carry. Asked per batch, since the run can move HEAD.
///
/// Called by [`run_lanes_if_useful`], once per batch.
fn landing_for<'a>(
    review: Option<(&'a dyn crate::tools::pull_request::Forge, &str)>,
    repo_root: &Path,
    notes: &mut Vec<String>,
) -> crate::parallel::Landing<'a> {
    match review {
        Some((forge, base)) => match crate::parallel::nothing_unpushed(repo_root, base) {
            Ok(()) => crate::parallel::Landing::Review {
                forge,
                base: base.to_string(),
            },
            Err(why) => {
                notes.push(format!(
                    "this batch is merged here, not opened for review: {why}"
                ));
                crate::parallel::Landing::Here
            }
        },
        None => crate::parallel::Landing::Here,
    }
}

/// Take one lane's outcome into the run: charge it, say what happened to it,
/// and leave its report for the loop to settle.
///
/// Called by [`run_lanes_if_useful`], once per lane in the batch.
fn settle_lane(
    outcome: crate::parallel::LaneOutcome,
    workspace: &Path,
    ledger: &mut Ledger,
    reports: &mut LaneReports,
    budget: &mut Budget,
) {
    // CHARGED FOR EVERY LANE, including the ones that failed or could not
    // merge. A lane that spent tokens and produced nothing usable spent
    // them all the same, and a ceiling that only counted successes would
    // never stop a run that is going badly — which is the only run whose
    // ceiling matters.
    budget.record(outcome.spent);

    // The lane the outcome came from, rebuilt so the summary can name the
    // BRANCH. It used to be rebuilt with an empty branch, which made every
    // conflict note say the work was "kept on branch ``" — pointing the
    // operator at nothing, in the one message whose whole job is to say
    // where the work went.
    let lane = crate::parallel::Lane {
        task_id: outcome.task.id.clone(),
        workspace: workspace.to_path_buf(),
        branch: outcome.branch.clone(),
    };
    reports.notes.push(outcome.merged.summary(&lane));

    remember_unmerged(ledger, &outcome);

    // OPENED FOR REVIEW: not done and not failed. It waits on a person,
    // out of the queue, and no report is cached — the runner must not
    // check a tree that does not hold the change. `reviews::settle` picks
    // it up once the pull request is merged or closed.
    if let crate::parallel::Merged::Opened { url } = &outcome.merged {
        if let Err(why) = ledger.await_review(&outcome.task.id, url, &outcome.branch) {
            reports.notes.push(why);
        }
        return;
    }

    // A LANE THAT DID NOT LAND IS A FAILED ATTEMPT, whatever the agent
    // said. Its work is on a branch, not in the tree the check is about
    // to run over — so reporting success here would have the runner prove
    // a tree that does not contain the change and call the task done.
    let report = match (&outcome.report, outcome.merged.landed()) {
        (Ok(said), true) => Ok(said.clone()),
        (Ok(_), false) => Err(outcome.merged.summary(&lane)),
        (Err(why), _) => Err(why.clone()),
    };
    reports.ready.insert(outcome.task.id.clone(), report);
}

/// WHERE THE UNMERGED WORK IS, written onto the task itself so the retry can
/// be pointed at it.
///
/// Recorded for a conflict — a real change on a branch, with the tree moved
/// underneath it — and for a lane whose finished work failed its own check: a
/// whole change the retry can read and correct rather than redo. A lane whose
/// agent failed left no change worth reconciling, and telling the next attempt
/// to go and merge one would send it after a branch with nothing on it.
///
/// Called by [`settle_lane`], for every lane, before its report is kept.
fn remember_unmerged(ledger: &mut Ledger, outcome: &crate::parallel::LaneOutcome) {
    if let Some(entry) = ledger.get_mut_public(&outcome.task.id) {
        match &outcome.merged {
            crate::parallel::Merged::Conflict { .. }
            | crate::parallel::Merged::CheckFailed { .. }
                if !outcome.branch.is_empty() =>
            {
                entry.unmerged_branch = Some(outcome.branch.clone());
            }
            // Landed, so any branch remembered from an earlier round is
            // stale. Clearing it here as well as on completion keeps a
            // task that merged this time from being told to reconcile
            // last time's branch.
            m if m.landed() => entry.unmerged_branch = None,
            _ => {}
        }
    }
}

/// Stands in for a lane worker a pool declined to supply mid-batch.
struct UnavailableWorker;

impl TaskWorker for UnavailableWorker {
    fn work(&mut self, _task: &Task) -> Result<String, String> {
        Err("no agent was available for this lane".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that would supply workers, and is never asked to here.
    struct Willing;

    impl LaneWorkers for Willing {
        fn spawn_for(&self, _workspace: &Path) -> Option<Box<dyn TaskWorker + Send>> {
            None
        }
    }

    fn two_ready() -> Ledger {
        let mut tasks = vec![
            Task::new("t-1", "r-1", "one"),
            Task::new("t-2", "r-2", "two"),
        ];
        for task in &mut tasks {
            task.state = TaskState::Ready;
        }
        Ledger { version: 1, tasks }
    }

    /// **EVERY REASON NOT TO RUN A BATCH IS ITS OWN REASON.** With two ready
    /// tasks, two lanes asked for, a repository and a willing pool, a batch
    /// runs — and taking away any ONE of those, or leaving a report from the
    /// last batch unconsumed, or cancelling, means no worktree is made.
    #[test]
    fn a_batch_runs_only_when_every_condition_holds() {
        let _env = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::parallel::LANES_ENV, "2");

        let ledger = two_ready();
        let root = std::path::PathBuf::from("repo-root");
        let worktrees = crate::parallel::Isolation::Worktrees {
            repo_root: root.clone(),
        };
        let none = crate::parallel::Isolation::None {
            why: "not a git repository".to_string(),
        };
        fn round<'a>(
            isolation: &'a crate::parallel::Isolation,
            lanes: &'a dyn LaneWorkers,
            cancelled: &'a dyn Fn() -> bool,
        ) -> LaneRound<'a> {
            LaneRound {
                workspace: Path::new("."),
                isolation,
                lanes,
                cancelled,
                review: None,
            }
        }
        let carry_on = || false;
        let stop = || true;
        let empty = LaneReports::default();
        let mut waiting = LaneReports::default();
        waiting
            .ready
            .insert("t-0".to_string(), Ok("from the last batch".to_string()));

        let runs = batch_worth_running(&round(&worktrees, &Willing, &carry_on), &ledger, &empty);
        let refused = [
            batch_worth_running(&round(&worktrees, &Willing, &stop), &ledger, &empty),
            batch_worth_running(&round(&worktrees, &Willing, &carry_on), &ledger, &waiting),
            batch_worth_running(&round(&none, &Willing, &carry_on), &ledger, &empty),
            batch_worth_running(&round(&worktrees, &NoLanes, &carry_on), &ledger, &empty),
        ];
        std::env::remove_var(crate::parallel::LANES_ENV);

        let (at, batch) = runs.expect("every condition holds, so a batch runs");
        assert_eq!(at, root.as_path());
        assert_eq!(
            batch.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t-1", "t-2"]
        );
        for (which, outcome) in refused.iter().enumerate() {
            assert!(outcome.is_none(), "refusal {which} still ran a batch");
        }
    }

    /// **A BRANCH THAT CANNOT BE CHECKED AGAINST ITS REMOTE IS MERGED HERE,
    /// AND SAYS WHY.** Opening it for review would carry commits nobody asked
    /// to review; landing it silently here would leave the operator who asked
    /// for pull requests wondering where they went.
    #[test]
    fn a_batch_that_cannot_be_opened_for_review_is_merged_here_and_says_why() {
        let dir = tempfile::tempdir().expect("temp dir");
        let forge = crate::tools::pull_request::GitHubCli;

        let mut notes = Vec::new();
        let landing = landing_for(Some((&forge, "main")), dir.path(), &mut notes);
        assert!(matches!(landing, crate::parallel::Landing::Here));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("this batch is merged here, not opened for review: "),
            "{notes:?}"
        );

        // Not asked for review at all: merged here, and nothing to explain.
        let mut quiet = Vec::new();
        let landing = landing_for(None, dir.path(), &mut quiet);
        assert!(matches!(landing, crate::parallel::Landing::Here));
        assert!(quiet.is_empty(), "{quiet:?}");
    }
}
