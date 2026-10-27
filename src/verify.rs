//! Proof that work is finished, in a form a model cannot fabricate.
//!
//! Both autonomous loops built here have failed the same way: an automated
//! approver sat in a seat that required judgement, and its verdict was recorded
//! as verification. A card was auto-answered and the agent told the owner had
//! approved; findings reached `approved` because an agent confirmed them after
//! reading half the code.
//!
//! So completion is not an opinion here. It is a process that exited zero.
//!
//! # Why a type and not a boolean
//!
//! A `bool` can be set by whoever is holding it, including a caller that has
//! just read a confident summary. [`Verified`] wraps a private field and is
//! constructible only from [`CommandEvidence`] whose exit code is zero — so a
//! function that demands one cannot be called without a command having actually
//! passed. The law stops being a rule people remember and becomes a thing the
//! compiler enforces.
//!
//! A model's report is still worth keeping; it is simply not evidence. Record it
//! as a claim beside the proof, never in place of it.

use std::path::Path;
use std::time::{Duration, Instant};

/// How long a check may run before it is abandoned.
///
/// Generous, because a real suite is slow, and bounded, because an unattended
/// runner waiting forever on a hung command is indistinguishable from one that
/// crashed.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(600);

/// How much of each stream to keep as evidence.
const OUTPUT_TAIL: usize = 4_000;

/// What running a check actually produced.
///
/// **The fields are private, and that is the load-bearing part.** [`Verified`]
/// guards its own construction on `exit == Some(0)`, but a struct whose fields
/// anyone can fill is not evidence of anything — a caller could write
/// `exit: Some(0)` and mint proof for a command that never ran. Only [`run`]
/// builds one, so the only way to hold evidence is to have executed something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEvidence {
    /// The command as run, so the record says what was checked.
    argv: Vec<String>,
    /// The process's exit code. `None` when it was killed by a signal or a
    /// timeout — which is a failure, never a pass.
    exit: Option<i32>,
    stdout_tail: String,
    stderr_tail: String,
    elapsed_ms: u128,
}

impl CommandEvidence {
    /// Evidence for a command that never ran, for tests only.
    ///
    /// `#[cfg(test)]` so it cannot exist in a shipped binary: a fabricator
    /// compiled into production is the hole this module's privacy closes.
    #[cfg(test)]
    pub(crate) fn fabricated(argv: Vec<String>, exit: Option<i32>) -> Self {
        Self {
            argv,
            exit,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            elapsed_ms: 0,
        }
    }

    /// The command that was run.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The exit code, or `None` when the command did not finish.
    #[must_use]
    pub fn exit(&self) -> Option<i32> {
        self.exit
    }

    #[must_use]
    pub fn passed(&self) -> bool {
        self.exit == Some(0)
    }

    /// A one-line summary for a journal or a tool result.
    #[must_use]
    pub fn summary(&self) -> String {
        let status = match self.exit {
            Some(0) => "passed".to_string(),
            Some(code) => format!("failed (exit {code})"),
            None => "did not finish".to_string(),
        };
        format!(
            "`{}` {} in {}ms",
            self.argv.join(" "),
            status,
            self.elapsed_ms
        )
    }

    /// What to show when a check fails: the summary plus the end of the output.
    ///
    /// The END, not the beginning — a compiler or a test runner prints the
    /// failure last, so a head-truncated log is the half without the answer.
    #[must_use]
    pub fn failure_report(&self) -> String {
        let mut out = self.summary();
        if !self.stderr_tail.trim().is_empty() {
            out.push_str("\n--- stderr (tail) ---\n");
            out.push_str(compact_log(&self.stderr_tail).trim_end());
        }
        if !self.stdout_tail.trim().is_empty() {
            out.push_str("\n--- stdout (tail) ---\n");
            out.push_str(compact_log(&self.stdout_tail).trim_end());
        }
        out
    }
}

/// Compact a log stream by collapsing repetitive identical lines and noisy passing tests.
///
/// Long build and test outputs (e.g. hundreds of passing test cases or repeated compiler warnings)
/// consume context window space and can push out critical error traces or panic locations.
///
/// This collapses:
/// 1. Consecutive identical lines (3 or more identical lines into `[... repeated N times ...]`).
/// 2. Consecutive passing test lines (`test <name> ... ok` or `PASS <name>`) when failure indicators
///    are present (`FAILED`, `failures:`, `error:`, `panicked at`, `FAIL`).
#[must_use]
pub fn compact_log(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return text.to_string();
    }

    let has_failure = lines.iter().any(|l| {
        let trimmed = l.trim();
        trimmed.starts_with("error:")
            || trimmed.contains("FAILED")
            || trimmed.starts_with("failures:")
            || trimmed.contains("panicked at")
            || trimmed.contains("FAIL")
    });

    let mut compacted: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let current_line = lines[i];

        // 1. Check for consecutive identical lines (3 or more)
        let mut same_count = 1;
        while i + same_count < lines.len() && lines[i + same_count] == current_line {
            same_count += 1;
        }

        if same_count >= 3 {
            compacted.push(current_line.to_string());
            compacted.push(format!("[... repeated {} times ...]", same_count - 1));
            i += same_count;
            continue;
        }

        // 2. Check for passing test lines when failure exists
        let is_pass_line = |l: &str| {
            let t = l.trim();
            (t.starts_with("test ") && t.ends_with("... ok"))
                || t.starts_with("PASS ")
                || t.starts_with("✓ ")
        };

        if has_failure && is_pass_line(current_line) {
            let mut pass_count = 1;
            while i + pass_count < lines.len() && is_pass_line(lines[i + pass_count]) {
                pass_count += 1;
            }

            if pass_count >= 3 {
                compacted.push(format!("[... {} passing tests omitted ...]", pass_count));
                i += pass_count;
                continue;
            }
        }

        compacted.push(current_line.to_string());
        i += 1;
    }

    let mut result = compacted.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Proof that a check passed.
///
/// The field is private and the only constructor refuses a non-zero exit, so a
/// `Verified` cannot exist unless a process really did pass. Functions that mark
/// work complete should demand one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified(CommandEvidence);

impl Verified {
    /// The only way to obtain proof. `None` for anything but a clean exit.
    #[must_use]
    pub fn from_evidence(evidence: CommandEvidence) -> Option<Self> {
        evidence.passed().then_some(Self(evidence))
    }

    #[must_use]
    pub fn evidence(&self) -> &CommandEvidence {
        &self.0
    }
}

/// Keep the last `max` characters, marking that the front was dropped.
fn tail(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(max))
        .collect();
    format!("[earlier output dropped]\n{kept}")
}

