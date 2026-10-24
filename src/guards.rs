//! Paths the agent may read but never write.
//!
//! # The hole this closes
//!
//! [`crate::verify::Verified`] proves a command exited zero. It cannot prove the
//! command still checks what it checked yesterday. Permissions gate by TOOL NAME
//! (`permissions/mod.rs`), never by path — so in a mode that allows editing at
//! all, `edit_file` could rewrite the very tests `verify` then runs, and hand
//! back a genuine, type-safe proof for work that does nothing.
//!
//! An agent does not need to be devious for this to happen. "Make the tests
//! pass" and "make the code correct" are different instructions, and the first
//! one has a much cheaper solution that a model under pressure will find.
//!
//! So the check that decides whether work is done is put out of reach of the
//! thing being judged. That is the whole idea: **you do not get to edit your own
//! exam.**
//!
//! # What is protected by default
//!
//! Tests, CI configuration, migrations, environment files, and the ledger. Each
//! is a case where a write is either self-marking (tests, CI), destructive and
//! irreversible (migrations), a credential leak (`.env`), or a way to declare
//! work finished without doing it (the ledger).
//!
//! # Reading is always allowed
//!
//! An agent that cannot read the tests cannot write code that satisfies them.
//! Only writes are refused, and the refusal says what to do instead.
//!
//! # What this does NOT claim
//!
//! Shell commands are matched textually (`shell`). `bash` can reach a protected
//! file in ways no pattern catches — a variable, a glob expanded by the shell, a
//! script that writes on its behalf. The textual check stops the direct and
//! common forms; the real boundary for shell is the permission tier, which
//! decides whether `bash` may run at all. Anything stronger needs a sandbox, and
//! pretending otherwise would be worse than saying so.

mod shell;

use std::path::Path;
use std::time::Duration;

/// Where the operator's own list lives, under the workspace.
pub const GUARDS_FILE: &str = ".incode/guards.json";

/// Where the body's standing guard categories are read from.
pub const ENFORCEMENT_PATH: &str = "/v1/memory/enforcement";

/// Bounded: read once at session start, with the user waiting.
/// Bounded by the shared waiting deadline: this is advisory session context,
/// fetched before the prompt appears, and it fails open. See
/// [`crate::recall::WAITING_TIMEOUT`].
const TIMEOUT: Duration = crate::recall::WAITING_TIMEOUT;

/// Path fragments that are never writable unless the operator says otherwise.
///
/// Matched against the workspace-relative path with forward slashes, so a
/// pattern like `tests/` catches `tests/api/users.rs` and `crates/x/tests/y.rs`.
/// Each shape is described at [`matches_path`].
///
/// Every place that protects a path reads this list through
/// [`Guards::effective`] — the toolbox's refusal, the [`Sentinel`]'s
/// fingerprints, the deny rules a delegated agent is handed — so a name added
/// here is protected everywhere at once.
pub const DEFAULT_PROTECTED: [&str; 22] = [
    // Self-marking: the check that decides "done", in the shapes each test
    // runner looks for. `test_` is pytest's `test_*.py` (and the same habit in
    // C and shell suites); `conftest.py` is loaded by pytest on its own and its
    // fixtures decide what every test sees; `__tests__/` is Jest's;
    // `.spec.` and `_spec.` are Jasmine/Vitest's and RSpec's; `testdata/` is
    // Go's fixtures, which a test compares against.
    "tests/",
    "test/",
    "_test.",
    ".test.",
    "test_",
    "conftest.py",
    "__tests__/",
    "spec/",
    ".spec.",
    "_spec.",
    "testdata/",
    // Self-marking: what CI runs, and what it is allowed to skip.
    ".github/workflows/",
    ".gitlab-ci.yml",
    ".circleci/",
    "Jenkinsfile",
    "azure-pipelines.yml",
    ".buildkite/",
    // Destructive and usually irreversible.
    "migrations/",
    // Credentials.
    ".env",
    // The record of what was finished, and the operator's settings for how.
    ".incode/ledger.json",
    ".incode/guards.json",
    "admin.json",
];

/// The operator's protected-path policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Guards {
    /// Extra fragments to protect, on top of the defaults.
    #[serde(default)]
    pub protected_paths: Vec<String>,
    /// Defaults the operator has deliberately switched off.
    ///
    /// Removing a default is a decision worth writing down, so it is expressed
    /// as an explicit exemption rather than by replacing the whole list —
    /// a replaced list silently loses every protection added later.
    #[serde(default)]
    pub allow_writes_to: Vec<String>,
}
impl Guards {
    /// Read the operator's guards, or the defaults when there are none.
    ///
    /// A malformed file is an ERROR rather than a silent default: guards that
    /// quietly stop applying are the worst possible failure for this file,
    /// because everything keeps working and nothing is protected.
    pub fn load(workspace: &Path) -> Result<Self, String> {
        let path = workspace.join(GUARDS_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = crate::text::read_authored(&path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid guards: {e}", path.display()))
    }

    /// Every fragment in force, defaults included.
    #[must_use]
    pub fn effective(&self) -> Vec<String> {
        DEFAULT_PROTECTED
            .iter()
            .map(|p| (*p).to_string())
            .chain(self.protected_paths.iter().cloned())
            .filter(|p| !self.allow_writes_to.iter().any(|a| a == p))
            .collect()
    }

    /// The same guards, plus whatever this project's CHECK actually names.
    ///
    /// # The hole this closes
    ///
    /// [`DEFAULT_PROTECTED`] protects by NAME, and a name is not what makes a
    /// file the exam — being run by the check is. So the list was wrong in both
    /// directions: it protected `ball.test.js`, which `node --check game.js`
    /// never reads, and protected nothing at all about a check named
    /// `check-greet.js`. Observed live: refused `ball.test.js`, the agent wrote
    /// `check-rest.js`, `check-jump.js` and `jump-behavior-check.js` instead —
    /// three renames, no pattern matched any of them, and the guard reported
    /// safety it was not providing.
    ///
    /// Deriving protection from the check's own argv cannot be renamed around:
    /// rename the file and the check no longer finds it, so the run fails and
    /// the agent gains nothing. The name-based list stays as well — it covers
    /// CI config, migrations and `.env`, which no verify command names.
    #[must_use]
    pub fn with_check(&self, verify: &[String]) -> Self {
        let mut guards = self.clone();
        for target in verify_targets(verify) {
            if !guards.protected_paths.contains(&target) {
                guards.protected_paths.push(target);
            }
        }
        guards
    }

    /// The protected pattern a write to `path` matches, or `None` when the
    /// write is allowed. The rule [`Self::refuse_write`] names.
    #[must_use]
    pub fn protecting(&self, path: &str) -> Option<String> {
        self.effective()
            .into_iter()
            .find(|fragment| matches_path(fragment, path))
    }

    /// The protected pattern a shell command writes to, or `None`. The rule
    /// [`Self::refuse_command`] names.
    #[must_use]
    pub fn protecting_in_command(&self, command: &str) -> Option<String> {
        // Only what the command WRITES is checked: a redirect target, an
        // argument of a writer. A `grep` or a `cat` over the tests is exactly
        // what an agent should be doing, and a document that merely mentions
        // a test path is not a write to it. The targets are matched by the same
        // rules as a file tool's path, not a second, looser set that would
        // disagree with them.
        let targets = shell::write_targets(command);
        self.effective()
            .into_iter()
            .find(|fragment| targets.iter().any(|target| matches_path(fragment, target)))
    }

    /// Why a write to this path is refused, or `None` when it is allowed.
    #[must_use]
    pub fn refuse_write(&self, path: &str) -> Option<String> {
        let matched = self.protecting(path)?;
        Some(format!(
            "`{path}` is protected and cannot be written: it matches `{matched}`. \
             The check that decides whether work is finished must not be editable \
             by the thing being judged — a test you rewrote is not a test you \
             passed. Fix the code so the existing check passes.\n\n\
             If the check itself is genuinely wrong, say so plainly and ASK: the \
             owner can allow this one write, and a refusal here is a question, \
             not a wall."
        ))
    }

    /// Why this shell command is refused, or `None`.
    ///
    /// Textual, and honestly limited — see the module header. It catches the
    /// direct forms (`rm tests/x`, `>>.env`, `sed -i tests/y`, `python -c
    /// "open('tests/x','w')"`) and does not pretend to catch a determined route
    /// around it.
    #[must_use]
    pub fn refuse_command(&self, command: &str) -> Option<String> {
        let matched = self.protecting_in_command(command)?;
        Some(format!(
            "this command writes to a protected path (`{matched}`), so it is \
             refused. Reading those files is always allowed; changing them is a \
             person's decision, because they are what proves the work."
        ))
    }
}

// ---------------------------------------------------------------------------
// The body's half
// ---------------------------------------------------------------------------

/// One category of operation the owner has asked to be consulted about.
///
/// These come from the memory service, where they are the owner's standing
/// policy across every project and every agent on the machine — not a copy kept
/// here. The harness reads them; it does not decide them and does not classify
/// against them (see [`Categories`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    /// Stable key, e.g. `git_destructive`.
    pub key: String,
    /// What it is called, for a person.
    pub title: String,
    /// What it covers, in the owner's own words.
    pub description: String,
    /// True when the owner set allow-always: the category is no longer guarded.
    pub allowed_always: bool,
}

/// The owner's standing guard categories.
///
/// # Why this does not classify anything
///
/// Deciding whether a given shell command *is* a destructive git operation is a
/// real classifier, and it already exists in the memory service
/// (`cli/guard_match.rs`) where it is applied by the enforcement hook to every
/// agent on this machine. There is no HTTP endpoint for it — verified against
/// the running service — so a harness that wanted to classify would have to
/// **reimplement** it.
///
/// A second classifier that disagrees with the first is worse than none: two
/// answers to "is this dangerous?", and the safe-looking one wins whenever the
/// agent asks the wrong component. So this reads the list and puts it in front
/// of the model, which is the part that is genuinely missing — an agent that
/// knows a category will be stopped does not burn a turn discovering it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Categories {
    pub rules: Vec<Category>,
    /// Whether the owner has the proxy reviewing every command in this project.
    pub forward_all: bool,
}

