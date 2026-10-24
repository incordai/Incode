//! Managed settings: a machine-wide file an administrator owns, whose rules
//! no user or project can take back.
//!
//! # Where
//!
//! - Linux and other Unix: `/etc/incode/managed.toml`
//! - macOS: `/Library/Application Support/incode/managed.toml`
//! - Windows: `<ProgramData>\incode\managed.toml`, where `<ProgramData>` is
//!   the folder Windows records (`C:\ProgramData` as installed) — asked of the
//!   OS, never read from the `%ProgramData%` variable
//!
//! Written by an administrator and readable by everyone; incode never writes
//! it. Its location is fixed in code, deliberately: an environment variable
//! that moved it would let the person it constrains point it somewhere empty.
//!
//! # What it holds
//!
//! ```toml
//! [permissions]
//! deny  = ["Bash(curl:*)", "Read(.env)"]   # unioned with every other layer
//! ask   = ["Bash(git push:*)"]
//! allow = ["Bash(npm test:*)"]
//!
//! [mcp]
//! # When present, ONLY servers matching an entry may connect.
//! allow = ["github", "command:/opt/approved/*", "url:https://mcp.example.com/*"]
//! # Never connected, whatever else says so.
//! deny  = ["*filesystem*"]
//! ```
//!
//! Permission rules are unioned with the user's and the project's
//! ([`super::rules`]), and deny beats everything — so a managed deny cannot be
//! overridden. The MCP lists are applied to every server a session would
//! start (`.mcp.json`, `~/.incode/mcp.json`, installed tools, an ACP client's),
//! at the one place they are connected.
//!
//! # A file that cannot be read fails closed
//!
//! If the file exists and is not valid, every tool call is refused and no MCP
//! server connects, until it is fixed. A policy that silently stops applying
//! because of a typo is no policy.

use std::path::PathBuf;

use serde::Deserialize;

use super::rules::RuleLists;

/// The file's name, in [`managed_dir`].
pub const MANAGED_FILE: &str = "managed.toml";

/// The directory the managed file is in, on this OS.
#[must_use]
pub fn managed_dir() -> PathBuf {
    if cfg!(windows) {
        program_data().join("incode")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/incode")
    } else {
        PathBuf::from("/etc/incode")
    }
}

/// Windows' ProgramData folder as the OS records it. NOT `%ProgramData%`: the
/// person the managed file constrains sets their own environment, and one
/// pointing it at an empty folder would leave no policy at all.
fn program_data() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = known_program_data() {
        return path;
    }
    // The OS always answers; were it not to, its own default, still never
    // a value from the environment.
    PathBuf::from(r"C:\ProgramData")
}

/// `FOLDERID_ProgramData`, from `SHGetKnownFolderPath`.
#[cfg(windows)]
fn known_program_data() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};

    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: a known-folder id, no flags, no token (this user), and an out
    // pointer the call fills with a string for CoTaskMemFree.
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, std::ptr::null_mut(), &mut raw) };
    let path = (status >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a NUL-terminated wide string.
        let wide = unsafe {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            std::slice::from_raw_parts(raw, len)
        };
        PathBuf::from(std::ffi::OsString::from_wide(wide))
    });
    // SAFETY: frees what the call allocated, on success or failure (null is
    // allowed).
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

/// The managed file, on this OS.
#[must_use]
pub fn managed_path() -> PathBuf {
    managed_dir().join(MANAGED_FILE)
}

/// What the managed file says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSettings {
    #[serde(default)]
    pub permissions: RuleLists,
    #[serde(default)]
    pub mcp: McpPolicy,
}

/// Which MCP servers may connect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpPolicy {
    /// When set, only servers matching an entry connect. Empty: none do.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    /// Servers matching an entry never connect.
    #[serde(default)]
    pub deny: Vec<String>,
}

/// One server, as the policy sees it.
#[derive(Debug, Clone, Copy)]
pub struct ServerFacts<'a> {
    pub name: &'a str,
    /// The program a stdio server runs.
    pub command: Option<&'a str>,
    /// The address an HTTP server is reached at.
    pub url: Option<&'a str>,
}

impl McpPolicy {
    /// Why `server` may not connect, or `None` when it may.
    #[must_use]
    pub fn refuses(&self, server: ServerFacts<'_>) -> Option<String> {
        if let Some(entry) = self.deny.iter().find(|e| entry_matches(e, server)) {
            return Some(format!(
                "the managed settings deny it (`{entry}` in [mcp] deny)"
            ));
        }
        match &self.allow {
            Some(allow) if !allow.iter().any(|e| entry_matches(e, server)) => Some(
                "the managed settings allow only listed MCP servers ([mcp] allow), and it is \
                 not one"
                    .to_string(),
            ),
            _ => None,
        }
    }
}

/// `name-glob`, `command:<glob>` or `url:<glob>`.
fn entry_matches(entry: &str, server: ServerFacts<'_>) -> bool {
    let (pattern, against) = if let Some(rest) = entry.strip_prefix("command:") {
        (rest, server.command)
    } else if let Some(rest) = entry.strip_prefix("url:") {
        (rest, server.url)
    } else {
        (entry, Some(server.name))
    };
    let Some(against) = against else {
        return false;
    };
    globset::GlobBuilder::new(pattern.trim())
        .literal_separator(false)
        .build()
        .map(|glob| glob.compile_matcher().is_match(against))
        // A pattern that does not compile is compared as written.
        .unwrap_or_else(|_| pattern.trim() == against)
}

