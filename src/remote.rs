//! `incode remote`: run a task with `incode -t` on GitHub Actions, wait for
//! it, and print its result.
//!
//! # What a run is
//!
//! `incode remote run "<task>"` dispatches the repository's
//! `.github/workflows/incode-remote.yml` (`workflow_dispatch`, inputs `task`,
//! `model` and `id`), finds the run that started by the id the workflow
//! titles it with (`run-name: incode remote <id>`), polls it until it
//! completes or the wait runs out, and reads `result.json` from the run's
//! `incode-result` artifact: the `incode -t --output-format json` result
//! object plus `branch`, where the run pushed its changes
//! (`incode/remote-<id>`), or null when there were none. A wait that runs
//! out says so and leaves the run going: nothing is cancelled. The workflow
//! to copy is `.github/workflows/incode-remote.yml.example`; setup is in
//! docs/remote-runs.md.
//!
//! # The token
//!
//! Read from `GH_TOKEN`, else `GITHUB_TOKEN` ([`TOKEN_VARS`]). It travels in
//! the `Authorization` header, only to the API's own origin, and only when
//! that origin is https or this machine
//! (`agent::catalogue::key_may_travel_to`). Redirects are followed here, one
//! hop at a time, over the shared client that follows none by itself: a hop
//! that leaves the API's origin — the artifact download, which GitHub
//! redirects to its blob storage — is fetched without the token. The token
//! is never printed, never put in an address and never in an error; a
//! transport error is reported without its address, since a blob address
//! carries a signature.
//!
//! # Bounds
//!
//! Each request [`REQUEST_BOUND`]; an API answer [`MAX_API_BYTES`]; the
//! artifact [`MAX_ARTIFACT_BYTES`]; `result.json` inside it
//! [`MAX_RESULT_BYTES`]. Finding the run: [`Pace::discover_for`].
//!
//! # Wired to
//!
//! - [`cli`]: `incode remote …` (`main.rs`), which prints [`Outcome::text`]
//!   and exits non-zero when [`Outcome::ok`] is false.
//! - HTTP: `runtime::http::{no_redirect_client, run_async, error_chain}`; the
//!   token rule: `agent::catalogue::key_may_travel_to`.
//! - The artifact: the `zip` crate, one entry read in memory.
//! - The repository when `-R` is not given: `git remote get-url origin`.
//! - The other end: `.github/workflows/incode-remote.yml.example`, whose
//!   inputs, `run-name`, artifact name and branch name this module relies on.
//! - Siblings: `ext_tools::fetch::follow` follows redirects one hop at a
//!   time the same way, for release downloads that carry no credential; the
//!   GitHub Action (`.github/actions/incode`) runs `incode -t` in CI from an
//!   issue comment rather than from this command.

use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// `incode remote help`.
pub const USAGE: &str = "\
incode remote run \"<task>\" [-R owner/repo] [--ref <branch>] [-m <model>] [--timeout <minutes>] [--no-wait]
    Run the task with `incode -t` on GitHub Actions (the repository's
    .github/workflows/incode-remote.yml), wait for it, and print its result as
    `incode -t --output-format json` does, with `branch`: where the run pushed
    its changes (incode/remote-<id>), or null. Exits non-zero when the run or
    the task failed, or the wait ran out (the run is left going, not
    cancelled). --ref picks the branch to run on (default: the repository's
    default branch); --timeout the wait in minutes (default 60); --no-wait
    prints the run's address and returns.
incode remote list [-R owner/repo]            recent runs: id, state, title, address
incode remote logs <run-id> [-R owner/repo]   one run's state, address, and result

The repository is -R, else the github.com repository `origin` names. The token
is read from GH_TOKEN, else GITHUB_TOKEN, and needs Actions: read and write on
the repository (`gh auth token` prints yours). Setup: docs/remote-runs.md.";

/// GitHub's REST API root.
const GITHUB_API: &str = "https://api.github.com";
/// The workflow a run dispatches, by its file name.
const WORKFLOW: &str = "incode-remote.yml";
/// The artifact a run leaves its result in, and the file inside it.
const ARTIFACT: &str = "incode-result";
const RESULT_FILE: &str = "result.json";
/// Where the token is read from, in order.
const TOKEN_VARS: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];
/// What every request says it is: the program, nothing about who runs it.
const AGENT_NAME: &str = "incode";
const API_VERSION: &str = "2022-11-28";
/// How long `run` waits by default, and at most.
const DEFAULT_WAIT_MINUTES: u64 = 60;
const MAX_WAIT_MINUTES: u64 = 7 * 24 * 60;
/// How long one request may take, start to finish.
const REQUEST_BOUND: Duration = Duration::from_secs(60);
/// The most redirects followed from one address.
const MAX_REDIRECTS: usize = 5;
/// The most of an API answer read.
const MAX_API_BYTES: usize = 8 * 1024 * 1024;
/// The largest artifact downloaded.
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
/// The most of `result.json` read.
const MAX_RESULT_BYTES: u64 = 16 * 1024 * 1024;

/// What `incode remote …` prints on stdout, and whether it exits 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub text: String,
    pub ok: bool,
}

/// `incode remote …` in `workspace` (whose `origin` names the repository when
/// `-R` does not), against GitHub's API; progress goes to stderr.
///
/// Called by `main.rs`; calls [`cli_with`] with the real API, the process
/// environment and the real polling pace.
///
/// # Errors
///
/// A bad argument, no token, no repository, or a request GitHub refused or
/// that could not be made.
pub fn cli(args: &[String], workspace: &Path) -> Result<Outcome, String> {
    cli_with(
        args,
        workspace,
        GITHUB_API,
        &|name| std::env::var(name).ok(),
        &Pace::REAL,
        &mut |line| eprintln!("incode remote: {line}"),
    )
}

/// How often a run is looked for and polled.
struct Pace {
    /// Between two looks for the dispatched run.
    discover_every: Duration,
    /// How long the dispatched run may take to appear.
    discover_for: Duration,
    /// Between two polls of a run.
    poll_every: Duration,
}

impl Pace {
    const REAL: Pace = Pace {
        discover_every: Duration::from_secs(3),
        discover_for: Duration::from_secs(60),
        poll_every: Duration::from_secs(10),
    };
}

/// [`cli`] with the API root, the environment (`env`), the pace and where
/// progress goes given: parses, reads the token and the repository, then
/// hands the verb to [`run`], [`list`] or [`logs`].
fn cli_with(
    args: &[String],
    workspace: &Path,
    api: &str,
    env: &dyn Fn(&str) -> Option<String>,
    pace: &Pace,
    progress: &mut dyn FnMut(&str),
) -> Result<Outcome, String> {
    let verb = parse(args)?;
    let given_repo = match &verb {
        Verb::Help => {
            return Ok(Outcome {
                text: USAGE.to_string(),
                ok: true,
            })
        }
        Verb::Run(run) => run.repo.clone(),
        Verb::List { repo } | Verb::Logs { repo, .. } => repo.clone(),
    };
    let token = token_from(env)?;
    let repo = match given_repo {
        Some(given) => Repo::parse(&given)?,
        None => origin_repo(workspace)?,
    };
    let gh = GitHub {
        api,
        repo: &repo,
        token: &token,
    };
    match verb {
        Verb::Run(args) => run(&gh, &args, pace, progress),
        Verb::List { .. } => list(&gh),
        Verb::Logs { run_id, .. } => logs(&gh, run_id),
        Verb::Help => unreachable!("answered above"),
    }
}

