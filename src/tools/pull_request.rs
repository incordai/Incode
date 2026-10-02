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
pub fn open(
    repo: &Path,
    branch: Option<&str>,
    draft: bool,
    forge: &dyn Forge,
) -> Result<Opened, String> {
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
    // A name git accepts as a branch, which also rules out one that starts
    // with `-` and would be read as an option.
    git(repo, &["check-ref-format", "--branch", &branch])
        .map_err(|_| format!("\"{branch}\" is not a valid branch name"))?;
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .map_err(|_| format!("there is no local branch \"{branch}\""))?;
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
    }

    fn forge(existing: Option<&str>) -> FakeForge {
        FakeForge {
            default: "main".to_string(),
            existing: existing.map(str::to_string),
            created: RefCell::new(Vec::new()),
        }
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