#[cfg(windows)]
fn resolve_windows_executable(program: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(program);
    let extensions = [".cmd", ".bat", ".exe", ""];

    // If program has a path component (relative or absolute)
    if p.parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
        || program.contains(['/', '\\'])
    {
        let full = cwd.join(p);
        for ext in &extensions {
            let with_ext = if ext.is_empty() {
                full.clone()
            } else {
                full.with_extension(&ext[1..])
            };
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
        return None;
    }

    // Look in PATH
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for ext in &extensions {
            let candidate = if ext.is_empty() {
                dir.join(program)
            } else {
                dir.join(format!("{program}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The program name a check uses to mean "this harness, whichever copy is
/// running".
///
/// # Why a name and not a path
///
/// A check is written into the ledger and read back on later runs, possibly on
/// another machine. Baking `std::env::current_exe()` into it at planning time
/// would freeze one absolute path into a file that outlives the binary that
/// wrote it — the check would then fail for a reason that has nothing to do
/// with the work. Resolving the name here, at the moment the check runs, means
/// the ledger stays portable and the exit code still comes from a real process.
pub const SELF_PROGRAM: &str = "incode";

/// Where the harness's own binary is, for a process that is not it.
///
/// The library can be embedded — a test harness is the everyday example, and
/// any host that links it is another. In that case the running process is not
/// `incode` and does not understand its flags, so a check resolved to
/// `current_exe` would fail for a reason that has nothing to do with the work.
/// This says where the real one is.
pub const SELF_BIN_ENV: &str = "INCODE_SELF_BIN";

/// Turn [`SELF_PROGRAM`] into a binary that can actually answer, leaving every
/// other program name alone.
///
/// In order: what the operator pointed at, then this process if this process
/// IS the harness, then the bare name for an ordinary `PATH` lookup. The last
/// is a real fallback rather than a give-up — an installed copy answers the
/// same way.
fn resolve_self(program: &str) -> String {
    if program != SELF_PROGRAM {
        return program.to_string();
    }
    if let Some(named) = std::env::var_os(SELF_BIN_ENV).filter(|v| !v.is_empty()) {
        return named.to_string_lossy().into_owned();
    }
    // The STEM is the test, not merely that `current_exe` answered. It answers
    // for every process, including the ones that would not recognise the flag.
    let running = std::env::current_exe().ok();
    let is_us = running
        .as_ref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .is_some_and(|stem| stem == SELF_PROGRAM);
    match (is_us, running) {
        (true, Some(path)) => path.to_string_lossy().into_owned(),
        _ => program.to_string(),
    }
}

/// Run a check and report exactly what happened.
///
/// Returns an error only when the command could not be started at all. A command
/// that ran and failed is a successful *measurement* of a failure, and comes back
/// as evidence — the caller needs to know the difference between "the check says
/// no" and "there was no check".
pub fn run(argv: &[String], cwd: &Path) -> Result<CommandEvidence, String> {
    let Some((program, args)) = argv.split_first() else {
        // A task with no check can never be completed by this path. That is the
        // intended dead end: something nobody can verify is a decision for a
        // person, not a chore to mark done.
        return Err("no verify command was given, so nothing can be proven".to_string());
    };
    let program = &resolve_self(program);

    // THE COMMAND THAT DECIDES "DONE", named before it runs. Which check ran is
    // the first thing anyone asks when work is reported finished, and it was
    // only ever visible in the evidence text afterwards.
    crate::trace!("verify", "running: {}", argv.join(" "));
    let started = Instant::now();

    #[cfg(windows)]
    let mut cmd = {
        let is_script = program.ends_with(".cmd") || program.ends_with(".bat");
        let resolved = resolve_windows_executable(program, cwd);
        let resolved_is_script = resolved.as_ref().is_some_and(|p| {
            p.extension().and_then(|e| e.to_str()).is_some_and(|ext| {
                ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat")
            })
        });

        if is_script || resolved_is_script {
            let mut c = std::process::Command::new("cmd");
            c.arg("/C").arg(program).args(args);
            c
        } else {
            let mut c = std::process::Command::new(program);
            c.args(args);
            c
        }
    };

    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = std::process::Command::new(program);
        c.args(args);
        c
    };

    crate::agent::process_tree::prepare(&mut cmd);
    let spawned = cmd
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run `{program}`: {e}"))?;

    // Reuses the bash tool's waiter: one timeout implementation in the crate,
    // and a check that hangs is killed rather than hanging the run.
    let output = crate::tools::bash::wait_with_timeout(spawned, VERIFY_TIMEOUT);
    let elapsed_ms = started.elapsed().as_millis();

    match output {
        Ok(output) => Ok(CommandEvidence {
            argv: argv.to_vec(),
            exit: output.status.code(),
            stdout_tail: tail(&String::from_utf8_lossy(&output.stdout), OUTPUT_TAIL),
            stderr_tail: tail(&String::from_utf8_lossy(&output.stderr), OUTPUT_TAIL),
            elapsed_ms,
        }),
        // A timeout is a finished measurement with no exit code: the check did
        // not pass, and saying so is different from failing to run it.
        Err(e) => Ok(CommandEvidence {
            argv: argv.to_vec(),
            exit: None,
            stdout_tail: String::new(),
            stderr_tail: e.to_string(),
            elapsed_ms,
        }),
    }
}

/// The tool the model calls to prove its work.
pub const VERIFY_TOOL: &str = "verify";

/// Environment variable naming the project's check, overriding detection.
pub const VERIFY_COMMAND_ENV: &str = "INCODE_VERIFY_CMD";

/// The check for this project, from the operator or from its manifest.
///
/// **Never from the model.** A tool that let the caller name its own check would
/// let it pick something that always passes, and the proof would prove nothing —
/// the fabrication this whole module exists to prevent, reintroduced through the
/// argument list. The model may ask for the check to run; it does not get to say
/// what passing means.
///
/// The operator's layers come first — environment, then the admin file — and
/// detection is the last resort. Detection guesses from a manifest, which is
/// right for a plain crate and wrong for anything with a real test story, so it
/// must never win over something a human actually wrote down.
#[must_use]
pub fn command_for(workspace: &Path) -> Option<Vec<String>> {
    if let Ok(raw) = std::env::var(VERIFY_COMMAND_ENV) {
        let parts: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
        if !parts.is_empty() {
            return Some(parts);
        }
    }
    if let Some(argv) = crate::admin::AdminConfig::load_default()
        .verify_command
        .filter(|argv| !argv.is_empty())
    {
        return Some(argv);
    }
    detected_command(workspace)
}

/// A file whose presence names a better check than the ecosystem's canonical one.
///
/// Not a heuristic: it is the same kind of evidence as the manifest — a file is
/// there or it is not. It exists because for several ecosystems the obvious
/// command is the one that usually FAILS. `gradle test` needs Gradle installed
/// and version-matched, while every Gradle project checks in `gradlew` for
/// exactly that reason; `composer test` needs a script the project may never
/// have defined, while `vendor/bin/phpunit` is what its CI actually runs.
pub struct WhenPresent {
    /// Relative to the workspace root.
    pub file: &'static str,
    pub check: &'static [&'static str],
}

/// Test files that name an ecosystem when no manifest does.
///
/// For a language whose small projects routinely have no manifest at all:
/// `calc.py` beside `test_calc.py` is a Python project with tests, and
/// "nothing here names an ecosystem" sent its agent off to run them unproven.
pub struct TestFiles {
    /// A test file's name, as (prefix, suffix).
    pub named: &'static [(&'static str, &'static str)],
    /// The check, chosen from the files that were found.
    pub check: fn(&Path, &FoundTests) -> Vec<String>,
}

/// The test files a bounded search found, relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundTests {
    /// With `/` between components, sorted, so a command built from them is
    /// the same on every platform and every run.
    pub files: Vec<String>,
    /// False when the search stopped at its bound with directories unread:
    /// the list is then a sample, and nothing may be built that runs only
    /// the files on it.
    pub complete: bool,
}

/// How many directory levels below the root are searched for test files.
/// Three reaches `src/<package>/tests/`, the deepest place a small project
/// conventionally keeps them.
const TEST_SEARCH_DEPTH: usize = 3;

/// Directory entries read before the search stops. A project with tests has
/// them near the top; a tree this large without them is not a small project
/// missing its manifest.
const TEST_SEARCH_ENTRIES: usize = 5_000;

/// Directories never searched: installed dependencies, build output and
/// caches hold other people's tests, and running those proves nothing about
/// this project. Hidden directories (`.git`, `.venv`, `.tox`) are skipped as
/// well, and so is any directory holding a virtual environment's
/// `pyvenv.cfg`, whatever it is called.
const NOT_SEARCHED: &[&str] = &[
    "node_modules",
    "target",
    "venv",
    "__pycache__",
    "site-packages",
    "build",
    "dist",
];

/// One language ecosystem, recognised from the workspace root.
///
/// # Why a table, and not the chain of ifs this replaced
///
/// The chain knew four ecosystems and returned the FIRST match, so it could not
/// answer the only question that has to be settled before a command is chosen:
/// how many are present. A PHP application with a JavaScript front end matched
/// `package.json`, and the back end was then "proved" by running the front
/// end's tests — a check that passes while the changed code is never executed,
/// which is precisely the fabrication this module exists to prevent.
///
/// A table can be counted. See [`detected_command`] for what counting buys.
pub struct Ecosystem {
    /// What an operator would call it.
    pub name: &'static str,
    /// Any one of these in the workspace root means this ecosystem is present.
    pub manifests: &'static [&'static str],
    /// Or any root file with one of these extensions — for ecosystems whose
    /// project file is named after the project rather than fixed.
    pub extensions: &'static [&'static str],
    /// The canonical check.
    pub check: &'static [&'static str],
    /// Checked in ascending order of preference; the first whose file exists
    /// supersedes `check`.
    pub refine: &'static [WhenPresent],
    /// Test files that name this ecosystem when nothing else in the
    /// workspace does — see [`detected_ecosystems`] for why only then.
    pub tests: Option<TestFiles>,
}

