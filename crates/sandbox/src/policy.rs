//! The policy: one file that says what a confined command may do.
//!
//! ```toml
//! [fs]
//! mode = "workspace-write"        # off | read-only | workspace-write
//! write = ["."]                   # writable roots (workspace-write only)
//! deny_read = ["../private"]      # hidden: not even readable
//! protect_git = true              # .git/hooks and .git/config kept as they were (default)
//! block_wsl_interop = true        # inside WSL2: no starting Windows programs (default)
//!
//! [network]
//! mode = "proxy"                  # allow | deny | proxy
//! allow = ["crates.io", "*.crates.io:443", "registry.npmjs.org"]
//! allow_private = false           # may an allowed name resolve to a private address?
//! loopback_ports = [5173]         # this machine's own ports the proxy may reach
//!
//! [env]
//! inherit = "all"                 # all | none
//! pass = ["ANTHROPIC_API_KEY"]    # kept when inherit = "none"
//! remove = ["AWS_*"]              # removed; a trailing * matches a prefix
//! strip_agent_session = true      # CLAUDECODE, CLAUDE_CODE_SESSION*, ... (default)
//! strip_git_redirects = true      # GIT_DIR, GIT_WORK_TREE, ... (default)
//! strip_loader_injection = true   # LD_PRELOAD, LD_AUDIT, DYLD_INSERT_LIBRARIES (default)
//! set = { CI = "1" }
//!
//! [limits]
//! wall_secs = 1800
//! cpu_secs = 1200
//! memory_mb = 4096
//! file_size_mb = 1024
//! open_files = 4096
//! ```
//!
//! Every section is optional. An empty policy confines nothing, which is why
//! [`Policy::workspace`] exists: the shape most callers want, with the
//! defaults that are safe for an agent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Everything a confined command may do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub fs: Fs,
    pub network: Network,
    pub env: Env,
    pub limits: Limits,
}

/// What the command may do to the filesystem. Reading is allowed everywhere
/// (a command that cannot read its toolchain cannot run) except beneath
/// `deny_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fs {
    pub mode: FsMode,
    /// Writable roots under `workspace-write`. Relative paths are resolved
    /// against the directory the command starts in.
    pub write: Vec<PathBuf>,
    /// Paths the command may not even read: a benchmark's hidden tests, a
    /// secrets folder, another attempt's results. Needs `read-only` or
    /// `workspace-write`, must exist, and may not lie inside a writable root
    /// (writing implies reading). Relative paths resolve like `write`.
    pub deny_read: Vec<PathBuf>,
    /// Keep each writable root's `.git/hooks` and `.git/config` as they
    /// were: a planted hook runs, unsandboxed, the next time anyone uses git
    /// there (see [`crate::protect`]). On by default.
    pub protect_git: bool,
    /// Inside WSL2: stop the command starting Windows programs, which would
    /// run outside the sandbox altogether (see [`crate::wsl`]). On by
    /// default; only applies when the filesystem is confined.
    pub block_wsl_interop: bool,
}

impl Default for Fs {
    fn default() -> Self {
        Self {
            mode: FsMode::Off,
            write: Vec::new(),
            deny_read: Vec::new(),
            protect_git: true,
            block_wsl_interop: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsMode {
    /// Unconfined.
    #[default]
    Off,
    /// Read anywhere; write nowhere but the run's private temp directory and
    /// the null and terminal devices.
    ReadOnly,
    /// Read anywhere; write only beneath `write` and the private temp directory.
    WorkspaceWrite,
}

impl FsMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }

    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "off" => Some(Self::Off),
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            _ => None,
        }
    }
}

/// What the command may reach over the network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub mode: NetMode,
    /// Under `proxy`: the hosts the command may reach. `example.com` allows
    /// ports 80 and 443, `example.com:8443` that port, `example.com:*` any
    /// port, and `*.example.com` any subdomain (not the apex).
    pub allow: Vec<String>,
    /// Under `proxy`: whether an allowed NAME may resolve to a loopback,
    /// private, link-local or cloud-metadata address. Off by default, so a
    /// DNS answer cannot turn an allowed host into the metadata service.
    pub allow_private: bool,
    /// Under `proxy`: this machine's own ports the command may reach, on
    /// `localhost`, `127.0.0.1` or `::1` — a dev server a browser test loads,
    /// say. Only a loopback address is connected to, and nothing else private
    /// opens: `allow_private` stays as it is. The operator's setting alone:
    /// `sbx mcp` gives an agent no way to ask for it.
    pub loopback_ports: Vec<u16>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetMode {
    /// Unrestricted.
    #[default]
    Allow,
    /// No TCP at all, loopback included.
    Deny,
    /// TCP only to a proxy this process runs, which lets through only the
    /// hosts in `allow`.
    Proxy,
}

