//! Is this session actually able to work?
//!
//! # Why a single command for this
//!
//! Every one of these can fail on its own, quietly, and each one fails at a
//! different moment: an agent that is installed but signed out fails at the
//! first turn, a guards file that does not parse fails at the first delegated
//! task, a memory service that is down fails when a note is written, and an
//! incomplete spec set fails when the first line of source is attempted.
//!
//! Discovering them one at a time, hours apart, is how a working afternoon goes.
//! This asks all of them at once and names the fix for each.
//!
//! # What a check may and may not do
//!
//! **Read-only.** Nothing here logs in, writes a file, or repairs anything. A
//! diagnostic that changes state cannot be run twice with the same meaning, and
//! the first thing anyone does with a diagnostic is run it twice.

use std::path::Path;

/// How one check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Working.
    Ok,
    /// Working, with something the operator should know.
    Warn,
    /// Not working. Something will fail because of this.
    Fail,
}

impl Health {
    #[must_use]
    pub fn marker(self) -> &'static str {
        match self {
            Self::Ok => "ok  ",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

/// One thing that was checked.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub health: Health,
    /// What was found — a fact, not a verdict.
    pub detail: String,
    /// What to do about it. Empty when there is nothing to do.
    ///
    /// A failing check with no remedy tells the operator they have a problem
    /// and leaves them exactly as stuck as before.
    pub remedy: String,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            health: Health::Ok,
            detail: detail.into(),
            remedy: String::new(),
        }
    }

    fn warn(name: &str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            health: Health::Warn,
            detail: detail.into(),
            remedy: remedy.into(),
        }
    }

    fn fail(name: &str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            health: Health::Fail,
            detail: detail.into(),
            remedy: remedy.into(),
        }
    }
}

/// Run every check against this workspace.
///
/// Ordered by what fails first in practice, so the first `FAIL` a person reads
/// is usually the one blocking them.
#[must_use]
pub fn run(workspace: &Path) -> Vec<Check> {
    vec![
        check_agents(),
        check_session(),
        check_guards(workspace),
        check_specs(workspace),
        check_ledger(workspace),
        check_memory(),
    ]
}

/// Whether anything found is bad enough to stop work.
#[must_use]
pub fn worst(checks: &[Check]) -> Health {
    if checks.iter().any(|c| c.health == Health::Fail) {
        Health::Fail
    } else if checks.iter().any(|c| c.health == Health::Warn) {
        Health::Warn
    } else {
        Health::Ok
    }
}