/// The ecosystems detection can recognise.
///
/// Popularity is the entry criterion, not completeness: an ecosystem nobody
/// here builds in is a row that can only ever fire by accident, on a stray file
/// with the right name, and make a project ambiguous that was not.
pub const ECOSYSTEMS: &[Ecosystem] = &[
    Ecosystem {
        name: "Rust",
        manifests: &["Cargo.toml"],
        extensions: &[],
        check: &["cargo", "test"],
        refine: &[],
        tests: None,
    },
    Ecosystem {
        name: "Node.js",
        manifests: &["package.json"],
        extensions: &[],
        check: &["npm", "test"],
        refine: &[],
        tests: None,
    },
    Ecosystem {
        name: "Python",
        manifests: &["pyproject.toml", "pytest.ini", "tox.ini", "setup.py"],
        extensions: &[],
        check: &["pytest"],
        refine: &[],
        tests: Some(TestFiles {
            named: &[("test_", ".py"), ("", "_test.py")],
            check: python_tests_check,
        }),
    },
    Ecosystem {
        name: "Go",
        manifests: &["go.mod"],
        extensions: &[],
        check: &["go", "test", "./..."],
        refine: &[],
        tests: None,
    },
    Ecosystem {
        name: "PHP",
        manifests: &["composer.json", "phpunit.xml", "phpunit.xml.dist"],
        extensions: &[],
        check: &["composer", "test"],
        refine: &[WhenPresent {
            file: "vendor/bin/phpunit",
            check: &["vendor/bin/phpunit"],
        }],
        tests: None,
    },
    Ecosystem {
        name: "Java (Maven)",
        manifests: &["pom.xml"],
        extensions: &[],
        check: &["mvn", "test"],
        refine: &[WhenPresent {
            file: "mvnw",
            check: &["./mvnw", "test"],
        }],
        tests: None,
    },
    Ecosystem {
        name: "Gradle",
        manifests: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        extensions: &[],
        check: &["gradle", "test"],
        refine: &[WhenPresent {
            file: "gradlew",
            check: &["./gradlew", "test"],
        }],
        tests: None,
    },
    Ecosystem {
        name: "Ruby",
        manifests: &["Gemfile"],
        extensions: &[],
        check: &["bundle", "exec", "rake", "test"],
        refine: &[WhenPresent {
            file: ".rspec",
            check: &["bundle", "exec", "rspec"],
        }],
        tests: None,
    },
    Ecosystem {
        name: ".NET",
        manifests: &[],
        extensions: &["sln", "csproj", "fsproj", "vbproj"],
        check: &["dotnet", "test"],
        refine: &[],
        tests: None,
    },
    Ecosystem {
        name: "Elixir",
        manifests: &["mix.exs"],
        extensions: &[],
        check: &["mix", "test"],
        refine: &[],
        tests: None,
    },
    Ecosystem {
        name: "Swift",
        manifests: &["Package.swift"],
        extensions: &[],
        check: &["swift", "test"],
        refine: &[],
        tests: None,
    },
    Ecosystem {
        name: "Dart",
        manifests: &["pubspec.yaml"],
        extensions: &[],
        check: &["dart", "test"],
        refine: &[],
        tests: None,
    },
    Ecosystem {
        name: "CMake",
        manifests: &["CMakeLists.txt"],
        extensions: &[],
        check: &["ctest"],
        refine: &[],
        tests: None,
    },
];

/// Whether `file` exists in the workspace, counting a Windows launcher sibling.
///
/// `gradlew` and `mvnw` ship as a shell script AND a `.bat`, and a checkout on
/// Windows has both. Looking only for the bare name would miss the wrapper on
/// the platform this harness is developed on.
fn present(workspace: &Path, file: &str) -> bool {
    if workspace.join(file).is_file() {
        return true;
    }
    ["bat", "cmd"]
        .iter()
        .any(|ext| workspace.join(format!("{file}.{ext}")).is_file())
}

impl Ecosystem {
    /// Whether this ecosystem is present in `workspace`.
    #[must_use]
    pub fn detected_in(&self, workspace: &Path) -> bool {
        if self.manifests.iter().any(|m| workspace.join(m).is_file()) {
            return true;
        }
        if self.extensions.is_empty() {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(workspace) else {
            return false;
        };
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| self.extensions.contains(&e))
        })
    }

    /// The check for this ecosystem in this workspace.
    #[must_use]
    pub fn check_in(&self, workspace: &Path) -> Vec<String> {
        if let Some(tests) = self.tests.as_ref().filter(|_| !self.detected_in(workspace)) {
            let found = find_tests(workspace, tests.named);
            if !found.files.is_empty() {
                return (tests.check)(workspace, &found);
            }
        }
        let argv = self
            .refine
            .iter()
            .find(|r| present(workspace, r.file))
            .map_or(self.check, |r| r.check);
        argv.iter().map(|s| (*s).to_string()).collect()
    }
}

/// Every ecosystem present in the workspace.
///
/// Public because a caller that got `None` from [`detected_command`] needs to
/// say WHY — "two ecosystems, pick one" and "no ecosystem at all" are different
/// problems with different fixes, and an operator told only "no check" will go
/// looking for the wrong one.
///
/// # Test files count only where nothing else does
///
/// A manifest is a declaration; a `test_*.py` may be a helper script in a
/// project written in something else. Counted beside the manifests, one
/// release script would make every Rust crate that has one "two ecosystems"
/// and take `cargo test` away from it. So test files are asked about only
/// when no manifest names anything, which is exactly the gap they fill.
#[must_use]
pub fn detected_ecosystems(workspace: &Path) -> Vec<&'static Ecosystem> {
    let declared: Vec<&'static Ecosystem> = ECOSYSTEMS
        .iter()
        .filter(|eco| eco.detected_in(workspace))
        .collect();
    if !declared.is_empty() {
        return declared;
    }
    ECOSYSTEMS
        .iter()
        .filter(|eco| {
            eco.tests
                .as_ref()
                .is_some_and(|tests| !find_tests(workspace, tests.named).files.is_empty())
        })
        .collect()
}

/// Test files named one of `named` ways, within [`TEST_SEARCH_DEPTH`] of the
/// root and [`TEST_SEARCH_ENTRIES`] directory entries, outside
/// [`NOT_SEARCHED`].
fn find_tests(workspace: &Path, named: &[(&str, &str)]) -> FoundTests {
    let mut found = FoundTests {
        files: Vec::new(),
        complete: true,
    };
    let mut budget = TEST_SEARCH_ENTRIES;
    let mut pending: Vec<(std::path::PathBuf, usize)> = vec![(workspace.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if budget == 0 {
                found.complete = false;
                break;
            }
            budget -= 1;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let searched = !name.starts_with('.')
                    && !NOT_SEARCHED.contains(&name.as_str())
                    && !path.join("pyvenv.cfg").is_file();
                if searched && depth < TEST_SEARCH_DEPTH {
                    pending.push((path, depth + 1));
                } else if searched {
                    // Deeper than the search goes: tests may live there, so
                    // what was found is not all there is.
                    found.complete = false;
                }
            } else if kind.is_file()
                && named.iter().any(|(prefix, suffix)| {
                    name.len() > prefix.len() + suffix.len()
                        && name.starts_with(prefix)
                        && name.ends_with(suffix)
                })
            {
                if let Ok(relative) = path.strip_prefix(workspace) {
                    let parts: Vec<String> = relative
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect();
                    found.files.push(parts.join("/"));
                }
            }
        }
        if budget == 0 && !pending.is_empty() {
            found.complete = false;
            break;
        }
    }
    found.files.sort();
    found
}

/// The interpreters tried, in order, on this platform.
///
/// Windows installs answer to `python` or to the `py` launcher, and the
/// `python` on a fresh machine can be a store stub that runs nothing.
/// Elsewhere `python` may not exist at all — Ubuntu ships only `python3`.
const PYTHON_INTERPRETERS: &[&str] = if cfg!(windows) {
    &["python", "py", "python3"]
} else {
    &["python3", "python"]
};

/// Run by each interpreter to say whether pytest is importable: exit 0 when
/// it is, [`PYTEST_MISSING`] when the interpreter runs without it. Any other
/// outcome — no such program, a store stub's own exit code, a hang — means
/// this is not an interpreter that can run the tests.
///
/// `find_spec` locates the module without importing it, so nothing of the
/// project's, or of pytest's, runs to answer the question.
const PYTEST_PROBE: &str =
    "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec('pytest') else 3)";

/// [`PYTEST_PROBE`]'s answer for an interpreter that runs, without pytest.
const PYTEST_MISSING: i32 = 3;

/// How long one interpreter has to answer. A cold start on a slow disk takes
/// a second or two; a hung one must not hold up detection.
const PYTEST_PROBE_WAIT: Duration = Duration::from_secs(10);

