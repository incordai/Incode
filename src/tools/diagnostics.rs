//! Type-checking an edit the moment it is made.
//!
//! The harness could write a file that does not compile and carry on for the
//! rest of the turn believing it had succeeded. The mistake surfaced later —
//! sometimes several edits later, sometimes only when a human ran a build — and
//! by then it was tangled up with everything done after it. The cheapest moment
//! to learn an edit is broken is immediately after making it.
//!
//! Two decisions shape this module.
//!
//! **The project's own checker is the authority.** Not a parser written here,
//! and not a language server: the toolchain the project already builds with is
//! what decides whether the code compiles, and anything else would be a second
//! opinion that can disagree with the real one. Running it also costs nothing to
//! keep current — a toolchain upgrade upgrades the diagnostics.
//!
//! **Running a build runs code.** Build scripts execute during a check, so this
//! is not a read. That is why it never starts on its own from a mode that would
//! not have let the agent run a command in the first place; see
//! [`DiagnosticsRunner::for_mode`].

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::permissions::PermissionMode;

/// Environment variable naming the checker to run, overriding detection.
///
/// Splitting on whitespace is deliberate: this is an operator-set value, and a
/// checker command is a program and some flags. Anything needing quoting or a
/// pipe belongs in a script the operator points this at.
pub const CHECK_COMMAND_ENV: &str = "INCORD_CHECK_CMD";

/// How long a check may take before it is abandoned.
///
/// Bounded because this runs inside a tool call the agent is waiting on. A
/// checker that hangs would hang the turn, and a slow answer about the previous
/// edit is worth less than getting on with the next one.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// Most diagnostic lines reported back for one edit.
const MAX_FINDINGS: usize = 20;

/// Runs a project's own checker and reports what it said about one file.
#[derive(Debug, Clone)]
pub struct DiagnosticsRunner {
    command: Vec<String>,
    workspace: PathBuf,
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiagnosticsReport {
    /// Diagnostic lines naming the file that was just changed.
    pub findings: Vec<String>,
    /// Set when the checker itself could not be run or did not finish.
    ///
    /// Kept separate from `findings` so "the code is broken" and "nobody could
    /// tell you whether the code is broken" never read as the same thing.
    pub note: Option<String>,
}

impl DiagnosticsReport {
    #[must_use]
    pub fn is_silent(&self) -> bool {
        self.findings.is_empty() && self.note.is_none()
    }

