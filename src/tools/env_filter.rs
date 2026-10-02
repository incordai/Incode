//! Environment variable allowlist — filter env vars for subprocess spawning.
//!
//! When spawning bash commands, only pass essential + explicitly allowed env vars.
//! An unlisted variable does not reach the child, so a secret this list has
//! never heard of stays in this process.
//!
//! # What this does NOT do, said plainly
//!
//! This used to claim it "prevents leaking secrets, tokens, or internal state to
//! subprocesses". It does not, and reading it that way is how an operator ends
//! up surprised: [`ALLOWED_API_KEYS`] deliberately passes cloud, git-forge and
//! model credentials THROUGH to every command the agent runs.
//!
//! That is a trade, not an oversight — `gh` cannot open a pull request without a
//! forge token and a deploy cannot run without cloud credentials, so a filter
//! that withheld them would break the work it exists to enable. The honest
//! statement is the narrow one: **this bounds the environment to a known list;
//! it does not keep credentials away from a command the agent chose to run.**
//!
//! The consequence worth knowing: any command the agent runs inherits whatever
//! on that list is set in the operator's shell. An operator who does not want a
//! particular credential reachable unsets it before starting the session — the
//! filter cannot withhold what it is built to pass. Note that `--no-env-filter`
//! goes the other way: it hands the child the FULL environment, this list
//! included, so it widens the exposure rather than narrowing it.

use std::collections::HashMap;

/// Essential system env vars (always passed).
const ESSENTIAL_VARS: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "USER",
    "USERNAME",
    "LOGNAME",
    "SHELL",
    "PATH",
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "SSH_AUTH_SOCK",
    "EDITOR",
    "VISUAL",
    "PAGER",
    // Windows essentials
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "COMSPEC",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "WINDIR",
    "OS",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Developer tool env vars (passed through for builds/tests).
const DEV_TOOL_VARS: &[&str] = &[
    // Rust
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "RUSTFLAGS",
    "CARGO_TARGET_DIR",
    // Node
    "NODE_PATH",
    "NVM_DIR",
    "NPM_CONFIG_PREFIX",
    "PNPM_HOME",
    "BUN_INSTALL",
    "DENO_DIR",
    // Python
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "PYTHONPATH",
    "PYENV_ROOT",
    // Go
    "GOPATH",
    "GOROOT",
    "GOBIN",
    // Java
    "JAVA_HOME",
    "MAVEN_HOME",
    "GRADLE_HOME",
    // Docker
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    // Git
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    // CI/CD
    "CI",
    "GITHUB_ACTIONS",
    "GITHUB_REPOSITORY",
    "GITHUB_REF",
    "GITHUB_SHA",
    // Build
    "CC",
    "CXX",
    "CMAKE_PREFIX_PATH",
    "PKG_CONFIG_PATH",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
];

/// API key vars that are explicitly allowed (for tool use).
const ALLOWED_API_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "GROQ_API_KEY",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "BRAVE_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_DEFAULT_REGION",
    "AWS_REGION",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
];

/// The Watchman desktop app's per-session grants, by exact name.
///
/// When Watchman makes this session the lead of an orchestration run (or
/// gives it the `/operator` app CLI), it puts a loopback endpoint and a token
/// for THIS session into the environment and tells the model to run
/// `watchman control …` / `watchman app …`, which read them. Withholding them
/// would leave the lead unable to delegate, review or finish — every call
/// failing with "credentials missing". Each token is scoped to one session,
/// answered only on 127.0.0.1, and revoked by Watchman when the session ends;
/// no other `WATCHMAN_*` variable is passed.
const WATCHMAN_GRANTS: &[&str] = &[
    "WATCHMAN_CONTROL_ENDPOINT",
    "WATCHMAN_CONTROL_TOKEN",
    "WATCHMAN_APP_ENDPOINT",
    "WATCHMAN_APP_TOKEN",
];

/// Filter environment variables to only include allowed ones.
///
/// Returns a new HashMap containing only vars from the allowlists.
#[must_use]
pub fn filtered_env() -> HashMap<String, String> {
    let mut result = HashMap::new();

    for (key, value) in std::env::vars() {
        if is_allowed(&key) {
            result.insert(key, value);
        }
    }

    result
}

