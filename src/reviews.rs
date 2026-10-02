//! Tasks waiting on a pull request, settled at the start of a run.
//!
//! With `INCODE_LANES_AS=pr` a lane whose check passed is opened as a pull
//! request instead of merged ([`crate::parallel::Landing::Review`]), and its
//! task waits `Blocked` with the URL recorded
//! ([`crate::ledger::Ledger::await_review`]). A person merges or closes it on
//! the forge; this reads which, at the start of the next run, and moves the
//! task on through the same edges every other task takes:
//!
//! - **Merged:** the branch it was merged into is fast-forwarded here from
//!   `origin`, and the task's check runs on that tree — the single edge into
//!   `Done`. A failing check is a failed attempt.
//! - **Closed unmerged:** a failed attempt; the task goes back to the queue
//!   with its branch remembered, as a conflicted lane does.
//! - **Still open**, or the forge unreachable: left waiting.

use std::path::Path;

use crate::ledger::{Ledger, TaskState};
use crate::tools::pull_request::{Forge, PrState};

/// Settle every task waiting on a pull request. Returns one line per task
/// that moved or could not be read, and the ids it completed.
pub fn settle(
    ledger: &mut Ledger,
    workspace: &Path,
    forge: &dyn Forge,
) -> (Vec<String>, Vec<String>) {
    let waiting: Vec<(String, String)> = ledger
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Blocked)
        .filter_map(|t| t.pull_request.clone().map(|url| (t.id.clone(), url)))
        .collect();
    let mut notes = Vec::new();
    let mut completed = Vec::new();
    for (id, url) in waiting {
        let review = match forge.review(workspace, &url) {
            Ok(review) => review,
            Err(why) => {
                notes.push(format!(
                    "`{id}` is still waiting: {url} could not be read: {why}"
                ));
                continue;
            }
        };
        let note = match review.state {
            PrState::Open => continue,
            PrState::Closed => {
                let why = format!("its pull request {url} was closed without being merged");
                match ledger.record_failure(&id, &why) {
                    Ok(TaskState::Abandoned) => {
                        format!("`{id}`: {why}, and it is out of attempts: set aside")
                    }
                    _ => format!("`{id}`: {why}; it goes back to the queue"),
                }
            }
            PrState::Merged => match bring_in_merged(workspace, &review.base) {
                Err(why) => {
                    // Still Blocked: the work is merged on the forge and only
                    // this checkout is behind, which a person can fix.
                    let why = format!(
                        "{url} was merged into `{}`, but this checkout could not be brought \
                         up to it: {why}. Pull it, then run again.",
                        review.base
                    );
                    if let Some(task) = ledger.get_mut_public(&id) {
                        task.note = why.clone();
                    }
                    notes.push(format!("`{id}`: {why}"));
                    continue;
                }
                Ok(()) => {
                    let check = ledger
                        .get(&id)
                        .map(|t| t.verify.clone())
                        .unwrap_or_default();
                    match crate::verify::verify(&check, workspace) {
                        Ok(proof) => {
                            let _ = ledger.complete(&id, &proof);
                            completed.push(id.clone());
                            format!("`{id}`: {url} was merged and its check passes here: done")
                        }
                        Err(why) => {
                            let why = format!(
                                "{url} was merged, but its check fails on the merged tree: {}",
                                crate::text::head_bytes(&why, 600)
                            );
                            let _ = ledger.record_failure(&id, &why);
                            format!("`{id}`: {why}")
                        }
                    }
                }
            },
        };
        // Settled one way or the other: no review is pending any more.
        if let Some(task) = ledger.get_mut_public(&id) {
            task.pull_request = None;
        }
        notes.push(note);
    }
    if !notes.is_empty() {
        let _ = ledger.save(workspace);
    }
    (notes, completed)
}

