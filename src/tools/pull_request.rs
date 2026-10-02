//! `/pr` — push a branch and open (or find) its GitHub pull request.
//!
//! # Only when the operator asks
//!
//! Pushing publishes work and a pull request is seen by other people, so
//! nothing here runs on its own: `/pr` is typed by the operator, at the REPL
//! or over `--acp`, and no tool the model can call reaches it.
//!
//! # What it refuses, before anything leaves the machine
//!
//! A branch name git would not accept, a detached HEAD, the default branch
//! itself (that is a push, not a pull request), a current branch with
//! uncommitted changes (they would not be in the PR the operator thinks they
//! are opening), a branch with no commits its base lacks, and a missing
//! `origin`. Each is refused by name and nothing is pushed.
//!
//! # GitHub through `gh`
//!
//! The GitHub CLI holds the login (`gh auth login`), so incode reads, holds
//! and sends no token. Its calls sit behind [`Forge`], so the tests push for
//! real to a local bare repository and stand in for GitHub without touching
//! it.

use std::path::Path;
use std::process::Command;

/// What `/pr` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub url: String,
    pub branch: String,
    pub base: String,
    /// `false` when an open pull request for the branch already existed:
    /// the branch was pushed and that one is reported.
    pub created: bool,
}

/// The pull-request side: GitHub through `gh`, or a stand-in in tests.
pub trait Forge {
    /// The repository's default branch on the forge.
    fn default_branch(&self, repo: &Path) -> Result<String, String>;
    /// The URL of an OPEN pull request whose head is `branch`, if any.
    fn open_pr(&self, repo: &Path, branch: &str) -> Result<Option<String>, String>;
    /// Open one and return its URL.
    fn create(&self, repo: &Path, new: &NewPr) -> Result<String, String>;
    /// The OPEN pull request whose head is `branch`, with what decides
    /// whether it may be merged; `None` when there is none open.
    fn pr_status(&self, repo: &Path, branch: &str) -> Result<Option<PrStatus>, String>;
    /// Merge pull request `number` — only while its head is still `head`, so
    /// nothing pushed after the checks were read can ride along — and return
    /// the merge commit.
    fn merge(
        &self,
        repo: &Path,
        number: u64,
        method: MergeMethod,
        head: &str,
    ) -> Result<String, String>;
}

/// How `/pr merge` lands a pull request on its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    /// A merge commit; every commit of the branch is kept.
    Merge,
    /// One commit with the branch's changes.
    Squash,
    /// The branch's commits replayed onto the base.
    Rebase,
}

impl MergeMethod {
    fn flag(self) -> &'static str {
        match self {
            MergeMethod::Merge => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        }
    }
}

/// One check's verdict on a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    Failed,
    Pending,
}

/// An open pull request, as `/pr merge` needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub number: u64,
    pub url: String,
    pub draft: bool,
    /// GitHub's word: `MERGEABLE`, `CONFLICTING`, or `UNKNOWN` while it is
    /// still working it out.
    pub mergeable: String,
    /// The commit the pull request's head is at on the forge.
    pub head: String,
    pub checks: Vec<(String, CheckState)>,
}

/// What `/pr merge` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub number: u64,
    pub url: String,
    pub branch: String,
    pub commit: String,
    pub method: MergeMethod,
    /// How many checks had run and passed; zero is said out loud, since a
    /// merge with nothing checked is the operator's word alone.
    pub checks_passed: usize,
}

/// A pull request to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPr {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

/// GitHub, through the `gh` CLI the operator is logged in to.
pub struct GitHubCli;

impl GitHubCli {
    fn run(repo: &Path, args: &[&str]) -> Result<String, String> {
        Self::run_with_input(repo, args, None)
    }