/// What one interpreter said about pytest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pytest {
    Importable,
    Missing,
    NoInterpreter,
}

/// The check for a Python project recognised from its test files alone.
///
/// # Why the interpreter is asked, at detection time
///
/// `python -m pytest` with no pytest installed fails before running a test,
/// and the standard library's `unittest` is always there — but which of the
/// two can run is a fact about the machine, not about any file, so it costs
/// a process to learn. Detection runs just before the check it chooses, and
/// beside a test suite one interpreter start is noise; a cached answer would
/// go stale the moment the agent installs pytest. Probing also finds the
/// interpreter that exists, which is what [`PYTHON_INTERPRETERS`] is for.
fn python_tests_check(workspace: &Path, found: &FoundTests) -> Vec<String> {
    python_check_with(workspace, found, &|interpreter| {
        probe_pytest(interpreter, workspace)
    })
}

/// [`python_tests_check`] with the probe passed in, so the choice can be
/// tested without the machine's interpreters deciding the outcome.
///
/// # When `unittest` stands in for pytest
///
/// Only when every test file found defines a `TestCase`, and the search saw
/// them all. `unittest` silently skips a plain `def test_…` function, and
/// before Python 3.12 a run that found no tests exits 0 — so running it over
/// pytest-style files would MINT PROOF from a suite that executed nothing.
/// Otherwise the command stays `-m pytest`, which fails naming the missing
/// module: a check that cannot run says so, and installing pytest fixes it.
///
/// The files are named on the command line rather than discovered, because
/// discovery uses one pattern (`test*.py` misses `calc_test.py`) and does not
/// enter a directory without `__init__.py`.
fn python_check_with(
    workspace: &Path,
    found: &FoundTests,
    probe: &dyn Fn(&str) -> Pytest,
) -> Vec<String> {
    let mut runs_without_pytest = None;
    for &interpreter in PYTHON_INTERPRETERS {
        match probe(interpreter) {
            Pytest::Importable => return argv(&[interpreter, "-m", "pytest", "-q"]),
            Pytest::Missing => {
                runs_without_pytest.get_or_insert(interpreter);
            }
            Pytest::NoInterpreter => {}
        }
    }
    match runs_without_pytest {
        Some(interpreter) if found.complete && every_file_is_unittest(workspace, &found.files) => {
            let mut command = argv(&[interpreter, "-m", "unittest"]);
            command.extend(found.files.iter().cloned());
            command
        }
        Some(interpreter) => argv(&[interpreter, "-m", "pytest", "-q"]),
        None => argv(&[PYTHON_INTERPRETERS[0], "-m", "pytest", "-q"]),
    }
}

/// Whether every one of `files` defines a `unittest` test case.
///
/// A file too large to be a test module, or unreadable, answers no: the safe
/// reading is the one that keeps `unittest` from running.
fn every_file_is_unittest(workspace: &Path, files: &[String]) -> bool {
    const LARGEST_READ: u64 = 1_000_000;
    files.iter().all(|file| {
        let path = workspace.join(file);
        std::fs::metadata(&path).is_ok_and(|m| m.len() <= LARGEST_READ)
            && std::fs::read_to_string(&path).is_ok_and(|text| text.contains("TestCase"))
    })
}

/// Ask `interpreter`, in `workspace`, whether pytest is importable.
///
/// Run where the check will run, so a pytest vendored into the project counts
/// exactly as it would for the real command.
fn probe_pytest(interpreter: &str, workspace: &Path) -> Pytest {
    let mut cmd = std::process::Command::new(interpreter);
    cmd.args(["-c", PYTEST_PROBE])
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::agent::process_tree::prepare(&mut cmd);
    let Ok(child) = cmd.spawn() else {
        return Pytest::NoInterpreter;
    };
    match crate::tools::bash::wait_with_timeout(child, PYTEST_PROBE_WAIT)
        .map(|out| out.status.code())
    {
        Ok(Some(0)) => Pytest::Importable,
        Ok(Some(PYTEST_MISSING)) => Pytest::Missing,
        _ => Pytest::NoInterpreter,
    }
}

/// A command as owned strings.
fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// The check a project's own manifest implies, with nothing configured.
///
/// **`None` when more than one ecosystem is present**, rather than the first
/// one found. Running one ecosystem's tests to prove another's code is a check
/// that passes without executing a line of what changed, and [`Verified`] is
/// minted from the exit code — so the wrong command here does not fail loudly,
/// it certifies. When that happens the operator names the check once, with
/// [`VERIFY_COMMAND_ENV`] or the admin file, and both layers already beat
/// detection.
///
/// This is deliberately stricter than what it replaced: a Rust workspace that
/// also has a `package.json` for its documentation site used to get
/// `cargo test` and now gets nothing until someone says. That is the correct
/// direction to fail — the cost is one setting, and the alternative is a
/// proof nobody can trust.
#[must_use]
pub fn detected_command(workspace: &Path) -> Option<Vec<String>> {
    match detected_ecosystems(workspace).as_slice() {
        [only] => Some(only.check_in(workspace)),
        _ => None,
    }
}

/// Why detection produced no command, in words an operator can act on.
#[must_use]
pub fn detection_note(workspace: &Path) -> String {
    let found = detected_ecosystems(workspace);
    match found.as_slice() {
        [] => format!(
            "no check detected: nothing in this workspace names an ecosystem this \
             harness recognises. Set {VERIFY_COMMAND_ENV} to the command that proves \
             work here."
        ),
        [only] => format!("{} ({})", only.name, only.check_in(workspace).join(" ")),
        many => {
            let names: Vec<&str> = many.iter().map(|e| e.name).collect();
            format!(
                "no check detected: {} ecosystems are present here ({}), and running \
                 one's tests would not prove the other's code. Set {VERIFY_COMMAND_ENV} \
                 to the command that proves work in this project.",
                many.len(),
                names.join(", ")
            )
        }
    }
}

fn tokenize_command(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = None;
    let mut escaped = false;

    for ch in cmd.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(quote) = in_quotes {
            if ch == quote {
                in_quotes = None;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_known_command(first: &str) -> bool {
    const KNOWN_COMMANDS: &[&str] = &[
        "cargo",
        "npm",
        "npx",
        "pnpm",
        "yarn",
        "pytest",
        "python",
        "python3",
        "py",
        "go",
        "node",
        "deno",
        "bun",
        "make",
        "mvn",
        "gradle",
        "gradlew",
        "ctest",
        "dotnet",
        "flutter",
        "ruby",
        "rake",
        "bash",
        "sh",
        "zsh",
        "cmd",
        "powershell",
        "pwsh",
        "git",
        "test",
        "true",
        "false",
        "echo",
        "cat",
        "tap",
        "vitest",
        "jest",
        "mocha",
        "playwright",
        "cypress",
    ];
    let lower = first.to_ascii_lowercase();
    if KNOWN_COMMANDS.contains(&lower.as_str()) {
        return true;
    }
    let lower_path = lower.replace('\\', "/");
    if lower_path.starts_with("./")
        || lower_path.starts_with("../")
        || lower_path.starts_with('/')
        || (lower.len() > 2
            && lower
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && lower.as_bytes().get(1) == Some(&b':'))
    {
        return true;
    }
    const EXECUTABLE_EXTS: &[&str] = &[
        ".exe", ".cmd", ".bat", ".sh", ".ps1", ".py", ".js", ".mjs", ".ts",
    ];
    for ext in EXECUTABLE_EXTS {
        if lower.ends_with(ext) {
            return true;
        }
    }
    false
}

/// Parse a command string into an argv list, respecting quotes, code backticks,
/// and labels. Returns empty if the text does not look like a command.
#[must_use]
pub fn parse_command(input: &str) -> Vec<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let is_backticked = if let (Some(open), Some(close)) = (trimmed.find('`'), trimmed.rfind('`')) {
        open < close
    } else {
        false
    };

    let (source, explicit_command) = if is_backticked {
        let open = trimmed.find('`').unwrap();
        let close = trimmed.rfind('`').unwrap();
        (&trimmed[open + 1..close], true)
    } else {
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower
            .strip_prefix("verify:")
            .or_else(|| lower.strip_prefix("check:"))
            .or_else(|| lower.strip_prefix("run:"))
            .or_else(|| lower.strip_prefix("command:"))
        {
            let offset = trimmed.len() - rest.len();
            (&trimmed[offset..], true)
        } else {
            (trimmed, false)
        }
    };

    let tokens = tokenize_command(source);
    if tokens.is_empty() {
        return Vec::new();
    }
    if explicit_command || is_known_command(&tokens[0]) {
        tokens
    } else {
        Vec::new()
    }
}

/// `verify` as advertised to the model.
#[must_use]
pub fn tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": VERIFY_TOOL,
        "description": "Run this project's own check and report what it said. \
            This is the only thing that establishes whether work is finished — \
            your own reading of the code is a claim, and a passing check is \
            evidence. Call it after making a change, and believe the exit code \
            over your expectations. You cannot choose what is run: the check \
            belongs to the project, so that passing cannot be arranged. To see \
            what it would run before running it, use verify_plan.",
        "input_schema": { "type": "object", "properties": {} }
    })
}