/// Fast-forward this checkout to `origin/<base>`, which a pull request was
/// just merged into. Only a fast-forward, and only on `base` itself: a
/// checkout on another branch, or with commits of its own, is left for a
/// person rather than merged into.
fn bring_in_merged(workspace: &Path, base: &str) -> Result<(), String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(args)
            .output()
            .map_err(|e| format!("could not run git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    };
    let current = git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current != base {
        return Err(format!("it is on `{current}`, not `{base}`"));
    }
    git(&["fetch", "origin", base])?;
    git(&["merge", "--ff-only", &format!("origin/{base}")])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Task;
    use crate::tools::pull_request::{MergeMethod, NewPr, PrStatus, Review};
    use std::path::PathBuf;
    use std::process::Command;

    /// The forge, stood in for: every pull request is in `state`, onto `main`.
    struct Settled(PrState);

    impl Forge for Settled {
        fn default_branch(&self, _: &Path) -> Result<String, String> {
            Ok("main".to_string())
        }
        fn open_pr(&self, _: &Path, _: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn create(&self, _: &Path, _: &NewPr) -> Result<String, String> {
            Err("not asked".to_string())
        }
        fn pr_status(&self, _: &Path, _: &str) -> Result<Option<PrStatus>, String> {
            Ok(None)
        }
        fn merge(&self, _: &Path, _: u64, _: MergeMethod, _: &str) -> Result<String, String> {
            Err("not asked".to_string())
        }
        fn review(&self, _: &Path, _: &str) -> Result<Review, String> {
            Ok(Review {
                state: self.0,
                base: "main".to_string(),
            })
        }
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A checkout of a bare `origin` whose `main` has since gained
    /// `merged.txt` — what merging a pull request on the forge leaves
    /// behind — while this checkout's `main` has not.
    fn merged_on_the_forge() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let work = dir.path().join("work");
        sh(dir.path(), &["init", "--bare", "-b", "main", "remote.git"]);
        sh(dir.path(), &["init", "-b", "main", "work"]);
        for (k, v) in [
            ("user.email", "t@example.invalid"),
            ("user.name", "t"),
            ("core.autocrlf", "false"),
        ] {
            sh(&work, &["config", k, v]);
        }
        std::fs::write(work.join("a.txt"), "a").expect("write");
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-m", "base"]);
        let remote = dir.path().join("remote.git");
        sh(
            &work,
            &["remote", "add", "origin", remote.to_str().expect("utf8")],
        );
        sh(&work, &["push", "origin", "main"]);
        sh(&work, &["checkout", "-b", "lane/t-1"]);
        std::fs::write(work.join("merged.txt"), "m").expect("write");
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-m", "the lane's change"]);
        sh(&work, &["push", "origin", "lane/t-1:main"]);
        sh(&work, &["checkout", "main"]);
        (dir, work)
    }

    /// One task waiting on a review, checked by `verify`.
    fn waiting(verify: &[&str]) -> Ledger {
        let mut ledger = Ledger {
            version: 1,
            tasks: vec![Task::new("t-1", "r-1", "add merged.txt").with_verify(verify.to_vec())],
        };
        ledger
            .await_review("t-1", "https://github.com/o/r/pull/9", "lane/t-1")
            .expect("waiting");
        ledger
    }

    /// **A MERGED REVIEW IS FINISHED ONLY BY ITS CHECK, ON THE PULLED TREE.**
    #[test]
    fn a_merged_pull_request_is_pulled_checked_and_done() {
        let (_dir, work) = merged_on_the_forge();
        let mut ledger = waiting(&["git", "ls-files", "--error-unmatch", "merged.txt"]);

        let (notes, completed) = settle(&mut ledger, &work, &Settled(PrState::Merged));
        assert_eq!(completed, ["t-1"], "{notes:?}");
        let task = ledger.get("t-1").expect("task");
        assert_eq!(task.state, TaskState::Done);
        assert_eq!(task.pull_request, None);
        assert!(work.join("merged.txt").exists(), "the merge was not pulled");
    }

    /// Merged on the forge and still broken here: a failed attempt, not
    /// `Done` — the review is not the proof.
    #[test]
    fn a_merged_pull_request_whose_check_fails_is_a_failed_attempt() {
        let (_dir, work) = merged_on_the_forge();
        let mut ledger = waiting(&["git", "ls-files", "--error-unmatch", "absent.txt"]);

        let (notes, completed) = settle(&mut ledger, &work, &Settled(PrState::Merged));
        assert!(completed.is_empty());
        let task = ledger.get("t-1").expect("task");
        assert_eq!(task.state, TaskState::Ready, "{notes:?}");
        assert_eq!(task.attempts, 1);
        assert_eq!(task.pull_request, None);
        assert!(task.note.contains("check fails"), "{}", task.note);
    }

    /// Closed without merging: the task is done again, pointed at the
    /// branch that already holds the work.
    #[test]
    fn a_closed_pull_request_sends_the_task_back_with_its_branch() {
        let dir = tempfile::tempdir().expect("dir");
        let mut ledger = waiting(&["true"]);

        let (notes, _) = settle(&mut ledger, dir.path(), &Settled(PrState::Closed));
        let task = ledger.get("t-1").expect("task");
        assert_eq!(task.state, TaskState::Ready, "{notes:?}");
        assert_eq!(task.attempts, 1);
        assert_eq!(task.unmerged_branch.as_deref(), Some("lane/t-1"));
        assert_eq!(task.pull_request, None);
    }

    /// Still under review: nothing moves and nothing is said.
    #[test]
    fn an_open_pull_request_leaves_the_task_waiting() {
        let dir = tempfile::tempdir().expect("dir");
        let mut ledger = waiting(&["true"]);

        let (notes, completed) = settle(&mut ledger, dir.path(), &Settled(PrState::Open));
        assert!(notes.is_empty() && completed.is_empty());
        let task = ledger.get("t-1").expect("task");
        assert_eq!(task.state, TaskState::Blocked);
        assert!(task.pull_request.is_some());
    }

    /// Merged into `main` while this checkout is on another branch: nothing
    /// is merged into it, and the task waits with a note saying what to do.
    #[test]
    fn a_checkout_on_another_branch_is_not_merged_into() {
        let (_dir, work) = merged_on_the_forge();
        sh(&work, &["checkout", "-b", "elsewhere"]);
        let mut ledger = waiting(&["git", "ls-files", "--error-unmatch", "merged.txt"]);

        let (notes, completed) = settle(&mut ledger, &work, &Settled(PrState::Merged));
        assert!(completed.is_empty());
        let task = ledger.get("t-1").expect("task");
        assert_eq!(task.state, TaskState::Blocked);
        assert!(task.pull_request.is_some());
        assert!(notes[0].contains("Pull it"), "{notes:?}");
        assert!(!work.join("merged.txt").exists());
    }

    /// **WORK BUILT ON A CHANGE UNDER REVIEW WAITS FOR IT.** A dependant is
    /// not started while its dependency is in review, and the run says both
    /// are waiting rather than that the work is finished.
    #[test]
    fn the_dependants_of_a_task_in_review_are_not_started() {
        let mut ledger = waiting(&["true"]);
        ledger
            .tasks
            .push(Task::new("t-2", "r-1", "builds on it").with_dependencies(["t-1"]));
        ledger.refresh_ready();
        assert_eq!(ledger.get("t-2").expect("t-2").state, TaskState::Pending);
        assert!(ledger.next_ready().is_none());
        assert_eq!(ledger.waiting_ids(), ["t-1", "t-2"]);
    }
}