// --- arguments ---------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Verb {
    Help,
    Run(RunArgs),
    List { repo: Option<String> },
    Logs { repo: Option<String>, run_id: u64 },
}

#[derive(Debug, PartialEq, Eq)]
struct RunArgs {
    task: String,
    repo: Option<String>,
    git_ref: Option<String>,
    model: Option<String>,
    /// How long to wait for the run; `None` with `--no-wait`.
    wait: Option<Duration>,
}

/// The value after `flag`.
fn value_of(rest: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    rest.next()
        .cloned()
        .ok_or_else(|| format!("`{flag}` needs a value"))
}

/// `incode remote`'s arguments, read in full before anything runs: an
/// unknown option is refused even beside `--help`. Called by [`cli_with`].
fn parse(args: &[String]) -> Result<Verb, String> {
    let Some((verb, rest)) = args.split_first() else {
        return Ok(Verb::Help);
    };
    let verb = verb.as_str();
    match verb {
        "help" | "-h" | "--help" => return Ok(Verb::Help),
        "run" | "list" | "logs" => {}
        other => return Err(format!("unknown `incode remote {other}`\n\n{USAGE}")),
    }
    let only_run = |flag: &str| {
        if verb == "run" {
            Ok(())
        } else {
            Err(format!(
                "`{flag}` is not an option of `incode remote {verb}`"
            ))
        }
    };
    let (mut help, mut no_wait) = (false, false);
    let (mut repo, mut git_ref, mut model, mut timeout) = (None, None, None, None);
    let mut positional = Vec::new();
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => help = true,
            "-R" | "--repo" => repo = Some(value_of(&mut rest, arg)?),
            "--ref" => {
                only_run(arg)?;
                git_ref = Some(value_of(&mut rest, arg)?);
            }
            "-m" | "--model" => {
                only_run(arg)?;
                model = Some(value_of(&mut rest, arg)?);
            }
            "--timeout" => {
                only_run(arg)?;
                timeout = Some(value_of(&mut rest, arg)?);
            }
            "--no-wait" => {
                only_run(arg)?;
                no_wait = true;
            }
            "--" => positional.extend(rest.by_ref().cloned()),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!(
                    "unknown option `{other}` for `incode remote {verb}`\n\n{USAGE}"
                ))
            }
            other => positional.push(other.to_string()),
        }
    }
    if help {
        return Ok(Verb::Help);
    }
    match verb {
        "run" => {
            let task = match positional.as_slice() {
                [task] if !task.trim().is_empty() => task.clone(),
                [] | [_] => return Err(
                    "`incode remote run` needs the task: incode remote run \"fix the flaky test\""
                        .to_string(),
                ),
                more => {
                    return Err(format!(
                        "`incode remote run` takes one task, quoted; it was given {} words",
                        more.len()
                    ))
                }
            };
            let wait = match (no_wait, timeout) {
                (true, Some(_)) => {
                    return Err("`--no-wait` and `--timeout` cannot be given together".to_string())
                }
                (true, None) => None,
                (false, None) => Some(Duration::from_secs(DEFAULT_WAIT_MINUTES * 60)),
                (false, Some(minutes)) => match minutes.parse::<u64>() {
                    Ok(m) if (1..=MAX_WAIT_MINUTES).contains(&m) => {
                        Some(Duration::from_secs(m * 60))
                    }
                    _ => {
                        return Err(format!(
                            "`--timeout` is a number of minutes from 1 to {MAX_WAIT_MINUTES}, \
                             not `{minutes}` (`--no-wait` returns at once)"
                        ))
                    }
                },
            };
            Ok(Verb::Run(RunArgs {
                task,
                repo,
                git_ref: git_ref.filter(|r| !r.trim().is_empty()),
                model: model.filter(|m| !m.trim().is_empty()),
                wait,
            }))
        }
        "list" => match positional.as_slice() {
            [] => Ok(Verb::List { repo }),
            _ => Err("`incode remote list` takes no arguments but -R".to_string()),
        },
        _ => match positional.as_slice() {
            [id] => id
                .parse::<u64>()
                .map(|run_id| Verb::Logs { repo, run_id })
                .map_err(|_| {
                    format!("`{id}` is not a run id (a number, as `incode remote list` shows)")
                }),
            _ => Err("`incode remote logs` needs one run id".to_string()),
        },
    }
}

/// The token, from the first of [`TOKEN_VARS`] that is set and not blank.
fn token_from(env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    TOKEN_VARS
        .iter()
        .find_map(|name| {
            env(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .ok_or_else(|| {
            format!(
                "no GitHub token: set {} or {} to a token with Actions: read and write on the \
                 repository (`gh auth token` prints yours)",
                TOKEN_VARS[0], TOKEN_VARS[1]
            )
        })
}

// --- the repository ----------------------------------------------------------

/// A repository on github.com, as `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Repo {
    owner: String,
    name: String,
}

impl std::fmt::Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl Repo {
    /// `owner/name` (a trailing `.git` dropped), each part made only of the
    /// characters GitHub allows, so neither can reshape an API path.
    fn parse(text: &str) -> Result<Repo, String> {
        let refused = || format!("`{text}` is not a GitHub repository written owner/name");
        let (owner, name) = text.trim().split_once('/').ok_or_else(refused)?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        let owner_ok = !owner.is_empty()
            && owner.len() <= 39
            && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let name_ok = !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if owner_ok && name_ok {
            Ok(Repo {
                owner: owner.to_string(),
                name: name.to_string(),
            })
        } else {
            Err(refused())
        }
    }

    /// The github.com repository a git remote address names: https
    /// (`https://github.com/o/r.git`, credentials or not), scp-like ssh
    /// (`git@github.com:o/r.git`) or an `ssh://` address. `None` for any
    /// other host or shape.
    fn from_remote(url: &str) -> Option<Repo> {
        let url = url.trim();
        let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
            rest.to_string()
        } else {
            let parsed = reqwest::Url::parse(url).ok()?;
            if !matches!(parsed.scheme(), "https" | "http" | "ssh" | "git") {
                return None;
            }
            let host = parsed.host_str()?.to_ascii_lowercase();
            if !matches!(
                host.as_str(),
                "github.com" | "www.github.com" | "ssh.github.com"
            ) {
                return None;
            }
            parsed.path().trim_start_matches('/').to_string()
        };
        let (owner, name) = path.trim_end_matches('/').split_once('/')?;
        if name.contains('/') {
            return None;
        }
        Repo::parse(&format!("{owner}/{name}")).ok()
    }
}

/// The repository `workspace`'s `origin` names. Its address is never echoed:
/// it may hold a credential.
fn origin_repo(workspace: &Path) -> Result<Repo, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["remote", "get-url", "origin"])
        .output()
        .map_err(|e| format!("git could not be run to read `origin` ({e}); pass -R owner/repo"))?;
    if !out.status.success() {
        return Err(
            "no -R given, and this folder has no git remote `origin`; pass -R owner/repo"
                .to_string(),
        );
    }
    Repo::from_remote(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "`origin` is not a repository on github.com; pass -R owner/repo".to_string())
}

