//! The environment a confined command starts with.
//!
//! Computed once, as the complete list, and applied with `env_clear` first:
//! a variable the policy removed must not come back because some later code
//! forgot to remove it again.

use std::ffi::OsString;

use crate::policy::{Env, Inherit};

/// Variables that attach a process to the agent session that started it.
/// An agent CLI started inside another agent's session reads these as its
/// own and joins (or continues) that session; measured with Claude Code,
/// where three concurrent attempts all reported the operator's session id.
/// Credentials (`ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, ...) are NOT
/// here: they say who may run a session, not which one this is.
pub const AGENT_SESSION: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_SESSION_INGRESS_TOKEN_FILE",
    "SESSION_INGRESS_URL",
    "CLAUDE_CODE_SESSION*",
    "CLAUDE_CODE_REMOTE*",
];

/// Variables that point git somewhere other than the directory it runs in.
/// A process started from a git hook inherits `GIT_DIR`, and every `git` it
/// runs then acts on the hook's repository instead of its own checkout.
pub const GIT_REDIRECTS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// Kept under `inherit = "none"`: without them almost nothing starts.
pub const ESSENTIAL: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    // Windows: the loader, the shell and the profile directories.
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
];

/// Whether `name` matches `pattern`: exact, or a prefix when the pattern ends
/// in `*`. Case-insensitive, because Windows reads names that way and a
/// differently-cased `Git_Dir` must not slip through.
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    let pattern = pattern.to_ascii_uppercase();
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// The full environment for the command, from `inherited` (normally
/// `std::env::vars_os()`) under `policy`, with `extra` (the proxy settings,
/// the private temp directory) applied before the policy's own `set`.
#[must_use]
pub fn compute(
    policy: &Env,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    extra: &[(String, String)],
) -> Vec<(OsString, OsString)> {
    let mut removed: Vec<&str> = policy.remove.iter().map(String::as_str).collect();
    if policy.strip_agent_session {
        removed.extend_from_slice(AGENT_SESSION);
    }
    if policy.strip_git_redirects {
        removed.extend_from_slice(GIT_REDIRECTS);
    }

    let mut out: Vec<(OsString, OsString)> = inherited
        .into_iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            let kept = match policy.inherit {
                Inherit::All => true,
                Inherit::None => ESSENTIAL
                    .iter()
                    .copied()
                    .chain(policy.pass.iter().map(String::as_str))
                    .any(|p| matches(p, &name)),
            };
            kept && !removed.iter().any(|p| matches(p, &name))
        })
        .collect();

    let set = extra
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain(policy.set.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    for (name, value) in set {
        out.retain(|(n, _)| !n.to_string_lossy().eq_ignore_ascii_case(name));
        out.push((name.into(), value.into()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn names(env: &[(OsString, OsString)]) -> Vec<String> {
        env.iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_defaults_strip_the_session_and_git_redirects_and_keep_credentials() {
        let env = compute(
            &Env::default(),
            vars(&[
                ("PATH", "/bin"),
                ("CLAUDECODE", "1"),
                ("CLAUDE_CODE_SESSION_ID", "parent"),
                ("CLAUDE_CODE_REMOTE_SESSION_ID", "x"),
                ("Git_Dir", "/elsewhere/.git"),
                ("GIT_WORK_TREE", "/elsewhere"),
                ("ANTHROPIC_API_KEY", "k"),
                ("CLAUDE_CODE_OAUTH_TOKEN", "t"),
            ]),
            &[],
        );
        assert_eq!(
            names(&env),
            ["PATH", "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]
        );
    }

    #[test]
    fn inherit_none_keeps_only_the_essentials_and_what_is_passed() {
        let policy = Env {
            inherit: Inherit::None,
            pass: vec!["NPM_*".into()],
            ..Env::default()
        };
        let env = compute(
            &policy,
            vars(&[
                ("PATH", "/bin"),
                ("HOME", "/h"),
                ("AWS_SECRET_ACCESS_KEY", "s"),
                ("NPM_TOKEN", "n"),
            ]),
            &[],
        );
        assert_eq!(names(&env), ["PATH", "HOME", "NPM_TOKEN"]);
    }

    #[test]
    fn remove_wins_over_pass_and_set_wins_over_everything() {
        let mut policy = Env {
            inherit: Inherit::None,
            pass: vec!["TOKEN".into()],
            remove: vec!["TOKEN".into()],
            ..Env::default()
        };
        policy.set.insert("TMPDIR".into(), "/policy".into());
        let env = compute(
            &policy,
            vars(&[("TOKEN", "x"), ("PATH", "/bin")]),
            &[
                ("TMPDIR".into(), "/private".into()),
                ("https_proxy".into(), "p".into()),
            ],
        );
        assert_eq!(names(&env), ["PATH", "https_proxy", "TMPDIR"]);
        assert_eq!(env.last().map(|(_, v)| v.clone()), Some("/policy".into()));
    }
}
