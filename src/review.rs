//! `incode review` and `/review`: a model reads the change and says what is
//! wrong with it.
//!
//! Not [`crate::reviews`], which settles tasks waiting on a pull request, and
//! not [`crate::auditor`], which reads what the memory service's auditor has
//! already filed. This is the review a person asks for before pushing: the
//! diff of the branch against where it forked, plus whatever is not committed
//! yet, handed to the model with one job.
//!
//! # What is collected
//!
//! - `git diff <base>...HEAD`: what the branch committed since it left the
//!   base. The base defaults to the merge-base with `origin`'s default branch,
//!   and falls back to `HEAD` (only the uncommitted work) when there is none.
//! - `git diff HEAD`: staged and unstaged changes to tracked files.
//! - Untracked files that git does not ignore, in full when they are small
//!   text, by name otherwise.
//!
//! The whole is bounded ([`MAX_DIFF_BYTES`]); a larger change is cut and the
//! prompt says so, rather than the model believing it saw everything.

use std::path::Path;
use std::process::Command;

/// The most diff text sent to the model. A change larger than this is cut,
/// and the prompt says it was.
pub const MAX_DIFF_BYTES: usize = 200_000;

/// An untracked file larger than this is named rather than shown.
const MAX_UNTRACKED_BYTES: u64 = 32_000;

/// What the review instructs, ahead of the diff.
pub const REVIEW_INSTRUCTIONS: &str = "\
You are reviewing a code change. Read the diff below (and the surrounding files in the \
repository when you need context), then report problems in this order:

1. Correctness bugs: wrong logic, unhandled errors or edge cases, broken invariants, race \
conditions, regressions in behaviour callers rely on.
2. Security: injection, unsafe handling of untrusted input, secrets, path traversal, \
missing authorisation, unsafe defaults.
3. Simplifications: code that duplicates something that already exists, or could be \
markedly simpler.

For every finding cite the location as `path:line` (the line in the new version of the \
file), say what is wrong, why it matters, and what to change. Put the most severe first. \
Report only real problems you can point at — no style nits, no praise, no summary of what \
the change does. If you find nothing worth reporting, say exactly \"No issues found.\"

This is a review only: do not modify, create or delete any file, and do not run commands \
that change the repository.";

/// The change under review.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Change {
    /// The ref the branch is compared with, as resolved.
    pub base: String,
    /// `git diff <base>...HEAD`; empty when the base is `HEAD`.
    pub committed: String,
    /// `git diff HEAD`.
    pub uncommitted: String,
    /// Untracked files, each as a header and its contents (or a note).
    pub untracked: String,
}

impl Change {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.committed.trim().is_empty()
            && self.uncommitted.trim().is_empty()
            && self.untracked.trim().is_empty()
    }
}

fn git(workspace: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .map_err(|e| format!("git could not be run: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The branch to compare against: `origin`'s default branch, or `None`.
fn origin_default(workspace: &Path) -> Option<String> {
    if let Ok(head) = git(
        workspace,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) {
        let head = head.trim().trim_start_matches("refs/remotes/");
        if !head.is_empty() {
            return Some(head.to_string());
        }
    }
    ["origin/main", "origin/master"]
        .into_iter()
        .find(|candidate| {
            git(
                workspace,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{candidate}^{{commit}}"),
                ],
            )
            .is_ok()
        })
        .map(str::to_string)
}

/// The default base: where this branch forked from `origin`'s default
/// branch, or `HEAD` when there is no such branch to fork from.
#[must_use]
pub fn default_base(workspace: &Path) -> String {
    origin_default(workspace)
        .and_then(|branch| git(workspace, &["merge-base", "HEAD", &branch]).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "HEAD".to_string())
}

/// Read the change in `workspace` against `base` (or [`default_base`]).
///
/// # Errors
///
/// When the workspace is not a git repository, or the base does not name a
/// commit.
pub fn collect(workspace: &Path, base: Option<&str>) -> Result<Change, String> {
    git(workspace, &["rev-parse", "--git-dir"])
        .map_err(|_| format!("{} is not a git repository", workspace.display()))?;
    let base = match base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(named) => {
            git(
                workspace,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{named}^{{commit}}"),
                ],
            )
            .map_err(|_| format!("`{named}` does not name a commit to review against"))?;
            named.to_string()
        }
        None => default_base(workspace),
    };
    // A repository with no commits has no HEAD to diff against; everything
    // in it is untracked, which is still worth reviewing.
    let has_head = git(workspace, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
    let committed = if has_head && base != "HEAD" {
        git(
            workspace,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                &format!("{base}...HEAD"),
            ],
        )?
    } else {
        String::new()
    };
    let uncommitted = if has_head {
        git(workspace, &["diff", "--no-color", "--no-ext-diff", "HEAD"])?
    } else {
        String::new()
    };
    let listed = git(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let mut untracked = String::new();
    for name in listed.split('\0').filter(|n| !n.is_empty()) {
        untracked.push_str(&format!("--- /dev/null\n+++ b/{name} (new, untracked)\n"));
        let path = workspace.join(name);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_UNTRACKED_BYTES {
            untracked.push_str(&format!("({size} bytes, not shown)\n"));
            continue;
        }
        match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(text)) => {
                for line in text.lines() {
                    untracked.push('+');
                    untracked.push_str(line);
                    untracked.push('\n');
                }
            }
            _ => untracked.push_str("(binary or unreadable, not shown)\n"),
        }
    }
    Ok(Change {
        base,
        committed,
        uncommitted,
        untracked,
    })
}

