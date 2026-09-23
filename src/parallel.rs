//! Running more than one agent at once, without them overwriting each other.
//!
//! # Why this is not just "spawn N workers"
//!
//! Measured on this ledger, not assumed: **every task a plan produces writes
//! code.** [`crate::ledger::Ledger::from_plan`] folds the read-only roles —
//! `explore`, `diagnose`, `review` — into a writing task's title as
//! preparation, because a step that only produces information cannot be proved
//! by a build. And `Task::with_dependencies` had no production caller until
//! the planner learned to declare ordering (`plan::Step::after`), so a plan
//! that says nothing still produces tasks that are all ready at once.
//!
//! So the two facts that decide the design are that all the work is WRITES and
//! none of it is ordered. Running two of those in one directory is not slow or
//! untidy, it is wrong: each agent edits files the other is reading, and then
//! [`crate::verify`] runs one command over a tree neither of them intended and
//! mints a `Verified` from it. The check would pass. It would be proving
//! something nobody wrote.
//!
//! # So isolation is the precondition, not an optimisation
//!
//! A lane is a git worktree: its own directory, its own branch, off the same
//! base commit. Agents cannot see each other's edits, and each one's check
//! runs over a tree exactly one agent produced.
//!
//! **Without git there is no isolation and therefore no parallelism.** This
//! does not error — it reports one lane and the run proceeds exactly as it
//! always has. A harness that refused to work in a directory that is not a
//! repository would be trading a feature nobody asked for against the thing
//! people actually do.
//!
//! # The merge is where correctness is won or lost
//!
//! Lanes rejoin by merging each branch back in turn. A conflict is NOT
//! resolved here and never will be: two agents changed the same lines, and
//! picking one is a judgement about the work, which is the one thing this
//! harness does not do on its own. The lane fails, its branch is left intact
//! with the work on it, and the task goes back to the ledger to be done again
//! — sequentially, where it can see what the other agent did.

use std::path::{Path, PathBuf};

/// How many lanes an operator asked for.
///
/// One means today's behaviour exactly: no worktrees, no threads, no merge.
/// That is the default, because parallelism changes where an agent's files
/// live and that should never happen because somebody upgraded.
pub const LANES_ENV: &str = "INCORD_LANES";

/// More than this and the lanes contend for the machine rather than the work.
///
/// Each lane is an agent CLI, which is a process with a model behind it and a
/// full checkout to read. The limit is here so a mistyped `INCORD_LANES=40`
/// costs a clamp rather than a machine.
pub const MAX_LANES: usize = 8;

/// Whether this workspace can run lanes at all, and why not when it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Isolation {
    /// Lanes are possible: this is a git repository, rooted here.
    Worktrees { repo_root: PathBuf },
    /// One lane only, with the reason an operator can act on.
    None { why: String },
}

impl Isolation {
    /// Whether more than one lane is possible.
    #[must_use]
    pub fn allows_lanes(&self) -> bool {
        matches!(self, Isolation::Worktrees { .. })
    }

    /// What to tell an operator who asked for lanes and got one.
    #[must_use]
    pub fn why_not(&self) -> Option<&str> {
        match self {
            Isolation::Worktrees { .. } => None,
            Isolation::None { why } => Some(why),
        }
    }
}

/// Whether `workspace` is inside a git repository, and where its root is.
///
/// Asks git rather than looking for a `.git` directory: a worktree's `.git` is
/// a FILE, and a repository can be somewhere above the workspace. Looking for
/// the directory would report "not a repository" in exactly the setup this
/// feature runs in.
#[must_use]
pub fn detect_isolation(workspace: &Path) -> Isolation {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--show-toplevel"])
        .output();

    match output {
        Err(e) => Isolation::None {
            why: format!("git could not be run ({e}), so lanes cannot be isolated"),
        },
        Ok(out) if !out.status.success() => Isolation::None {
            why: format!(
                "{} is not a git repository, and a lane needs a worktree to keep \
                 one agent's edits away from another's",
                workspace.display()
            ),
        },
        Ok(out) => {
            let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if root.is_empty() {
                return Isolation::None {
                    why: "git named no repository root".to_string(),
                };
            }
            Isolation::Worktrees {
                repo_root: PathBuf::from(root),
            }
        }
    }
}

/// How many lanes the operator asked for, from the environment.
///
/// An unreadable or absent value is one lane, never an error: this decides how
/// work is laid out on disk, and a typo must fall back to the behaviour that
/// has always worked rather than stopping the run.
#[must_use]
pub fn requested_lanes() -> usize {
    std::env::var(LANES_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_LANES)
}