    /// `gh`, with `input` on its stdin when given (the PR description, read
    /// by `--body-file -`: a commit list can outgrow a command line, and
    /// nothing in it is then read as an argument).
    fn run_with_input(repo: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
        let mut child = Command::new("gh")
            .args(args)
            .current_dir(repo)
            // Never wait on a prompt nobody will see.
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(if input.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "the GitHub CLI `gh` is not installed; install it from \
                     https://cli.github.com and run `gh auth login`"
                        .to_string()
                }
                _ => format!("could not run gh: {e}"),
            })?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            std::io::Write::write_all(&mut stdin, text.as_bytes())
                .map_err(|e| format!("could not hand gh the description: {e}"))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("gh did not finish: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(format!(
                "gh {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

impl Forge for GitHubCli {
    fn default_branch(&self, repo: &Path) -> Result<String, String> {
        let name = Self::run(
            repo,
            &[
                "repo",
                "view",
                "--json",
                "defaultBranchRef",
                "--jq",
                ".defaultBranchRef.name",
            ],
        )?;
        if name.is_empty() {
            return Err("GitHub reported no default branch for this repository".to_string());
        }
        Ok(name)
    }

    fn open_pr(&self, repo: &Path, branch: &str) -> Result<Option<String>, String> {
        let url = Self::run(
            repo,
            &[
                "pr", "list", "--head", branch, "--state", "open", "--json", "url", "--jq",
                ".[0].url",
            ],
        )?;
        Ok((!url.is_empty() && url != "null").then_some(url))
    }

    fn create(&self, repo: &Path, new: &NewPr) -> Result<String, String> {
        let mut args = vec![
            "pr",
            "create",
            "--base",
            &new.base,
            "--head",
            &new.head,
            "--title",
            &new.title,
            "--body-file",
            "-",
        ];
        if new.draft {
            args.push("--draft");
        }
        let out = Self::run_with_input(repo, &args, Some(&new.body))?;
        pr_url(&out).ok_or_else(|| format!("gh pr create printed no pull request URL: {out}"))
    }

    fn pr_status(&self, repo: &Path, branch: &str) -> Result<Option<PrStatus>, String> {
        let fields = "number,url,state,isDraft,mergeable,headRefOid,statusCheckRollup";
        match Self::run(repo, &["pr", "view", branch, "--json", fields]) {
            Ok(json) => parse_status(&json),
            // gh's own wording when the branch has no pull request at all.
            Err(why) if why.contains("no pull requests found") => Ok(None),
            Err(why) => Err(why),
        }
    }

    fn merge(
        &self,
        repo: &Path,
        number: u64,
        method: MergeMethod,
        head: &str,
    ) -> Result<String, String> {
        let number = number.to_string();
        Self::run(
            repo,
            &[
                "pr",
                "merge",
                &number,
                method.flag(),
                "--match-head-commit",
                head,
            ],
        )?;
        Self::run(
            repo,
            &[
                "pr",
                "view",
                &number,
                "--json",
                "mergeCommit",
                "--jq",
                ".mergeCommit.oid",
            ],
        )
    }
}

/// [`PrStatus`] from `gh pr view --json …`; `None` for one that is not open.
///
/// A check is a CheckRun (`status`, `conclusion`) or a commit status
/// (`state`); both are read. Anything not plainly passed or plainly still
/// running counts as failed — an unknown verdict must not let a merge
/// through.
pub fn parse_status(json: &str) -> Result<Option<PrStatus>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("gh printed unreadable JSON: {e}"))?;
    if v["state"].as_str() != Some("OPEN") {
        return Ok(None);
    }
    let checks = v["statusCheckRollup"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|c| {
                    let name = c["name"]
                        .as_str()
                        .or_else(|| c["context"].as_str())
                        .unwrap_or("a check")
                        .to_string();
                    let state = match (c["status"].as_str(), c["conclusion"].as_str()) {
                        (Some(status), _) if status != "COMPLETED" => CheckState::Pending,
                        (Some(_), Some("SUCCESS" | "NEUTRAL" | "SKIPPED")) => CheckState::Passed,
                        (Some(_), _) => CheckState::Failed,
                        (None, _) => match c["state"].as_str() {
                            Some("SUCCESS") => CheckState::Passed,
                            Some("PENDING" | "EXPECTED") => CheckState::Pending,
                            _ => CheckState::Failed,
                        },
                    };
                    (name, state)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(PrStatus {
        number: v["number"]
            .as_u64()
            .ok_or("gh printed a pull request with no number")?,
        url: v["url"].as_str().unwrap_or_default().to_string(),
        draft: v["isDraft"].as_bool().unwrap_or(false),
        mergeable: v["mergeable"].as_str().unwrap_or("UNKNOWN").to_string(),
        head: v["headRefOid"].as_str().unwrap_or_default().to_string(),
        checks,
    }))
}