/// The tool that says what `verify` would check, running nothing.
///
/// A tool of its OWN, not an argument to `verify`: `verify` takes no input at
/// all, so its answer can only ever mean a check ran (pinned by
/// `the_model_cannot_choose_what_counts_as_passing`). A `verify` that could
/// also answer without running would be one whose success no longer proves
/// anything by itself.
pub const PLAN_TOOL: &str = "verify_plan";

/// `verify_plan` as advertised to the model.
#[must_use]
pub fn plan_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": PLAN_TOOL,
        "description": "Say what `verify` would check here, WITHOUT running \
            anything: the command it runs, and what the project's frozen contracts \
            (.incode/contracts.json) declare — or why that file cannot be read, \
            which would fail every verify. Use it to plan the work against the \
            check before starting. It proves nothing; only verify does.",
        "input_schema": { "type": "object", "properties": {} }
    })
}

/// Run `verify_plan`. Called by the toolbox's dispatch.
#[must_use]
pub fn run_plan_tool(workspace: &Path) -> String {
    plan(workspace, command_for(workspace).as_deref())
}

/// Run the project's check on the model's request and render the outcome.
///
/// Called by the toolbox's dispatch (`toolbox/mod.rs` `run_tool`).
pub fn run_tool(workspace: &Path) -> Result<String, String> {
    let Some(argv) = command_for(workspace) else {
        return Err(format!(
            "no check is configured for this project, so nothing can be proven. \
             Set {VERIFY_COMMAND_ENV} to the command that decides whether the \
             work is correct."
        ));
    };
    let evidence = run(&argv, workspace)?;
    if !evidence.passed() {
        // Returned as an error so the model cannot read a failure as a pass.
        return Err(evidence.failure_report());
    }
    // The project's own tests can pass while the interfaces they were written
    // against have moved. Checked here, on the same call, so a contract breach
    // fails the same way a broken test does — a check that runs somewhere else
    // is one an agent can finish without ever seeing.
    if let Some(breaches) = contract_breaches(workspace) {
        return Err(format!("{}\n\n{breaches}", evidence.summary()));
    }
    Ok(format!("{}\nThe check passed.", evidence.summary()))
}

/// What `verify` would check in `workspace`, without running any of it.
///
/// The same two halves a real run checks, from the same sources: the command
/// [`command_for`] picks, and the contracts [`contract_breaches`] loads. A
/// contracts file that cannot be read is said here — before the first run,
/// rather than as the reason a finished task fails.
#[must_use]
pub fn plan(workspace: &Path, argv: Option<&[String]>) -> String {
    let command = match argv {
        Some(argv) => format!(
            "`verify` would run `{}` in {}.",
            argv.join(" "),
            workspace.display()
        ),
        None => format!(
            "No check is configured for this project, so `verify` cannot prove anything. \
             Set {VERIFY_COMMAND_ENV} to the command that decides whether the work is correct."
        ),
    };
    let contracts = match crate::contracts::Contracts::load(workspace) {
        Ok(c) if c.is_empty() => format!(
            "No frozen contracts ({} is absent or empty), so only the command decides.",
            crate::contracts::CONTRACTS_FILE
        ),
        Ok(c) => format!(
            "It then holds the code to {}: {} schema(s) ({}) and {} endpoint(s).",
            crate::contracts::CONTRACTS_FILE,
            c.schemas.len(),
            c.schemas.keys().cloned().collect::<Vec<_>>().join(", "),
            c.endpoints.len()
        ),
        Err(e) => format!(
            "The contracts file could not be read, so `verify` will FAIL until it is \
             fixed: {e}"
        ),
    };
    format!("{command}\n{contracts}\nNothing was run.")
}

/// Run a check and return proof, or the reason there is none.
pub fn verify(argv: &[String], cwd: &Path) -> Result<Verified, String> {
    let evidence = run(argv, cwd)?;
    // The same rule on the runner's path as on the model's: a passing suite plus
    // a broken interface is not finished work. Checked BEFORE proof is minted,
    // because `Verified` is the one thing the ledger will not question later.
    if let Some(breaches) = contract_breaches(cwd) {
        return Err(format!(
            "the check passed but a frozen interface did not.\n\n{breaches}"
        ));
    }
    let proof = Verified::from_evidence(evidence.clone())
        .ok_or_else(|| format!("the check did not pass.\n{}", evidence.failure_report()));
    crate::trace!(
        "verify",
        "{:?} -> {}",
        argv,
        match &proof {
            Ok(_) => "passed".to_string(),
            Err(e) => format!("no proof: {}", e.lines().next().unwrap_or(e)),
        }
    );
    proof
}