/// Cut `text` to at most `max` bytes on a character boundary.
fn bounded(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// The prompt that asks for the review of `change`.
#[must_use]
pub fn prompt(change: &Change) -> String {
    let mut diff = String::new();
    if !change.committed.trim().is_empty() {
        diff.push_str(&format!(
            "### Committed on this branch (`git diff {}...HEAD`)\n{}\n",
            change.base, change.committed
        ));
    }
    if !change.uncommitted.trim().is_empty() {
        diff.push_str(&format!(
            "### Not yet committed (`git diff HEAD`)\n{}\n",
            change.uncommitted
        ));
    }
    if !change.untracked.trim().is_empty() {
        diff.push_str(&format!("### New untracked files\n{}\n", change.untracked));
    }
    let (shown, cut) = bounded(&diff, MAX_DIFF_BYTES);
    let note = if cut {
        format!(
            "\n\n(The diff was cut at {MAX_DIFF_BYTES} bytes of {}. Read the remaining files \
             from the repository before concluding there is nothing wrong with them.)",
            diff.len()
        )
    } else {
        String::new()
    };
    format!("{REVIEW_INSTRUCTIONS}\n\n<diff>\n{shown}</diff>{note}")
}

/// How `incode review` is used.
pub const USAGE: &str = "\
incode review — have a model review the current change

USAGE:
    incode review [--base <REF>] [-p] [-m <MODEL>] [-w <PATH>]

OPTIONS:
        --base <REF>          Compare with REF (default: the merge-base with
                              origin's default branch, else HEAD). Uncommitted
                              and untracked changes are always included
    -p, --print               Print only the findings on stdout (progress goes
                              to stderr), e.g. to post them as a comment
    -m, --model <NAME>        Model to review with
    -w, --workspace <PATH>    Repository to review (default: current directory)
    -h, --help                Print this help
";

/// `incode review`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub workspace: std::path::PathBuf,
    pub base: Option<String>,
    pub model: Option<String>,
    pub print_only: bool,
    /// `-h`: print [`USAGE`] and nothing else.
    pub help: bool,
}

impl Args {
    /// Read the arguments after `review`.
    ///
    /// # Errors
    ///
    /// An unknown argument, or a flag without its value — refused rather than
    /// ignored, so a mistyped base is not silently the default.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed = Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            base: None,
            model: None,
            print_only: false,
            help: false,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("`{arg}` requires a value\n\n{USAGE}"))
            };
            match arg.as_str() {
                "--base" | "-b" => parsed.base = Some(value()?),
                "-m" | "--model" => parsed.model = Some(value()?),
                "-w" | "--workspace" => parsed.workspace = value()?.into(),
                "-p" | "--print" => parsed.print_only = true,
                "-h" | "--help" => parsed.help = true,
                other => match other.strip_prefix("--base=") {
                    Some(base) => parsed.base = Some(base.to_string()),
                    None => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
                },
            }
        }
        Ok(parsed)
    }
}