/// The pull request URL `gh pr create` printed: its last `https://` line.
pub fn pr_url(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("https://") && l.contains("/pull/"))
        .map(str::to_string)
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// A title and description from the branch's commits: the one subject when
/// there is one commit, else the branch name; the body lists every subject.
pub fn describe(branch: &str, subjects: &[String]) -> (String, String) {
    let title = match subjects {
        [only] => only.clone(),
        _ => branch
            .rsplit('/')
            .next()
            .unwrap_or(branch)
            .replace(['-', '_'], " "),
    };
    let mut body = String::from("## Commits\n\n");
    for subject in subjects {
        body.push_str(&format!("- {subject}\n"));
    }
    body.push_str("\nOpened with incode `/pr`.\n");
    (title, body)
}

/// Push `branch` (the current one when `None`) to `origin` and open its pull
/// request against the repository's default branch — or report the open one.
/// The branch named (or the current one), as `(branch, current, its tip)`:
/// refused when HEAD is detached and nothing is named, when the name is not
/// one git accepts — which also rules out one that starts with `-` and would
/// be read as an option — or when no local branch has it.
fn resolve_branch(repo: &Path, branch: Option<&str>) -> Result<(String, String, String), String> {
    let current = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map_err(|e| format!("not a git repository here: {e}"))?;
    let branch = match branch.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None if current == "HEAD" => {
            return Err(
                "HEAD is detached: check out a branch, or name one: /pr <branch>".to_string(),
            )
        }
        None => current.clone(),
    };
    git(repo, &["check-ref-format", "--branch", &branch])
        .map_err(|_| format!("\"{branch}\" is not a valid branch name"))?;
    let tip = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .map_err(|_| format!("there is no local branch \"{branch}\""))?;
    Ok((branch, current, tip))
}

