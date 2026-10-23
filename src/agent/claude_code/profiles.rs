//! Which agent profile a turn is pointed at, and the logins that make
//! pointing it anywhere unnecessary.

use std::process::{Command, Stdio};

/// The environment variable naming which agent profile to use.
pub(super) const PROFILE_ENV: &str = "CLAUDE_CONFIG_DIR";
/// Profile directories are the home entries beginning with this.
const PROFILE_PREFIX: &str = ".claude";
/// Where a profile records its session.
const CREDENTIALS_FILE: &str = ".credentials.json";
/// A session with less than this left is treated as already gone, so a turn is
/// not started on a login that will expire while it runs.
const SESSION_MARGIN_SECS: i64 = 120;

/// One agent profile and how much longer its session is good for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The directory to point the agent at.
    pub dir: std::path::PathBuf,
    /// Seconds of session left; negative means it expired that long ago.
    pub seconds_left: i64,
}

/// Every agent profile in the home directory, whether or not its session is live.
///
/// # Why this exists
///
/// The agent falls back to its default profile when nothing says otherwise, and
/// that profile is not necessarily the one the operator logged in with. On this
/// machine the default held a session that had expired while a second profile
/// was live, so every turn failed with an authentication error that named
/// neither profile. Choosing the profile that can actually work removes a whole
/// class of failure that looks exactly like a broken harness.
///
/// # Credentials are never read
///
/// Only the expiry timestamp is taken from the file. The tokens beside it are
/// not read, not copied and not logged — the harness has no use for them and
/// every reason not to touch them.
#[must_use]
pub fn profiles() -> Vec<Profile> {
    home_dir()
        .map(|home| profiles_in(&home))
        .unwrap_or_default()
}

/// The profiles under a given home directory.
///
/// Split from [`profiles`] so the selection can be tested against a directory
/// built for the purpose, rather than against whatever the machine happens to
/// hold.
#[must_use]
pub fn profiles_in(home: &std::path::Path) -> Vec<Profile> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut found: Vec<Profile> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        // A directory only: the home also holds `.claude.json` and a long tail
        // of its backups, none of which are profiles.
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(PROFILE_PREFIX))
        })
        .filter_map(|dir| {
            session_seconds_left(&dir).map(|seconds_left| Profile { dir, seconds_left })
        })
        .collect();
    // Longest-lived first, then by path so the order is the same on every run.
    found.sort_by(|a, b| {
        b.seconds_left
            .cmp(&a.seconds_left)
            .then_with(|| a.dir.cmp(&b.dir))
    });
    found
}

/// The profile whose session is live and lasts longest, if any is — and none
/// when the agent already has a login that no profile file records.
///
/// # Why a login elsewhere wins over a live file
///
/// Pointing the CLI at a profile directory is the only lever this has, and it
/// moves the login along with everything else. On macOS the default profile's
/// login lives in the Keychain under a service name that changes once
/// [`PROFILE_ENV`] names another directory, so redirecting to a leftover
/// `.credentials.json` swapped a working login for a stale one. With a
/// credential in the environment the redirect buys nothing — the CLI
/// authenticates from the variable whichever directory it reads — and costs
/// the operator's own settings for that directory.
#[must_use]
pub fn usable_profile() -> Option<Profile> {
    choose_profile(profiles(), &|| {
        login_outside_profiles(&process_env, &keychain_holds_login)
    })
}

/// [`pick_usable`], unless `outside` names a login the CLI would use anyway.
///
/// `outside` is asked only once a file profile would otherwise be chosen: the
/// Keychain answer costs a process, and with no live file there is nothing it
/// could change.
pub(super) fn choose_profile(
    found: Vec<Profile>,
    outside: &dyn Fn() -> Option<OutsideLogin>,
) -> Option<Profile> {
    let live = pick_usable(found)?;
    if let Some(login) = outside() {
        crate::trace!(
            "agent",
            "{} not chosen: the agent already logs in via {login:?}",
            live.dir.display()
        );
        return None;
    }
    Some(live)
}

/// A login the agent CLI uses that is not a profile's credentials file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsideLogin {
    /// This variable is set. Only its NAME is carried: the value is a secret
    /// and nothing here has a use for it.
    Environment(&'static str),
    /// The macOS Keychain holds the default profile's login.
    Keychain,
}