// --- GitHub's API --------------------------------------------------------------

/// An answer: its status and body.
struct Answer {
    status: reqwest::StatusCode,
    body: Vec<u8>,
}

/// One repository on one API, with the token for it.
struct GitHub<'a> {
    api: &'a str,
    repo: &'a Repo,
    token: &'a str,
}

impl GitHub<'_> {
    /// The address of `path` under this repository.
    fn repo_url(&self, path: &str) -> String {
        format!(
            "{}/repos/{}/{}{path}",
            self.api.trim_end_matches('/'),
            self.repo.owner,
            self.repo.name
        )
    }

    /// One request to `url`, redirects followed by [`send`], within
    /// [`REQUEST_BOUND`]: its answer, of at most `cap` bytes.
    fn fetch(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
        cap: usize,
    ) -> Result<Answer, String> {
        let (api, token, url) = (
            self.api.to_string(),
            self.token.to_string(),
            url.to_string(),
        );
        crate::runtime::http::run_async(async move {
            tokio::time::timeout(REQUEST_BOUND, send(&api, &token, method, url, body, cap))
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "GitHub gave no answer within {}s",
                        REQUEST_BOUND.as_secs()
                    ))
                })
        })
    }

    /// `path` under this repository as JSON (`Null` for an empty answer);
    /// `what` names the request in an error. Called for every API request
    /// but the artifact download.
    fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        what: &str,
    ) -> Result<Value, String> {
        let answer = self.fetch(method, &self.repo_url(path), body, MAX_API_BYTES)?;
        if !answer.status.is_success() {
            return Err(self.refused(&answer, what));
        }
        if answer.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&answer.body)
            .map_err(|e| format!("GitHub's answer to {what} did not parse: {e}"))
    }

    /// What to say when GitHub answered `answer` (not a success) to `what`:
    /// the status, GitHub's own message, and what usually causes it.
    fn refused(&self, answer: &Answer, what: &str) -> String {
        let message = serde_json::from_slice::<Value>(&answer.body)
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(printable))
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        let hint = match answer.status.as_u16() {
            401 => format!(
                " (the token in {} / {} was not accepted)",
                TOKEN_VARS[0], TOKEN_VARS[1]
            ),
            403 => format!(
                " (the token may not do this; it needs Actions: read and write on {})",
                self.repo
            ),
            404 => format!(
                " (nothing the token can see by that name: does {} exist, and is \
                 .github/workflows/{WORKFLOW} on the branch? See docs/remote-runs.md)",
                self.repo
            ),
            _ => String::new(),
        };
        format!(
            "GitHub answered {} to {what} in {}{message}{hint}",
            answer.status, self.repo
        )
    }
}

/// Whether `url` is on `api`'s origin: the same scheme, host and port.
fn same_origin(url: &reqwest::Url, api: &str) -> bool {
    reqwest::Url::parse(api).is_ok_and(|api| {
        api.scheme() == url.scheme()
            && api.host_str().map(str::to_ascii_lowercase)
                == url.host_str().map(str::to_ascii_lowercase)
            && api.port_or_known_default() == url.port_or_known_default()
    })
}

/// Send `method` to `url`, following up to [`MAX_REDIRECTS`] redirects one
/// hop at a time over `runtime::http::no_redirect_client`. Every hop must be
/// https or this machine (`key_may_travel_to`); the token and GitHub's
/// headers go only on a hop to `api`'s origin, so the artifact download's
/// redirect to blob storage is fetched without them. The body read is at
/// most `cap` bytes. Called by [`GitHub::fetch`].
async fn send(
    api: &str,
    token: &str,
    mut method: reqwest::Method,
    url: String,
    mut body: Option<Value>,
    cap: usize,
) -> Result<Answer, String> {
    let mut at = url;
    for _ in 0..=MAX_REDIRECTS {
        let parsed = reqwest::Url::parse(&at)
            .map_err(|e| format!("an address to fetch is not valid: {e}"))?;
        let host = parsed.host_str().unwrap_or("?").to_string();
        let carries_token = same_origin(&parsed, api);
        crate::agent::catalogue::key_may_travel_to(&at).map_err(|why| {
            if carries_token {
                format!("the GitHub token is not sent to {api}: {why}")
            } else {
                format!("{host} was not fetched: {why}")
            }
        })?;
        let mut request = crate::runtime::http::no_redirect_client(&at)?
            .request(method.clone(), parsed)
            .header(reqwest::header::USER_AGENT, AGENT_NAME);
        if carries_token {
            request = request
                .bearer_auth(token)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION);
        }
        if let Some(body) = &body {
            request = request.json(body);
        }
        let mut response = request.send().await.map_err(|e| {
            format!(
                "the request to {host} failed: {}",
                crate::runtime::http::error_chain(&e.without_url())
            )
        })?;
        let status = response.status();
        if status.is_redirection() {
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{host} redirected ({status}) to no address"))?;
            at = response
                .url()
                .join(next)
                .map_err(|e| format!("{host} redirected to an address that is not valid: {e}"))?
                .to_string();
            // As a browser does: a 303, or a 301/302 after a POST, becomes a GET.
            let to_get = status == reqwest::StatusCode::SEE_OTHER
                || (method == reqwest::Method::POST
                    && matches!(
                        status,
                        reqwest::StatusCode::MOVED_PERMANENTLY | reqwest::StatusCode::FOUND
                    ));
            if to_get {
                method = reqwest::Method::GET;
                body = None;
            }
            continue;
        }
        let mut out = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            format!(
                "reading {host}'s answer failed: {}",
                crate::runtime::http::error_chain(&e.without_url())
            )
        })? {
            if out.len() + chunk.len() > cap {
                return Err(format!("{host}'s answer is larger than {cap} bytes"));
            }
            out.extend_from_slice(&chunk);
        }
        return Ok(Answer { status, body: out });
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

/// `text` with control characters dropped and at most 300 characters kept:
/// what GitHub wrote, safe to put on a terminal.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(300).collect()
}

// --- runs ----------------------------------------------------------------------

/// A workflow run, as GitHub describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    id: u64,
    url: String,
    title: String,
    status: String,
    conclusion: Option<String>,
}