/// Merge `branch`'s open pull request (the current branch's when `None`).
///
/// Refused — before GitHub is asked to merge anything — when there is no
/// open pull request, it is a draft, GitHub reports a conflict or has not
/// finished working out whether there is one, any check failed or is still
/// running, or the pull request's head is not the local branch's tip (work
/// pushed by someone else, or local commits not pushed yet: either way the
/// operator would be merging something other than what they have). The
/// merge itself is told that head, so a push in between is refused by
/// GitHub too. The branch is kept.
pub fn merge(
    repo: &Path,
    branch: Option<&str>,
    method: MergeMethod,
    forge: &dyn Forge,
) -> Result<Merged, String> {
    let (branch, _, tip) = resolve_branch(repo, branch)?;
    let pr = forge
        .pr_status(repo, &branch)?
        .ok_or_else(|| format!("\"{branch}\" has no open pull request; open one with /pr"))?;
    if pr.head != tip {
        return Err(format!(
            "pull request #{} is at {} but local \"{branch}\" is at {}: push with /pr (or pull) \
             so you merge what you have reviewed",
            pr.number,
            short(&pr.head),
            short(&tip)
        ));
    }
    if pr.draft {
        return Err(format!(
            "pull request #{} is a draft; mark it ready for review first",
            pr.number
        ));
    }
    match pr.mergeable.as_str() {
        "MERGEABLE" => {}
        "CONFLICTING" => {
            return Err(format!(
                "pull request #{} conflicts with its base; resolve that first",
                pr.number
            ))
        }
        _ => {
            return Err(format!(
                "GitHub has not finished checking whether #{} merges cleanly; try again in a \
                 moment",
                pr.number
            ))
        }
    }
    let names = |want: CheckState| -> Vec<&str> {
        pr.checks
            .iter()
            .filter(|(_, s)| *s == want)
            .map(|(n, _)| n.as_str())
            .collect()
    };
    let failed = names(CheckState::Failed);
    if !failed.is_empty() {
        return Err(format!(
            "pull request #{} has failing checks: {}",
            pr.number,
            failed.join(", ")
        ));
    }
    let pending = names(CheckState::Pending);
    if !pending.is_empty() {
        return Err(format!(
            "pull request #{} has checks still running: {}",
            pr.number,
            pending.join(", ")
        ));
    }
    let commit = forge.merge(repo, pr.number, method, &pr.head)?;
    Ok(Merged {
        number: pr.number,
        url: pr.url,
        branch,
        commit,
        method,
        checks_passed: pr.checks.len(),
    })
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

pub fn open(
    repo: &Path,
    branch: Option<&str>,
    draft: bool,
    forge: &dyn Forge,
) -> Result<Opened, String> {
    let (branch, current, _) = resolve_branch(repo, branch)?;
    if branch == current {
        let dirty = git(repo, &["status", "--porcelain"])?;
        if !dirty.is_empty() {
            return Err(format!(
                "\"{branch}\" has uncommitted changes, which would not be in the pull request; \
                 commit or stash them first"
            ));
        }
    }
    git(repo, &["remote", "get-url", "origin"])
        .map_err(|_| "this repository has no `origin` remote to push to".to_string())?;

    let base = forge.default_branch(repo)?;
    if branch == base {
        return Err(format!(
            "\"{branch}\" is the default branch; a pull request needs a branch of its own \
             (e.g. /worktree create <name>)"
        ));
    }
    // What the branch has that the base does not, measured against the base
    // as the remote has it when it is known here, else the local one.
    let base_ref = [format!("origin/{base}"), base.clone()]
        .into_iter()
        .find(|r| git(repo, &["rev-parse", "--verify", "--quiet", r]).is_ok())
        .ok_or_else(|| format!("the base branch \"{base}\" is not known here; git fetch first"))?;
    let range = format!("{base_ref}..{branch}");
    let log = git(repo, &["log", "--reverse", "--format=%s", &range])?;
    let subjects: Vec<String> = log.lines().map(str::to_string).collect();
    if subjects.is_empty() {
        return Err(format!(
            "\"{branch}\" has no commits that {base_ref} lacks; nothing to open a pull request for"
        ));
    }

    git(
        repo,
        &[
            "push",
            "--set-upstream",
            "origin",
            &format!("refs/heads/{branch}:refs/heads/{branch}"),
        ],
    )
    .map_err(|e| format!("git push failed: {e}"))?;

    if let Some(url) = forge.open_pr(repo, &branch)? {
        return Ok(Opened {
            url,
            branch,
            base,
            created: false,
        });
    }
    let (title, body) = describe(&branch, &subjects);
    let url = forge.create(
        repo,
        &NewPr {
            head: branch.clone(),
            base: base.clone(),
            title,
            body,
            draft,
        },
    )?;
    Ok(Opened {
        url,
        branch,
        base,
        created: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// GitHub, stood in for: a fixed default branch, an optional open PR, and
    /// a record of what would have been created.
    struct FakeForge {
        default: String,
        existing: Option<String>,
        created: RefCell<Vec<NewPr>>,
        status: RefCell<Option<PrStatus>>,
        merged: RefCell<Vec<(u64, MergeMethod, String)>>,
    }

    impl Forge for FakeForge {
        fn default_branch(&self, _: &Path) -> Result<String, String> {
            Ok(self.default.clone())
        }
        fn open_pr(&self, _: &Path, _: &str) -> Result<Option<String>, String> {
            Ok(self.existing.clone())
        }
        fn create(&self, _: &Path, new: &NewPr) -> Result<String, String> {
            self.created.borrow_mut().push(new.clone());
            Ok("https://github.com/o/r/pull/7".to_string())
        }
        fn pr_status(&self, _: &Path, _: &str) -> Result<Option<PrStatus>, String> {
            Ok(self.status.borrow().clone())
        }
        fn merge(
            &self,
            _: &Path,
            number: u64,
            method: MergeMethod,
            head: &str,
        ) -> Result<String, String> {
            self.merged
                .borrow_mut()
                .push((number, method, head.to_string()));
            Ok("0123456789abcdef".to_string())
        }
    }

    fn forge(existing: Option<&str>) -> FakeForge {
        FakeForge {
            default: "main".to_string(),
            existing: existing.map(str::to_string),
            created: RefCell::new(Vec::new()),
            status: RefCell::new(None),
            merged: RefCell::new(Vec::new()),
        }
    }

    fn tip(work: &Path, branch: &str) -> String {
        let out = Command::new("git")
            .args(["rev-parse", branch])
            .current_dir(work)
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An open, ready, mergeable PR at `head` with the given checks.
    fn ready(head: &str, checks: &[(&str, CheckState)]) -> PrStatus {
        PrStatus {
            number: 12,
            url: "https://github.com/o/r/pull/12".to_string(),
            draft: false,
            mergeable: "MERGEABLE".to_string(),
            head: head.to_string(),
            checks: checks.iter().map(|(n, s)| (n.to_string(), *s)).collect(),
        }
    }

    /// **A PR IS MERGED ONLY AS REVIEWED**: at the local tip, with every
    /// check passed — and GitHub is told that head, so a later push cannot
    /// ride along.
    #[test]
    fn a_ready_pull_request_is_merged_at_the_head_that_was_checked() {
        let (_dir, _remote, work) = repo();
        sh(&work, &["checkout", "-b", "feature/x"]);
        commit(&work, "x.txt", "x");
        let head = tip(&work, "feature/x");
        let forge = forge(None);
        *forge.status.borrow_mut() = Some(ready(&head, &[("build", CheckState::Passed)]));

        let merged = merge(&work, None, MergeMethod::Squash, &forge).expect("merged");
        assert_eq!(merged.number, 12);
        assert_eq!(merged.checks_passed, 1);
        assert_eq!(
            forge.merged.borrow().as_slice(),
            [(12, MergeMethod::Squash, head)]
        );
    }

    /// **NOTHING IS MERGED** without an open PR, from a draft, over a
    /// conflict or an unknown answer, past a failing or running check, or
    /// when the PR is not at the local branch's tip.
    #[test]
    fn what_may_not_be_merged_is_refused_before_github_is_asked() {
        let (_dir, _remote, work) = repo();
        sh(&work, &["checkout", "-b", "feature/y"]);
        commit(&work, "y.txt", "y");
        let head = tip(&work, "feature/y");
        let forge = forge(None);

        let why = merge(&work, None, MergeMethod::Merge, &forge).expect_err("no PR");
        assert!(why.contains("no open pull request"), "{why}");

        let cases: Vec<(PrStatus, &str)> = vec![
            (ready("0000000000", &[]), "push with /pr"),
            (
                PrStatus {
                    draft: true,
                    ..ready(&head, &[])
                },
                "draft",
            ),
            (
                PrStatus {
                    mergeable: "CONFLICTING".into(),
                    ..ready(&head, &[])
                },
                "conflicts",
            ),
            (
                PrStatus {
                    mergeable: "UNKNOWN".into(),
                    ..ready(&head, &[])
                },
                "try again",
            ),
            (
                ready(
                    &head,
                    &[("build", CheckState::Passed), ("lint", CheckState::Failed)],
                ),
                "failing checks: lint",
            ),
            (
                ready(&head, &[("test", CheckState::Pending)]),
                "still running: test",
            ),
        ];
        for (status, expect) in cases {
            *forge.status.borrow_mut() = Some(status);
            let why = merge(&work, None, MergeMethod::Merge, &forge).expect_err(expect);
            assert!(why.contains(expect), "{expect}: {why}");
        }
        assert!(forge.merged.borrow().is_empty(), "nothing reached GitHub");
    }

    /// Both kinds of check `gh` reports are read, an unknown verdict counts
    /// as failed, and a PR that is not open is no PR to merge.
    #[test]
    fn the_status_gh_prints_is_read_with_every_kind_of_check() {
        let json = r#"{"number":5,"url":"https://github.com/o/r/pull/5","state":"OPEN",
            "isDraft":false,"mergeable":"MERGEABLE","headRefOid":"abc",
            "statusCheckRollup":[
              {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS"},
              {"__typename":"CheckRun","name":"test","status":"IN_PROGRESS","conclusion":""},
              {"__typename":"CheckRun","name":"e2e","status":"COMPLETED","conclusion":"TIMED_OUT"},
              {"__typename":"StatusContext","context":"ci/legacy","state":"PENDING"},
              {"__typename":"StatusContext","context":"ci/old","state":"WEIRD"}]}"#;
        let status = parse_status(json).expect("json").expect("open");
        assert_eq!(status.number, 5);
        assert_eq!(
            status.checks,
            [
                ("build".to_string(), CheckState::Passed),
                ("test".to_string(), CheckState::Pending),
                ("e2e".to_string(), CheckState::Failed),
                ("ci/legacy".to_string(), CheckState::Pending),
                ("ci/old".to_string(), CheckState::Failed),
            ]
        );
        let closed = r#"{"number":5,"state":"MERGED","statusCheckRollup":[]}"#;
        assert_eq!(parse_status(closed).expect("json"), None);
    }

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            ok.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    /// A clone of a local bare "GitHub" with one commit on `main`.
    fn repo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let remote = dir.path().join("remote.git");
        let work = dir.path().join("work");
        sh(dir.path(), &["init", "--bare", "-b", "main", "remote.git"]);
        sh(dir.path(), &["init", "-b", "main", "work"]);
        for (k, v) in [("user.email", "t@example.invalid"), ("user.name", "t")] {
            sh(&work, &["config", k, v]);
        }
        std::fs::write(work.join("a.txt"), "a").expect("write");
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-m", "first"]);
        sh(
            &work,
            &["remote", "add", "origin", remote.to_str().expect("utf8")],
        );
        sh(&work, &["push", "origin", "main"]);
        sh(&work, &["fetch", "origin"]);
        (dir, remote, work)
    }

    fn commit(work: &Path, file: &str, message: &str) {
        std::fs::write(work.join(file), message).expect("write");
        sh(work, &["add", "."]);
        sh(work, &["commit", "-m", message]);
    }

    /// **A BRANCH IS PUSHED AND ITS PR OPENED**, titled by its one commit,
    /// against the default branch — and the push really landed.
    #[test]
    fn a_branch_is_pushed_and_its_pull_request_opened() {
        let (_dir, remote, work) = repo();
        sh(&work, &["checkout", "-b", "feature/health"]);
        commit(&work, "h.txt", "add a health endpoint");
        let forge = forge(None);

        let opened = open(&work, None, false, &forge).expect("opened");
        assert_eq!(opened.url, "https://github.com/o/r/pull/7");
        assert!(opened.created);
        assert_eq!(
            (opened.branch.as_str(), opened.base.as_str()),
            ("feature/health", "main")
        );
        let created = forge.created.borrow();
        assert_eq!(created[0].title, "add a health endpoint");
        assert!(created[0].body.contains("- add a health endpoint"));
        sh(
            &remote,
            &["rev-parse", "--verify", "refs/heads/feature/health"],
        );
    }

    /// An open PR for the branch is reported, not duplicated; a named branch
    /// that is not checked out is pushed all the same.
    #[test]
    fn an_open_pull_request_is_reported_and_a_named_branch_is_pushed() {
        let (_dir, remote, work) = repo();
        sh(&work, &["branch", "incode/lane-1"]);
        sh(&work, &["checkout", "incode/lane-1"]);
        commit(&work, "l.txt", "lane work");
        commit(&work, "m.txt", "more lane work");
        sh(&work, &["checkout", "main"]);
        let forge = forge(Some("https://github.com/o/r/pull/3"));

        let opened = open(&work, Some("incode/lane-1"), false, &forge).expect("opened");
        assert!(!opened.created);
        assert_eq!(opened.url, "https://github.com/o/r/pull/3");
        assert!(forge.created.borrow().is_empty());
        sh(
            &remote,
            &["rev-parse", "--verify", "refs/heads/incode/lane-1"],
        );
    }

    /// **NOTHING IS PUSHED** for the default branch, uncommitted work, a
    /// branch with nothing new, a name that is not a branch, or one that
    /// would be read as an option.
    #[test]
    fn what_cannot_be_a_pull_request_is_refused_before_any_push() {
        let (_dir, remote, work) = repo();
        let forge = forge(None);

        let why = open(&work, None, false, &forge).expect_err("default branch");
        assert!(why.contains("default branch"), "{why}");

        sh(&work, &["checkout", "-b", "empty"]);
        let why = open(&work, None, false, &forge).expect_err("nothing new");
        assert!(why.contains("no commits"), "{why}");

        commit(&work, "x.txt", "x");
        std::fs::write(work.join("x.txt"), "changed").expect("write");
        let why = open(&work, None, false, &forge).expect_err("dirty");
        assert!(why.contains("uncommitted"), "{why}");

        for bad in ["--force", "no such branch", "missing"] {
            let why = open(&work, Some(bad), false, &forge).expect_err(bad);
            assert!(why.contains("branch"), "{bad}: {why}");
        }
        assert!(forge.created.borrow().is_empty());
        assert!(Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/empty"])
            .current_dir(&remote)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(false));
    }

    #[test]
    fn the_url_is_read_from_what_gh_printed() {
        let out = "Creating pull request for x into main\n\nhttps://github.com/o/r/pull/12\n";
        assert_eq!(
            pr_url(out).as_deref(),
            Some("https://github.com/o/r/pull/12")
        );
        assert_eq!(pr_url("no url here"), None);
        let (title, body) = describe("incode/fix-login-2a9", &["a".into(), "b".into()]);
        assert_eq!(title, "fix login 2a9");
        assert!(body.contains("- a\n- b\n"));
    }
}
