//! The `sbx` engine: the session's shell commands confined by the standalone
//! sandbox in `crates/sandbox`, chosen with `admin.json` `sandbox_engine:
//! "sbx"` or [`super::SANDBOX_ENGINE_ENV`].
//!
//! # What it adds over the built-in engine
//!
//! - **A host allowlist.** `sandbox_allow_hosts` (or
//!   [`ALLOW_HOSTS_ENV`]) lets a command reach only those hosts, through a
//!   proxy sbx runs for the command; the built-in engine can only allow or
//!   deny the network whole. `sandbox_loopback_ports` adds this machine's own
//!   ports (a dev server a browser test loads) without opening anything
//!   else private.
//! - **A seccomp filter on Linux** that refuses terminal injection
//!   (`TIOCSTI`), `io_uring` and vsock, whatever the filesystem mode.
//! - **`.git/hooks` and `.git/config` kept as they were** in every writable
//!   root: a hook the command plants would otherwise run, unconfined, the next
//!   time anyone uses git there. A change is undone when the command ends and
//!   traced.
//! - **The environment scrubbed** of the variables that attach a process to
//!   the agent session, point git at another repository, or make every
//!   program load a chosen library — on top of incode's own env filter.
//! - **A private temp directory per command**, removed when it ends.
//!
//! # How, per OS
//!
//! - **Linux, macOS** — the library, in this process. The helper sbx
//!   re-executes is incode itself, which hands the `__sbx-enter` argument to
//!   [`incord_sandbox::enter_if_requested`] before anything else in `main`.
//! - **Windows** — `sbx.exe`, beside `incode.exe` or on `PATH`, which runs the
//!   command natively (restricted token, ACL grant, job object) or inside WSL2
//!   (Landlock and seccomp), as `sandbox_backend` (or [`BACKEND_ENV`]) says:
//!   `native`, `wsl` or `auto`. Under `wsl` the command runs in WSL's `sh`.
//!
//! Best effort, never silent: what this machine cannot enforce of the policy
//! is named by [`note`], and the command still runs with the rest. A run sbx
//! cannot start at all (a missing root, a proxy that will not bind) is
//! refused rather than run unconfined.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use crate::tools::shell_choice::Shell;

use super::{SandboxMode, SandboxNetwork, SandboxPolicy};

/// Hosts a command may reach (comma-separated), ahead of `admin.json`
/// `sandbox_allow_hosts`.
pub const ALLOW_HOSTS_ENV: &str = "INCODE_SANDBOX_ALLOW_HOSTS";

/// Windows only: `native`, `wsl` or `auto`, ahead of `admin.json`
/// `sandbox_backend`. Handed to `sbx.exe` as `SBX_BACKEND`.
pub const BACKEND_ENV: &str = "INCODE_SANDBOX_BACKEND";

/// What the sbx engine reads beyond the mode and the network switch, once
/// per process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// Hosts a command may reach; empty leaves the network as the switch says.
    pub allow_hosts: Vec<String>,
    /// This machine's own ports a command may reach (`admin.json`
    /// `sandbox_loopback_ports`); like hosts, they put the network behind
    /// sbx's proxy.
    pub loopback_ports: Vec<u16>,
    /// Windows: where `sbx.exe` runs the command. `None` is sbx's own `auto`.
    pub backend: Option<String>,
}

impl Settings {
    /// From the environment first, then `admin.json`.
    fn load() -> Self {
        let admin = crate::admin::AdminConfig::load_default();
        let allow_hosts = match std::env::var(ALLOW_HOSTS_ENV) {
            Ok(raw) if !raw.trim().is_empty() => hosts(&raw),
            _ => admin.sandbox_allow_hosts.clone(),
        };
        let backend = std::env::var(BACKEND_ENV)
            .ok()
            .or(admin.sandbox_backend)
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        Self {
            allow_hosts,
            loopback_ports: admin.sandbox_loopback_ports,
            backend,
        }
    }

    /// Whether an open network goes through sbx's proxy instead.
    fn limits_network(&self) -> bool {
        !self.allow_hosts.is_empty() || !self.loopback_ports.is_empty()
    }
}