/// `incode review`: collect the change, have a model review it, print the
/// findings. With `print_only` the findings alone go to stdout — what the
/// model did on the way goes to stderr — so the output can be posted as is.
///
/// # Errors
///
/// When the change cannot be read, or no agent could run the review.
pub fn run(
    workspace: &Path,
    base: Option<&str>,
    model: Option<&str>,
    print_only: bool,
) -> Result<(), String> {
    let change = collect(workspace, base)?;
    if change.is_empty() {
        println!("No changes to review against {}.", change.base);
        return Ok(());
    }
    let ask = prompt(&change);
    if !print_only {
        return crate::task_run::run_task_as(
            workspace,
            &ask,
            model,
            &[],
            crate::task_run::OutputFormat::Text,
            // A review writes nothing, so the intake the spec gate asks for
            // would only steer the model away from the diff.
            crate::spec::SpecGate::Off,
            &[],
        )
        .map(|_| ());
    }
    // Everything the run prints lands on stderr; the findings go to the real
    // stdout once, at the end.
    let claimed = crate::acp_agent::stdio::claim()
        .map_err(|e| format!("stdout could not be taken for the findings: {e}"))?;
    let findings = crate::task_run::run_task_as(
        workspace,
        &ask,
        model,
        &[],
        crate::task_run::OutputFormat::Text,
        crate::spec::SpecGate::Off,
        &[],
    )?;
    let mut out = claimed.output;
    std::io::Write::write_all(&mut out, findings.trim().as_bytes())
        .and_then(|()| std::io::Write::write_all(&mut out, b"\n"))
        .map_err(|e| format!("the findings could not be written: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        sh(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
        sh(dir.path(), &["add", "."]);
        sh(dir.path(), &["commit", "-q", "-m", "first"]);
        dir
    }

    #[test]
    fn a_branch_is_reviewed_against_its_base_with_uncommitted_work() {
        let dir = repo();
        let root = dir.path();
        sh(root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").expect("write");
        sh(root, &["commit", "-q", "-am", "two"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(root.join("new.rs"), "fn new() {}\n").expect("write");

        let change = collect(root, Some("main")).expect("collects");
        assert_eq!(change.base, "main");
        assert!(change.committed.contains("+two"));
        assert!(!change.committed.contains("+three"));
        assert!(change.uncommitted.contains("+three"));
        assert!(change.untracked.contains("+fn new() {}"));

        let asked = prompt(&change);
        assert!(asked.starts_with(REVIEW_INSTRUCTIONS));
        assert!(asked.contains("git diff main...HEAD"));
        assert!(asked.contains("No issues found."));
    }

    #[test]
    fn without_an_origin_only_the_uncommitted_work_is_reviewed() {
        let dir = repo();
        assert_eq!(default_base(dir.path()), "HEAD");
        let change = collect(dir.path(), None).expect("collects");
        assert!(change.is_empty());
        std::fs::write(dir.path().join("a.txt"), "changed\n").expect("write");
        let change = collect(dir.path(), None).expect("collects");
        assert!(change.committed.is_empty());
        assert!(change.uncommitted.contains("+changed"));
    }

    #[test]
    fn an_unknown_base_is_refused() {
        let dir = repo();
        let err = collect(dir.path(), Some("no-such-branch")).expect_err("refused");
        assert!(err.contains("no-such-branch"));
    }

    #[test]
    fn the_arguments_are_read_and_unknown_ones_refused() {
        let args =
            |a: &[&str]| Args::parse(&a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
        let read =
            args(&["--base", "origin/main", "-p", "-m", "opus", "-w", "/tmp/x"]).expect("parses");
        assert_eq!(read.base.as_deref(), Some("origin/main"));
        assert!(read.print_only);
        assert_eq!(read.model.as_deref(), Some("opus"));
        assert_eq!(read.workspace, std::path::PathBuf::from("/tmp/x"));
        assert_eq!(
            args(&["--base=main"]).expect("parses").base.as_deref(),
            Some("main")
        );
        assert!(args(&["--base"]).is_err());
        assert!(args(&["--nope"]).is_err());
        assert!(args(&["-h"]).expect("parses").help);
    }

    #[test]
    fn a_large_diff_is_cut_and_says_so() {
        let change = Change {
            base: "HEAD".into(),
            uncommitted: "+é".repeat(MAX_DIFF_BYTES),
            ..Change::default()
        };
        let asked = prompt(&change);
        assert!(asked.contains("The diff was cut"));
        assert!(asked.len() < MAX_DIFF_BYTES + REVIEW_INSTRUCTIONS.len() + 1_000);
    }
}