impl Categories {
    /// Read the owner's standing categories from the body.
    pub fn load() -> Result<Self, String> {
        parse_categories(&get(&format!(
            "{}{ENFORCEMENT_PATH}",
            crate::brief::memory_url().trim_end_matches('/')
        ))?)
    }

    /// The categories still in force — allow-always ones are not guarded.
    #[must_use]
    pub fn guarded(&self) -> Vec<&Category> {
        self.rules.iter().filter(|r| !r.allowed_always).collect()
    }

    /// The block that tells the model what will be stopped before it tries.
    ///
    /// Not a refusal and not a rule the model could satisfy by rewording: these
    /// are decisions the owner has reserved for themselves, and the useful thing
    /// to say is that the work will pause for an answer rather than fail.
    #[must_use]
    pub fn as_context(&self) -> Option<String> {
        let guarded = self.guarded();
        if guarded.is_empty() {
            return None;
        }
        let mut out = String::from(
            "The owner has reserved these kinds of operation for their own \
             decision. You may still propose one — it goes to them and the work \
             waits for an answer, it does not fail — but expect the pause, and \
             prefer a route that does not need one:\n",
        );
        for category in guarded {
            out.push_str(&format!(
                "- {} ({}): {}\n",
                category.title, category.key, category.description
            ));
        }
        if self.forward_all {
            out.push_str(
                "\nEvery command in this project is reviewed before it runs, so \
                 batching work into fewer, clearer commands costs less waiting.\n",
            );
        }
        Some(out)
    }
}