/// The settings in force for this process.
pub fn settings() -> &'static Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    SETTINGS.get_or_init(Settings::load)
}

/// A comma-separated host list, trimmed, empties dropped.
fn hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .collect()
}

/// The sbx policy for incode's: the same filesystem mode over the same
/// roots, and the network denied, limited to the settings' hosts and this
/// machine's ports, or open.
#[must_use]
pub fn policy(
    policy: SandboxPolicy,
    roots: &[PathBuf],
    reach: &Settings,
) -> incord_sandbox::Policy {
    use incord_sandbox::{FsMode, NetMode};
    let mut out = incord_sandbox::Policy::default();
    out.fs.mode = match policy.mode {
        SandboxMode::Off => FsMode::Off,
        SandboxMode::ReadOnly => FsMode::ReadOnly,
        SandboxMode::WorkspaceWrite => FsMode::WorkspaceWrite,
    };
    if policy.mode == SandboxMode::WorkspaceWrite {
        out.fs.write = roots.to_vec();
    }
    out.network.mode = match policy.network {
        SandboxNetwork::Deny => NetMode::Deny,
        SandboxNetwork::Allow if reach.limits_network() => NetMode::Proxy,
        SandboxNetwork::Allow => NetMode::Allow,
    };
    if out.network.mode == NetMode::Proxy {
        out.network.allow = reach.allow_hosts.clone();
        out.network.loopback_ports = reach.loopback_ports.clone();
    }
    out
}

/// One line for the trace and `/doctor`: what sbx confines here, and what it
/// cannot.
#[must_use]
pub fn note(policy: SandboxPolicy, roots: &[PathBuf]) -> String {
    let reach = settings();
    let network = match policy.network {
        SandboxNetwork::Deny => "network denied".to_string(),
        SandboxNetwork::Allow if reach.limits_network() => {
            let mut to: Vec<String> = reach.allow_hosts.clone();
            to.extend(
                reach
                    .loopback_ports
                    .iter()
                    .map(|p| format!("this machine's port {p}")),
            );
            format!("network limited to {}", to.join(", "))
        }
        SandboxNetwork::Allow => "network open".to_string(),
    };
    let head = format!("sandbox {} by sbx; {network}", policy.mode.as_str());
    #[cfg(windows)]
    {
        let _ = roots;
        let backend = settings().backend.as_deref().unwrap_or("auto");
        format!("{head}; through sbx.exe ({backend} backend), which reports its own gaps")
    }
    #[cfg(not(windows))]
    {
        let gaps = incord_sandbox::enforcement(&self::policy(policy, roots, reach)).gaps();
        if gaps.is_empty() {
            head
        } else {
            format!("{head}; NOT enforced here: {}", gaps.join("; "))
        }
    }
}

/// What a confined command needs kept alive until it exits: sbx's proxy, its
/// temp directory and the `.git` snapshot. Dropping it undoes any change to
/// `.git/hooks` or `.git/config`, and traces what was undone.
#[derive(Default)]
pub struct Keep {
    #[cfg(not(windows))]
    guard: Option<incord_sandbox::Guard>,
}

impl std::fmt::Debug for Keep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keep")
    }
}

impl Drop for Keep {
    fn drop(&mut self) {
        #[cfg(not(windows))]
        if let Some(guard) = self.guard.take() {
            let restored = guard.restore_protected();
            if !restored.is_empty() {
                crate::trace!(
                    "sandbox",
                    "sbx undid the command's change to {}",
                    restored.join(", ")
                );
            }
        }
    }
}