impl NetMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Proxy => "proxy",
        }
    }

    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "proxy" => Some(Self::Proxy),
            _ => None,
        }
    }
}

/// The environment the command starts with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Env {
    pub inherit: Inherit,
    /// Kept under `inherit = "none"` (beside the few every program needs).
    pub pass: Vec<String>,
    /// Removed in either mode. A trailing `*` matches a prefix.
    pub remove: Vec<String>,
    /// Remove the variables that attach a process to the agent session that
    /// started this one (see [`crate::env::AGENT_SESSION`]).
    pub strip_agent_session: bool,
    /// Remove the variables that point git at another repository (see
    /// [`crate::env::GIT_REDIRECTS`]).
    pub strip_git_redirects: bool,
    /// Remove the variables that make every program load a library someone
    /// chose (see [`crate::env::LOADER_INJECTION`]).
    pub strip_loader_injection: bool,
    /// Set last, so they win over everything above.
    pub set: BTreeMap<String, String>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            inherit: Inherit::All,
            pass: Vec::new(),
            remove: Vec::new(),
            strip_agent_session: true,
            strip_git_redirects: true,
            strip_loader_injection: true,
            set: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Inherit {
    #[default]
    All,
    None,
}

/// Resource limits. `None` is unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Wall-clock time; the whole process group is killed when it runs out.
    pub wall_secs: Option<u64>,
    /// CPU time per process (RLIMIT_CPU).
    pub cpu_secs: Option<u64>,
    /// Address space per process (RLIMIT_AS).
    pub memory_mb: Option<u64>,
    /// Largest file a process may write (RLIMIT_FSIZE).
    pub file_size_mb: Option<u64>,
    /// Open file descriptors per process (RLIMIT_NOFILE).
    pub open_files: Option<u64>,
}

impl Policy {
    /// The policy most agent callers want: write only inside `root`, network
    /// open, the agent session and git redirects stripped, no limits.
    #[must_use]
    pub fn workspace(root: impl Into<PathBuf>) -> Self {
        Self {
            fs: Fs {
                mode: FsMode::WorkspaceWrite,
                write: vec![root.into()],
                deny_read: Vec::new(),
                protect_git: true,
                block_wsl_interop: true,
            },
            ..Self::default()
        }
    }