/// The contract report when something is broken, or `None` when nothing is.
///
/// A contracts file that cannot be READ is itself a failure: a project that
/// froze its interfaces and can no longer check them is not in a position to
/// call anything done.
fn contract_breaches(workspace: &Path) -> Option<String> {
    let contracts = match crate::contracts::Contracts::load(workspace) {
        Ok(contracts) => contracts,
        Err(e) => return Some(format!("the contracts file could not be read: {e}")),
    };
    if contracts.is_empty() {
        return None;
    }
    match crate::contracts::check(workspace, &contracts) {
        Ok(breaches) if breaches.is_empty() => None,
        Ok(breaches) => Some(crate::contracts::report(&breaches)),
        Err(e) => Some(format!("the contract check could not finish: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **THE NAME MUST BE THE BINARY'S, OR THE CHECK RESOLVES TO NOTHING.**
    ///
    /// `SELF_PROGRAM` is a literal because `CARGO_BIN_NAME` is not defined
    /// when the library is compiled. A literal can drift from the `[[bin]]`
    /// it names, and the drift would be silent: `current_exe` still answers,
    /// so every check keeps passing here, and the day someone runs one
    /// against an installed copy on `PATH` it looks for a program that was
    /// renamed. Read from the manifest rather than repeated, so the test can
    /// disagree with the code.
    #[test]
    fn the_self_program_is_the_name_cargo_builds() {
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("the crate's own manifest");
        let declared = manifest
            .split("[[bin]]")
            .nth(1)
            .and_then(|section| {
                section
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("name"))
            })
            .and_then(|rest| rest.split('"').nth(1).map(str::to_string))
            .expect("a [[bin]] with a name");
        assert_eq!(
            SELF_PROGRAM, declared,
            "the binary is `{declared}`; a check written as `{SELF_PROGRAM}` names nothing"
        );
    }

    fn evidence(exit: Option<i32>) -> CommandEvidence {
        CommandEvidence {
            argv: vec!["check".to_string()],
            exit,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            elapsed_ms: 1,
        }
    }

    /// The whole point: proof cannot be manufactured from a failure, however
    /// convincing the surrounding report.
    #[test]
    fn proof_cannot_be_built_from_anything_but_a_clean_exit() {
        assert!(Verified::from_evidence(evidence(Some(0))).is_some());
        assert!(Verified::from_evidence(evidence(Some(1))).is_none());
        assert!(Verified::from_evidence(evidence(Some(101))).is_none());
        assert!(
            Verified::from_evidence(evidence(None)).is_none(),
            "a killed or timed-out check is not a pass"
        );
    }

    /// **A green suite is not proof when the interface moved.** This is the
    /// whole reason contracts are checked inside `verify` rather than beside it:
    /// the module's own tests pass, and the shape the next module was written
    /// against is gone. No `Verified` may be minted from that.
    #[test]
    fn a_passing_check_cannot_mint_proof_while_a_contract_is_breached() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/user.rs"), "pub struct Nothing {}").expect("write");

        let mut fields = std::collections::BTreeMap::new();
        fields.insert("email".to_string(), "string".to_string());
        let mut schemas = std::collections::BTreeMap::new();
        schemas.insert("User".to_string(), crate::contracts::Schema { fields });
        crate::contracts::Contracts {
            schemas,
            endpoints: Vec::new(),
        }
        .save(dir.path())
        .expect("save contracts");

        // A command that really does exit zero.
        let argv = vec!["cargo".to_string(), "--version".to_string()];
        let refused = verify(&argv, dir.path())
            .expect_err("a breached contract must block proof even on a green check");
        assert!(refused.contains("frozen interface"), "got: {refused}");
        assert!(
            refused.contains("User"),
            "it must name what broke: {refused}"
        );
    }

    /// And with the contract honoured, the same command still proves the work —
    /// otherwise the check would simply block everything.
    #[test]
    fn an_honoured_contract_does_not_stand_in_the_way() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/user.rs"),
            "pub struct User { pub email: String }",
        )
        .expect("write");

        let mut fields = std::collections::BTreeMap::new();
        fields.insert("email".to_string(), "string".to_string());
        let mut schemas = std::collections::BTreeMap::new();
        schemas.insert("User".to_string(), crate::contracts::Schema { fields });
        crate::contracts::Contracts {
            schemas,
            endpoints: Vec::new(),
        }
        .save(dir.path())
        .expect("save contracts");

        let argv = vec!["cargo".to_string(), "--version".to_string()];
        assert!(verify(&argv, dir.path()).is_ok());
    }

    /// A real command that really passes.
    #[test]
    fn a_passing_command_produces_proof() {
        let argv = vec!["cargo".to_string(), "--version".to_string()];
        let proof = verify(&argv, Path::new(".")).expect("cargo --version passes");
        assert_eq!(proof.evidence().exit, Some(0));
        assert!(
            proof.evidence().stdout_tail.contains("cargo"),
            "the output must be captured as evidence; got: {:?}",
            proof.evidence().stdout_tail
        );
    }

    /// A real command that really fails, and the reason survives.
    #[test]
    fn a_failing_command_yields_no_proof_and_says_why() {
        let argv = vec![
            "cargo".to_string(),
            "--definitely-not-a-real-flag".to_string(),
        ];
        let error = verify(&argv, Path::new(".")).expect_err("this must not pass");
        assert!(error.contains("did not pass"), "got: {error}");
        assert!(
            error.contains("cargo --definitely-not-a-real-flag"),
            "the report must name what was checked; got: {error}"
        );
    }

    /// "The check says no" and "there was no check" are different facts, and a
    /// caller that conflates them would treat a missing toolchain as a bug in
    /// the code.
    #[test]
    fn a_command_that_cannot_start_is_distinct_from_one_that_fails() {
        let argv = vec!["a-program-that-does-not-exist-anywhere".to_string()];
        let error = run(&argv, Path::new(".")).expect_err("it cannot start");
        assert!(error.contains("could not run"), "got: {error}");
    }

    /// No verify, no done — and the refusal has to explain itself, because this
    /// is the dead end a task with no check is supposed to hit.
    #[test]
    fn an_empty_command_is_refused_rather_than_treated_as_passing() {
        let error = run(&[], Path::new(".")).expect_err("nothing can be proven");
        assert!(error.contains("no verify command"), "got: {error}");
    }

    /// A failure prints its reason at the END, so keeping the head would keep
    /// the half without the answer.
    #[test]
    fn output_is_truncated_from_the_front_so_the_failure_survives() {
        let long = format!(
            "{}error: the thing that actually broke",
            "noise\n".repeat(2000)
        );
        let kept = tail(&long, 100);
        assert!(
            kept.contains("the thing that actually broke"),
            "the tail must survive truncation"
        );
        assert!(
            kept.contains("[earlier output dropped]"),
            "and say it truncated"
        );
        assert!(kept.chars().count() < 200);
    }

    #[test]
    fn short_output_is_left_exactly_as_it_was() {
        assert_eq!(tail("all fine", 100), "all fine");
    }

    /// The loophole this design closes. If the tool took a command from the
    /// caller, a model could ask for something that always passes and its
    /// "proof" would prove nothing — the fabrication this module exists to
    /// prevent, reintroduced through the argument list.
    #[test]
    fn the_model_cannot_choose_what_counts_as_passing() {
        let schema = tool_definition();
        let properties = &schema["input_schema"]["properties"];
        assert_eq!(
            properties.as_object().map(serde_json::Map::len),
            Some(0),
            "the tool must take no arguments at all; got: {properties}"
        );
        let rendered = schema.to_string();
        for forbidden in ["command", "argv", "cmd", "script"] {
            assert!(
                !properties.to_string().contains(forbidden),
                "`{forbidden}` must not be settable by the caller; got: {rendered}"
            );
        }
    }

    #[test]
    fn the_check_comes_from_the_operator_before_the_manifest() {
        // Held for the WHOLE test, not just the half that sets a variable: the
        // first assertion reads the environment too, and taking the lock after
        // it lets another test's value leak into this one.
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(VERIFY_COMMAND_ENV);

        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").expect("write");

        // Point the settings at an empty directory: without this the test reads
        // whatever the operator of this machine happens to have configured, and
        // passes or fails for a reason that has nothing to do with the code.
        let settings = tempfile::tempdir().expect("temp dir");
        std::env::set_var(crate::admin::DATA_DIR_ENV, settings.path());

        // Detected from the manifest when nothing is configured.
        assert_eq!(
            command_for(dir.path()),
            Some(vec!["cargo".to_string(), "test".to_string()])
        );

        // The admin file beats detection: a manifest guess must never override
        // something a human wrote down.
        crate::admin::AdminConfig {
            verify_command: Some(vec!["from".to_string(), "the-file".to_string()]),
            ..Default::default()
        }
        .save(settings.path())
        .expect("save settings");
        assert_eq!(
            command_for(dir.path()),
            Some(vec!["from".to_string(), "the-file".to_string()])
        );

        // The immediate choice wins over the standing one.
        std::env::set_var(VERIFY_COMMAND_ENV, "just the-check");
        let configured = command_for(dir.path());
        std::env::remove_var(VERIFY_COMMAND_ENV);
        std::env::remove_var(crate::admin::DATA_DIR_ENV);
        assert_eq!(
            configured,
            Some(vec!["just".to_string(), "the-check".to_string()])
        );
    }

    /// A project with no check cannot be proven finished, and the tool has to
    /// say so rather than return something that reads like success.
    #[test]
    fn a_project_with_no_check_configured_cannot_pass() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(VERIFY_COMMAND_ENV);
        let error = run_tool(dir.path()).expect_err("nothing can be proven");
        assert!(error.contains("no check is configured"), "got: {error}");
        assert!(error.contains(VERIFY_COMMAND_ENV), "got: {error}");
    }

    /// A failing check must come back as an ERROR, so the model cannot read it
    /// as a pass. This is the whole tool: the exit code overrules the story.
    #[test]
    fn a_failing_check_is_reported_as_a_failure_not_a_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(VERIFY_COMMAND_ENV, "cargo --definitely-not-a-real-flag");
        let outcome = run_tool(dir.path());
        std::env::remove_var(VERIFY_COMMAND_ENV);

        let error = outcome.expect_err("a failing check must not read as success");
        assert!(error.contains("failed"), "got: {error}");
    }

    /// `verify_plan` names the command `verify` would run — the same
    /// [`command_for`] — and says it ran nothing.
    #[test]
    fn a_plan_names_the_command_and_runs_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(VERIFY_COMMAND_ENV, "cargo --definitely-not-a-real-flag");
        let said = run_plan_tool(dir.path());
        std::env::remove_var(VERIFY_COMMAND_ENV);

        assert!(
            said.contains("`cargo --definitely-not-a-real-flag`"),
            "{said}"
        );
        assert!(said.contains("Nothing was run"), "{said}");
        assert!(said.contains("No frozen contracts"), "{said}");
    }

    /// A contracts file that cannot be read is said in the PLAN, before the
    /// work, rather than as the reason the finished task fails.
    #[test]
    fn a_plan_says_a_broken_contracts_file_will_fail_the_check() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join(".incode")).expect("dir");
        std::fs::write(
            dir.path().join(crate::contracts::CONTRACTS_FILE),
            r#"{"interfaces":{"User":{}}}"#,
        )
        .expect("write");
        let broken = plan(dir.path(), Some(&["make".to_string(), "check".to_string()]));
        assert!(broken.contains("will FAIL"), "{broken}");
        assert!(
            broken.contains("`interfaces`"),
            "the unread key is named: {broken}"
        );

        std::fs::write(
            dir.path().join(crate::contracts::CONTRACTS_FILE),
            r#"{"schemas":{"User":{"fields":{"id":"uuid"}}},"endpoints":[{"path":"GET /u"}]}"#,
        )
        .expect("write");
        let frozen = plan(dir.path(), None);
        assert!(
            frozen.contains("1 schema(s) (User) and 1 endpoint(s)"),
            "{frozen}"
        );
        assert!(frozen.contains("No check is configured"), "{frozen}");
    }

    #[test]
    fn the_summary_states_the_command_and_the_outcome() {
        let passed = evidence(Some(0));
        assert!(passed.summary().contains("passed"));
        let failed = evidence(Some(2));
        assert!(failed.summary().contains("failed (exit 2)"));
        let killed = evidence(None);
        assert!(killed.summary().contains("did not finish"));
    }

    #[test]
    fn detected_command_recognizes_python_and_go() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("pyproject.toml"), "[project]").expect("write");
        assert_eq!(
            detected_command(dir.path()),
            Some(vec!["pytest".to_string()])
        );

        let go_dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(go_dir.path().join("go.mod"), "module example.com/m").expect("write");
        assert_eq!(
            detected_command(go_dir.path()),
            Some(vec![
                "go".to_string(),
                "test".to_string(),
                "./...".to_string()
            ])
        );
    }

    /// Every popular ecosystem is recognised from its own manifest alone.
    ///
    /// Table-driven because the failure this guards is a MISSING ROW, and a
    /// test written per-language only ever covers the rows someone remembered.
    #[test]
    fn each_ecosystem_is_detected_from_its_own_manifest() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("Rust", "Cargo.toml", &["cargo", "test"]),
            ("Node.js", "package.json", &["npm", "test"]),
            ("Python", "pyproject.toml", &["pytest"]),
            ("Go", "go.mod", &["go", "test", "./..."]),
            ("PHP", "composer.json", &["composer", "test"]),
            ("Java (Maven)", "pom.xml", &["mvn", "test"]),
            ("Gradle", "build.gradle.kts", &["gradle", "test"]),
            ("Ruby", "Gemfile", &["bundle", "exec", "rake", "test"]),
            ("Elixir", "mix.exs", &["mix", "test"]),
            ("Swift", "Package.swift", &["swift", "test"]),
            ("Dart", "pubspec.yaml", &["dart", "test"]),
            ("CMake", "CMakeLists.txt", &["ctest"]),
        ];

        for (name, manifest, expected) in cases {
            let dir = tempfile::tempdir().expect("temp dir");
            std::fs::write(dir.path().join(manifest), "x").expect("write");
            let want: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
            assert_eq!(
                detected_command(dir.path()),
                Some(want),
                "{name} was not detected from {manifest}"
            );
        }

        // .NET names its project file after the project, so it is matched by
        // extension rather than by a fixed name.
        let dotnet = tempfile::tempdir().expect("temp dir");
        std::fs::write(dotnet.path().join("Shop.csproj"), "<Project/>").expect("write");
        assert_eq!(
            detected_command(dotnet.path()),
            Some(vec!["dotnet".to_string(), "test".to_string()])
        );
    }

    /// **TWO ECOSYSTEMS MUST NOT SILENTLY PICK ONE.**
    ///
    /// The shape that motivated the table: a PHP application with a JavaScript
    /// front end. The chain this replaced returned the first match, so the back
    /// end was "proved" by running the front end's tests — a check that passes
    /// without executing a line of the changed code, and `Verified` is minted
    /// from its exit code.
    #[test]
    fn a_project_in_two_ecosystems_refuses_to_guess() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("composer.json"), "{}").expect("write");
        std::fs::write(dir.path().join("package.json"), "{}").expect("write");

        assert_eq!(
            detected_command(dir.path()),
            None,
            "detection picked one ecosystem's tests to prove the other's code"
        );

        // And it must say WHY: "pick one" and "nothing here" are different
        // problems, and an operator told only "no check" fixes the wrong one.
        let note = detection_note(dir.path());
        assert!(note.contains("PHP"), "got: {note}");
        assert!(note.contains("Node.js"), "got: {note}");
        assert!(note.contains(VERIFY_COMMAND_ENV), "got: {note}");
    }

    /// Several manifests from the SAME ecosystem are one ecosystem, not two.
    ///
    /// Python projects routinely carry pyproject.toml and setup.py and tox.ini.
    /// Counting files instead of ecosystems would make every one of them
    /// ambiguous and refuse a check it could perfectly well determine.
    #[test]
    fn one_ecosystem_with_several_manifests_is_not_ambiguous() {
        let dir = tempfile::tempdir().expect("temp dir");
        for f in ["pyproject.toml", "setup.py", "tox.ini"] {
            std::fs::write(dir.path().join(f), "x").expect("write");
        }
        assert_eq!(
            detected_command(dir.path()),
            Some(vec!["pytest".to_string()])
        );
    }

    /// A checked-in runner beats the canonical command.
    ///
    /// `composer test` needs a script the project may never have defined;
    /// `vendor/bin/phpunit` is what its CI actually runs.
    #[test]
    fn a_checked_in_runner_supersedes_the_canonical_command() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("composer.json"), "{}").expect("write");
        assert_eq!(
            detected_command(dir.path()),
            Some(vec!["composer".to_string(), "test".to_string()]),
            "without the runner, the canonical command stands"
        );

        std::fs::create_dir_all(dir.path().join("vendor/bin")).expect("mkdir");
        std::fs::write(dir.path().join("vendor/bin/phpunit"), "#!/bin/sh").expect("write");
        assert_eq!(
            detected_command(dir.path()),
            Some(vec!["vendor/bin/phpunit".to_string()]),
            "the runner the project checked in was ignored"
        );
    }

    /// The Windows launcher counts as the wrapper being present.
    ///
    /// `gradlew` and `mvnw` ship as a shell script AND a `.bat`. A checkout on
    /// Windows has both, but looking only for the bare name would miss the
    /// wrapper on the platform this harness is developed on — and then run
    /// `gradle test`, which needs Gradle installed and version-matched.
    #[test]
    fn a_windows_launcher_counts_as_the_wrapper() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("build.gradle"), "x").expect("write");
        std::fs::write(dir.path().join("gradlew.bat"), "@echo off").expect("write");

        assert_eq!(
            detected_command(dir.path()),
            Some(vec!["./gradlew".to_string(), "test".to_string()])
        );
    }

    /// A workspace with nothing recognisable says so, and says what to do.
    #[test]
    fn an_unrecognised_workspace_names_the_setting_to_use() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(detected_command(dir.path()), None);
        let note = detection_note(dir.path());
        assert!(note.contains(VERIFY_COMMAND_ENV), "got: {note}");
    }

    /// Write `files` (relative paths) under `root`, creating directories.
    fn write_files(root: &Path, files: &[(&str, &str)]) {
        for (file, text) in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(path, text).expect("write");
        }
    }

    /// **A PYTHON PROJECT WITH TESTS AND NO MANIFEST IS A PYTHON PROJECT.**
    /// `calc.py` beside `test_calc.py` was "nothing here names an
    /// ecosystem", and its agent fell back to a run nothing proved.
    #[test]
    fn python_test_files_name_the_ecosystem_when_no_manifest_does() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_files(
            dir.path(),
            &[
                ("calc.py", "def add(a, b): return a + b\n"),
                ("test_calc.py", ""),
            ],
        );
        let names: Vec<&str> = detected_ecosystems(dir.path())
            .iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["Python"], "the tests were not recognised");

        // Which runner depends on this machine; that it runs a module of the
        // interpreter does not.
        let argv = detected_command(dir.path()).expect("a check for a project with tests");
        assert_eq!(argv.get(1).map(String::as_str), Some("-m"), "{argv:?}");
    }

    /// Both naming conventions are found, below the root as well, and named
    /// the same way on every platform.
    #[test]
    fn test_files_are_found_by_either_convention_and_named_portably() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_files(
            dir.path(),
            &[
                ("calc_test.py", ""),
                ("tests/test_io.py", ""),
                ("conftest.py", ""),
                ("test_.py", ""),
            ],
        );
        let found = find_tests(dir.path(), &[("test_", ".py"), ("", "_test.py")]);
        assert_eq!(found.files, ["calc_test.py", "tests/test_io.py"]);
        assert!(found.complete, "nothing was left unread");
    }

    /// Installed dependencies, virtual environments under any name, and
    /// hidden directories hold other people's tests.
    #[test]
    fn other_peoples_tests_do_not_name_the_project() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_files(
            dir.path(),
            &[
                ("node_modules/pkg/test_a.py", ""),
                (".venv/lib/test_b.py", ""),
                ("env/pyvenv.cfg", "home = /usr/bin\n"),
                ("env/lib/test_c.py", ""),
                ("target/test_d.py", ""),
            ],
        );
        assert!(
            detected_ecosystems(dir.path()).is_empty(),
            "a dependency's tests were taken for the project's"
        );
    }

    /// **A HELPER SCRIPT DOES NOT MAKE A PROJECT AMBIGUOUS.** Test files are
    /// asked about only where no manifest names anything: counted beside one,
    /// a Rust crate with a Python release script would lose `cargo test`.
    #[test]
    fn a_test_script_does_not_take_the_check_from_a_manifest_project() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_files(
            dir.path(),
            &[("Cargo.toml", "x"), ("scripts/test_release.py", "")],
        );
        assert_eq!(
            detected_command(dir.path()),
            Some(vec!["cargo".to_string(), "test".to_string()])
        );

        // And two manifests are still two ecosystems, not three.
        let two = tempfile::tempdir().expect("temp dir");
        write_files(
            two.path(),
            &[
                ("composer.json", "{}"),
                ("package.json", "{}"),
                ("test_x.py", ""),
            ],
        );
        assert_eq!(detected_command(two.path()), None);
        let note = detection_note(two.path());
        assert!(note.contains("2 ecosystems"), "got: {note}");
        assert!(!note.contains("Python"), "got: {note}");
    }

    /// A probe that answers `answers[i]` for the i-th interpreter tried.
    fn probe_of(answers: &'static [Pytest]) -> impl Fn(&str) -> Pytest {
        move |interpreter| {
            PYTHON_INTERPRETERS
                .iter()
                .position(|i| *i == interpreter)
                .and_then(|at| answers.get(at).copied())
                .unwrap_or(Pytest::NoInterpreter)
        }
    }

    /// pytest wherever an interpreter can import it — and the interpreter
    /// is the one that exists, not the first name on the list.
    #[test]
    fn pytest_runs_on_the_interpreter_that_has_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let found = FoundTests {
            files: vec!["test_calc.py".to_string()],
            complete: true,
        };
        let first = python_check_with(dir.path(), &found, &probe_of(&[Pytest::Importable]));
        assert_eq!(first, [PYTHON_INTERPRETERS[0], "-m", "pytest", "-q"]);

        let second = python_check_with(
            dir.path(),
            &found,
            &probe_of(&[Pytest::NoInterpreter, Pytest::Importable]),
        );
        assert_eq!(second, [PYTHON_INTERPRETERS[1], "-m", "pytest", "-q"]);
    }

    /// **UNITTEST STANDS IN ONLY FOR FILES IT CAN RUN.** It skips a plain
    /// `def test_…` and, before 3.12, exits 0 having run nothing — so over
    /// pytest-style files it would mint proof from a suite that executed
    /// nothing. There the command stays pytest, and fails saying so.
    #[test]
    fn unittest_stands_in_for_pytest_only_over_test_cases() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_files(
            dir.path(),
            &[
                (
                    "test_calc.py",
                    "import unittest\nclass T(unittest.TestCase): pass\n",
                ),
                ("tests/io_test.py", "from unittest import TestCase\n"),
                ("test_plain.py", "def test_add():\n    assert 1 + 1 == 2\n"),
            ],
        );
        let missing = probe_of(&[Pytest::Missing]);
        let interpreter = PYTHON_INTERPRETERS[0];

        let cases = FoundTests {
            files: vec!["test_calc.py".to_string(), "tests/io_test.py".to_string()],
            complete: true,
        };
        assert_eq!(
            python_check_with(dir.path(), &cases, &missing),
            [
                interpreter,
                "-m",
                "unittest",
                "test_calc.py",
                "tests/io_test.py"
            ],
            "the test cases run without pytest, every file named"
        );

        let mixed = FoundTests {
            files: vec!["test_calc.py".to_string(), "test_plain.py".to_string()],
            complete: true,
        };
        assert_eq!(
            python_check_with(dir.path(), &mixed, &missing),
            [interpreter, "-m", "pytest", "-q"],
            "a plain test function would have been skipped and the run passed"
        );

        let sample = FoundTests {
            complete: false,
            ..cases
        };
        assert_eq!(
            python_check_with(dir.path(), &sample, &missing),
            [interpreter, "-m", "pytest", "-q"],
            "a partial list would leave the unread files unrun"
        );
    }

    /// With no interpreter at all, the check still names one, so the failure
    /// says what is missing instead of there being no check.
    #[test]
    fn no_interpreter_still_yields_a_check_that_says_what_is_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let found = FoundTests {
            files: vec!["test_calc.py".to_string()],
            complete: true,
        };
        assert_eq!(
            python_check_with(dir.path(), &found, &probe_of(&[])),
            [PYTHON_INTERPRETERS[0], "-m", "pytest", "-q"]
        );
    }

    #[test]
    fn script_or_batch_execution_works() {
        let dir = tempfile::tempdir().expect("temp dir");
        #[cfg(windows)]
        {
            let script_path = dir.path().join("check.cmd");
            std::fs::write(&script_path, "@echo off\r\necho verified_ok\r\nexit 0\r\n")
                .expect("write");
            let argv = vec![script_path.to_string_lossy().to_string()];
            let outcome = run(&argv, dir.path()).expect("batch script runs");
            assert_eq!(outcome.exit(), Some(0));
            assert!(outcome.stdout_tail.contains("verified_ok"));
        }
        #[cfg(not(windows))]
        {
            let script_path = dir.path().join("check.sh");
            std::fs::write(&script_path, "#!/bin/sh\necho verified_ok\nexit 0\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&script_path, perms).unwrap();
            }
            let argv = vec![script_path.to_string_lossy().to_string()];
            let outcome = run(&argv, dir.path()).expect("script runs");
            assert_eq!(outcome.exit(), Some(0));
            assert!(outcome.stdout_tail.contains("verified_ok"));
        }
    }

    #[test]
    fn parse_command_detects_commands_and_skips_prose() {
        assert_eq!(
            parse_command("cargo test --test foo"),
            vec!["cargo", "test", "--test", "foo"]
        );
        assert_eq!(
            parse_command("`cargo test --test foo`"),
            vec!["cargo", "test", "--test", "foo"]
        );
        assert_eq!(parse_command("verify: cargo test"), vec!["cargo", "test"]);
        assert_eq!(
            parse_command("run: pytest -k \"my test\""),
            vec!["pytest", "-k", "my test"]
        );
        assert_eq!(
            parse_command("./scripts/check.sh"),
            vec!["./scripts/check.sh"]
        );
        assert_eq!(parse_command("true"), vec!["true"]);
        // English prose should NOT be parsed as a command
        assert!(parse_command("use the docs check").is_empty());
        assert!(parse_command("try again").is_empty());
        assert!(parse_command("use the docs check, not cargo test").is_empty());
        assert!(parse_command("").is_empty());
    }

    #[test]
    fn compact_log_collapses_repeated_lines() {
        let input = "warning: unused var\nwarning: unused var\nwarning: unused var\nwarning: unused var\nfinal line\n";
        let output = compact_log(input);
        assert!(output.contains("warning: unused var\n[... repeated 3 times ...]"));
        assert!(output.contains("final line"));
        assert!(!output.contains("warning: unused var\nwarning: unused var"));
    }

    #[test]
    fn compact_log_omits_passing_tests_when_failure_present() {
        let input = "test test_1 ... ok\ntest test_2 ... ok\ntest test_3 ... ok\ntest test_4 ... ok\ntest test_fail ... FAILED\nfailures:\n    test_fail\n";
        let output = compact_log(input);
        assert!(output.contains("[... 4 passing tests omitted ...]"));
        assert!(output.contains("test test_fail ... FAILED"));
        assert!(output.contains("failures:\n    test_fail"));
        assert!(!output.contains("test test_1 ... ok"));
    }

    #[test]
    fn compact_log_preserves_passing_tests_when_no_failure() {
        let input = "test test_1 ... ok\ntest test_2 ... ok\ntest test_3 ... ok\n";
        let output = compact_log(input);
        assert_eq!(output, input);
    }

    #[test]
    fn failure_report_uses_compact_log() {
        let mut ev = evidence(Some(1));
        ev.stdout_tail = "test a ... ok\ntest b ... ok\ntest c ... ok\nerror: broke\n".to_string();
        let report = ev.failure_report();
        assert!(report.contains("[... 3 passing tests omitted ...]"));
        assert!(report.contains("error: broke"));
    }
}