/// What to tell an operator who asked for lanes and is not getting them.
///
/// `None` when there is nothing to say — either they did not ask, or they are
/// getting what they asked for.
///
/// # Why this is a function and not an `if` at the call site
///
/// The reason for a single lane was already computed, carried on
/// `Isolation::None`, and returned by [`Isolation::why_not`] — and nothing
/// ever asked. An operator who set `INCORD_LANES=4` on a workspace that is
/// not a git repository got a sequential run indistinguishable from one that
/// simply had nothing to parallelise.
///
/// Putting the decision here rather than inline in the runner means the rule
/// — *say it only when they asked* — can be tested. What is left at the call
/// site is one print.
#[must_use]
pub fn shortfall_note(requested: usize, isolation: &Isolation) -> Option<String> {
    // A default run has not asked for lanes. Explaining why it did not get
    // them is noise about a feature nobody reached for.
    if requested <= 1 {
        return None;
    }
    isolation
        .why_not()
        .map(|why| format!("lanes: {requested} requested, running one at a time — {why}"))
}

/// How many lanes this run will actually use.
///
/// Never more than there is work for: three lanes and two ready tasks is two
/// worktrees created, one of them for nothing.
#[must_use]
pub fn lanes_for(requested: usize, isolation: &Isolation, ready: usize) -> usize {
    if !isolation.allows_lanes() {
        return 1;
    }
    requested.clamp(1, MAX_LANES).min(ready.max(1))
}

/// The tasks to run together, oldest first.
///
/// # Why every ready task is eligible
///
/// They are all writes, and they are all unordered — so there is no subset
/// that is "safe to parallelise" and no ordering to respect. What makes them
/// safe is the worktree, not the choice. Picking in ledger order keeps a run
/// reading the same way it did when it was sequential.
#[must_use]
pub fn batch(ledger: &crate::ledger::Ledger, lanes: usize) -> Vec<crate::ledger::Task> {
    if lanes <= 1 {
        return ledger.next_ready().cloned().into_iter().collect();
    }
    ledger
        .tasks
        .iter()
        .filter(|t| t.state == crate::ledger::TaskState::Ready)
        .take(lanes)
        .cloned()
        .collect()
}

/// A lane's own checkout, and the branch its work lands on.
#[derive(Debug, Clone)]
pub struct Lane {
    /// The task this lane is working.
    pub task_id: String,
    /// Where the agent works. A directory of its own.
    pub workspace: PathBuf,
    /// The branch holding this lane's commits.
    pub branch: String,
}

/// What happened when a lane's work was merged back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// The branch went in cleanly.
    Clean,
    /// Nothing to merge: the agent changed no files.
    Nothing,
    /// Two lanes touched the same lines. NOT resolved here.
    Conflict { files: Vec<String> },
    /// The merge could not be attempted.
    Failed { why: String },
}

impl Merged {
    /// Whether the lane's work is now in the main tree.
    #[must_use]
    pub fn landed(&self) -> bool {
        matches!(self, Merged::Clean | Merged::Nothing)
    }

    /// What to record about this lane.
    #[must_use]
    pub fn summary(&self, lane: &Lane) -> String {
        match self {
            Merged::Clean => format!("`{}` merged from {}", lane.task_id, lane.branch),
            Merged::Nothing => format!("`{}` changed nothing", lane.task_id),
            Merged::Conflict { files } => format!(
                "`{}` CONFLICTS with work another lane did, in {}. Its work is kept on \
                 branch `{}` and the task goes back to the queue to be done again in \
                 one tree, where it can see the other change. Nothing was merged and \
                 nothing was discarded.",
                lane.task_id,
                files.join(", "),
                lane.branch
            ),
            Merged::Failed { why } => {
                format!("`{}` could not be merged: {why}", lane.task_id)
            }
        }
    }
}

/// Run git in a directory and return stdout, or the error it printed.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Give each task its own checkout, branched from the same commit.
///
/// Every lane starts from ONE base, taken once before any of them opens. Read
/// per lane it would drift — the first lane's merge would move `HEAD`, and the
/// lanes after it would be branched off work their agent never saw and never
/// agreed to build on.
///
/// # Errors
///
/// The first failure, with the lanes already opened returned alongside so a
/// caller can close them rather than leak worktrees.
pub fn open_lanes(repo_root: &Path, tasks: &[crate::ledger::Task]) -> (Vec<Lane>, Option<String>) {
    let base = match git(repo_root, &["rev-parse", "HEAD"]) {
        Ok(commit) => commit,
        Err(why) => {
            return (
                Vec::new(),
                Some(format!("no base commit to branch from: {why}")),
            )
        }
    };

    let manager = crate::tools::worktree::WorktreeManager::new(repo_root.to_path_buf());
    let mut lanes = Vec::new();
    for task in tasks {
        match manager.create(&task.id, Some(&base)) {
            Ok(worktree) => lanes.push(Lane {
                task_id: task.id.clone(),
                workspace: worktree.path,
                branch: worktree.branch,
            }),
            Err(why) => return (lanes, Some(format!("lane for `{}`: {why}", task.id))),
        }
    }
    (lanes, None)
}