impl Run {
    fn from_json(v: &Value) -> Option<Run> {
        let text = |key: &str| v.get(key).and_then(Value::as_str).map(printable);
        Some(Run {
            id: v.get("id")?.as_u64()?,
            url: text("html_url")?,
            title: text("display_title")
                .or_else(|| text("name"))
                .unwrap_or_default(),
            status: text("status").unwrap_or_default(),
            conclusion: text("conclusion"),
        })
    }

    /// Its conclusion once completed, else its status.
    fn state(&self) -> &str {
        match (&self.conclusion, self.status.as_str()) {
            (Some(conclusion), "completed") => conclusion,
            _ => &self.status,
        }
    }
}

/// The runs of [`WORKFLOW`] that `listing` (a runs answer) holds.
fn runs_in(listing: &Value) -> Vec<Run> {
    listing
        .get("workflow_runs")
        .and_then(Value::as_array)
        .map(|runs| runs.iter().filter_map(Run::from_json).collect())
        .unwrap_or_default()
}

/// Run `run_id` as it is now.
fn get_run(gh: &GitHub<'_>, run_id: u64) -> Result<Run, String> {
    let v = gh.json(
        reqwest::Method::GET,
        &format!("/actions/runs/{run_id}"),
        None,
        &format!("the read of run {run_id}"),
    )?;
    Run::from_json(&v).ok_or_else(|| format!("GitHub's description of run {run_id} is incomplete"))
}

/// `incode remote run`: dispatch [`WORKFLOW`] with the task and a new id,
/// find its run ([`discover`]), and unless `--no-wait` poll it every
/// [`Pace::poll_every`] until it completes or `args.wait` runs out, then
/// read its result ([`finish`]). Called by [`cli_with`].
fn run(
    gh: &GitHub<'_>,
    args: &RunArgs,
    pace: &Pace,
    progress: &mut dyn FnMut(&str),
) -> Result<Outcome, String> {
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let git_ref = match &args.git_ref {
        Some(git_ref) => git_ref.clone(),
        None => gh
            .json(reqwest::Method::GET, "", None, "the read of the repository")?
            .get("default_branch")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("GitHub named no default branch for {}; pass --ref", gh.repo))?,
    };
    let mut inputs = json!({"task": args.task, "id": id});
    if let Some(model) = &args.model {
        inputs["model"] = json!(model);
    }
    gh.json(
        reqwest::Method::POST,
        &format!("/actions/workflows/{WORKFLOW}/dispatches"),
        Some(json!({"ref": git_ref, "inputs": inputs})),
        &format!("the dispatch of {WORKFLOW} on `{git_ref}`"),
    )?;
    progress(&format!("dispatched {id} on {} ({git_ref})", gh.repo));
    let found = discover(gh, &id, pace)?;
    let Some(wait) = args.wait else {
        return Ok(Outcome {
            text: format!(
                "Run {} started: {}\n`incode remote logs {} -R {}` shows its result once it ends.",
                found.id, found.url, found.id, gh.repo
            ),
            ok: true,
        });
    };
    progress(&format!(
        "run {}: {} (waiting up to {} min)",
        found.id,
        found.url,
        wait.as_secs().div_ceil(60)
    ));
    let deadline = Instant::now().checked_add(wait);
    let finished = loop {
        let now = get_run(gh, found.id)?;
        if now.status == "completed" {
            break now;
        }
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            return Ok(Outcome {
                text: format!(
                    "Run {} is still running ({}) after {} min; it was not cancelled: {}\n\
                     `incode remote logs {} -R {}` shows its result once it ends.",
                    now.id,
                    now.status,
                    wait.as_secs().div_ceil(60),
                    now.url,
                    now.id,
                    gh.repo
                ),
                ok: false,
            });
        }
        std::thread::sleep(left.map_or(pace.poll_every, |left| left.min(pace.poll_every)));
    };
    finish(gh, &finished, progress)
}

/// The run [`run`] dispatched: the one among the latest dispatched runs of
/// [`WORKFLOW`] whose title holds `id` as a word, looked for every
/// [`Pace::discover_every`] for [`Pace::discover_for`].
fn discover(gh: &GitHub<'_>, id: &str, pace: &Pace) -> Result<Run, String> {
    let deadline = Instant::now() + pace.discover_for;
    loop {
        let listing = gh.json(
            reqwest::Method::GET,
            &format!("/actions/workflows/{WORKFLOW}/runs?event=workflow_dispatch&per_page=20"),
            None,
            "the list of runs",
        )?;
        if let Some(found) = runs_in(&listing)
            .into_iter()
            .find(|r| r.title.split_whitespace().any(|word| word == id))
        {
            return Ok(found);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the run for {id} did not appear within {}s of its dispatch; \
                 `incode remote list -R {}` shows the latest",
                pace.discover_for.as_secs(),
                gh.repo
            ));
        }
        std::thread::sleep(pace.discover_every);
    }
}

/// A completed run's outcome: its result printed as `-t --output-format json`
/// prints it, `branch` always present; `ok` only when the run succeeded and
/// the task did not report an error. `Err` when it left no result. Called
/// by [`run`].
fn finish(
    gh: &GitHub<'_>,
    finished: &Run,
    progress: &mut dyn FnMut(&str),
) -> Result<Outcome, String> {
    let conclusion = finished
        .conclusion
        .as_deref()
        .unwrap_or("with no conclusion");
    let Some(mut result) = fetch_result(gh, finished.id)? else {
        return Err(format!(
            "run {} ended {conclusion} and left no {ARTIFACT} artifact: {}",
            finished.id, finished.url
        ));
    };
    if let Some(fields) = result.as_object_mut() {
        fields.entry("branch").or_insert(Value::Null);
    }
    let task_failed = result.get("is_error").and_then(Value::as_bool) == Some(true);
    let ok = conclusion == "success" && !task_failed;
    if !ok {
        progress(&format!(
            "run {} ended {conclusion}{}: {}",
            finished.id,
            if task_failed {
                ", and the task reported an error"
            } else {
                ""
            },
            finished.url
        ));
    }
    Ok(Outcome {
        text: result.to_string(),
        ok,
    })
}