    /// Read a policy file (TOML, or JSON when the name ends in `.json`).
    ///
    /// # Errors
    /// When the file cannot be read or does not parse.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the policy {}: {e}", path.display()))?;
        if path.extension().is_some_and(|e| e == "json") {
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
        } else {
            Self::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))
        }
    }

    /// A policy from TOML text.
    ///
    /// # Errors
    /// When the text does not parse or names an unknown key.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Writable roots made absolute against `cwd`, then canonicalised so a
    /// rule matches the path the kernel sees. A root that does not exist is
    /// an error: a typo would otherwise leave the command unable to write
    /// where it was meant to, or (worse) write nowhere it was meant to.
    ///
    /// # Errors
    /// When a root does not exist.
    pub fn resolved_roots(&self, cwd: &Path) -> Result<Vec<PathBuf>, String> {
        if self.fs.mode != FsMode::WorkspaceWrite {
            return Ok(Vec::new());
        }
        self.fs
            .write
            .iter()
            .map(|root| {
                let joined = absolute(root, cwd);
                std::fs::canonicalize(&joined).map_err(|e| {
                    format!("the writable root {} cannot be used: {e}", joined.display())
                })
            })
            .collect()
    }

    /// The `deny_read` paths, absolute and canonical, checked against the
    /// writable `roots` ([`Self::resolved_roots`]).
    ///
    /// # Errors
    /// When a path does not exist, the filesystem mode is `off`, or a path
    /// lies inside a writable root.
    pub fn resolved_deny_read(
        &self,
        cwd: &Path,
        roots: &[PathBuf],
    ) -> Result<Vec<PathBuf>, String> {
        if self.fs.deny_read.is_empty() {
            return Ok(Vec::new());
        }
        if self.fs.mode == FsMode::Off {
            return Err("fs.deny_read needs fs.mode = \"read-only\" or \"workspace-write\"".into());
        }
        self.fs
            .deny_read
            .iter()
            .map(|p| {
                let joined = absolute(p, cwd);
                let real = std::fs::canonicalize(&joined).map_err(|e| {
                    format!(
                        "the deny_read path {} cannot be used: {e}",
                        joined.display()
                    )
                })?;
                if let Some(root) = roots.iter().find(|r| real.starts_with(r)) {
                    return Err(format!(
                        "the deny_read path {} is inside the writable root {}, and writing \
                         implies reading",
                        real.display(),
                        root.display()
                    ));
                }
                Ok(real)
            })
            .collect()
    }

    /// Whether this policy changes anything about a command.
    #[must_use]
    pub fn is_unconfined(&self) -> bool {
        self.fs.mode == FsMode::Off
            && self.network.mode == NetMode::Allow
            && self.limits == Limits::default()
    }
}

/// `path` made absolute: a leading `~` is the home directory (policies are
/// often written by hand, and a harness passes arguments without a shell to
/// expand them), anything else relative is taken from `cwd`.
fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    };
    let mut parts = path.components();
    if parts.next().is_some_and(|c| c.as_os_str() == "~") {
        if let Some(home) = home() {
            return home.join(parts.as_path());
        }
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_example_parses() {
        let doc = include_str!("policy.rs")
            // Not `toml\n`: a Windows checkout has CRLF line endings.
            .split("//! ```toml")
            .nth(1)
            .and_then(|rest| rest.split("//! ```").next())
            .expect("the module docs carry an example");
        let text: String = doc
            .lines()
            .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");
        let policy = Policy::from_toml(&text).expect("the example parses");
        assert_eq!(policy.fs.mode, FsMode::WorkspaceWrite);
        assert_eq!(policy.network.mode, NetMode::Proxy);
        assert_eq!(policy.network.allow.len(), 3);
        assert_eq!(policy.network.loopback_ports, [5173]);
        assert!(!policy.network.allow_private);
        assert_eq!(policy.env.set.get("CI").map(String::as_str), Some("1"));
        assert_eq!(policy.limits.wall_secs, Some(1800));
    }

    #[test]
    fn an_unknown_key_is_refused_not_ignored() {
        // A misspelt `wirte = [...]` silently ignored would leave the
        // command writing nowhere it was meant to.
        let err = Policy::from_toml("[fs]\nmode = \"read-only\"\nwirte = [\"x\"]\n")
            .expect_err("unknown keys are errors");
        assert!(err.contains("wirte"), "{err}");
    }

    #[test]
    fn the_default_strips_the_agent_session_and_git_redirects() {
        let policy = Policy::default();
        assert!(policy.env.strip_agent_session);
        assert!(policy.env.strip_git_redirects);
        assert!(policy.is_unconfined());
    }

    #[test]
    fn a_leading_tilde_is_the_home_directory() {
        let cwd = Path::new("/work");
        // As `absolute` finds it: HOME, else USERPROFILE (Windows).
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .expect("a home directory");
        assert_eq!(absolute(Path::new("~/.claude"), cwd), home.join(".claude"));
        assert_eq!(absolute(Path::new("~"), cwd), home);
        assert_eq!(absolute(Path::new("a/~b"), cwd), cwd.join("a/~b"));
        assert_eq!(absolute(Path::new("/x"), cwd), PathBuf::from("/x"));
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = Policy::workspace("does-not-exist");
        assert!(policy.resolved_roots(dir.path()).is_err());
        let policy = Policy::workspace(".");
        let roots = policy.resolved_roots(dir.path()).expect("cwd exists");
        assert_eq!(
            roots,
            vec![std::fs::canonicalize(dir.path()).expect("canon")]
        );
    }
}