/// Check if an env var name is in the allowlist.
#[must_use]
pub fn is_allowed(name: &str) -> bool {
    let upper = name.to_uppercase();

    // Check exact matches in all lists
    if ESSENTIAL_VARS.iter().any(|v| v.eq_ignore_ascii_case(name)) {
        return true;
    }
    if DEV_TOOL_VARS.iter().any(|v| v.eq_ignore_ascii_case(name)) {
        return true;
    }
    if ALLOWED_API_KEYS
        .iter()
        .any(|v| v.eq_ignore_ascii_case(name))
    {
        return true;
    }
    if WATCHMAN_GRANTS.iter().any(|v| v.eq_ignore_ascii_case(name)) {
        return true;
    }

    // incode's own variables (`INCODE_*`), and the Incord Memory service's
    // (`INCORD_*`, e.g. `INCORD_MEMORY_URL`), which a command run here may need
    // to reach the memory engine the way incode itself does.
    if upper.starts_with("INCODE_") || upper.starts_with("INCORD_") {
        return true;
    }

    false
}

/// Get the count of currently set allowed env vars.
#[must_use]
pub fn allowed_count() -> usize {
    std::env::vars().filter(|(k, _)| is_allowed(k)).count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn essential_vars_allowed() {
        assert!(is_allowed("HOME"));
        assert!(is_allowed("PATH"));
        assert!(is_allowed("SHELL"));
        assert!(is_allowed("TERM"));
        assert!(is_allowed("USERPROFILE"));
    }

    #[test]
    fn dev_tool_vars_allowed() {
        assert!(is_allowed("CARGO_HOME"));
        assert!(is_allowed("RUSTUP_HOME"));
        assert!(is_allowed("GOPATH"));
        assert!(is_allowed("NODE_PATH"));
    }

    #[test]
    fn api_keys_allowed() {
        assert!(is_allowed("ANTHROPIC_API_KEY"));
        assert!(is_allowed("OPENAI_API_KEY"));
        assert!(is_allowed("GH_TOKEN"));
    }

    #[test]
    fn incode_and_incord_prefixed_allowed() {
        assert!(is_allowed("INCODE_CUSTOM_VAR"));
        assert!(is_allowed("INCODE_TRACE"));
        assert!(is_allowed("INCORD_MEMORY_URL"));
        assert!(is_allowed("INCORD_BINANCE_KEY"));
    }

    /// **AN INCODE LEAD UNDER WATCHMAN CAN REACH ITS ORCHESTRATOR.** The four
    /// grant variables pass; nothing else of Watchman's does.
    #[test]
    fn watchman_session_grants_pass_and_nothing_else_of_its() {
        for name in [
            "WATCHMAN_CONTROL_ENDPOINT",
            "WATCHMAN_CONTROL_TOKEN",
            "WATCHMAN_APP_ENDPOINT",
            "WATCHMAN_APP_TOKEN",
        ] {
            assert!(is_allowed(name), "{name}");
        }
        assert!(!is_allowed("WATCHMAN_DATA_DIR"));
        assert!(!is_allowed("WATCHMAN_ANYTHING_ELSE"));
    }

    #[test]
    fn random_vars_blocked() {
        assert!(!is_allowed("MY_SECRET_TOKEN"));
        assert!(!is_allowed("INTERNAL_DEBUG_FLAG"));
        assert!(!is_allowed("PRIVATE_KEY_DATA"));
    }

    #[test]
    fn case_insensitive() {
        assert!(is_allowed("home"));
        assert!(is_allowed("Home"));
        assert!(is_allowed("path"));
    }

    #[test]
    fn filtered_env_returns_subset() {
        let filtered = filtered_env();
        // PATH should always be present
        assert!(filtered.contains_key("PATH") || filtered.contains_key("Path"));
        // Random internal vars should not be present
        for key in filtered.keys() {
            assert!(is_allowed(key), "unexpected var in filtered env: {key}");
        }
    }
}