/// The `result.json` in run `run_id`'s [`ARTIFACT`] artifact; `None` when the
/// run has no such artifact. The download is fetched by [`GitHub::fetch`],
/// so the token goes to the API and not where it redirects.
fn fetch_result(gh: &GitHub<'_>, run_id: u64) -> Result<Option<Value>, String> {
    let listing = gh.json(
        reqwest::Method::GET,
        &format!("/actions/runs/{run_id}/artifacts?per_page=100"),
        None,
        &format!("the list of run {run_id}'s artifacts"),
    )?;
    let Some(artifact) = listing
        .get("artifacts")
        .and_then(Value::as_array)
        .and_then(|all| {
            all.iter()
                .find(|a| a.get("name").and_then(Value::as_str) == Some(ARTIFACT))
        })
    else {
        return Ok(None);
    };
    if artifact.get("expired").and_then(Value::as_bool) == Some(true) {
        return Err(format!("run {run_id}'s {ARTIFACT} artifact has expired"));
    }
    let url = artifact
        .get("archive_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("run {run_id}'s {ARTIFACT} artifact has no download address"))?;
    let answer = gh.fetch(reqwest::Method::GET, url, None, MAX_ARTIFACT_BYTES)?;
    if !answer.status.is_success() {
        return Err(gh.refused(&answer, &format!("the download of run {run_id}'s result")));
    }
    read_result(&answer.body).map(Some)
}

/// [`RESULT_FILE`] in the zip `archive`, as a JSON object, read in memory
/// and at most [`MAX_RESULT_BYTES`].
fn read_result(archive: &[u8]) -> Result<Value, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .map_err(|e| format!("the {ARTIFACT} artifact is not a readable zip: {e}"))?;
    let entry = zip
        .by_name(RESULT_FILE)
        .map_err(|_| format!("the {ARTIFACT} artifact holds no {RESULT_FILE}"))?;
    let mut text = Vec::new();
    entry
        .take(MAX_RESULT_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(|e| format!("{RESULT_FILE} could not be read: {e}"))?;
    if text.len() as u64 > MAX_RESULT_BYTES {
        return Err(format!(
            "{RESULT_FILE} is larger than {MAX_RESULT_BYTES} bytes"
        ));
    }
    let result: Value =
        serde_json::from_slice(&text).map_err(|e| format!("{RESULT_FILE} is not JSON: {e}"))?;
    if result.is_object() {
        Ok(result)
    } else {
        Err(format!("{RESULT_FILE} is not a JSON object"))
    }
}

/// `incode remote list`: the latest runs of [`WORKFLOW`], one a line.
fn list(gh: &GitHub<'_>) -> Result<Outcome, String> {
    let listing = gh.json(
        reqwest::Method::GET,
        &format!("/actions/workflows/{WORKFLOW}/runs?per_page=20"),
        None,
        "the list of runs",
    )?;
    let runs = runs_in(&listing);
    let text = if runs.is_empty() {
        format!("No runs of {WORKFLOW} in {} yet.", gh.repo)
    } else {
        runs.iter()
            .map(|r| format!("{:<12} {:<11} {}  {}", r.id, r.state(), r.title, r.url))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Outcome { text, ok: true })
}

/// `incode remote logs <run-id>`: the run's state and address, and its
/// result once it has completed.
fn logs(gh: &GitHub<'_>, run_id: u64) -> Result<Outcome, String> {
    let found = get_run(gh, run_id)?;
    let mut text = format!(
        "Run {} ({}): {}\n{}\n",
        found.id,
        found.title,
        found.state(),
        found.url
    );
    if found.status != "completed" {
        text.push_str("Still running; its result can be read once it ends.");
    } else {
        match fetch_result(gh, run_id)? {
            Some(result) => text.push_str(&result.to_string()),
            None => text.push_str(&format!("It left no {ARTIFACT} artifact.")),
        }
    }
    Ok(Outcome { text, ok: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "fake-token-for-tests";

    /// A request the fake saw.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
        fn path(&self) -> &str {
            self.target.split('?').next().unwrap_or_default()
        }
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).expect("a JSON body")
        }
    }

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    fn reply(status: u16, body: Value) -> Reply {
        Reply {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string().into_bytes(),
        }
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    /// A server on 127.0.0.1, addressed as `host` (`127.0.0.1` or
    /// `localhost`), that hands each request to `answer` along with its own
    /// base address: that base, and every request it saw.
    fn serve(host: &str, answer: impl Fn(&Seen, &str) -> Reply + Send + 'static) -> (String, Log) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!(
            "http://{host}:{}",
            listener.local_addr().expect("addr").port()
        );
        let log: Log = Arc::default();
        let (seen_by_thread, base_for_thread) = (Arc::clone(&log), base.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(seen) = read_request(&mut stream) else {
                    continue;
                };
                seen_by_thread.lock().expect("log").push(seen.clone());
                let out = answer(&seen, &base_for_thread);
                let mut head = format!("HTTP/1.1 {} X\r\n", out.status);
                for (name, value) in &out.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str(&format!(
                    "content-length: {}\r\nconnection: close\r\n\r\n",
                    out.body.len()
                ));
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&out.body);
            }
        });
        (base, log)
    }

    /// One request: its line, headers, and the body its content-length names.
    fn read_request(stream: &mut std::net::TcpStream) -> Option<Seen> {
        let mut seen = Vec::new();
        let mut buf = [0u8; 8192];
        let end = loop {
            let n = stream.read(&mut buf).ok()?;
            if n == 0 {
                return None;
            }
            seen.extend_from_slice(&buf[..n]);
            if let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                break end;
            }
        };
        let head = String::from_utf8_lossy(&seen[..end]).to_string();
        let mut lines = head.lines();
        let mut first = lines.next()?.split_whitespace();
        let (method, target) = (first.next()?.to_string(), first.next()?.to_string());
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
            .collect();
        let length = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = seen[end + 4..].to_vec();
        while body.len() < length {
            let n = stream.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        }
        Some(Seen {
            method,
            target,
            headers,
            body,
        })
    }

    /// A zip holding `result.json` = `result`, written with the zip crate.
    fn result_zip(result: &Value) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(RESULT_FILE, zip::write::SimpleFileOptions::default())
            .expect("entry");
        writer
            .write_all(result.to_string().as_bytes())
            .expect("body");
        writer.finish().expect("finish").into_inner()
    }

    /// How the fake run ends: its conclusion (`None`: it never completes)
    /// and the `result.json` its artifact holds (`None`: no artifact).
    struct Script {
        conclusion: Option<&'static str>,
        result: Option<Value>,
    }

    /// A fake GitHub for repository `o/r`, with the artifact served by a
    /// second server on another host name (`localhost`) that the API's
    /// download address redirects to — as GitHub's blob storage is.
    /// The dispatched run appears on the second listing and completes on
    /// the third poll. Returns the API base, its log, and the blob's log.
    fn fake_github(script: Script) -> (String, Log, Log) {
        let zip = script.result.as_ref().map(result_zip);
        let (blob, blob_log) = serve("localhost", move |seen, _| match &zip {
            Some(zip) if seen.path() == "/blob/result.zip" => Reply {
                status: 200,
                headers: vec![("content-type".into(), "application/zip".into())],
                body: zip.clone(),
            },
            _ => reply(404, json!({"message": "no such blob"})),
        });
        let dispatched_id: Arc<Mutex<Option<String>>> = Arc::default();
        let listings = AtomicUsize::new(0);
        let polls = AtomicUsize::new(0);
        let has_artifact = script.result.is_some();
        let (api, api_log) = serve("127.0.0.1", move |seen, base| {
            let run = |status: &str, conclusion: Option<&str>| {
                json!({
                    "id": 77,
                    "html_url": "https://github.com/o/r/actions/runs/77",
                    "display_title": format!(
                        "incode remote {}",
                        dispatched_id.lock().expect("id").clone().unwrap_or_default()
                    ),
                    "status": status,
                    "conclusion": conclusion,
                })
            };
            match (seen.method.as_str(), seen.path()) {
                ("GET", "/repos/o/r") => reply(200, json!({"default_branch": "main"})),
                ("POST", "/repos/o/r/actions/workflows/incode-remote.yml/dispatches") => {
                    let id = seen.json()["inputs"]["id"].as_str().map(str::to_string);
                    *dispatched_id.lock().expect("id") = id;
                    Reply {
                        status: 204,
                        headers: Vec::new(),
                        body: Vec::new(),
                    }
                }
                ("GET", "/repos/o/r/actions/workflows/incode-remote.yml/runs") => {
                    let other = json!({
                        "id": 76, "html_url": "https://github.com/o/r/actions/runs/76",
                        "display_title": "incode remote 000000000000",
                        "status": "completed", "conclusion": "success"
                    });
                    if listings.fetch_add(1, Ordering::SeqCst) == 0 {
                        reply(200, json!({"workflow_runs": [other]}))
                    } else {
                        reply(200, json!({"workflow_runs": [run("queued", None), other]}))
                    }
                }
                ("GET", "/repos/o/r/actions/runs/77") => {
                    let n = polls.fetch_add(1, Ordering::SeqCst);
                    match script.conclusion {
                        Some(conclusion) if n >= 2 => {
                            reply(200, run("completed", Some(conclusion)))
                        }
                        _ => reply(200, run("in_progress", None)),
                    }
                }
                ("GET", "/repos/o/r/actions/runs/77/artifacts") => {
                    let artifacts = if has_artifact {
                        json!([{
                            "name": ARTIFACT,
                            "expired": false,
                            "archive_download_url": format!("{base}/repos/o/r/actions/artifacts/5/zip"),
                        }])
                    } else {
                        json!([])
                    };
                    reply(200, json!({"artifacts": artifacts}))
                }
                ("GET", "/repos/o/r/actions/artifacts/5/zip") => Reply {
                    status: 302,
                    headers: vec![(
                        "location".into(),
                        format!("{blob}/blob/result.zip?sig=signed"),
                    )],
                    body: Vec::new(),
                },
                _ => reply(404, json!({"message": "Not Found"})),
            }
        });
        (api, api_log, blob_log)
    }

    const FAST: Pace = Pace {
        discover_every: Duration::from_millis(20),
        discover_for: Duration::from_secs(5),
        poll_every: Duration::from_millis(20),
    };

    fn repo() -> Repo {
        Repo::parse("o/r").expect("repo")
    }

    fn run_args(wait: Option<Duration>) -> RunArgs {
        RunArgs {
            task: "fix the flaky test".to_string(),
            repo: None,
            git_ref: None,
            model: Some("model-x".to_string()),
            wait,
        }
    }

    /// Run `args` against `api`: the outcome and the progress lines.
    fn run_against(api: &str, args: &RunArgs) -> (Result<Outcome, String>, Vec<String>) {
        let repo = repo();
        let gh = GitHub {
            api,
            repo: &repo,
            token: TOKEN,
        };
        let mut said = Vec::new();
        let outcome = run(&gh, args, &FAST, &mut |line| said.push(line.to_string()));
        (outcome, said)
    }

    fn seen(log: &Log) -> Vec<Seen> {
        log.lock().expect("log").clone()
    }

    /// **remote::run DISPATCHES THE TASK, FINDS ITS RUN BY ID, WAITS FOR IT
    /// AND PRINTS ITS RESULT**: the dispatch carries the task, model and id
    /// on the default branch, with the token in the Authorization header and
    /// never in an address; the run is told apart from another by the id in
    /// its title; it is polled to completion; its artifact's result.json is
    /// printed with its branch.
    #[test]
    fn remote_run_dispatches_finds_its_run_by_id_waits_and_prints_the_result() {
        let result = json!({
            "type": "result", "is_error": false, "result": "fixed it",
            "branch": "incode/remote-abc"
        });
        let (api, api_log, blob_log) = fake_github(Script {
            conclusion: Some("success"),
            result: Some(result.clone()),
        });
        let (outcome, said) = run_against(&api, &run_args(Some(Duration::from_secs(30))));
        let outcome = outcome.expect("the run finishes");
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&outcome.text).expect("JSON"),
            result
        );

        let api_seen = seen(&api_log);
        let dispatch = api_seen
            .iter()
            .find(|s| s.method == "POST")
            .expect("a dispatch");
        assert_eq!(
            dispatch.path(),
            "/repos/o/r/actions/workflows/incode-remote.yml/dispatches"
        );
        let body = dispatch.json();
        assert_eq!(body["ref"], "main", "the default branch: {body}");
        assert_eq!(body["inputs"]["task"], "fix the flaky test");
        assert_eq!(body["inputs"]["model"], "model-x");
        let id = body["inputs"]["id"].as_str().expect("an id").to_string();
        assert!(
            id.len() == 12 && id.chars().all(|c| c.is_ascii_hexdigit()),
            "{id}"
        );
        for request in &api_seen {
            assert_eq!(
                request.header("authorization"),
                Some(format!("Bearer {TOKEN}").as_str()),
                "{} {}",
                request.method,
                request.target
            );
            assert_eq!(request.header("x-github-api-version"), Some(API_VERSION));
            assert_eq!(request.header("user-agent"), Some(AGENT_NAME));
            assert!(!request.target.contains(TOKEN), "{}", request.target);
        }
        let listings = api_seen
            .iter()
            .filter(|s| s.path().ends_with("/incode-remote.yml/runs"))
            .count();
        assert_eq!(listings, 2, "found on the second listing, not before");
        assert!(api_seen
            .iter()
            .filter(|s| s.path().ends_with("/incode-remote.yml/runs"))
            .all(|s| s.target.contains("event=workflow_dispatch")));
        let polls = api_seen
            .iter()
            .filter(|s| s.path() == "/repos/o/r/actions/runs/77")
            .count();
        assert_eq!(polls, 3, "polled until completed");
        assert!(said.iter().any(|l| l.contains(&id)), "{said:?}");
        assert!(
            said.iter()
                .any(|l| l.contains("https://github.com/o/r/actions/runs/77")),
            "{said:?}"
        );

        let blob_seen = seen(&blob_log);
        assert_eq!(blob_seen.len(), 1, "the artifact was downloaded once");
        assert_eq!(blob_seen[0].header("authorization"), None);
    }

    /// **remote::fetch_result DOES NOT SEND THE TOKEN WHERE THE ARTIFACT'S
    /// REDIRECT LEADS**: the API's download address gets it; the blob host
    /// it redirects to (another host name) does not, nor any GitHub header.
    #[test]
    fn remote_fetch_result_keeps_the_token_off_the_redirect_target() {
        let (api, api_log, blob_log) = fake_github(Script {
            conclusion: Some("success"),
            result: Some(json!({"type": "result", "is_error": false})),
        });
        let repo = repo();
        let gh = GitHub {
            api: &api,
            repo: &repo,
            token: TOKEN,
        };
        let result = fetch_result(&gh, 77).expect("fetched").expect("a result");
        assert_eq!(result["type"], "result");

        let download = seen(&api_log)
            .into_iter()
            .find(|s| s.path() == "/repos/o/r/actions/artifacts/5/zip")
            .expect("the API's download address was asked");
        assert_eq!(
            download.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        let blob = seen(&blob_log);
        assert_eq!(blob.len(), 1, "{blob:?}");
        assert_eq!(blob[0].target, "/blob/result.zip?sig=signed");
        assert_eq!(
            blob[0].header("authorization"),
            None,
            "the token went to the redirect's host"
        );
        assert_eq!(blob[0].header("x-github-api-version"), None);
    }

    /// **remote::run, WHEN THE WAIT RUNS OUT, SAYS THE RUN IS STILL RUNNING
    /// AND CANCELS NOTHING**: it exits non-zero with the run's address, and
    /// asks GitHub nothing but reads after the dispatch.
    #[test]
    fn remote_run_reports_a_run_still_going_when_the_timeout_expires() {
        let (api, api_log, _) = fake_github(Script {
            conclusion: None,
            result: None,
        });
        let (outcome, _) = run_against(&api, &run_args(Some(Duration::from_millis(300))));
        let outcome = outcome.expect("an outcome");
        assert!(!outcome.ok, "{outcome:?}");
        assert!(outcome.text.contains("still running"), "{}", outcome.text);
        assert!(outcome.text.contains("not cancelled"), "{}", outcome.text);
        assert!(
            outcome
                .text
                .contains("https://github.com/o/r/actions/runs/77"),
            "{}",
            outcome.text
        );
        let api_seen = seen(&api_log);
        assert_eq!(
            api_seen.iter().filter(|s| s.method != "GET").count(),
            1,
            "only the dispatch writes: {api_seen:?}"
        );
        assert!(!api_seen.iter().any(|s| s.target.contains("cancel")));
    }

    /// **remote::run --no-wait RETURNS ONCE THE RUN IS FOUND**, with its
    /// address, without polling it.
    #[test]
    fn remote_run_without_waiting_returns_the_runs_address() {
        let (api, api_log, _) = fake_github(Script {
            conclusion: Some("success"),
            result: None,
        });
        let (outcome, _) = run_against(&api, &run_args(None));
        let outcome = outcome.expect("an outcome");
        assert!(outcome.ok);
        assert!(outcome
            .text
            .contains("Run 77 started: https://github.com/o/r/actions/runs/77"));
        assert!(!seen(&api_log)
            .iter()
            .any(|s| s.path() == "/repos/o/r/actions/runs/77"));
    }

    /// **remote::finish: A FAILED RUN, OR A TASK THAT REPORTED AN ERROR,
    /// EXITS NON-ZERO** — still printing the result it left — and a failed
    /// run that left none is an error naming its conclusion.
    #[test]
    fn remote_run_with_a_failed_conclusion_or_task_exits_non_zero() {
        // The run failed though the task did not say so.
        let (api, _, _) = fake_github(Script {
            conclusion: Some("failure"),
            result: Some(json!({"type": "result", "is_error": false, "result": "done"})),
        });
        let (outcome, said) = run_against(&api, &run_args(Some(Duration::from_secs(30))));
        let outcome = outcome.expect("an outcome");
        assert!(!outcome.ok, "a failed run exited 0");
        let printed: Value = serde_json::from_str(&outcome.text).expect("JSON");
        assert_eq!(printed["result"], "done");
        assert_eq!(printed["branch"], Value::Null, "branch is always present");
        assert!(said.iter().any(|l| l.contains("ended failure")), "{said:?}");

        // The run succeeded but the task reported an error.
        let (api, _, _) = fake_github(Script {
            conclusion: Some("success"),
            result: Some(json!({"type": "result", "is_error": true, "error": "verify failed"})),
        });
        let (outcome, _) = run_against(&api, &run_args(Some(Duration::from_secs(30))));
        assert!(!outcome.expect("an outcome").ok, "is_error exited 0");

        let (api, _, _) = fake_github(Script {
            conclusion: Some("cancelled"),
            result: None,
        });
        let (outcome, _) = run_against(&api, &run_args(Some(Duration::from_secs(30))));
        let said = outcome.expect_err("no result");
        assert!(
            said.contains("ended cancelled") && said.contains(ARTIFACT),
            "{said}"
        );
    }

    /// **remote::Repo::from_remote READS THE REPOSITORY FROM HTTPS AND SSH
    /// REMOTES** of github.com, and nothing else; `-R` is held to the same
    /// characters, so it cannot reshape an API path.
    #[test]
    fn remote_repo_is_parsed_from_https_and_ssh_remotes() {
        let want = Some(Repo {
            owner: "incordai".into(),
            name: "incode".into(),
        });
        for url in [
            "https://github.com/incordai/incode.git",
            "https://github.com/incordai/incode",
            "https://github.com/incordai/incode/\n",
            "https://user:secret@github.com/incordai/incode.git",
            "git@github.com:incordai/incode.git",
            "git@github.com:incordai/incode",
            "ssh://git@github.com/incordai/incode.git",
            "ssh://git@ssh.github.com:443/incordai/incode.git",
        ] {
            assert_eq!(Repo::from_remote(url), want, "{url}");
        }
        for url in [
            "https://gitlab.com/incordai/incode.git",
            "git@gitlab.com:incordai/incode.git",
            "https://github.com/incordai",
            "https://github.com/incordai/incode/tree/main",
            "/home/me/incode",
            "",
        ] {
            assert_eq!(Repo::from_remote(url), None, "{url}");
        }
        assert_eq!(
            Repo::parse("incordai/incode.git").map(|r| r.to_string()),
            Ok("incordai/incode".to_string())
        );
        for bad in [
            "incordai", "a/b/c", "../x", "o/..", "o/r?x=1", "o r/x", "/r",
        ] {
            assert!(Repo::parse(bad).is_err(), "{bad}");
        }
    }

    /// **remote::token_from: NO TOKEN IS A CLEAR ERROR NAMING THE
    /// VARIABLES**, raised before any request; GH_TOKEN comes before
    /// GITHUB_TOKEN, and a blank one is not a token.
    #[test]
    fn remote_without_a_token_names_the_variables_to_set() {
        let none = |_: &str| None;
        let args: Vec<String> = ["list", "-R", "o/r"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let said = cli_with(
            &args,
            Path::new("."),
            "http://127.0.0.1:9/",
            &none,
            &FAST,
            &mut |_| {},
        )
        .expect_err("no token");
        assert!(
            said.contains("GH_TOKEN") && said.contains("GITHUB_TOKEN"),
            "{said}"
        );

        let both = |name: &str| match name {
            "GH_TOKEN" => Some("first".to_string()),
            "GITHUB_TOKEN" => Some("second".to_string()),
            _ => None,
        };
        assert_eq!(token_from(&both), Ok("first".to_string()));
        let blank_first = |name: &str| match name {
            "GH_TOKEN" => Some("  ".to_string()),
            "GITHUB_TOKEN" => Some("second".to_string()),
            _ => None,
        };
        assert_eq!(token_from(&blank_first), Ok("second".to_string()));
    }

    /// **remote::send NEVER SENDS THE TOKEN IN CLEAR TO ANOTHER MACHINE**,
    /// and an error never holds the token: plain http elsewhere is refused
    /// before connecting; a 401 says what was refused, not the token.
    #[test]
    fn remote_send_holds_the_token_to_https_or_this_machine() {
        let repo = repo();
        let gh = GitHub {
            api: "http://api.example.invalid",
            repo: &repo,
            token: TOKEN,
        };
        let said = list(&gh).expect_err("plain http elsewhere");
        assert!(said.contains("not sent"), "{said}");
        assert!(!said.contains(TOKEN));

        let (api, _) = serve("127.0.0.1", |_, _| {
            reply(401, json!({"message": "Bad credentials"}))
        });
        let gh = GitHub {
            api: &api,
            repo: &repo,
            token: TOKEN,
        };
        let said = list(&gh).expect_err("refused");
        assert!(
            said.contains("401") && said.contains("Bad credentials"),
            "{said}"
        );
        assert!(said.contains("GH_TOKEN"), "{said}");
        assert!(!said.contains(TOKEN), "{said}");
    }

    /// **remote::list AND remote::logs SHOW RUNS AND A RESULT**: one line a
    /// run (id, state, title, address); a completed run's result read from
    /// its artifact.
    #[test]
    fn remote_list_and_logs_show_runs_and_a_result() {
        let (api, _, _) = fake_github(Script {
            conclusion: Some("success"),
            result: Some(json!({"type": "result", "is_error": false, "result": "ok"})),
        });
        let repo = repo();
        let gh = GitHub {
            api: &api,
            repo: &repo,
            token: TOKEN,
        };
        let listed = list(&gh).expect("listed");
        assert!(listed.ok);
        assert!(
            listed.text.contains("76")
                && listed.text.contains("success")
                && listed.text.contains("incode remote 000000000000")
                && listed
                    .text
                    .contains("https://github.com/o/r/actions/runs/76"),
            "{}",
            listed.text
        );

        let running = logs(&gh, 77).expect("logs");
        assert!(running.text.contains("in_progress"), "{}", running.text);
        assert!(running.text.contains("Still running"), "{}", running.text);
        let _ = logs(&gh, 77).expect("logs");
        let done = logs(&gh, 77).expect("logs");
        assert!(done.text.contains("success"), "{}", done.text);
        assert!(done.text.contains(r#""result":"ok""#), "{}", done.text);
    }

    /// **remote::parse READS EACH VERB'S ARGUMENTS** and refuses what it
    /// does not know, even beside `--help`.
    #[test]
    fn remote_parse_reads_each_verbs_arguments() {
        let parse = |args: &[&str]| parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            parse(&[
                "run",
                "do it",
                "-R",
                "o/r",
                "--ref",
                "dev",
                "-m",
                "m1",
                "--timeout",
                "5"
            ]),
            Ok(Verb::Run(RunArgs {
                task: "do it".into(),
                repo: Some("o/r".into()),
                git_ref: Some("dev".into()),
                model: Some("m1".into()),
                wait: Some(Duration::from_secs(300)),
            }))
        );
        assert!(matches!(
            parse(&["run", "do it"]),
            Ok(Verb::Run(RunArgs { wait: Some(w), .. })) if w == Duration::from_secs(3600)
        ));
        assert!(matches!(
            parse(&["run", "do it", "--no-wait"]),
            Ok(Verb::Run(RunArgs { wait: None, .. }))
        ));
        assert!(matches!(
            parse(&["run", "--", "-starts with a dash"]),
            Ok(Verb::Run(RunArgs { task, .. })) if task == "-starts with a dash"
        ));
        assert_eq!(parse(&["list"]), Ok(Verb::List { repo: None }));
        assert_eq!(
            parse(&["logs", "42", "-R", "o/r"]),
            Ok(Verb::Logs {
                repo: Some("o/r".into()),
                run_id: 42
            })
        );
        for help in [&["help"][..], &[], &["--help"], &["run", "--help"]] {
            assert_eq!(parse(help), Ok(Verb::Help), "{help:?}");
        }
        for bad in [
            &["launch"][..],
            &["run"],
            &["run", "two", "words"],
            &["run", "x", "--timeout", "0"],
            &["run", "x", "--timeout", "soon"],
            &["run", "x", "--no-wait", "--timeout", "5"],
            &["run", "x", "--bogus", "--help"],
            &["list", "--timeout", "5"],
            &["list", "extra"],
            &["logs"],
            &["logs", "abc"],
            &["run", "x", "-R"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    /// **THE WORKFLOW TEMPLATE MATCHES WHAT remote.rs RELIES ON**: the
    /// workflow file name, the inputs, the run title the run is found by, the
    /// branch, the artifact and its file — and an input reaches a script only
    /// through an environment variable, never spliced into it.
    #[test]
    fn remote_workflow_template_matches_what_remote_relies_on() {
        let template = include_str!("../.github/workflows/incode-remote.yml.example");
        assert!(template.contains(&format!(".github/workflows/{WORKFLOW}")));
        for needle in [
            "workflow_dispatch:",
            "      task:",
            "      model:",
            "      id:",
            "run-name: \"incode remote ${{ inputs.id }}\"",
            "branch=\"incode/remote-$ID\"",
            &format!("name: {ARTIFACT}"),
            &format!("incode-result/{RESULT_FILE}"),
            "--output-format json",
        ] {
            assert!(template.contains(needle), "the template lacks {needle:?}");
        }
        for line in template.lines().filter(|l| l.contains("${{ inputs.")) {
            let line = line.trim();
            let env_mapping = line
                .split_once(": ${{ inputs.")
                .is_some_and(|(key, _)| key.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
            assert!(
                line.starts_with("run-name:") || env_mapping,
                "an input is used outside run-name and env: {line}"
            );
        }
    }

    /// **remote::read_result READS result.json FROM THE ARTIFACT'S ZIP**,
    /// and says what is wrong with one that has none or is not a zip.
    #[test]
    fn remote_read_result_takes_result_json_from_the_zip() {
        let zip = result_zip(&json!({"type": "result", "num_turns": 3}));
        assert_eq!(read_result(&zip).expect("read")["num_turns"], 3);
        let said = read_result(b"not a zip").expect_err("not a zip");
        assert!(said.contains("not a readable zip"), "{said}");
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("other.txt", zip::write::SimpleFileOptions::default())
            .expect("entry");
        let empty = writer.finish().expect("finish").into_inner();
        assert!(read_result(&empty)
            .expect_err("no result.json")
            .contains("holds no result.json"));
    }
}