/// The confined command for one shell `command`, and what to keep alive
/// until it exits. `env`, when given, is the environment to start from
/// (incode's filtered one); otherwise this process's.
///
/// # Errors
///
/// When sbx cannot start the run: a writable root is missing, the proxy will
/// not bind, or (Windows) `sbx.exe` cannot be found.
#[cfg(not(windows))]
pub(super) fn wrap(
    sandbox_policy: SandboxPolicy,
    roots: &[PathBuf],
    shell: &Shell,
    command: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<(Command, Keep), String> {
    use std::ffi::OsString;
    let policy = policy(sandbox_policy, roots, settings());
    let mut sandbox = incord_sandbox::Sandbox::new(policy).strict(false);
    if let Some(env) = env {
        sandbox = sandbox.inherit_from(
            env.iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
        );
    }
    let cwd = match roots.first() {
        Some(root) => root.clone(),
        None => std::env::current_dir().map_err(|e| format!("sbx: no working directory: {e}"))?,
    };
    let args = [OsString::from("-c"), OsString::from(command)];
    let (cmd, guard) = sandbox
        .command(shell.program(), &args, &cwd)
        .map_err(|e| format!("sbx: {e}"))?;
    Ok((cmd, Keep { guard: Some(guard) }))
}

/// Windows: the command run by `sbx.exe`, natively or in WSL2 — see the
/// module documentation.
#[cfg(windows)]
pub(super) fn wrap(
    sandbox_policy: SandboxPolicy,
    roots: &[PathBuf],
    shell: &Shell,
    command: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<(Command, Keep), String> {
    let settings = settings();
    let exe = sbx_exe().ok_or(
        "sbx: sandbox_engine is \"sbx\" but sbx.exe is neither beside incode.exe nor on PATH \
         (scripts/install.ps1 installs both)",
    )?;
    let mut cmd = Command::new(exe);
    cmd.args(sbx_args(
        sandbox_policy,
        roots,
        settings,
        settings.backend.as_deref() == Some("wsl"),
        shell,
        command,
    ));
    if let Some(env) = env {
        cmd.env_clear().envs(env);
    }
    if let Some(backend) = &settings.backend {
        cmd.env("SBX_BACKEND", backend);
    }
    Ok((cmd, Keep::default()))
}

/// `sbx run` arguments for one command: the policy as flags, then the
/// command — WSL's `sh` under the `wsl` backend, the session's shell
/// otherwise.
#[cfg_attr(not(windows), allow(dead_code))]
fn sbx_args(
    policy: SandboxPolicy,
    roots: &[PathBuf],
    reach: &Settings,
    in_wsl: bool,
    shell: &Shell,
    command: &str,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--best-effort".to_string(),
        "--quiet".to_string(),
        "--fs".to_string(),
        policy.mode.as_str().to_string(),
    ];
    if policy.mode == SandboxMode::WorkspaceWrite {
        for root in roots {
            args.push("--write".to_string());
            args.push(root.to_string_lossy().into_owned());
        }
    }
    match policy.network {
        SandboxNetwork::Deny => args.extend(["--net".to_string(), "deny".to_string()]),
        SandboxNetwork::Allow => {
            for host in &reach.allow_hosts {
                args.push("--allow-host".to_string());
                args.push(host.clone());
            }
            for port in &reach.loopback_ports {
                args.push("--allow-loopback-port".to_string());
                args.push(port.to_string());
            }
        }
    }
    args.push("--".to_string());
    if in_wsl {
        args.extend(["sh".to_string(), "-c".to_string(), command.to_string()]);
    } else {
        args.push(shell.program().to_string_lossy().into_owned());
        args.extend(shell.args(command));
    }
    args
}

/// `sbx.exe` beside this program, else the first on `PATH`.
#[cfg(windows)]
fn sbx_exe() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("sbx.exe")))
        .filter(|p| p.is_file());
    beside.or_else(|| {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("sbx.exe"))
                .find(|p| p.is_file())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxEngine;

    fn incode(mode: SandboxMode, network: SandboxNetwork) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            network,
            named: true,
            engine: SandboxEngine::Sbx,
        }
    }

    /// Settings naming `hosts` and this machine's `ports`.
    fn reach(hosts: &[&str], ports: &[u16]) -> Settings {
        Settings {
            allow_hosts: hosts.iter().map(|h| (*h).to_string()).collect(),
            loopback_ports: ports.to_vec(),
            backend: None,
        }
    }

    /// The mode and the roots carry over; read-only gets no writable root.
    #[test]
    fn the_filesystem_mode_and_roots_carry_over() {
        use incord_sandbox::FsMode;
        let roots = vec![PathBuf::from("/w"), PathBuf::from("/extra")];
        let open = reach(&[], &[]);
        let p = policy(
            incode(SandboxMode::WorkspaceWrite, SandboxNetwork::Allow),
            &roots,
            &open,
        );
        assert_eq!(p.fs.mode, FsMode::WorkspaceWrite);
        assert_eq!(p.fs.write, roots);
        assert!(p.fs.protect_git, "git protection stays on");
        let p = policy(
            incode(SandboxMode::ReadOnly, SandboxNetwork::Allow),
            &roots,
            &open,
        );
        assert_eq!(p.fs.mode, FsMode::ReadOnly);
        assert!(p.fs.write.is_empty());
        let p = policy(
            incode(SandboxMode::Off, SandboxNetwork::Allow),
            &roots,
            &open,
        );
        assert_eq!(p.fs.mode, FsMode::Off);
    }

    /// Deny wins over an allowlist and over this machine's ports; either one
    /// turns an open network into the proxy; neither leaves it open.
    #[test]
    fn the_network_follows_the_switch_and_the_allowlist() {
        use incord_sandbox::NetMode;
        let roots = [PathBuf::from("/w")];
        let write = |network| incode(SandboxMode::WorkspaceWrite, network);
        let hosts = reach(&["crates.io"], &[]);
        let p = policy(
            write(SandboxNetwork::Deny),
            &roots,
            &reach(&["crates.io"], &[5173]),
        );
        assert_eq!(p.network.mode, NetMode::Deny);
        assert!(p.network.loopback_ports.is_empty(), "deny opens no port");
        let p = policy(write(SandboxNetwork::Allow), &roots, &hosts);
        assert_eq!(p.network.mode, NetMode::Proxy);
        assert_eq!(p.network.allow, ["crates.io"]);
        assert!(p.network.loopback_ports.is_empty());
        let p = policy(write(SandboxNetwork::Allow), &roots, &reach(&[], &[5173]));
        assert_eq!(
            p.network.mode,
            NetMode::Proxy,
            "a port alone means the proxy"
        );
        assert!(p.network.allow.is_empty());
        assert_eq!(p.network.loopback_ports, [5173]);
        assert!(!p.network.allow_private, "nothing else private opens");
        let p = policy(write(SandboxNetwork::Allow), &roots, &reach(&[], &[]));
        assert_eq!(p.network.mode, NetMode::Allow);
    }

    #[test]
    fn a_host_list_is_split_on_commas() {
        assert_eq!(
            hosts(" crates.io, ,github.com ,"),
            vec!["crates.io".to_string(), "github.com".to_string()]
        );
    }

    /// What `sbx.exe run` is handed on Windows: the policy as flags, then the
    /// shell — or WSL's `sh` under the wsl backend.
    #[test]
    fn sbx_exe_gets_the_policy_as_flags() {
        let shell = Shell::from_parts(
            PathBuf::from("bash"),
            crate::tools::shell_choice::Flavor::Posix,
        );
        let args = sbx_args(
            incode(SandboxMode::WorkspaceWrite, SandboxNetwork::Allow),
            &[PathBuf::from("/w")],
            &reach(&["crates.io"], &[5173]),
            false,
            &shell,
            "echo hi",
        );
        assert_eq!(
            args,
            [
                "run",
                "--best-effort",
                "--quiet",
                "--fs",
                "workspace-write",
                "--write",
                "/w",
                "--allow-host",
                "crates.io",
                "--allow-loopback-port",
                "5173",
                "--",
                "bash",
                "-c",
                "echo hi"
            ]
        );
        let args = sbx_args(
            incode(SandboxMode::ReadOnly, SandboxNetwork::Deny),
            &[PathBuf::from("/w")],
            &reach(&["crates.io"], &[5173]),
            true,
            &shell,
            "ls",
        );
        assert_eq!(
            args,
            [
                "run",
                "--best-effort",
                "--quiet",
                "--fs",
                "read-only",
                "--net",
                "deny",
                "--",
                "sh",
                "-c",
                "ls"
            ]
        );
    }
}