/// Variables that carry a credential: set to anything, the CLI authenticates
/// with them. Names measured in the 2.1.303 binary.
///
/// `ANTHROPIC_BASE_URL` is deliberately absent. It moves where requests go,
/// not what authenticates them — the CLI still sends a key, a token or its
/// login — so a base URL with no credential is exactly the state in which
/// every turn fails, and counting it would hide that. A host that manages the
/// provider says so with a provider switch below.
const CREDENTIAL_ENV: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "ANTHROPIC_AUTH_TOKEN",
];

/// Switches to a provider whose own credentials then authenticate the CLI.
///
/// Read as switches, the way the CLI reads them (measured in 2.1.303: `1`,
/// `true`, `yes` or `on`, any case) — so `CLAUDE_CODE_USE_BEDROCK=0` is a
/// switch that is off, not a login.
const PROVIDER_ENV: [&str; 7] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_GATEWAY",
];

/// The Keychain service the CLI keeps the DEFAULT profile's login under.
///
/// Measured in 2.1.303: `"Claude Code" + "-credentials"`, with a hash of the
/// directory appended when [`PROFILE_ENV`] names another one — which is why
/// the Keychain is only consulted while that variable is unset.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// How long the Keychain lookup may take. A local lookup answers in
/// milliseconds; a locked or hung keychain must not hold up a turn or a
/// diagnosis.
const KEYCHAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// The login the CLI will use that no profile file records, if there is one.
///
/// `env` and `keychain` are parameters so the decision can be tested without
/// touching the process environment, which every test thread shares. The
/// environment is read first because it is free; the Keychain costs a process.
#[must_use]
pub fn login_outside_profiles(
    env: &dyn Fn(&str) -> Option<String>,
    keychain: &dyn Fn() -> bool,
) -> Option<OutsideLogin> {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    let switched_on = |name: &str| {
        env(name).is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    };
    if let Some(name) = CREDENTIAL_ENV.into_iter().find(|name| set(name)) {
        return Some(OutsideLogin::Environment(name));
    }
    if let Some(name) = PROVIDER_ENV.into_iter().find(|name| switched_on(name)) {
        return Some(OutsideLogin::Environment(name));
    }
    (env(PROFILE_ENV).is_none() && keychain()).then_some(OutsideLogin::Keychain)
}

/// One variable from this process's environment, as [`login_outside_profiles`]
/// reads it.
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Whether the macOS Keychain holds the default profile's login.
///
/// Asks for the item's EXISTENCE only: no `-w`, so the secret is never
/// printed, and an attribute lookup does not raise the access prompt a read
/// of the secret would. Output is discarded; the exit code is the answer.
/// Elsewhere there is no Keychain, and the answer is no without a process.
#[must_use]
pub fn keychain_holds_login() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let mut cmd = Command::new("security");
    cmd.args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::agent::process_tree::prepare(&mut cmd);
    let Ok(child) = cmd.spawn() else {
        return false;
    };
    crate::tools::bash::wait_with_timeout(child, KEYCHAIN_WAIT)
        .is_ok_and(|out| out.status.success())
}

/// The first profile with enough session left to be worth starting a turn on.
#[must_use]
pub fn pick_usable(found: Vec<Profile>) -> Option<Profile> {
    found
        .into_iter()
        .find(|p| p.seconds_left > SESSION_MARGIN_SECS)
}

/// How long this profile's session has left, or `None` if it records none.
fn session_seconds_left(dir: &std::path::Path) -> Option<i64> {
    let text = std::fs::read_to_string(dir.join(CREDENTIALS_FILE)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    let expires_at_ms = find_expiry(&parsed)?;
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    Some((expires_at_ms - now_ms) / 1_000)
}

/// The session expiry anywhere in the document, in milliseconds.
///
/// Searched rather than addressed by path: the key sits one level down today,
/// and a layout change should cost the selection, never break the harness.
pub(super) fn find_expiry(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(found) = map.get("expiresAt").and_then(serde_json::Value::as_i64) {
                return Some(found);
            }
            map.values().find_map(find_expiry)
        }
        _ => None,
    }
}

/// The operator's home directory.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}