/// Commit whatever the agent left in a lane, so there is something to merge.
///
/// # Why the harness commits and not the agent
///
/// An agent that was told to commit might not, might commit half, or might
/// commit the whole tree including the files it was told not to touch. The
/// lane's value is that its diff is exactly what one agent did to one
/// checkout, and that is only true if the same thing snapshots every lane the
/// same way.
///
/// `Ok(false)` means the agent changed nothing — not a failure, and not
/// something to merge.
pub fn seal_lane(lane: &Lane) -> Result<bool, String> {
    git(&lane.workspace, &["add", "-A"])?;
    // `diff --cached --quiet` exits non-zero when there IS something staged,
    // which is why this reads as an error meaning "yes".
    if git(&lane.workspace, &["diff", "--cached", "--quiet"]).is_ok() {
        return Ok(false);
    }
    git(
        &lane.workspace,
        &["commit", "-m", &format!("incord lane: {}", lane.task_id)],
    )?;
    Ok(true)
}

/// Merge one lane's branch into the tree the run started from.
///
/// # A conflict is never resolved here
///
/// Two agents changed the same lines. Choosing between them is a judgement
/// about the WORK, and this harness does not make those — the whole
/// [`crate::verify`] design exists because an automated judgement in that seat
/// is how both previous autonomous loops failed.
///
/// So the merge is aborted, the branch is left exactly as the agent wrote it,
/// and the caller puts the task back. Nothing is merged and nothing is lost.
pub fn merge_lane(repo_root: &Path, lane: &Lane) -> Merged {
    match seal_lane(lane) {
        Ok(false) => return Merged::Nothing,
        Ok(true) => {}
        Err(why) => return Merged::Failed { why },
    }

    // `--no-ff` so the lane is visible as a unit in the history: a run that
    // went wrong is read by asking which lane did what, and a fast-forward
    // would erase exactly that boundary.
    match git(
        repo_root,
        &[
            "merge",
            "--no-ff",
            "-m",
            &format!("incord: merge {}", lane.task_id),
            &lane.branch,
        ],
    ) {
        Ok(_) => Merged::Clean,
        Err(why) => {
            let files = git(repo_root, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();

            // ABORTED, always. A repository left mid-merge is one where the
            // next lane's merge, and the next task's check, run against a tree
            // with conflict markers in it.
            let _ = git(repo_root, &["merge", "--abort"]);

            if files.is_empty() {
                Merged::Failed { why }
            } else {
                Merged::Conflict { files }
            }
        }
    }
}

/// What one lane produced.
#[derive(Debug)]
pub struct LaneOutcome {
    pub task: crate::ledger::Task,
    /// What the agent claimed. A CLAIM — the runner still proves it.
    pub report: Result<String, String>,
    /// Whether the work reached the main tree.
    pub merged: Merged,
    /// The branch this lane committed to.
    ///
    /// Carried back because [`close_lane`] takes the checkout away and leaves
    /// the branch: when a merge conflicts, this name is the ONLY remaining
    /// pointer to work that has already been paid for. Empty when no lane was
    /// ever opened.
    pub branch: String,
    /// What this lane cost.
    ///
    /// # Why it has to come back
    ///
    /// The runner bounds a run by recording what its own worker spent. A lane
    /// is a DIFFERENT worker, in a different thread, that the runner never
    /// touches — so without carrying this, every token a lane spends is
    /// invisible and the ceiling silently stops applying to exactly the mode
    /// that spends fastest. A budget that is not enforced is worse than no
    /// budget, because somebody set it and believes in it.
    pub spent: crate::runtime::usage::TokenUsage,
}

/// Work a batch of tasks at once, each in its own checkout, then merge.
///
/// # What this deliberately does NOT do
///
/// It does not verify, does not touch the ledger, and does not decide that
/// anything is done. Those belong to [`crate::runner`], which owns the law
/// that the only edge into `Done` runs through [`crate::verify`]. This owns
/// the two things the runner cannot do from inside a sequential loop:
/// concurrency, and git.
///
/// # Why the check must run AFTER the merge, not in the lane
///
/// A lane's own tree contains one agent's work. Proving that tree proves the
/// agent did not break its own lane — which is not the question. Two lanes can
/// each pass alone and break the moment they are put together, which is the
/// ordinary result of two people editing one codebase. So the report comes
/// back as a claim and the runner checks the MERGED tree, serially, after.
///
/// # Merges are serial even though the work was not
///
/// `git merge` moves `HEAD`; two at once in one repository race. Merging in
/// order also means the second lane's conflict is reported against a tree
/// that actually contains the first lane's work, which is what a person needs
/// to resolve it.
pub fn run_batch<F>(
    repo_root: &Path,
    tasks: &[crate::ledger::Task],
    make_worker: F,
) -> Vec<LaneOutcome>
where
    F: Fn(&Path) -> Box<dyn crate::runner::TaskWorker + Send> + Sync,
{
    let (lanes, failure) = open_lanes(repo_root, tasks);
    if let Some(why) = failure {
        // Close whatever opened before giving up, or the next run inherits
        // orphaned worktrees and a repository nobody can reason about.
        for lane in &lanes {
            close_lane_or_report(repo_root, lane);
        }
        return tasks
            .iter()
            .map(|task| LaneOutcome {
                task: task.clone(),
                report: Err(format!("no lane could be opened: {why}")),
                merged: Merged::Failed { why: why.clone() },
                // No lane opened, so there is no branch to point anyone at.
                branch: String::new(),
                // Nothing ran, so nothing was spent.
                spent: crate::runtime::usage::TokenUsage::default(),
            })
            .collect();
    }

    // Scoped so the borrow of `lanes` and `make_worker` needs no Arc, and so
    // every thread is joined before anything is merged.
    type LaneResult = (Result<String, String>, crate::runtime::usage::TokenUsage);
    let mut reports: Vec<LaneResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = lanes
            .iter()
            .zip(tasks)
            .map(|(lane, task)| {
                let make = &make_worker;
                scope.spawn(move || {
                    let mut worker = make(&lane.workspace);
                    let report = worker.work(task);
                    // Read HERE, on the lane's own thread, because the worker
                    // is dropped the moment this closure ends and nothing
                    // outside can ask it afterwards.
                    (report, worker.spent_since_last_call())
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    // A panicked lane is a failed attempt, not a failed run.
                    // Propagating it would take down the other lanes' work,
                    // which is already committed to their branches. Nothing is
                    // charged: a lane that died mid-turn cannot say what it
                    // spent, and inventing a number would be worse than the
                    // undercount.
                    (
                        Err("the lane panicked".to_string()),
                        crate::runtime::usage::TokenUsage::default(),
                    )
                })
            })
            .collect()
    });

    lanes
        .iter()
        .zip(tasks)
        .zip(reports.drain(..))
        .map(|((lane, task), (report, spent))| {
            // A lane whose agent FAILED is not merged. Its work stays on the
            // branch: half a change that could not finish is the last thing
            // the main tree should be given, and the branch keeps it for
            // anyone who wants to look.
            let merged = if report.is_ok() {
                merge_lane(repo_root, lane)
            } else {
                Merged::Failed {
                    why: "the agent did not finish, so nothing was merged".to_string(),
                }
            };
            close_lane_or_report(repo_root, lane);
            LaneOutcome {
                task: task.clone(),
                report,
                merged,
                branch: lane.branch.clone(),
                spent,
            }
        })
        .collect()
}

