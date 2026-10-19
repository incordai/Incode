//! A spawned agent CLI starts a session of its own, never the one incode runs in.
//!
//! incode is often launched from inside another agent CLI's session — a person
//! runs `incode -t` from that CLI's shell, or an eval drives several incodes at
//! once from one. That parent session advertises itself through environment
//! variables, and a child CLI that inherits them reports the PARENT's session
//! id as its own. Measured (issue #3): every turn's `agent_session_id` equalled
//! the parent's in 5 of 5 `incode -t` runs, and three concurrent evals shared
//! one id — so resuming, attributing and recording a turn all pointed at a
//! session incode never started.
//!
//! The fix is subtractive: every agent CLI incode spawns has the parent's
//! session variables removed from its environment, and nothing else.
//! Credentials and configuration (`ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`,
//! `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CONFIG_DIR`, ...) are deliberately kept:
//! they say WHO may run a session, not WHICH session this is, and the child
//! cannot authenticate without them.
//!
//! The bash tool needs none of this by default: its allowlist
//! (`tools::env_filter::is_allowed`) admits none of these names, which a test
//! below holds.

use std::ffi::{OsStr, OsString};
use std::process::Command;

/// Names that tie a process to the session that launched it. Removed from the
/// child whether or not this process has them: removing an absent variable
/// costs nothing, and it keeps the removal visible on the built command for
/// the tests that guard each spawn site.
const SESSION_NAMES: [&str; 6] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_SESSION_INGRESS_TOKEN_FILE",
];

/// Families of session variables whose members are not a fixed list (the
/// session id, its attendance flag, a remote session's handles). A prefix can
/// only be matched against names that are actually inherited, so these are
/// read from the environment rather than removed blind.
const SESSION_PREFIXES: [&str; 2] = ["CLAUDE_CODE_SESSION", "CLAUDE_CODE_REMOTE"];

/// Matched exactly and inherited-only: an ingress endpoint belongs to the
/// parent's remote session, and a bare name this generic is removed only when
/// it is really there.
const INGRESS_URL: &str = "SESSION_INGRESS_URL";

/// Strip the parent agent session's variables from `cmd`, as inherited from
/// this process. Call it on every agent CLI command, straight after
/// `Command::new`, so a variable incode sets deliberately later still wins.
pub fn detach_from_parent_session(cmd: &mut Command) {
    remove_session_vars(cmd, std::env::vars_os().map(|(name, _)| name));
}

/// The removal itself, over an `inherited` list of variable names, so a test
/// can hand it a parent's environment without touching the process's own —
/// which the other tests, running on parallel threads, also read.
fn remove_session_vars(cmd: &mut Command, inherited: impl IntoIterator<Item = OsString>) {
    for name in SESSION_NAMES {
        cmd.env_remove(name);
    }
    for name in inherited {
        if is_inherited_session_var(&name) {
            cmd.env_remove(name);
        }
    }
}

/// Upper-cased before matching because Windows reads environment names
/// case-insensitively, so a mixed-case spelling there is the same variable.
/// The fixed [`SESSION_NAMES`] are not checked here: they are removed
/// unconditionally, and `env_remove` is itself case-insensitive on Windows.
fn is_inherited_session_var(name: &OsStr) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    upper == INGRESS_URL || SESSION_PREFIXES.iter().any(|p| upper.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parent agent session's environment, auth included, as the child
    /// would otherwise inherit it.
    fn parent_env() -> Vec<OsString> {
        [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_CODE_REMOTE_SESSION_ID",
            "SESSION_INGRESS_URL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "PATH",
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    /// What the built command does with `name`: `Some(None)` is removed,
    /// `Some(Some(_))` is set, `None` is left to inheritance.
    fn fate(cmd: &Command, name: &str) -> Option<Option<OsString>> {
        cmd.get_envs()
            .find(|(k, _)| *k == OsStr::new(name))
            .map(|(_, v)| v.map(OsStr::to_os_string))
    }

    #[test]
    fn a_child_cli_never_inherits_the_parent_agent_session() {
        let mut cmd = Command::new("agent");
        remove_session_vars(&mut cmd, parent_env());
        for name in [
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_CODE_REMOTE_SESSION_ID",
            "SESSION_INGRESS_URL",
        ] {
            assert_eq!(
                fate(&cmd, name),
                Some(None),
                "{name} reached the child, which then reports the parent's session as its own"
            );
        }
    }

    #[test]
    fn detaching_from_the_parent_session_keeps_the_childs_credentials() {
        let mut cmd = Command::new("agent");
        remove_session_vars(&mut cmd, parent_env());
        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "PATH",
        ] {
            assert_eq!(
                fate(&cmd, name),
                None,
                "{name} was removed, and the child cannot authenticate or run without it"
            );
        }
    }

    #[test]
    fn a_session_variable_is_recognised_in_any_case() {
        assert!(
            is_inherited_session_var(OsStr::new("Claude_Code_Session_Id")),
            "Windows reads this as CLAUDE_CODE_SESSION_ID, so it is still the parent's session id"
        );
    }

    #[test]
    fn the_bash_allowlist_never_passes_a_parent_session_variable() {
        for name in SESSION_NAMES.into_iter().chain([
            INGRESS_URL,
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_REMOTE",
        ]) {
            assert!(
                !crate::tools::env_filter::is_allowed(name),
                "{name} passes the bash allowlist, so a CLI the agent runs joins the parent session"
            );
        }
    }
}