/// Is there anything to delegate to, and can more than one share the load?
fn check_agents() -> Check {
    let found = crate::agent::discover();
    let fleet = crate::fleet::Fleet::new(found.clone());

    match fleet.agents().len() {
        0 if found.is_empty() => Check::fail(
            "agent",
            "none installed",
            "Install an agent CLI and log it in. This harness delegates the work and holds no API credential of its own.",
        ),
        // NAMED WITH THEIR VERSIONS. This is the arm someone reads while
        // actively diagnosing "why is nothing running", and a list of display
        // names does not answer it — whether an agent is too old is the first
        // thing to check, and the version was already being collected.
        // `Agent::display_line` wrote exactly this line and nothing called it.
        0 => Check::fail(
            "agent",
            format!(
                "{} installed, none drivable\n{}",
                found.len(),
                found
                    .iter()
                    .map(|a| format!("    {}", a.display_line()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            "Only agents whose stream protocol has been verified are driven. Install a supported one.",
        ),
        1 => Check::warn(
            "agent",
            format!("{} only", fleet.agents()[0].display_name),
            "A second agent on its own subscription would let work continue when this one hits a limit.",
        ),
        _ => Check::ok(
            "agent",
            format!(
                "{} — work can move between them on a limit",
                fleet
                    .agents()
                    .iter()
                    .map(|a| a.display_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    }
}

/// Can the protected-path policy be read, and what does it cover?
/// Is the agent actually logged in, and for how much longer?
///
/// # Why this check exists
///
/// An expired login does not announce itself. It surfaces as a turn that ends
/// without doing anything, which reads exactly like a broken harness — and cost
/// a full day of looking for one. The session is knowable before any turn runs,
/// so it is checked here instead.
///
/// Only expiry timestamps are read. The tokens beside them are never touched.
fn check_session() -> Check {
    if let Some(settings) = crate::agent::api::Settings::from_env() {
        return Check::ok(
            "session",
            format!("API endpoint configured ({})", settings.model),
        );
    }

    let found = crate::agent::claude_code::profiles();
    if found.is_empty() {
        if let Ok(agent) = crate::agent::registry::require_drivable() {
            if agent.protocol != crate::agent::registry::Protocol::ClaudeStreamJson {
                let ver = agent.version.as_deref().unwrap_or("detected");
                return Check::ok("session", format!("{} ready ({ver})", agent.display_name));
            }
        }
        return Check::warn(
            "session",
            "no agent profile found",
            "Log in with the agent's own CLI. Until then every turn will fail at the first request.",
        );
    }
    let live: Vec<_> = found.iter().filter(|p| p.seconds_left > 0).collect();
    let name = |p: &crate::agent::claude_code::Profile| {
        p.dir.file_name().map_or_else(
            || p.dir.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    };
    match live.first() {
        None => {
            if let Ok(agent) = crate::agent::registry::require_drivable() {
                if agent.protocol != crate::agent::registry::Protocol::ClaudeStreamJson {
                    let ver = agent.version.as_deref().unwrap_or("detected");
                    return Check::ok(
                        "session",
                        format!("{} ready ({ver}); Claude expired", agent.display_name),
                    );
                }
            }
            Check::fail(
                "session",
                format!(
                    "every profile expired ({})",
                    found.iter().map(&name).collect::<Vec<_>>().join(", ")
                ),
                "Log in again with the agent's own CLI. No turn can start until one profile has a live session.",
            )
        }
        Some(best) => {
            let hours = best.seconds_left / 3_600;
            let expired: Vec<String> = found
                .iter()
                .filter(|p| p.seconds_left <= 0)
                .map(&name)
                .collect();
            let summary = format!("{} good for {hours}h", name(best));
            if expired.is_empty() {
                Check::ok("session", summary)
            } else {
                // Worth saying: the expired one may be the profile this shell
                // would otherwise have fallen back to.
                Check::warn(
                    "session",
                    format!("{summary}; expired: {}", expired.join(", ")),
                    "Turns use the live profile. Log the expired one(s) in again, or remove them, so the choice is not surprising.",
                )
            }
        }
    }
}

fn check_guards(workspace: &Path) -> Check {
    match crate::guards::Guards::load(workspace) {
        Ok(guards) => {
            let paths = guards.effective();
            if paths.is_empty() {
                return Check::warn(
                    "guards",
                    "nothing is protected",
                    "Every default has been switched off. The agent can edit the tests that judge it.",
                );
            }
            Check::ok("guards", format!("{} path pattern(s) in force", paths.len()))
        }
        // The worst failure this file has is becoming "no guards" quietly, so
        // an unreadable policy is a FAIL rather than a warning: work will be
        // refused rather than run unprotected, and the operator needs to know
        // why before they wonder where their agent went.
        Err(e) => Check::fail(
            "guards",
            e,
            "Fix or delete .incord/guards.json. Until it parses, delegated work is refused rather than run unprotected.",
        ),
    }
}

/// Is the project specified enough to write code for?
fn check_specs(workspace: &Path) -> Check {
    let missing = crate::spec::missing(workspace);
    if missing.is_empty() {
        return Check::ok("specs", "all present");
    }
    Check::warn(
        "specs",
        format!(
            "{} missing: {}",
            missing.len(),
            missing
                .iter()
                .map(|f| f.path)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "Writing source is refused until these exist. Documents and configuration are writable now.",
    )
}

/// Is there work planned, and can it be read?
fn check_ledger(workspace: &Path) -> Check {
    match crate::ledger::Ledger::load(workspace) {
        Ok(ledger) if ledger.tasks.is_empty() => Check::ok("ledger", "no work planned yet"),
        Ok(ledger) => {
            let open = ledger.tasks.iter().filter(|t| t.state.is_open()).count();
            Check::ok(
                "ledger",
                format!("{} task(s), {open} still open", ledger.tasks.len()),
            )
        }
        Err(e) => Check::fail(
            "ledger",
            e,
            "Fix or remove .incord/ledger.json. `/run` cannot start while it is unreadable.",
        ),
    }
}

/// Is the shared memory reachable?
fn check_memory() -> Check {
    match crate::recall::probe() {
        Ok(()) => Check::ok("memory", "reachable"),
        // A warning, not a failure: retrieval fails open by design. The work
        // still runs, it just runs without what the machine already knew.
        Err(e) => Check::warn(
            "memory",
            e,
            "Work continues without recall, guard rules from memory, or saved notes. Start the memory service to restore them.",
        ),
    }
}

/// Render the checks for a terminal.
#[must_use]
pub fn report(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        out.push_str(&format!(
            "  [{}] {:<8} {}\n",
            check.health.marker(),
            check.name,
            check.detail
        ));
        if !check.remedy.is_empty() {
            out.push_str(&format!("              {}\n", check.remedy));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **"NONE DRIVABLE" MUST NAME VERSIONS, NOT JUST NAMES.**
    ///
    /// This is the line someone reads while working out why nothing runs.
    /// A list of display names does not answer it — whether the installed
    /// agent is simply too old is the first thing to check, and the version
    /// was already collected. `Agent::display_line` wrote exactly this and
    /// had no caller.
    #[test]
    fn an_undrivable_agent_is_reported_with_its_version() {
        let agent = crate::agent::registry::DetectedAgent {
            id: "some-agent".to_string(),
            display_name: "Some Agent".to_string(),
            command: "some-agent".to_string(),
            path: None,
            version: Some("0.1.0".to_string()),
            protocol: crate::agent::registry::Protocol::Unverified,
        };

        let line = agent.display_line();
        assert!(line.contains("some-agent"), "{line}");
        assert!(
            line.contains("0.1.0"),
            "the version is the first thing a diagnosis needs: {line}"
        );
        assert!(
            line.contains("detected"),
            "and whether it can be driven at all: {line}"
        );
    }

    /// **A failing check always says what to do.** One that reports a problem
    /// and no remedy leaves the operator exactly as stuck as before it ran.
    #[test]
    fn every_check_that_is_not_ok_names_a_remedy() {
        let dir = tempfile::tempdir().expect("temp dir");
        for check in run(dir.path()) {
            if check.health != Health::Ok {
                assert!(
                    !check.remedy.trim().is_empty(),
                    "`{}` reported {:?} with no remedy",
                    check.name,
                    check.health
                );
            }
        }
    }

    /// **An unreadable guards policy is a FAIL, not a warning.** Its worst
    /// failure mode is becoming "no guards" quietly, so it must be loud.
    #[test]
    fn a_broken_guards_policy_is_reported_as_a_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join(".incord")).expect("dir");
        std::fs::write(dir.path().join(".incord").join("guards.json"), "{ not json")
            .expect("write");

        let checks = run(dir.path());
        let guards = checks.iter().find(|c| c.name == "guards").expect("checked");
        assert_eq!(guards.health, Health::Fail);
        assert!(!guards.remedy.is_empty());
    }

    /// **Unreachable memory is a WARNING, not a failure.** Retrieval fails open
    /// by design: the work still runs, it just runs without what was known.
    /// Reporting it as fatal would say the session is broken when it is not.
    #[test]
    fn memory_being_down_does_not_read_as_a_broken_session() {
        let check = check_memory();
        assert_ne!(
            check.health,
            Health::Fail,
            "a store that is down costs recall, not the session"
        );
    }

    /// A tree with no spec set is warned about, and told what is refused until
    /// it exists — not simply marked bad.
    #[test]
    fn a_missing_spec_set_says_what_it_blocks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let checks = run(dir.path());
        let specs = checks.iter().find(|c| c.name == "specs").expect("checked");
        assert_eq!(specs.health, Health::Warn);
        assert!(specs.remedy.contains("refused"), "got: {}", specs.remedy);
    }

    /// The worst finding is what the run as a whole reports.
    #[test]
    fn the_worst_finding_decides_the_verdict() {
        let ok = Check::ok("a", "fine");
        let warn = Check::warn("b", "hmm", "do this");
        let fail = Check::fail("c", "no", "do that");

        assert_eq!(worst(std::slice::from_ref(&ok)), Health::Ok);
        assert_eq!(worst(&[ok.clone(), warn.clone()]), Health::Warn);
        assert_eq!(worst(&[ok, warn, fail]), Health::Fail);
    }

    /// The report shows the remedy under the finding, so a person reading the
    /// terminal is not sent to look anything up.
    #[test]
    fn the_report_shows_remedies_beneath_their_findings() {
        let out = report(&[Check::fail("thing", "it is broken", "fix it like so")]);
        assert!(out.contains("it is broken"), "got: {out}");
        assert!(out.contains("fix it like so"), "got: {out}");
    }

    #[test]
    fn check_session_accepts_configured_api_settings() {
        std::env::set_var(crate::agent::api::API_URL_ENV, "https://api.example.com");
        std::env::set_var(crate::agent::api::API_KEY_ENV, "sk-dummy");
        std::env::set_var(crate::agent::api::API_MODEL_ENV, "claude-sonnet-4");

        let check = check_session();
        assert_eq!(check.health, Health::Ok);
        assert!(check.detail.contains("API endpoint configured"));

        std::env::remove_var(crate::agent::api::API_URL_ENV);
        std::env::remove_var(crate::agent::api::API_KEY_ENV);
        std::env::remove_var(crate::agent::api::API_MODEL_ENV);
    }
}