    /// Render for appending to a tool result, or `None` when there is nothing
    /// worth the tokens.
    #[must_use]
    pub fn rendered(&self) -> Option<String> {
        if self.is_silent() {
            return None;
        }
        let mut out = String::from("\n\nDiagnostics for this file:");
        for finding in &self.findings {
            out.push_str("\n  ");
            out.push_str(finding);
        }
        if let Some(note) = &self.note {
            out.push_str("\n  (");
            out.push_str(note);
            out.push(')');
        }
        Some(out)
    }
}

/// Work out what this project is checked with, by the file that declares it.
///
/// Marker files rather than file extensions: a stray `.rs` in a directory does
/// not make it a project, and the manifest is what a toolchain actually keys on.
#[must_use]
pub fn detect_command(workspace: &Path) -> Option<Vec<String>> {
    if workspace.join("Cargo.toml").is_file() {
        return Some(
            ["cargo", "check", "--quiet", "--message-format=short"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        );
    }
    if workspace.join("tsconfig.json").is_file() {
        return Some(
            ["npx", "tsc", "--noEmit", "--pretty", "false"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        );
    }
    None
}

/// The operator's explicit choice, if they made one.
#[must_use]
pub fn command_from_env() -> Option<Vec<String>> {
    let raw = std::env::var(CHECK_COMMAND_ENV).ok()?;
    let parts: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts)
}

impl DiagnosticsRunner {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>, command: Vec<String>) -> Self {
        Self {
            command,
            workspace: workspace.into(),
        }
    }

    /// Build a runner if this session should be checking edits at all.
    ///
    /// Nothing is returned below [`PermissionMode::DangerFullAccess`]. A check
    /// compiles the workspace, and compiling runs build scripts written by
    /// whoever wrote the project — which is running an arbitrary command. The
    /// mode that governs running commands has to govern this too, or an edit
    /// would quietly do what the `bash` tool is gated to prevent.
    ///
    /// An explicit [`CHECK_COMMAND_ENV`] still cannot bypass that: naming the
    /// command says *what* to run, not that running is permitted.
    #[must_use]
    pub fn for_mode(workspace: impl Into<PathBuf>, mode: PermissionMode) -> Option<Self> {
        // Asked through `grants`, the one sanctioned way to compare modes, and
        // asked for the same capability the `bash` tool requires — so this
        // cannot drift from what running a command is gated on, and a policy
        // mode like `Allow` is judged correctly instead of by an equality test
        // that would refuse it.
        if !mode.grants(PermissionMode::DangerFullAccess) {
            return None;
        }
        let workspace = workspace.into();
        let command = command_from_env().or_else(|| detect_command(&workspace))?;
        Some(Self::new(workspace, command))
    }

    #[must_use]
    pub fn command(&self) -> &[String] {
        &self.command
    }

    /// Run the checker and report only what it said about `changed`.
    #[must_use]
    pub fn check(&self, changed: &Path) -> DiagnosticsReport {
        let Some((program, args)) = self.command.split_first() else {
            return DiagnosticsReport::default();
        };

        // Spawned rather than `output()` so the wait can be bounded, reusing the
        // same helper the bash tool waits with — one timeout implementation, and
        // a checker that hangs is killed instead of hanging the turn.
        let spawned = Command::new(program)
            .args(args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let output = match spawned {
            Ok(child) => super::bash::wait_with_timeout(child, CHECK_TIMEOUT),
            Err(e) => Err(e.into()),
        };

        match output {
            Ok(output) => {
                // Both streams: compilers differ on which one carries
                // diagnostics, and reading only one would silently miss them.
                let mut combined = String::from_utf8_lossy(&output.stderr).into_owned();
                combined.push('\n');
                combined.push_str(&String::from_utf8_lossy(&output.stdout));
                report_from_output(&combined, changed, &self.workspace)
            }
            Err(e) => DiagnosticsReport {
                findings: Vec::new(),
                note: Some(format!(
                    "could not run `{program}` to check this file: {e}. \
                     The edit itself was written."
                )),
            },
        }
    }
}

/// Pick the lines of checker output that are about `changed`.
///
/// Scoped to the one file on purpose. A check reports the whole workspace, and
/// most of that is either pre-existing or somebody else's problem; pasting all
/// of it into a tool result would bury the line that matters and be paid for on
/// every later round trip.
#[must_use]
pub fn report_from_output(output: &str, changed: &Path, workspace: &Path) -> DiagnosticsReport {
    // Compilers name a file however they were given it — absolute, relative to
    // the workspace, or with the other separator. Matching on the file name
    // alone would catch a same-named file in a different directory, so the
    // relative path is tried first and the bare name only as a fallback.
    let relative = changed.strip_prefix(workspace).unwrap_or(changed);
    let relative_display = relative.to_string_lossy().replace('\\', "/");
    let file_name = changed
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut findings: Vec<String> = Vec::new();
    let mut truncated = false;

    for line in output.lines() {
        let normalised = line.replace('\\', "/");
        let mentions_file = if relative_display.is_empty() {
            false
        } else {
            normalised.contains(&relative_display)
        } || (!file_name.is_empty() && normalised.contains(&file_name));

        if !mentions_file {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if findings.len() >= MAX_FINDINGS {
            truncated = true;
            break;
        }
        if !findings.iter().any(|existing| existing == trimmed) {
            findings.push(trimmed.to_string());
        }
    }

    let note = if truncated {
        Some(format!(
            "more than {MAX_FINDINGS} diagnostics; run the project's checker to see the rest"
        ))
    } else {
        None
    };

    DiagnosticsReport { findings, note }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_decides_the_checker_not_a_file_extension() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A stray source file is not a project.
        std::fs::write(dir.path().join("stray.rs"), "fn main() {}").expect("write");
        assert_eq!(detect_command(dir.path()), None);

        std::fs::write(dir.path().join("Cargo.toml"), "[package]").expect("write");
        let command = detect_command(dir.path()).expect("a manifest names a checker");
        assert_eq!(command[0], "cargo");
        assert!(command.iter().any(|a| a == "check"), "got: {command:?}");
    }

    /// Running a check compiles the project, and compiling runs build scripts.
    /// A mode that would not let the agent run a command must not let an edit
    /// run one either.
    #[test]
    fn checking_never_starts_below_the_mode_that_governs_running_commands() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").expect("write");

        for mode in [
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite,
            PermissionMode::Prompt,
        ] {
            assert!(
                DiagnosticsRunner::for_mode(dir.path(), mode).is_none(),
                "{mode:?} must not run a checker"
            );
        }
        for mode in [PermissionMode::DangerFullAccess, PermissionMode::Allow] {
            assert!(
                DiagnosticsRunner::for_mode(dir.path(), mode).is_some(),
                "{mode:?} allows running commands, so it allows checking"
            );
        }
    }

    #[test]
    fn only_lines_about_the_edited_file_are_reported() {
        let workspace = Path::new("/w");
        let output = "\
src/other.rs:3:1: error: something wrong elsewhere
src/target.rs:7:5: error: expected `;`
   Compiling something v0.1.0
src/target.rs:9:1: warning: unused variable `x`";

        let report = report_from_output(output, Path::new("/w/src/target.rs"), workspace);
        assert_eq!(
            report.findings,
            vec![
                "src/target.rs:7:5: error: expected `;`",
                "src/target.rs:9:1: warning: unused variable `x`",
            ]
        );
        assert!(report.note.is_none());
    }

    /// Compilers print whichever separator they were handed. Reporting nothing
    /// because of a backslash would look exactly like a clean check.
    #[test]
    fn a_windows_style_path_still_matches() {
        let report = report_from_output(
            "src\\deep\\thing.rs:2:1: error: boom",
            Path::new("C:/w/src/deep/thing.rs"),
            Path::new("C:/w"),
        );
        assert_eq!(report.findings.len(), 1, "got: {report:?}");
    }

    #[test]
    fn a_clean_check_says_nothing_at_all() {
        let report = report_from_output(
            "   Compiling incord-code v0.1.0\n    Finished dev profile",
            Path::new("/w/src/target.rs"),
            Path::new("/w"),
        );
        assert!(report.is_silent());
        assert_eq!(report.rendered(), None, "silence must cost no tokens");
    }

    #[test]
    fn a_flood_of_diagnostics_is_capped_and_says_so() {
        let mut output = String::new();
        for i in 0..(MAX_FINDINGS + 10) {
            output.push_str(&format!("src/target.rs:{i}:1: error: number {i}\n"));
        }
        let report = report_from_output(&output, Path::new("/w/src/target.rs"), Path::new("/w"));
        assert_eq!(report.findings.len(), MAX_FINDINGS);
        assert!(
            report
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("more than"),
            "the cap must be visible, not silent; got: {report:?}"
        );
    }

    /// A checker that will not start is not the same as code that compiles.
    /// Reporting silence here would tell the agent its broken edit was fine.
    #[test]
    fn a_checker_that_cannot_run_says_so_rather_than_reporting_success() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = DiagnosticsRunner::new(
            dir.path(),
            vec!["a-program-that-does-not-exist-anywhere".to_string()],
        );
        let report = runner.check(&dir.path().join("src/target.rs"));

        assert!(report.findings.is_empty());
        let note = report.note.expect("a failure to run must be reported");
        assert!(note.contains("could not run"), "got: {note}");
        assert!(
            note.contains("edit itself was written"),
            "the edit succeeded and the agent must not be told otherwise; got: {note}"
        );
    }

    /// Proves the process actually runs and its output is captured — the part
    /// the pure-function tests above cannot reach.
    #[test]
    fn the_checkers_output_is_actually_captured_from_the_process() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("Cargo.toml");
        std::fs::write(&target, "[package]").expect("write");

        // `cargo locate-project` is present wherever this test can run at all,
        // returns immediately, and names Cargo.toml in its output — so a
        // finding proves the spawn, the capture and the path filter together.
        let runner = DiagnosticsRunner::new(
            dir.path(),
            vec!["cargo".to_string(), "locate-project".to_string()],
        );
        let report = runner.check(&target);

        assert!(
            !report.is_silent(),
            "the checker's output must reach the report; got: {report:?}"
        );
    }
}