/// Read the service's answer.
///
/// Unknown fields are ignored: the enforcement payload carries a good deal this
/// harness has no business acting on, and it ships on its own schedule.
pub fn parse_categories(body: &str) -> Result<Categories, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("the guard settings did not parse: {e}"))?;
    let rules = value
        .get("rules")
        .and_then(|r| r.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    let key = r.get("key").and_then(|k| k.as_str())?;
                    Some(Category {
                        key: key.to_string(),
                        title: string_at(r, "title"),
                        description: string_at(r, "desc"),
                        allowed_always: r
                            .get("always")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Categories {
        rules,
        forward_all: value
            .get("forward_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The guard context for a session: what the body reserves, if it can be asked.
///
/// Silent when the service is unreachable. The categories are advisory here —
/// the service's own hook enforces them regardless of whether this harness ever
/// read the list, so losing the block costs a wasted turn, never a bypass.
#[must_use]
pub fn context_for_session() -> Option<String> {
    let categories = match Categories::load() {
        Ok(categories) => categories,
        Err(why) => {
            // GUARDS THAT QUIETLY STOP APPLYING is the failure this module calls
            // the worst one it has: everything keeps working and nothing is
            // protected. Silence here looked identical to having none.
            crate::trace!("memory", "guards: could not be loaded: {why}");
            return None;
        }
    };
    let guarded = categories.guarded().len();
    let block = categories.as_context();
    crate::trace!(
        "memory",
        "guards: {guarded} category(ies) in force, block={}",
        block.as_ref().map_or(0, String::len)
    );
    block
}

fn get(url: &str) -> Result<String, String> {
    let url = url.to_string();
    crate::runtime::http::request_text(
        move |client| client.get(&url).timeout(TIMEOUT),
        "guard settings",
    )
}

/// One comparable form: forward slashes, lowercase, no leading `./`.
fn normalise(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .to_lowercase()
}

/// Whether one protected fragment covers this path.
///
/// **Matching is by path SEGMENT, not by raw substring**, because a substring
/// match is wrong in a way that looks right until it bites: `test/` appears
/// inside `src/latest/handler.rs`, and refusing to write an ordinary source file
/// because its directory ends in the letters "test" is the kind of protection
/// that gets switched off within a day.
///
/// Four shapes, each matching what it looks like:
/// - `tests/`, `.github/workflows/` — one or more whole directory segments.
/// - `.env`, `.gitlab-ci.yml`, `admin.json` — a file name, or a name that
///   starts with it (`.env.local` is still an environment file).
/// - `_test.`, `.test.` — an infix in the file NAME only, which is how most
///   languages mark a test file.
/// - `test_` — a letter first and `_` last: a PREFIX of the file name, which
///   is how pytest marks one (`test_api.py`). A prefix and not an infix, so
///   `latest_api.py` and `contest_entry.py` stay writable.
///
/// `src/testing_utils.py` is deliberately NOT matched by `test_`: pytest
/// collects `test_*.py` and `*_test.py`, never `testing_*`, so that file is
/// support code the tests import, not a test — protecting it would refuse
/// ordinary work on a helper. A project whose helpers ARE part of its exam
/// says so in `.incode/guards.json`.
#[must_use]
fn matches_path(fragment: &str, path: &str) -> bool {
    let path = normalise(path);
    let fragment = normalise(fragment);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(file_name) = segments.last().copied() else {
        return false;
    };

    if let Some(dirs) = fragment.strip_suffix('/') {
        // A directory fragment matches a run of whole segments anywhere in the
        // path, so `tests/` catches both `tests/x.rs` and `crates/a/tests/x.rs`.
        let wanted: Vec<&str> = dirs.split('/').filter(|s| !s.is_empty()).collect();
        return !wanted.is_empty()
            && segments
                .windows(wanted.len())
                .any(|window| window == wanted.as_slice());
    }

    if is_name_prefix(&fragment) {
        return file_name.starts_with(&fragment);
    }

    // An infix marker, e.g. `_test.` in `users_test.go`.
    if fragment.starts_with('_') || (fragment.starts_with('.') && fragment.ends_with('.')) {
        return file_name.contains(&fragment);
    }

    // Otherwise a file name. A path fragment with a slash in it must match the
    // tail of the path exactly, so `.incode/ledger.json` does not match a
    // `ledger.json` sitting somewhere else.
    if fragment.contains('/') {
        return path == fragment || path.ends_with(&format!("/{fragment}"));
    }
    file_name == fragment || file_name.starts_with(&format!("{fragment}."))
}

/// The prefix shape of [`matches_path`], e.g. `test_`. One predicate for the
/// matcher and for [`glob_for`], so the harness's own check and the deny rule
/// handed to a delegated agent cannot disagree about what it covers.
fn is_name_prefix(fragment: &str) -> bool {
    fragment.ends_with('_') && fragment.starts_with(|c: char| c.is_ascii_alphabetic())
}

/// The files a check command names, which are the project's real exam.
///
/// # Wired to
///
/// Called by [`Guards::with_check`], which is called by
/// [`crate::agent::worker`] when it builds the sentinel for a task.
///
/// # What counts, and why each rule is there
///
/// - **A flag never does.** `--check` in `node --check game.js` is not a file.
/// - **The first word counts only if it is written as a path.** `node` and
///   `cargo` are programs on `PATH`; `./run-checks.sh` is the check itself, and
///   missing it would leave the one file most worth protecting unguarded.
/// - **Later words count when they look like a file** — a dot or a separator.
///   `cargo test` names none, and correctly adds nothing: its exam is the whole
///   test tree, which the name-based list already covers.
#[must_use]
pub fn verify_targets(verify: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (index, word) in verify.iter().enumerate() {
        let word = word.trim();
        if word.is_empty() || word.starts_with('-') {
            continue;
        }
        let looks_like_a_path = word.contains('/') || word.contains('\\');
        let names_a_file = if index == 0 {
            // The program itself is only a file when it was written as one.
            looks_like_a_path
        } else {
            looks_like_a_path || word.contains('.')
        };
        if !names_a_file {
            continue;
        }
        let normalised = word.replace('\\', "/");
        let normalised = normalised.trim_start_matches("./").to_string();
        if !normalised.is_empty() && !out.contains(&normalised) {
            out.push(normalised);
        }
    }
    out
}

/// The path a file tool is about to write, from its raw JSON arguments.
#[must_use]
pub fn target_path(input: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value
        .get("path")
        .and_then(|p| p.as_str())
        .map(str::to_string)
}

/// The command a bash call is about to run.
#[must_use]
pub fn target_command(input: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value
        .get("command")
        .and_then(|c| c.as_str())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The owner's question about a protected file
// ---------------------------------------------------------------------------

/// One file, as the sentinel keys it: absolute, `.` segments dropped, forward
/// slashes — and on Windows, where case does not tell two files apart, lower
/// case. The toolbox names an allowed write the same way, so the two agree on
/// which file a person said yes to.
#[must_use]
pub fn file_key(path: &Path) -> String {
    let joined: std::path::PathBuf = path.components().collect();
    let key = joined.to_string_lossy().replace(char::from(92), "/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// The project a protected-file question is filed under: the workspace's
/// folder name, or the whole path when it has none.
#[must_use]
pub fn breach_project(workspace: &Path) -> String {
    workspace.file_name().map_or_else(
        || workspace.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The title of the owner's card about a protected file in `project`.
///
/// It names the RULE and not the attempt, because it is what
/// `owner::past_verdict` matches on: an owner who has already said "this
/// project writes its own tests" is not asked again. One title for both
/// askers — the toolbox before a write, the [`Sentinel`] after a change — so
/// an answer to either is the answer to both, and a run whose toolbox was
/// allowed a test is not stopped by its sentinel for the same file.
#[must_use]
pub fn breach_title(project: &str) -> String {
    format!("Protected file changed in {project}")
}

// ---------------------------------------------------------------------------
// The sentinel — enforcement for an agent this harness does not dispatch for
// ---------------------------------------------------------------------------

/// A record of every protected file, taken before work starts and checked after.
///
/// # Why this exists
///
/// [`Guards::refuse_write`] is a door: it is consulted before the harness's own
/// file tools run, and it stops the write from happening at all. That door does
/// not exist on the delegated path. An installed agent brings its own file
/// tools, runs its own loop, and never calls this crate's executor — so on the
/// path that actually does the work, "you do not get to edit your own exam" was
/// a comment rather than a rule.
///
/// This closes it from the other side. The protected files are fingerprinted
/// before the agent is handed the task and checked again when it hands back. It
/// cannot be argued with, ignored, or prompted around, because it does not ask
/// the agent anything — it reads the tree.
///
/// # What it does and does not defend against
///
/// It catches an agent that edited, deleted or added a protected file, which is
/// the realistic failure: the cheap way to make a failing test pass is to change
/// the test. It is **detection, not prevention** — the write happens and is then
/// caught, so the attempt fails and names the file.
///
/// It deliberately does **not** revert. Rolling back files automatically is a
/// destructive act on someone's working tree, and a sentinel that guessed wrong
/// would delete real work. Naming what changed and refusing to call the task
/// finished is the enforcement; what to do about it is a person's call.
///
/// The fingerprint is FNV-1a over the file's bytes plus its length. That detects
/// every accidental and every lazy change. It is not a cryptographic hash and
/// would not stop an agent deliberately engineering a collision — stated rather
/// than implied, because a guard whose limits are unwritten gets trusted past
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sentinel {
    /// Workspace-relative path to fingerprint, for every protected file found.
    /// Keyed by ABSOLUTE path, valued by how it reads and what it contained.
    ///
    /// The key is absolute because two roots may each hold a `tests/t.rs`: keyed
    /// by the relative path they collapse into one entry, and a change to the
    /// second would go unnoticed behind the first. Measured — a test with two
    /// roots watched one file where it should have watched two.
    marks: std::collections::BTreeMap<String, (String, u64)>,
    /// Whether the walk hit its ceiling before it finished.
    ///
    /// Reported, never swallowed: a sentinel that silently covered half the tree
    /// reads exactly like one that covered all of it and found nothing.
    truncated: bool,
}

/// How many files the sentinel will walk.
///
/// Bounds the WALK, which is the action; it does not bound what is reported. A
/// tree big enough to hit this is reported as truncated rather than quietly
/// half-checked.
const MAX_WALKED: usize = 20_000;

/// What changed while the agent had the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Breach {
    /// The file is still there and its contents are different.
    Modified(String),
    /// The file is gone.
    Deleted(String),
    /// A protected file exists that did not before.
    ///
    /// Counted, because adding `tests/always_passes.rs` changes what the check
    /// runs just as surely as editing the test that fails.
    Created(String),
}

impl Breach {
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Modified(p) | Self::Deleted(p) | Self::Created(p) => p,
        }
    }

    #[must_use]
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Modified(_) => "modified",
            Self::Deleted(_) => "deleted",
            Self::Created(_) => "created",
        }
    }
}

impl Sentinel {
    /// Fingerprint every protected file in the workspace.
    ///
    /// Never fails. A directory that cannot be read is skipped: a sentinel that
    /// refused to start would make an unreadable folder a reason to run the
    /// agent with NO protection at all, which is backwards.
    #[must_use]
    pub fn over(workspace: &Path, guards: &Guards) -> Self {
        Self::over_roots(std::slice::from_ref(&workspace.to_path_buf()), guards)
    }

    /// Fingerprint every protected file across EVERY root the agent can reach.
    ///
    /// # Why this takes a list
    ///
    /// An agent given a second directory can write in it. If the sentinel
    /// watched only the first, adding a directory would silently strip the
    /// protection from everything in it — so a project could be made editable
    /// by its own agent simply by passing `--add-dir` at the tests it keeps
    /// there. Extending reach and extending protection have to be the same act.
    #[must_use]
    pub fn over_roots(roots: &[std::path::PathBuf], guards: &Guards) -> Self {
        let fragments = guards.effective();
        let mut marks = std::collections::BTreeMap::new();
        let mut walked = 0usize;
        let mut truncated = false;
        let mut stack: Vec<std::path::PathBuf> = roots.to_vec();
        // Each file is recorded relative to the root it was found under, so two
        // roots holding a `tests/` each stay distinguishable.
        let roots_owned: Vec<std::path::PathBuf> = roots.to_vec();

        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Build output and dependency trees hold no protected file and
                // are enormous. `.git` is skipped for the same reason. Dot
                // directories in general are NOT skipped — `.github/workflows/`
                // and `.incode/ledger.json` are two of the things being guarded.
                if matches!(name.as_str(), "target" | "node_modules" | "dist" | ".git") {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                walked += 1;
                if walked > MAX_WALKED {
                    truncated = true;
                    break;
                }
                let Some(shown) = roots_owned.iter().find_map(|root| relative_to(root, &path))
                else {
                    continue;
                };
                let absolute = file_key(&path);
                if !fragments.iter().any(|f| matches_path(f, &shown)) {
                    continue;
                }
                if let Some(mark) = fingerprint_file(&path) {
                    marks.insert(absolute, (shown, mark));
                }
            }
            if truncated {
                break;
            }
        }

        Self { marks, truncated }
    }

    /// How many protected files are being watched.
    #[must_use]
    pub fn watched(&self) -> usize {
        self.marks.len()
    }

    /// Whether the walk stopped early, so a caller can say so.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// What changed since this sentinel was taken.
    #[must_use]
    pub fn breaches(&self, workspace: &Path, guards: &Guards) -> Vec<Breach> {
        self.breaches_across(std::slice::from_ref(&workspace.to_path_buf()), guards)
    }

    /// What changed across every root this sentinel was taken over.
    #[must_use]
    pub fn breaches_across(&self, roots: &[std::path::PathBuf], guards: &Guards) -> Vec<Breach> {
        self.breaches_allowing(roots, guards, &[])
    }

    /// [`Self::breaches_across`], less the files a person allowed to be
    /// written while the agent worked, named by [`file_key`]. A change they
    /// said yes to is not a change made behind their back.
    #[must_use]
    pub fn breaches_allowing(
        &self,
        roots: &[std::path::PathBuf],
        guards: &Guards,
        allowed: &[String],
    ) -> Vec<Breach> {
        let now = Self::over_roots(roots, guards);
        let mut out = Vec::new();

        for (path, (shown, before)) in &self.marks {
            if allowed.contains(path) {
                continue;
            }
            match now.marks.get(path) {
                Some((_, after)) if after == before => {}
                Some(_) => out.push(Breach::Modified(shown.clone())),
                None => out.push(Breach::Deleted(shown.clone())),
            }
        }
        for (path, (shown, _)) in &now.marks {
            if !self.marks.contains_key(path) && !allowed.contains(path) {
                out.push(Breach::Created(shown.clone()));
            }
        }
        out
    }

    /// What to tell the runner when a protected file moved.
    ///
    /// Returns `None` when nothing did — so the caller reads as "refuse if there
    /// is something to refuse", with no separate emptiness check to forget.
    #[must_use]
    pub fn refusal(breaches: &[Breach]) -> Option<String> {
        if breaches.is_empty() {
            return None;
        }
        let listed = breaches
            .iter()
            .map(|b| format!("  - `{}` was {}", b.path(), b.verb()))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "This attempt changed {} protected file(s):\n{listed}\n\n\
             The check that decides whether work is finished must not be editable \
             by the thing being judged — a test you rewrote is not a test you \
             passed. The attempt is recorded as failed and the change was NOT \
             reverted; the files are as the agent left them, for a person to \
             look at.\n\n\
             If the check itself is genuinely wrong, that is a question for the \
             owner, not something to edit around.",
            breaches.len()
        ))
    }
}

/// A file's contents as one number, for telling "changed" from "did not".
///
/// FNV-1a over the bytes, mixed with the length so a truncation that happens to
/// preserve the hash is still seen. Hand-rolled for the same reason as
/// [`crate::identity::AgentId::fingerprint`]: the standard library's hasher does
/// not promise a stable value between versions, and a sentinel taken before a
/// toolchain upgrade must still be comparable after one.
fn fingerprint_file(path: &Path) -> Option<u64> {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let bytes = std::fs::read(path).ok()?;
    let mut hash = OFFSET;
    for byte in &bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    // Length folded in last: two different files that collide on content are
    // vanishingly unlikely to also share a length.
    hash ^= bytes.len() as u64;
    Some(hash.wrapping_mul(PRIME))
}

/// The workspace-relative form of a path, with forward slashes.
///
/// Forward slashes because every protected fragment is written that way, and a
/// guard that matched on Unix and not on Windows would be worse than none.
fn relative_to(workspace: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(workspace).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// What a delegated agent must not be allowed to do.
///
/// # Policy here, vocabulary in the adapter
///
/// This is stated as INTENT — do not write these paths, do not spawn your own
/// sub-agents — and deliberately names no tool, no flag and no CLI syntax. Every
/// agent spells those differently, and a policy written in one vendor's
/// vocabulary is a policy only that vendor can enforce.
///
/// Each [`crate::agent::AgentBackend`] renders this into whatever its own
/// process understands. That keeps the rule in one place and the dialect in as
/// many places as there are agents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restrictions {
    /// Path globs that must not be WRITTEN, workspace-relative and absolute.
    ///
    /// Bare globs — `**/tests/**`, not any tool's wrapper around one.
    pub no_write: Vec<String>,
    /// The agent must not create sub-agents of its own.
    ///
    /// # Why delegation is the harness's job
    ///
    /// A sub-agent spawned inside a delegated agent's process is invisible to
    /// everything this harness exists to provide: it is not routed by
    /// [`crate::roles`], so it runs on whatever model the child picks rather
    /// than the tier that was planned; it is not counted by the budget, so its
    /// tokens are spent outside the ceiling; it never reaches the ledger, so
    /// nothing records that it happened; and it is proved by nothing, because
    /// [`crate::verify`] only ever sees the parent's task.
    ///
    /// NOT a containment measure — measured, a nested sub-agent does inherit
    /// its parent's write denials. This is about control: delegation happens
    /// where it can be planned, routed, bounded, recorded and verified.
    pub no_subagents: bool,
}

/// What a backend can PREVENT, as opposed to what must be detected after it.
///
/// # Why a backend has to say
///
/// Measured on two real agents, the difference is not cosmetic. One accepts a
/// per-tool deny list and honours it — a probe confirmed the refused tool is
/// reported ABSENT rather than merely blocked. The other has no per-tool deny
/// surface at all, and ships four sub-agent tools it will happily use.
///
/// Rendering a policy for the first and silently rendering nothing for the
/// second would leave the harness reporting a guard it is not providing, which
/// this module's own docs call worse than no guard. So a backend declares what
/// it can actually enforce, and what it cannot is stated rather than assumed.
///
/// The default is [`Self::NOTHING`]: a backend that has not been measured does
/// not get the benefit of the doubt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Enforcement {
    /// The agent will refuse writes to the protected paths itself.
    pub no_write: bool,
    /// The agent will refuse to create sub-agents of its own.
    pub no_subagents: bool,
}

impl Enforcement {
    /// Enforces neither half — everything falls to [`Sentinel`] or to nothing.
    pub const NOTHING: Self = Self {
        no_write: false,
        no_subagents: false,
    };
    /// Enforces both halves itself.
    pub const EVERYTHING: Self = Self {
        no_write: true,
        no_subagents: true,
    };
}

impl Restrictions {
    /// The parts of this policy the backend will NOT enforce, named plainly.
    ///
    /// Empty when the backend covers everything asked of it. Anything returned
    /// here is a rule the operator believes is in force and the agent will not
    /// apply — so it is reported, never dropped quietly.
    #[must_use]
    pub fn unenforced_by(&self, enforcement: &Enforcement) -> Vec<String> {
        let mut gaps = Vec::new();
        if !self.no_write.is_empty() && !enforcement.no_write {
            gaps.push(format!(
                "{} protected path(s) are not refused by the agent — only detected after a write",
                self.no_write.len()
            ));
        }
        if self.no_subagents && !enforcement.no_subagents {
            gaps.push(
                "the agent can create its own sub-agents, which nothing here can see or stop"
                    .to_string(),
            );
        }
        gaps
    }
}

/// # Why restrictions exist beside the sentinel, and why the sentinel stays
///
/// [`Sentinel`] can only DETECT: the agent writes, and the attempt is failed
/// afterwards. A restriction the agent enforces for itself PREVENTS — the write
/// never happens. Where the agent can be told, telling it is strictly better.
///
/// The sentinel is kept behind it anyway. These two catch different things: a
/// restriction covers whatever the agent's own permission layer checks, and the
/// sentinel covers whatever it did not. Neither is asked to be complete alone,
/// and the sentinel needs no cooperation from the agent at all — which is what
/// makes it the one that still works on a backend that honours nothing.
///
/// Anchored to every root the agent can reach.
///
/// # Why the extra roots need their own rules
///
/// A bare `**/tests/**` is matched RELATIVE to the agent's working directory,
/// so it does not cover a directory outside it. Measured: with a second root
/// added, `**/tests/**` in force, the agent created a file in that root's
/// `tests/` without being stopped. The same probe with the root's ABSOLUTE path
/// in the pattern was refused.
///
/// So each additional root contributes its own absolute rules. The relative
/// form is kept as well — it is the one verified against the workspace, and two
/// rules that both refuse are cheaper than discovering which one was load
/// bearing.
#[must_use]
pub fn restrictions_for(guards: &Guards, extra_roots: &[std::path::PathBuf]) -> Restrictions {
    let fragments = guards.effective();
    let mut no_write: Vec<String> = fragments.iter().map(|f| glob_for(f)).collect();

    for root in extra_roots {
        // Forward slashes: the patterns are globs, and a backslash in one is an
        // escape rather than a separator.
        let base = root.to_string_lossy().replace(char::from(92), "/");
        let base = base.trim_end_matches('/').to_string();
        for fragment in &fragments {
            let glob = glob_for(fragment);
            // TWO forms per fragment, because `**` matches one or more segments
            // and not zero: anchored as `<root>/**/tests/**` alone, a `tests/`
            // sitting DIRECTLY under the root is missed. Measured — the write
            // went through, and the same probe with `<root>/tests/**` refused.
            let nested = format!("{base}/{glob}");
            let direct = format!("{base}/{}", glob.trim_start_matches("**/"));
            if direct != nested {
                no_write.push(direct);
            }
            no_write.push(nested);
        }
    }
    Restrictions {
        no_write,
        // Always. An operator who wants uncontrolled fan-out is asking for a
        // different tool, not a setting on this one.
        no_subagents: true,
    }
}

/// The same, for the workspace alone.
#[must_use]
pub fn restrictions(guards: &Guards) -> Restrictions {
    restrictions_for(guards, &[])
}

/// A protected fragment as a path glob.
///
/// The fragments are matched by [`matches_path`] as *segments or suffixes*, and
/// a glob has to say which was meant:
/// - `tests/` is a directory — everything under it, at any depth.
/// - `_test.` is an infix on a file name — anything containing it.
/// - `test_` is a prefix of a file name — names that start with it.
/// - `.env` and `admin.json` are file names — at any depth.
fn glob_for(fragment: &str) -> String {
    let trimmed = fragment.trim_end_matches('/');
    if fragment.ends_with('/') {
        // A directory, and everything below it, wherever it sits in the tree.
        return format!("**/{trimmed}/**");
    }
    if fragment.contains('/') {
        // Already a path, e.g. `.incode/ledger.json` or `.github/workflows/`.
        return format!("**/{trimmed}");
    }
    if is_name_prefix(fragment) {
        // No wildcard in front: `**/*test_*` would deny `latest_x.py` to the
        // delegated agent while the harness's own check allowed it.
        return format!("**/{trimmed}*");
    }
    if fragment.starts_with('.') && !fragment.ends_with('.') {
        // A file name like `.env`.
        return format!("**/{trimmed}*");
    }
    // An infix like `_test.` or `.test.`, or a bare file name like
    // `admin.json`. Both want a wildcard on each side.
    format!("**/*{trimmed}*")
}

#[cfg(test)]
mod tests {

    /// **A bare glob does not reach outside the working directory.** Measured
    /// against the real agent: with a second root added and `**/tests/**` in
    /// force, a file was created in that root's `tests/` unstopped. The same
    /// probe with the root's absolute path was refused.
    #[test]
    fn an_added_root_gets_its_own_absolute_rules() {
        let guards = Guards::default();
        let extra = std::path::PathBuf::from("/some/other/root");
        let rules = restrictions_for(&guards, std::slice::from_ref(&extra)).no_write;

        assert!(
            rules.iter().any(|r| r == "**/tests/**"),
            "the workspace form must remain: {rules:?}"
        );
        assert!(
            rules.iter().any(|r| r == "/some/other/root/**/tests/**"),
            "the added root needs an absolute rule: {rules:?}"
        );
    }

    /// Every protected fragment is anchored to every added root — one that was
    /// missed is a hole with no symptom.
    #[test]
    fn every_fragment_is_anchored_to_every_added_root() {
        let guards = Guards::default();
        let roots = vec![
            std::path::PathBuf::from("/a"),
            std::path::PathBuf::from("/b"),
        ];
        let base = restrictions(&guards).no_write.len();
        let all = restrictions_for(&guards, &roots).no_write;
        for root in ["/a", "/b"] {
            assert!(
                all.iter().filter(|r| r.contains(root)).count() >= base,
                "every fragment must be anchored to {root}: {all:?}"
            );
        }
    }

    /// **A protected directory sitting DIRECTLY under an added root is
    /// covered.** `**` matches one or more segments, not zero, so
    /// `<root>/**/tests/**` alone misses `<root>/tests/`. Measured against the
    /// real agent: the write went through with only the nested form in force.
    #[test]
    fn a_protected_directory_at_the_top_of_an_added_root_is_covered() {
        let guards = Guards::default();
        let extra = std::path::PathBuf::from("/other/root");
        let rules = restrictions_for(&guards, std::slice::from_ref(&extra)).no_write;

        assert!(
            rules.iter().any(|r| r == "/other/root/tests/**"),
            "a tests/ directly under the root must be covered: {rules:?}"
        );
        assert!(
            rules.iter().any(|r| r == "/other/root/**/tests/**"),
            "and one nested deeper: {rules:?}"
        );
    }

    /// Backslashes become forward slashes: in a glob a backslash escapes rather
    /// than separates, so a Windows path left as-is would match nothing.
    #[test]
    fn a_windows_root_is_written_with_forward_slashes() {
        let guards = Guards {
            protected_paths: vec!["secrets/".to_string()],
            allow_writes_to: crate::guards::DEFAULT_PROTECTED
                .iter()
                .map(|p| (*p).to_string())
                .collect(),
        };
        let extra = std::path::PathBuf::from(r"C:\work\other");
        let rules = restrictions_for(&guards, std::slice::from_ref(&extra)).no_write;
        assert!(
            rules.iter().any(|r| r.contains("C:/work/other/")),
            "got: {rules:?}"
        );
        assert!(
            !rules.iter().any(|r| r.contains(char::from(92))),
            "no backslash may survive into a glob: {rules:?}"
        );
    }

    /// **THE security property of multi-root.** An agent given a second
    /// directory can write in it. If the sentinel watched only the first, a
    /// project could be made editable by its own agent simply by naming the
    /// directory its tests live in — which is the entire hole the guards exist
    /// to close, reopened by a convenience flag.
    #[test]
    fn a_protected_file_in_an_added_directory_is_still_protected() {
        let main = tempfile::tempdir().expect("main");
        let extra = tempfile::tempdir().expect("extra");
        std::fs::create_dir_all(extra.path().join("tests")).expect("tests dir");
        std::fs::write(extra.path().join("tests").join("t.rs"), "assert!(false);")
            .expect("test file");

        let guards = Guards::default();
        let roots = vec![main.path().to_path_buf(), extra.path().to_path_buf()];
        let before = Sentinel::over_roots(&roots, &guards);
        assert_eq!(before.watched(), 1, "the added directory must be watched");

        // The lazy way out, in the directory that was added for convenience.
        std::fs::write(extra.path().join("tests").join("t.rs"), "assert!(true);").expect("rewrite");

        let breaches = before.breaches_across(&roots, &guards);
        assert_eq!(breaches.len(), 1, "got: {breaches:?}");
        assert_eq!(breaches[0].verb(), "modified");
    }

    /// The control: the SAME edit goes unnoticed when that root was never
    /// added. Without this, the test above would pass even if `over_roots`
    /// ignored its argument entirely.
    #[test]
    fn a_directory_that_was_not_added_is_not_watched() {
        let main = tempfile::tempdir().expect("main");
        let extra = tempfile::tempdir().expect("extra");
        std::fs::create_dir_all(extra.path().join("tests")).expect("tests dir");
        std::fs::write(extra.path().join("tests").join("t.rs"), "assert!(false);")
            .expect("test file");

        let guards = Guards::default();
        let only_main = vec![main.path().to_path_buf()];
        let before = Sentinel::over_roots(&only_main, &guards);
        assert_eq!(before.watched(), 0);

        std::fs::write(extra.path().join("tests").join("t.rs"), "assert!(true);").expect("rewrite");
        assert!(before.breaches_across(&only_main, &guards).is_empty());
    }

    /// Two roots each holding a `tests/` stay distinguishable, so a report
    /// names the file that actually moved.
    #[test]
    fn files_from_different_roots_do_not_collide() {
        let a = tempfile::tempdir().expect("a");
        let b = tempfile::tempdir().expect("b");
        for dir in [a.path(), b.path()] {
            std::fs::create_dir_all(dir.join("tests")).expect("tests dir");
            std::fs::write(dir.join("tests").join("t.rs"), "one").expect("write");
        }

        let guards = Guards::default();
        let roots = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        let before = Sentinel::over_roots(&roots, &guards);
        assert_eq!(
            before.watched(),
            2,
            "both roots' tests must be watched separately"
        );
    }

    /// Sub-agents are refused as POLICY, whatever an agent calls the tool.
    ///
    /// The word for it belongs to each backend; that the harness forbids it does
    /// not. Pinned because the cost of it silently dropping out is invisible at
    /// runtime.
    #[test]
    fn delegation_by_the_agent_itself_is_refused_by_policy() {
        assert!(
            restrictions(&Guards::default()).no_subagents,
            "un-ledgered delegation is permitted again"
        );
    }

    /// Every protected default becomes a glob — a fragment that quietly failed
    /// to translate would be a hole with no symptom.
    #[test]
    fn every_protected_default_becomes_a_rule() {
        let guards = Guards::default();
        assert_eq!(
            restrictions(&guards).no_write.len(),
            guards.effective().len(),
            "one glob per protected fragment"
        );
    }

    /// The policy names PATHS, never one agent's tool syntax.
    ///
    /// A glob that arrived pre-wrapped in some CLI's vocabulary would be
    /// unusable by every other backend, which is the coupling this separation
    /// exists to prevent.
    #[test]
    fn the_policy_carries_bare_globs_not_any_agents_syntax() {
        for glob in restrictions(&Guards::default()).no_write {
            assert!(
                !glob.contains('(') && !glob.contains(')'),
                "a vendor's rule syntax leaked into the policy: {glob}"
            );
        }
    }

    /// A directory is protected at every depth, not only at the root.
    #[test]
    fn a_protected_directory_covers_everything_below_it() {
        let rules = restrictions(&Guards::default()).no_write;
        assert!(rules.contains(&"**/tests/**".to_string()), "got: {rules:?}");
        assert!(
            rules.contains(&"**/migrations/**".to_string()),
            "got: {rules:?}"
        );
    }

    /// A name fragment matches wherever it appears in a file name.
    #[test]
    fn a_name_fragment_matches_anywhere_in_the_name() {
        assert_eq!(glob_for("_test."), "**/*_test.*");
        assert_eq!(glob_for("admin.json"), "**/*admin.json*");
        assert_eq!(glob_for(".env"), "**/.env*");
    }

    /// A fragment that is already a path keeps its shape.
    #[test]
    fn a_path_fragment_is_not_turned_into_a_name_match() {
        assert_eq!(glob_for(".incode/ledger.json"), "**/.incode/ledger.json");
        assert_eq!(glob_for(".github/workflows/"), "**/.github/workflows/**");
    }

    /// An operator exemption reaches the deny rules too — otherwise switching a
    /// default off would work for the harness's tools and not for the agent's.
    #[test]
    fn an_exempted_default_produces_no_rule() {
        let guards = Guards {
            protected_paths: Vec::new(),
            allow_writes_to: vec!["tests/".to_string()],
        };
        let rules = restrictions(&guards).no_write;
        assert!(
            !rules.contains(&"**/tests/**".to_string()),
            "the operator switched this off on purpose: {rules:?}"
        );
    }

    fn workspace_with_a_test() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
        std::fs::write(
            dir.path().join("tests").join("it_works.rs"),
            "assert!(false);",
        )
        .expect("test file");
        std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
        std::fs::write(dir.path().join("src").join("lib.rs"), "fn main() {}").expect("source");
        dir
    }

    /// **The case this exists for.** An agent that cannot pass the test edits
    /// the test. The harness cannot stop the write on this path — it can see it.
    #[test]
    fn editing_the_test_that_judges_you_is_caught() {
        let dir = workspace_with_a_test();
        let guards = Guards::default();
        let before = Sentinel::over(dir.path(), &guards);
        assert!(before.watched() >= 1, "the test file must be watched");

        // The lazy way to make a failing test pass.
        std::fs::write(
            dir.path().join("tests").join("it_works.rs"),
            "assert!(true);",
        )
        .expect("rewrite");

        let breaches = before.breaches(dir.path(), &guards);
        assert_eq!(breaches.len(), 1, "got: {breaches:?}");
        assert_eq!(
            breaches[0],
            Breach::Modified("tests/it_works.rs".to_string())
        );

        let refusal = Sentinel::refusal(&breaches).expect("it must refuse");
        assert!(refusal.contains("tests/it_works.rs"), "got: {refusal}");
        assert!(
            refusal.contains("not reverted") || refusal.contains("NOT reverted"),
            "it must say the tree was left alone: {refusal}"
        );
    }

    /// **Ordinary work is not a breach.** A guard that fired on every source
    /// edit would be turned off within a day.
    #[test]
    fn editing_the_code_under_test_is_not_a_breach() {
        let dir = workspace_with_a_test();
        let guards = Guards::default();
        let before = Sentinel::over(dir.path(), &guards);

        std::fs::write(
            dir.path().join("src").join("lib.rs"),
            "fn main() { /* fixed */ }",
        )
        .expect("edit");

        assert!(
            before.breaches(dir.path(), &guards).is_empty(),
            "changing the code being judged is the WHOLE JOB"
        );
    }

    /// Deleting the exam is not a way around it.
    #[test]
    fn deleting_a_protected_file_is_caught() {
        let dir = workspace_with_a_test();
        let guards = Guards::default();
        let before = Sentinel::over(dir.path(), &guards);

        std::fs::remove_file(dir.path().join("tests").join("it_works.rs")).expect("delete");

        let breaches = before.breaches(dir.path(), &guards);
        assert_eq!(
            breaches,
            vec![Breach::Deleted("tests/it_works.rs".to_string())]
        );
    }

    /// **Adding a passing test is caught too.** It changes what the check runs
    /// just as surely as editing the one that fails.
    #[test]
    fn adding_a_new_protected_file_is_caught() {
        let dir = workspace_with_a_test();
        let guards = Guards::default();
        let before = Sentinel::over(dir.path(), &guards);

        std::fs::write(
            dir.path().join("tests").join("always_passes.rs"),
            "assert!(true);",
        )
        .expect("add");

        let breaches = before.breaches(dir.path(), &guards);
        assert_eq!(
            breaches,
            vec![Breach::Created("tests/always_passes.rs".to_string())]
        );
    }

    /// **Dot directories are watched.** `.github/workflows/` and
    /// `.incode/ledger.json` are two of the things being guarded, so a walker
    /// that skipped hidden files — as the spec walker deliberately does — would
    /// protect neither.
    #[test]
    fn protected_files_inside_dot_directories_are_watched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).expect("workflows");
        std::fs::write(workflows.join("ci.yml"), "on: push").expect("ci");

        let guards = Guards::default();
        let before = Sentinel::over(dir.path(), &guards);
        assert_eq!(before.watched(), 1, "the workflow must be watched");

        std::fs::write(workflows.join("ci.yml"), "on: never").expect("weaken ci");
        let breaches = before.breaches(dir.path(), &guards);
        assert_eq!(
            breaches,
            vec![Breach::Modified(".github/workflows/ci.yml".to_string())]
        );
    }

    /// An unchanged tree refuses nothing — otherwise every attempt would fail.
    #[test]
    fn an_untouched_tree_is_not_a_breach() {
        let dir = workspace_with_a_test();
        let guards = Guards::default();
        let before = Sentinel::over(dir.path(), &guards);
        assert!(before.breaches(dir.path(), &guards).is_empty());
        assert!(Sentinel::refusal(&[]).is_none());
    }

    /// **A change the fingerprint must not miss.** Same length, different
    /// bytes — the case a size-only check would wave through.
    #[test]
    fn a_same_length_edit_is_still_a_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
        let file = dir.path().join("tests").join("t.rs");
        std::fs::write(&file, "assert_eq!(1, 2);").expect("write");

        let guards = Guards::default();
        let before = Sentinel::over(dir.path(), &guards);
        std::fs::write(&file, "assert_eq!(2, 2);").expect("rewrite");

        assert_eq!(
            before.breaches(dir.path(), &guards),
            vec![Breach::Modified("tests/t.rs".to_string())]
        );
    }

    /// An operator who exempted a path is not overruled by the sentinel.
    #[test]
    fn a_path_the_operator_exempted_is_not_watched() {
        let dir = workspace_with_a_test();
        let guards = Guards {
            protected_paths: Vec::new(),
            allow_writes_to: vec!["tests/".to_string()],
        };
        let before = Sentinel::over(dir.path(), &guards);
        std::fs::write(
            dir.path().join("tests").join("it_works.rs"),
            "assert!(true);",
        )
        .expect("rewrite");
        assert!(
            before.breaches(dir.path(), &guards).is_empty(),
            "the operator switched this default off on purpose"
        );
    }
    use super::*;

    /// **The whole point.** An agent told to make the tests pass must not be
    /// able to take the cheap route.
    #[test]
    fn the_exam_cannot_be_edited_by_the_candidate() {
        let guards = Guards::default();
        for protected in [
            "tests/api/users.rs",
            "crates/core/tests/integration.rs",
            "src/users_test.go",
            "app/user.test.ts",
            ".github/workflows/ci.yml",
            "migrations/0004_add_users.sql",
            ".env",
            ".env.local",
            ".incode/ledger.json",
        ] {
            assert!(
                guards.refuse_write(protected).is_some(),
                "`{protected}` must not be writable"
            );
        }
    }

    /// And the refusal has to teach, or it just gets worked around.
    #[test]
    fn the_refusal_says_what_to_do_instead() {
        let refusal = Guards::default()
            .refuse_write("tests/api.rs")
            .expect("refused");
        assert!(refusal.contains("Fix the code"), "got: {refusal}");
        // The requirement, not the phrasing: a genuinely wrong check needs a
        // route that is not "edit it", or the refusal is a dead end and the
        // agent reroutes around the guard instead of raising the problem.
        assert!(
            refusal.contains("owner can allow"),
            "a genuinely wrong test needs a route that is not 'edit it': {refusal}"
        );
    }

    /// Ordinary source must stay writable, or the harness cannot work at all.
    #[test]
    fn ordinary_source_is_untouched() {
        let guards = Guards::default();
        for allowed in [
            "src/main.rs",
            "src/latest/handler.rs",
            "docs/PRD.md",
            "Cargo.toml",
            "src/protester.rs",
        ] {
            assert!(
                guards.refuse_write(allowed).is_none(),
                "`{allowed}` must stay writable"
            );
        }
    }

    /// Separators and case differ by platform and habit; the protection must not.
    #[test]
    fn a_windows_path_is_the_same_path() {
        let guards = Guards::default();
        assert!(guards.refuse_write(r"tests\api\users.rs").is_some());
        assert!(guards.refuse_write("./tests/api.rs").is_some());
        assert!(guards.refuse_write("TESTS/API.RS").is_some());
    }

    /// **Reading is always allowed.** An agent that cannot read the tests cannot
    /// write code that satisfies them.
    #[test]
    fn reading_a_protected_file_is_never_refused() {
        let guards = Guards::default();
        for read in ["cat tests/api.rs", "grep -r assert tests/", "ls tests/"] {
            assert!(
                guards.refuse_command(read).is_none(),
                "reading must be allowed: {read}"
            );
        }
    }

    /// The direct shell routes to the same place.
    #[test]
    fn the_obvious_shell_routes_are_closed() {
        let guards = Guards::default();
        for command in [
            "rm tests/api.rs",
            "sed -i s/assert/ignore/ tests/api.rs",
            "echo nothing > .env",
            "mv migrations/0001.sql /tmp/",
        ] {
            assert!(
                guards.refuse_command(command).is_some(),
                "`{command}` must be refused"
            );
        }
    }

    /// **incordai/Incode#4.** `printf x >>tests/it.rs` appended to a test: the
    /// command was split on whitespace, so `>>tests/it.rs` was one word whose
    /// first segment was `>>tests`, and `tests/` never matched. Every route
    /// here wrote a protected file through the Bash tool, and only the sentinel
    /// saw it — after the fact.
    #[test]
    fn a_redirect_or_writer_without_a_space_still_reaches_the_guard() {
        let guards = Guards::default();
        for command in [
            "printf '// guard probe\\n' >>tests/it.rs",
            "echo x >tests/a.rs",
            "echo x >>tests/a.rs",
            "echo x 2>tests/a.rs",
            "echo x &>tests/a.rs",
            "cat <<E >tests/a.rs",
            "cat <<E >tests/a.rs\nwhatever\nE\n",
            r#"python3 -c "open('tests/a.rs','w').write('')""#,
            r#"node -e "require('fs').writeFileSync('tests/a.js','')""#,
            "perl -pi -e s/a/b/ tests/a.rs",
            "git checkout -- tests/a.rs",
            "git restore tests/a.rs",
            "install -m 644 /tmp/x tests/a.rs",
            "ln -sf /tmp/x tests/a.rs",
            "bash -c 'rm tests/a.rs'",
            "echo $(rm tests/a.rs)",
            "for f in tests/*; do rm \"$f\"; done",
            "ls tests/ | xargs rm",
            "python3 - <<'EOF'\nopen('tests/a.rs','w').write('')\nEOF\n",
        ] {
            assert!(
                guards.refuse_command(command).is_some(),
                "`{command}` writes a protected file and must be refused"
            );
        }
    }

    /// **incordai/Incode#11.** A heredoc writing `docs/` was refused because
    /// the DOCUMENT mentioned `tests/it.rs`. A path in the text being written
    /// is data, not a target — the redirect target is what is written.
    #[test]
    fn a_command_that_only_mentions_a_protected_path_is_allowed() {
        let guards = Guards::default();
        for command in [
            "cat > docs/a.md <<'E'\nsee tests/it.rs\nE",
            "cat > docs/a.md <<E\nrm tests/it.rs, then > tests/x\nE\n",
            "cat <<-E > docs/a.md\n\tsee tests/it.rs\n\tE\n",
            r#"git commit -m "fix tests/it.rs""#,
            r#"echo "see tests/it.rs" > docs/notes.md"#,
            "cat tests/a.rs",
            "grep -r foo tests/",
            "cargo test --test a",
            "cargo test 2>&1 | tee /tmp/out.txt",
            "python3 -m pytest tests/",
        ] {
            assert!(
                guards.refuse_command(command).is_none(),
                "`{command}` writes nothing protected; refusing it teaches the agent to dodge the guard"
            );
        }
    }

    /// **incordai/Incode#5.** The defaults knew Rust, Go and JS test names and
    /// nothing of pytest, Jest's `__tests__/`, RSpec or the other CI systems,
    /// so in those projects the exam was writable by the candidate.
    #[test]
    fn the_test_and_ci_files_of_other_ecosystems_are_protected() {
        let guards = Guards::default();
        for protected in [
            "test_calc.py",
            "pkg/test_api.py",
            "conftest.py",
            "pkg/conftest.py",
            "src/__tests__/a.js",
            "app/a.spec.ts",
            "spec/models/user_spec.rb",
            "lib/user_spec.rb",
            "x/testdata/in.txt",
            "Jenkinsfile",
            "azure-pipelines.yml",
            ".circleci/config.yml",
            ".buildkite/pipeline.yml",
        ] {
            assert!(
                guards.refuse_write(protected).is_some(),
                "`{protected}` is part of the exam and must not be writable"
            );
        }
    }

    /// The new shapes must not catch ordinary source. `test_` is a PREFIX, so
    /// `latest.py` and `contest.py` are untouched; `testing_utils.py` is
    /// support code by decision (see [`matches_path`]).
    #[test]
    fn names_that_only_resemble_a_test_stay_writable() {
        let guards = Guards::default();
        for allowed in [
            "latest.py",
            "contest.py",
            "src/latest_api.py",
            "inspector.ts",
            "src/testing_utils.py",
            "src/respec.rb",
        ] {
            assert!(
                guards.refuse_write(allowed).is_none(),
                "`{allowed}` is ordinary source and must stay writable"
            );
        }
    }

    /// Every default can be switched off on its own, the new ones included —
    /// an exemption that silently failed to apply would leave a project with
    /// no way to let its agent write the files it legitimately owns.
    #[test]
    fn every_default_can_be_exempted_on_its_own() {
        let samples = [
            ("tests/", "tests/a.rs"),
            ("test/", "test/a.rs"),
            ("_test.", "a_test.go"),
            (".test.", "a.test.ts"),
            ("test_", "test_a.py"),
            ("conftest.py", "conftest.py"),
            ("__tests__/", "__tests__/a.js"),
            ("spec/", "spec/a.rb"),
            (".spec.", "a.spec.ts"),
            ("_spec.", "a_spec.rb"),
            ("testdata/", "testdata/in.txt"),
            (".github/workflows/", ".github/workflows/ci.yml"),
            (".gitlab-ci.yml", ".gitlab-ci.yml"),
            (".circleci/", ".circleci/config.yml"),
            ("Jenkinsfile", "Jenkinsfile"),
            ("azure-pipelines.yml", "azure-pipelines.yml"),
            (".buildkite/", ".buildkite/pipeline.yml"),
            ("migrations/", "migrations/0001.sql"),
            (".env", ".env"),
            (".incode/ledger.json", ".incode/ledger.json"),
            (".incode/guards.json", ".incode/guards.json"),
            ("admin.json", "admin.json"),
        ];
        assert_eq!(
            samples.len(),
            DEFAULT_PROTECTED.len(),
            "a default with no sample here is a default nobody proved can be lifted"
        );
        for (fragment, path) in samples {
            assert!(
                DEFAULT_PROTECTED.contains(&fragment),
                "`{fragment}` is not a default"
            );
            let lifted = Guards {
                protected_paths: Vec::new(),
                allow_writes_to: vec![fragment.to_string()],
            };
            assert_eq!(
                Guards::default().protecting(path).as_deref(),
                Some(fragment),
                "`{path}` must be protected by `{fragment}` itself, or lifting it proves nothing"
            );
            assert!(
                lifted.refuse_write(path).is_none(),
                "the operator lifted `{fragment}`, so `{path}` must be writable"
            );
        }
    }

    /// The deny rule handed to a delegated agent covers what the harness's own
    /// check covers, and no more: a prefix glob with a wildcard in front would
    /// stop the agent editing `latest_api.py`.
    #[test]
    fn a_prefix_fragment_becomes_a_prefix_glob() {
        assert_eq!(glob_for("test_"), "**/test_*");
    }

    /// An operator may switch a default off, and that is a decision they take
    /// deliberately — expressed as an exemption, so protections added later are
    /// not silently lost with it.
    #[test]
    fn a_default_can_be_lifted_without_discarding_the_rest() {
        let guards = Guards {
            protected_paths: Vec::new(),
            allow_writes_to: vec!["migrations/".to_string()],
        };
        assert!(guards.refuse_write("migrations/0001.sql").is_none());
        assert!(
            guards.refuse_write("tests/api.rs").is_some(),
            "lifting one protection must not lift the others"
        );
    }

    #[test]
    fn an_operator_can_protect_more() {
        let guards = Guards {
            protected_paths: vec!["infra/".to_string()],
            allow_writes_to: Vec::new(),
        };
        assert!(guards.refuse_write("infra/terraform/main.tf").is_some());
    }

    /// A malformed guards file must be loud. Guards that quietly stop applying
    /// are the worst outcome: everything keeps working and nothing is protected.
    #[test]
    fn a_broken_guards_file_is_an_error_not_an_empty_policy() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join(".incode")).expect("mkdir");
        std::fs::write(dir.path().join(GUARDS_FILE), "{ not json").expect("write");
        assert!(Guards::load(dir.path()).is_err());
    }

    #[test]
    fn no_guards_file_means_the_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        let guards = Guards::load(dir.path()).expect("defaults");
        assert!(guards.refuse_write("tests/x.rs").is_some());
    }

    #[test]
    fn the_target_is_read_from_the_tools_own_arguments() {
        assert_eq!(
            target_path(r#"{"path":"src/main.rs","content":"x"}"#),
            Some("src/main.rs".to_string())
        );
        assert_eq!(
            target_command(r#"{"command":"cargo test"}"#),
            Some("cargo test".to_string())
        );
        assert_eq!(target_path("not json"), None);
    }

    /// The exact body the running service returned, trimmed to the fields this
    /// reads — including one category the owner has already allowed always.
    const REAL_ENFORCEMENT: &str = r#"{"autopilot":true,"forward_all":true,
        "rules":[
          {"always":false,"desc":"hard reset, force-push, clean","key":"git_destructive","title":"Destructive git"},
          {"always":false,"desc":"publishing commits to the shared remote","key":"git_push","title":"Git push"},
          {"always":true,"desc":"recording a commit","key":"git_commit","title":"Git commit"}
        ]}"#;

    #[test]
    fn the_services_own_guard_shape_parses() {
        let categories = parse_categories(REAL_ENFORCEMENT).expect("the observed body parses");
        assert_eq!(categories.rules.len(), 3);
        assert!(categories.forward_all);
        assert_eq!(categories.rules[0].key, "git_destructive");
        assert_eq!(categories.rules[0].title, "Destructive git");
    }

    /// **A category the owner has already allowed is not guarded.** Warning
    /// about it would train the agent to ignore the whole list.
    #[test]
    fn an_allow_always_category_is_not_reported_as_guarded() {
        let categories = parse_categories(REAL_ENFORCEMENT).expect("parses");
        let keys: Vec<&str> = categories
            .guarded()
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        assert_eq!(keys, vec!["git_destructive", "git_push"]);
        assert!(
            !categories
                .as_context()
                .expect("a block")
                .contains("Git commit"),
            "an allowed category must not appear in the warning"
        );
    }

    /// The block says the work PAUSES rather than fails — an agent told it will
    /// fail avoids the operation entirely, which is not what a guard is for.
    #[test]
    fn the_block_says_the_work_waits_rather_than_dies() {
        let block = parse_categories(REAL_ENFORCEMENT)
            .expect("parses")
            .as_context()
            .expect("a block");
        assert!(block.contains("the work waits"), "got: {block}");
        assert!(block.contains("does not fail"), "got: {block}");
        assert!(
            block.contains("reviewed before it runs"),
            "forward_all changes how to work, so it must be said: {block}"
        );
    }

    /// Nothing guarded means no block at all, rather than an empty banner.
    #[test]
    fn nothing_guarded_produces_no_block() {
        let all_allowed =
            r#"{"rules":[{"always":true,"key":"git_push","title":"Git push","desc":"x"}]}"#;
        assert!(parse_categories(all_allowed)
            .expect("parses")
            .as_context()
            .is_none());
        assert!(parse_categories(r#"{}"#)
            .expect("parses")
            .as_context()
            .is_none());
    }

    #[test]
    fn a_malformed_enforcement_payload_is_reported() {
        assert!(parse_categories("not json").is_err());
    }

    /// **The two halves guard different things and must both apply.** The body
    /// says which OPERATIONS the owner reserves; the local list says which PATHS
    /// nothing may write. Neither covers the other: no category stops an agent
    /// editing its own test, and no path list stops a force-push.
    #[test]
    fn the_local_paths_and_the_bodys_categories_do_not_overlap() {
        let categories = parse_categories(REAL_ENFORCEMENT).expect("parses");
        assert!(
            !categories
                .guarded()
                .iter()
                .any(|c| c.description.contains("test")),
            "no body category covers the exam problem, which is why the local list exists"
        );
        assert!(
            Guards::default()
                .refuse_command("git push --force")
                .is_none(),
            "and the local list does not cover operations, which is why the body's does"
        );
    }

    /// A refusal is a question, not a wall: the owner can allow the one write,
    /// and an agent told only "no" reroutes around the guard instead of asking.
    #[test]
    fn a_path_refusal_points_at_the_owner_rather_than_ending_there() {
        let refusal = Guards::default()
            .refuse_write("tests/api.rs")
            .expect("refused");
        assert!(refusal.contains("ASK"), "got: {refusal}");
        assert!(refusal.contains("not a wall"), "got: {refusal}");
    }

    /// An unreachable body costs the advisory block, never the session — the
    /// service's own hook enforces the categories either way, so losing the list
    /// wastes a turn and cannot become a bypass.
    #[test]
    fn an_unreachable_body_costs_the_block_not_the_session() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let block = context_for_session();
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);
        assert!(block.is_none());
    }
}