/// Take a lane's checkout away without losing anything that was in it.
///
/// Whatever the agent left uncommitted is sealed onto the branch FIRST — a
/// lane whose agent failed never reached [`merge_lane`], so its half-done work
/// exists only in the checkout that is about to go. Then the checkout is
/// removed, and the branch goes with it only if git proves its work already
/// landed ([`crate::tools::worktree::WorktreeManager::remove`]). A conflicted
/// or failed lane's branch STAYS: it is the only copy of work already paid for.
///
/// # Errors
///
/// The work could not be sealed (the checkout is then left in place rather
/// than wiped), or git would not remove the checkout.
pub fn close_lane(repo_root: &Path, lane: &Lane) -> Result<(), String> {
    seal_lane(lane).map_err(|why| {
        format!(
            "kept the checkout at {}: its work could not be committed ({why})",
            lane.workspace.display()
        )
    })?;
    let manager = crate::tools::worktree::WorktreeManager::new(repo_root.to_path_buf());
    manager
        .remove(&lane.workspace.to_string_lossy())
        .map(|_| ())
}

/// Close a lane, saying so when it could not be closed.
///
/// A lane left open is a directory and a registered worktree the operator has
/// to find by hand, so the failure is printed rather than dropped.
fn close_lane_or_report(repo_root: &Path, lane: &Lane) {
    if let Err(why) = close_lane(repo_root, lane) {
        eprintln!("lane `{}` was not closed: {why}", lane.task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{Ledger, Task, TaskState};

    fn ready(id: &str) -> Task {
        let mut task = Task::new(id, "r-1", "work");
        task.state = TaskState::Ready;
        task
    }

    /// **NO GIT MEANS ONE LANE, NOT AN ERROR.**
    ///
    /// A lane is a worktree; without a repository there is nowhere to put one.
    /// Refusing to run at all would trade a feature nobody asked for against
    /// the thing people actually do, so it falls back to the behaviour that
    /// has always worked — and says why, so an operator who asked for four
    /// lanes and got one is not left guessing.
    #[test]
    fn a_workspace_that_is_not_a_repository_gets_one_lane() {
        let dir = tempfile::tempdir().expect("temp dir");
        let isolation = detect_isolation(dir.path());

        assert!(!isolation.allows_lanes(), "got: {isolation:?}");
        let why = isolation.why_not().expect("a reason");
        assert!(
            why.contains("not a git repository") || why.contains("git could not be run"),
            "the reason must be actionable: {why}"
        );

        assert_eq!(
            lanes_for(4, &isolation, 10),
            1,
            "four lanes in one directory is four agents overwriting each other"
        );
    }

    /// **ASKING FOR FOUR AND GETTING ONE MUST NOT BE SILENT.**
    ///
    /// The reason was computed, carried on `Isolation::None`, and returned by
    /// `why_not` — and nothing asked for it. An operator who set
    /// `INCORD_LANES=4` on a workspace that is not a git repository got a
    /// sequential run that looked exactly like one with nothing to
    /// parallelise, and no way to tell the difference.
    #[test]
    fn asking_for_lanes_and_not_getting_them_is_explained() {
        let blocked = Isolation::None {
            why: "not a git repository".to_string(),
        };

        let note = shortfall_note(4, &blocked).expect("four were asked for and refused");
        assert!(
            note.contains("not a git repository"),
            "the note must carry the actionable reason: {note}"
        );
        assert!(
            note.contains('4'),
            "and how many were asked for, or it reads as a general complaint: {note}"
        );
    }

    /// **AND A RUN THAT DID NOT ASK MUST STAY QUIET.**
    ///
    /// One lane is the default. Explaining why a default run is not doing
    /// something nobody requested trains an operator to ignore the line — and
    /// the line matters in the case above.
    #[test]
    fn a_default_run_is_not_told_about_lanes_it_never_wanted() {
        let blocked = Isolation::None {
            why: "not a git repository".to_string(),
        };
        assert_eq!(shortfall_note(1, &blocked), None);

        // Nor is a run that asked and is getting them.
        let fine = Isolation::Worktrees {
            repo_root: PathBuf::from("."),
        };
        assert_eq!(shortfall_note(4, &fine), None);
    }

    /// This repository IS one, so the detection is exercised against a real
    /// git tree rather than only against its absence.
    #[test]
    fn a_real_repository_is_recognised() {
        // The crate's own root, discovered the same way production does.
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        match detect_isolation(here) {
            Isolation::Worktrees { repo_root } => {
                assert!(repo_root.exists(), "the root must be a real path");
            }
            // Not a failure: this checkout may legitimately not be a repo, and
            // the point of the other test is that such a case is handled.
            Isolation::None { why } => {
                assert!(!why.is_empty(), "a refusal must carry a reason");
            }
        }
    }

    #[test]
    fn lanes_never_exceed_the_work_or_the_ceiling() {
        let isolation = Isolation::Worktrees {
            repo_root: PathBuf::from("/repo"),
        };

        assert_eq!(
            lanes_for(4, &isolation, 2),
            2,
            "a lane with no task is a worktree for nothing"
        );
        assert_eq!(
            lanes_for(100, &isolation, 100),
            MAX_LANES,
            "a typo must cost a clamp, not a machine"
        );
        assert_eq!(lanes_for(1, &isolation, 10), 1);
        // No work at all still reports one, so a caller never divides by zero.
        assert_eq!(lanes_for(4, &isolation, 0), 1);
    }

    /// **ONE LANE MUST BEHAVE EXACTLY AS IT ALWAYS HAS.**
    ///
    /// The default. If `batch` returned two tasks at one lane, every existing
    /// run would change shape because somebody upgraded.
    #[test]
    fn one_lane_takes_exactly_the_next_ready_task() {
        let ledger = Ledger {
            version: 1,
            tasks: vec![ready("t-1"), ready("t-2"), ready("t-3")],
        };
        let picked = batch(&ledger, 1);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, "t-1", "and in ledger order");
    }

    #[test]
    fn several_lanes_take_several_ready_tasks_in_order() {
        let ledger = Ledger {
            version: 1,
            tasks: vec![ready("t-1"), ready("t-2"), ready("t-3")],
        };
        let picked = batch(&ledger, 2);
        assert_eq!(
            picked.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t-1", "t-2"]
        );
    }

    /// A task that is not READY is not picked, however many lanes there are.
    #[test]
    fn only_ready_tasks_are_batched() {
        let mut blocked = ready("t-2");
        blocked.state = TaskState::Blocked;
        let ledger = Ledger {
            version: 1,
            tasks: vec![ready("t-1"), blocked, ready("t-3")],
        };
        let picked = batch(&ledger, 3);
        assert_eq!(
            picked.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t-1", "t-3"],
            "a blocked task was handed to a lane"
        );
    }

    /// **A CONFLICT IS NOT A LOST TASK, AND NOT A RESOLVED ONE.**
    ///
    /// Two agents changed the same lines. Choosing between them is a judgement
    /// about the work, which is the one thing this harness never does on its
    /// own — so the message has to say all three of: what clashed, that the
    /// work still exists, and what happens next.
    #[test]
    fn a_conflict_says_where_the_work_went() {
        let lane = Lane {
            task_id: "t-1".to_string(),
            workspace: PathBuf::from("/tmp/lane"),
            branch: "incord/t-1".to_string(),
        };
        let conflict = Merged::Conflict {
            files: vec!["src/auth.rs".to_string()],
        };

        assert!(!conflict.landed());
        let said = conflict.summary(&lane);
        assert!(said.contains("src/auth.rs"), "got: {said}");
        assert!(said.contains("incord/t-1"), "the work's location: {said}");
        assert!(said.contains("nothing was discarded"), "got: {said}");

        assert!(Merged::Clean.landed());
        assert!(
            Merged::Nothing.landed(),
            "an agent that changed nothing has nothing to merge, and that is not a failure"
        );
        assert!(!Merged::Failed {
            why: "detached head".to_string()
        }
        .landed());
    }

    /// A real repository with one commit, or `None` where git is unusable.
    ///
    /// Returns the tempdir too: dropping it deletes the repository, and a
    /// test that let it drop early would be running git against a path that
    /// no longer exists.
    fn a_repo() -> Option<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir().ok()?;
        let root = dir.path().to_path_buf();
        git(&root, &["init"]).ok()?;
        // Identity, or the commit below fails on a machine with no global
        // git config — which is most CI.
        git(&root, &["config", "user.email", "test@example.invalid"]).ok()?;
        git(&root, &["config", "user.name", "incord test"]).ok()?;
        // Windows git rewrites LF to CRLF on checkout by default, so a file
        // written here and read back through a worktree would differ by line
        // ending alone — a failure about nothing, on one platform.
        git(&root, &["config", "core.autocrlf", "false"]).ok()?;
        std::fs::write(root.join("shared.txt"), "line one\nline two\nline three\n").ok()?;
        git(&root, &["add", "-A"]).ok()?;
        git(&root, &["commit", "-m", "base"]).ok()?;
        Some((dir, root))
    }

    /// **TWO LANES, TWO CHECKOUTS, AND NEITHER SEES THE OTHER.**
    ///
    /// The whole reason parallelism needs worktrees. Both agents write to the
    /// same path in their own tree; in one shared directory the second would
    /// overwrite the first and `verify` would then prove a tree neither of
    /// them wrote.
    #[test]
    fn lanes_are_isolated_and_a_clean_merge_lands() {
        let Some((_guard, root)) = a_repo() else {
            // git is not usable here; the no-repository path is covered above.
            return;
        };

        let tasks = vec![ready("t-1"), ready("t-2")];
        let (lanes, failure) = open_lanes(&root, &tasks);
        assert!(failure.is_none(), "{failure:?}");
        assert_eq!(lanes.len(), 2);

        // Separate directories, and neither is the main tree.
        assert_ne!(lanes[0].workspace, lanes[1].workspace);
        assert_ne!(lanes[0].workspace, root);

        // Each lane writes a file only it touches.
        std::fs::write(lanes[0].workspace.join("one.txt"), "from lane one\n").expect("write");
        std::fs::write(lanes[1].workspace.join("two.txt"), "from lane two\n").expect("write");

        // The other lane cannot see it — that is the isolation.
        assert!(
            !lanes[1].workspace.join("one.txt").exists(),
            "lane two could see lane one's edit"
        );

        assert_eq!(merge_lane(&root, &lanes[0]), Merged::Clean);
        assert_eq!(merge_lane(&root, &lanes[1]), Merged::Clean);

        // Both landed in the main tree.
        assert!(root.join("one.txt").exists(), "lane one's work is missing");
        assert!(root.join("two.txt").exists(), "lane two's work is missing");

        for lane in &lanes {
            close_lane(&root, lane).expect("close");
        }
    }

    /// **A CONFLICT IS ABORTED, REPORTED, AND LOSES NOTHING.**
    ///
    /// Two agents change the same lines. Resolving that is a judgement about
    /// the work — the seat this harness never sits in — so the merge is
    /// undone, the tree is left clean for the next lane, and the work stays
    /// on its branch.
    #[test]
    fn a_conflicting_lane_is_aborted_and_its_work_survives() {
        let Some((_guard, root)) = a_repo() else {
            return;
        };

        let tasks = vec![ready("t-1"), ready("t-2")];
        let (lanes, failure) = open_lanes(&root, &tasks);
        assert!(failure.is_none(), "{failure:?}");

        // Both rewrite the SAME line, differently.
        std::fs::write(
            lanes[0].workspace.join("shared.txt"),
            "ONE\nline two\nline three\n",
        )
        .expect("write");
        std::fs::write(
            lanes[1].workspace.join("shared.txt"),
            "TWO\nline two\nline three\n",
        )
        .expect("write");

        assert_eq!(merge_lane(&root, &lanes[0]), Merged::Clean);

        match merge_lane(&root, &lanes[1]) {
            Merged::Conflict { files } => {
                assert!(
                    files.iter().any(|f| f.contains("shared.txt")),
                    "got: {files:?}"
                );
            }
            other => panic!("two agents rewrote one line and it was not reported: {other:?}"),
        }

        // THE TREE IS CLEAN. A repository left mid-merge would hand the next
        // task a checkout with conflict markers in it, and `verify` would run
        // over that.
        let status = git(&root, &["status", "--porcelain"]).expect("status");
        assert!(status.is_empty(), "the merge was not aborted: {status}");
        let content = std::fs::read_to_string(root.join("shared.txt")).expect("read");
        assert!(
            !content.contains("<<<<<<<"),
            "conflict markers left in the tree"
        );
        assert_eq!(
            content, "ONE\nline two\nline three\n",
            "the first lane's work was disturbed"
        );

        // Closed the way `run_batch` closes them, BEFORE looking for the work.
        // Checking the branch while the lane was still open is how this test
        // once passed while `close_lane` deleted the very branch it names.
        for lane in &lanes {
            close_lane(&root, lane).expect("close");
        }

        // AND THE LOSER'S WORK STILL EXISTS, on its branch.
        let kept = git(&root, &["show", &format!("{}:shared.txt", lanes[1].branch)])
            .expect("closing the lane deleted the branch that holds the work");
        assert!(
            kept.contains("TWO"),
            "the conflicting lane's work was lost: {kept}"
        );
        assert!(
            !lanes[1].workspace.exists(),
            "the checkout was left behind: {}",
            lanes[1].workspace.display()
        );
    }

    /// Every worktree git knows about for `root`, the main tree included.
    fn worktree_count(root: &Path) -> usize {
        git(root, &["worktree", "list", "--porcelain"])
            .expect("worktree list")
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    }

    /// **A FAILED LANE'S HALF-DONE WORK IS KEPT, NOT WIPED.**
    ///
    /// It is not merged — unproven work stays out of the main tree — but it
    /// was paid for, and the checkout it sat in is about to be removed. Only
    /// a commit on the branch survives that.
    #[test]
    fn a_failed_lanes_partial_work_survives_on_its_branch() {
        let Some((_guard, root)) = a_repo() else {
            return;
        };

        struct GivesUp(PathBuf);
        impl crate::runner::TaskWorker for GivesUp {
            fn work(&mut self, _task: &crate::ledger::Task) -> Result<String, String> {
                let _ = std::fs::write(self.0.join("half.txt"), "incomplete\n");
                Err("could not finish".to_string())
            }
        }

        let outcomes = run_batch(&root, &[ready("t-1")], |workspace| {
            Box::new(GivesUp(workspace.to_path_buf()))
        });

        assert!(
            !root.join("half.txt").exists(),
            "unfinished work was merged"
        );
        let kept = git(
            &root,
            &["show", &format!("{}:half.txt", outcomes[0].branch)],
        )
        .expect("the partial work was destroyed with the checkout");
        assert!(kept.contains("incomplete"), "got: {kept}");
        assert_eq!(
            worktree_count(&root),
            1,
            "the lane's checkout was left behind"
        );
    }

    /// **REMOVING A CHECKOUT THAT HOLDS UNCOMMITTED WORK IS REFUSED.**
    ///
    /// `/worktree remove` reaches the same call as a lane. With `--force` it
    /// wiped whatever the person had not committed; git's own refusal is the
    /// right answer, and the work must still be there after it.
    #[test]
    fn removing_a_checkout_with_uncommitted_work_is_refused() {
        let Some((_guard, root)) = a_repo() else {
            return;
        };
        let (lanes, failure) = open_lanes(&root, &[ready("t-1")]);
        assert!(failure.is_none(), "{failure:?}");
        let unsaved = lanes[0].workspace.join("unsaved.txt");
        std::fs::write(&unsaved, "not committed\n").expect("write");

        let manager = crate::tools::worktree::WorktreeManager::new(root.clone());
        let refused = manager.remove(&lanes[0].workspace.to_string_lossy());

        assert!(
            refused.is_err(),
            "a dirty checkout was removed: {refused:?}"
        );
        assert!(unsaved.exists(), "the uncommitted work was destroyed");
        close_lane(&root, &lanes[0]).expect("close");
    }

    /// **A LANE WHOSE WORK LANDED LEAVES NOTHING BEHIND.**
    ///
    /// Its branch holds nothing the main tree does not, so deleting it loses
    /// nothing — the one case where tidying up is safe, and git proves it
    /// rather than the merge's exit code.
    #[test]
    fn a_landed_lane_leaves_no_branch_and_no_checkout() {
        let Some((_guard, root)) = a_repo() else {
            return;
        };
        let (lanes, failure) = open_lanes(&root, &[ready("t-1")]);
        assert!(failure.is_none(), "{failure:?}");
        std::fs::write(lanes[0].workspace.join("one.txt"), "landed\n").expect("write");
        assert_eq!(merge_lane(&root, &lanes[0]), Merged::Clean);

        close_lane(&root, &lanes[0]).expect("close");

        assert!(
            git(
                &root,
                &["rev-parse", "--verify", "--quiet", &lanes[0].branch]
            )
            .is_err(),
            "a branch whose work landed was kept"
        );
        assert_eq!(
            worktree_count(&root),
            1,
            "the lane's checkout was left behind"
        );
    }

    /// **THE LANES REALLY DO OVERLAP IN TIME.**
    ///
    /// The claim worth proving, and the easiest to fake: two tasks in two
    /// directories is not parallelism if they run one after the other. So
    /// each lane blocks until BOTH have started. Run sequentially this
    /// deadlocks and the test times out; run concurrently it completes.
    ///
    /// It also writes from inside the lane, so what merges is what the agent
    /// actually did rather than something the test placed there.
    #[test]
    fn the_lanes_run_at_the_same_time_and_both_land() {
        let Some((_guard, root)) = a_repo() else {
            return;
        };

        /// Writes a file in whatever tree it was given, once both have met.
        struct Rendezvous {
            workspace: PathBuf,
            gate: std::sync::Arc<std::sync::Barrier>,
        }

        impl crate::runner::TaskWorker for Rendezvous {
            fn work(&mut self, task: &crate::ledger::Task) -> Result<String, String> {
                // Neither lane can pass this alone.
                self.gate.wait();
                std::fs::write(
                    self.workspace.join(format!("{}.txt", task.id)),
                    format!("written by {}\n", task.id),
                )
                .map_err(|e| e.to_string())?;
                Ok(format!("{} did the work", task.id))
            }
        }

        let tasks = vec![ready("t-1"), ready("t-2")];
        let gate = std::sync::Arc::new(std::sync::Barrier::new(tasks.len()));

        let outcomes = run_batch(&root, &tasks, |workspace| {
            Box::new(Rendezvous {
                workspace: workspace.to_path_buf(),
                gate: std::sync::Arc::clone(&gate),
            })
        });

        assert_eq!(outcomes.len(), 2);
        for outcome in &outcomes {
            assert!(outcome.report.is_ok(), "{:?}", outcome.report);
            assert_eq!(
                outcome.merged,
                Merged::Clean,
                "{}",
                outcome.merged.summary(&Lane {
                    task_id: outcome.task.id.clone(),
                    workspace: PathBuf::new(),
                    branch: String::new(),
                })
            );
        }

        // Both agents' files are in the MAIN tree, which is the only place
        // that matters.
        assert!(
            root.join("t-1.txt").exists(),
            "lane one's work never landed"
        );
        assert!(
            root.join("t-2.txt").exists(),
            "lane two's work never landed"
        );
    }

    /// **A LANE THAT FAILED IS NOT MERGED.**
    ///
    /// Half a change that could not finish is the last thing the main tree
    /// should be handed — and the runner has not verified anything yet, so
    /// merging on the agent's say-so would put unproven work in front of the
    /// next lane's check.
    #[test]
    fn a_failed_lane_leaves_the_main_tree_alone() {
        let Some((_guard, root)) = a_repo() else {
            return;
        };

        struct Refuses(PathBuf);
        impl crate::runner::TaskWorker for Refuses {
            fn work(&mut self, _task: &crate::ledger::Task) -> Result<String, String> {
                // It got halfway: the file exists, the work does not.
                let _ = std::fs::write(self.0.join("half.txt"), "incomplete\n");
                Err("could not finish".to_string())
            }
        }

        let tasks = vec![ready("t-1")];
        let outcomes = run_batch(&root, &tasks, |workspace| {
            Box::new(Refuses(workspace.to_path_buf()))
        });

        assert!(outcomes[0].report.is_err());
        assert!(!outcomes[0].merged.landed(), "a failed lane was merged");
        assert!(
            !root.join("half.txt").exists(),
            "unfinished work reached the main tree"
        );
    }

    /// An agent that changed nothing has nothing to merge, and that is fine.
    #[test]
    fn a_lane_that_changed_nothing_merges_nothing() {
        let Some((_guard, root)) = a_repo() else {
            return;
        };
        let (lanes, failure) = open_lanes(&root, &[ready("t-1")]);
        assert!(failure.is_none(), "{failure:?}");

        assert_eq!(merge_lane(&root, &lanes[0]), Merged::Nothing);
        close_lane(&root, &lanes[0]).expect("close");
    }

    /// The requested count is read from the environment and clamped.
    #[test]
    fn the_requested_count_falls_back_to_one() {
        std::env::remove_var(LANES_ENV);
        assert_eq!(
            requested_lanes(),
            1,
            "the default must be today's behaviour"
        );

        std::env::set_var(LANES_ENV, "not a number");
        assert_eq!(requested_lanes(), 1, "a typo must not stop the run");

        std::env::set_var(LANES_ENV, "3");
        assert_eq!(requested_lanes(), 3);

        std::env::set_var(LANES_ENV, "999");
        assert_eq!(requested_lanes(), MAX_LANES);

        std::env::remove_var(LANES_ENV);
    }
}