/// What became of reading the managed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Managed {
    /// There is none: nothing is managed.
    Absent,
    Loaded(ManagedSettings),
    /// It exists and could not be read: fail closed.
    Broken(String),
}

/// Parse the managed file's text.
///
/// # Errors
///
/// The text is not valid TOML, or names a key this file does not have — a
/// misspelt `[permisions]` must not quietly stop applying.
pub fn parse(text: &str) -> Result<ManagedSettings, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Read the managed file. Under `cfg(test)` there is none: a test must not
/// depend on the machine it runs on.
#[must_use]
pub fn load() -> Managed {
    if cfg!(test) {
        return Managed::Absent;
    }
    let path = managed_path();
    match crate::text::read_authored(&path) {
        Ok(text) => match parse(&text) {
            Ok(settings) => Managed::Loaded(settings),
            Err(why) => Managed::Broken(format!("{}: {why}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Managed::Absent,
        Err(e) => Managed::Broken(format!("{}: {e}", path.display())),
    }
}

/// The MCP policy in force: none when nothing is managed, refuse-all when
/// the managed file is broken.
#[must_use]
pub fn mcp_policy() -> McpPolicy {
    match load() {
        Managed::Absent => McpPolicy::default(),
        Managed::Loaded(settings) => settings.mcp,
        Managed::Broken(_) => McpPolicy {
            allow: Some(Vec::new()),
            deny: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_managed_file_parses_permissions_and_mcp_lists() {
        let parsed = parse(
            r#"
            [permissions]
            deny = ["Bash(curl:*)", "Read(.env)"]
            allow = ["Bash(npm test:*)"]

            [mcp]
            allow = ["github", "url:https://mcp.example.com/*"]
            deny = ["evil*"]
            "#,
        )
        .expect("parses");
        assert_eq!(parsed.permissions.deny.len(), 2);
        assert_eq!(parsed.permissions.allow, ["Bash(npm test:*)"]);
        assert_eq!(parsed.mcp.deny, ["evil*"]);
        assert_eq!(parsed.mcp.allow.as_ref().map(Vec::len), Some(2));
    }

    /// A misspelt section must not silently switch the policy off.
    #[test]
    fn an_unknown_key_is_an_error_not_an_empty_policy() {
        assert!(parse("[permisions]\ndeny = [\"Bash\"]\n").is_err());
        assert!(parse("[mcp]\nallowed = []\n").is_err());
        assert!(parse("not toml at all [").is_err());
        assert_eq!(
            parse("").expect("empty is fine"),
            ManagedSettings::default()
        );
    }

    fn stdio<'a>(name: &'a str, command: &'a str) -> ServerFacts<'a> {
        ServerFacts {
            name,
            command: Some(command),
            url: None,
        }
    }

    #[test]
    fn mcp_deny_beats_allow_and_an_allowlist_excludes_the_rest() {
        let policy = McpPolicy {
            allow: Some(vec![
                "github".to_string(),
                "command:/opt/ok/*".to_string(),
                "url:https://mcp.example.com/*".to_string(),
            ]),
            deny: vec!["github".to_string()],
        };
        assert!(
            policy.refuses(stdio("github", "npx")).is_some(),
            "deny wins"
        );
        assert!(policy.refuses(stdio("tools", "/opt/ok/server")).is_none());
        assert!(policy.refuses(stdio("tools", "/tmp/evil")).is_some());
        let http = ServerFacts {
            name: "remote",
            command: None,
            url: Some("https://mcp.example.com/v1"),
        };
        assert!(policy.refuses(http).is_none());
        let elsewhere = ServerFacts {
            url: Some("https://evil.example/v1"),
            ..http
        };
        assert!(policy.refuses(elsewhere).is_some());
    }

    #[test]
    fn no_policy_refuses_nothing_and_an_empty_allowlist_refuses_everything() {
        assert!(McpPolicy::default().refuses(stdio("x", "y")).is_none());
        let none = McpPolicy {
            allow: Some(Vec::new()),
            deny: Vec::new(),
        };
        assert!(none.refuses(stdio("x", "y")).is_some());
    }

    #[test]
    fn a_test_never_reads_the_machines_managed_file() {
        assert_eq!(load(), Managed::Absent);
    }

    /// **THE PERSON THE FILE CONSTRAINS SETS THEIR OWN ENVIRONMENT.** Pointing
    /// `%ProgramData%`, or the variables Windows builds it from, at an empty
    /// folder must not move the managed file there.
    #[cfg(windows)]
    #[test]
    fn the_managed_folder_does_not_follow_the_environment() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let real = managed_dir();
        let decoy = tempfile::tempdir().expect("temp dir");
        let names = ["ProgramData", "ALLUSERSPROFILE", "SystemDrive"];
        let saved: Vec<_> = names.iter().map(std::env::var_os).collect();
        for name in names {
            std::env::set_var(name, decoy.path());
        }
        let moved = managed_dir();
        for (name, value) in names.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        assert!(
            !moved.starts_with(decoy.path()),
            "the environment moved the managed folder to {}",
            moved.display()
        );
        assert_eq!(moved, real);
        assert!(
            real.is_absolute() && real.ends_with("incode"),
            "{}",
            real.display()
        );
    }
}
