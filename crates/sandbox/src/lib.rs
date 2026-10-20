//! Run any command under one policy: what it may write, what it may reach on
//! the network, which environment it gets, and how much it may use.
//!
//! Built for coding agents and the harnesses around them: an agent CLI, a
//! test suite a worker runs, a tool call. The policy is one file
//! ([`policy`]); the same file means the same thing on every platform, and
//! where a platform cannot enforce part of it, [`Sandbox::enforcement`] says
//! so before anything runs. In strict mode (the default) a policy that cannot
//! be enforced in full is refused rather than run with less.
//!
//! # How a command is confined
//!
//! ```text
//! parent (sbx, or your program)          helper (same binary, re-executed)
//!   private temp dir                       setrlimit
//!   allowlisting proxy (network=proxy)     Landlock restrict_self (Linux)
//!   environment computed                   seccomp socket filter (Linux, network)
//!   wall-clock timer, process group        exec(command)  ── confined from here on
//! ```
//!
//! On macOS the helper runs under `sandbox-exec` with a generated Seatbelt
//! profile instead of Landlock. On Windows, which has no `exec`, the parent
//! grants the writable roots to a private per-run SID first, and the helper
//! starts the command under a write-restricted token, in a job object, and
//! stays to supervise it (see [`windows`]). The helper is the calling binary
//! re-executed with a private first argument, so a program that embeds this
//! library must call [`enter_if_requested`] first thing in `main`.
//!
//! # Platforms
//!
//! | | filesystem | network | limits |
//! |---|---|---|---|
//! | Linux | Landlock (5.13+) | Landlock TCP (6.7+) + seccomp | setrlimit + wall |
//! | macOS | Seatbelt | Seatbelt | setrlimit + wall |
//! | Windows | write-restricted token + ACL grant (writes only; no `deny_read`) | after `sbx setup-windows`: an offline account WFP cuts off ([`windows::offline`]); else WSL2 (see [`backend`]) | job object + wall |

pub mod backend;
pub mod env;
pub mod limits;
pub mod policy;
pub mod protect;
pub mod proxy;
pub mod seatbelt;
pub mod server;
pub mod wslpath;

pub mod windows;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod seccomp;
#[cfg(target_os = "linux")]
pub mod wsl;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use policy::{FsMode, Inherit, Limits, NetMode, Policy};

/// The first argument that turns the binary into the helper.
pub const ENTER_ARG: &str = "__sbx-enter";

/// Exit code for "the sandbox refused or failed", as `docker run` uses it.
pub const EXIT_SANDBOX: i32 = 125;
/// Exit code when the wall-clock limit killed the command, as `timeout` uses it.
pub const EXIT_TIMEOUT: i32 = 124;

/// Whether one part of the policy is in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "kebab-case")]
pub enum Status {
    /// The policy asks for nothing here.
    NotRequested,
    /// In force, and how.
    Enforced(String),
    /// Asked for but not in force on this machine, and why.
    NotEnforced(String),
}

impl Status {
    #[must_use]
    pub fn is_gap(&self) -> bool {
        matches!(self, Self::NotEnforced(_))
    }

    fn line(&self, what: &str) -> String {
        match self {
            Self::NotRequested => format!("{what}: not restricted"),
            Self::Enforced(how) => format!("{what}: enforced - {how}"),
            Self::NotEnforced(why) => format!("{what}: NOT ENFORCED - {why}"),
        }
    }
}

/// What this machine will enforce of a policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Enforcement {
    pub filesystem: Status,
    pub network: Status,
    pub limits: Status,
    /// Always enforced: the environment is computed by this process.
    pub environment: Status,
}

impl Enforcement {
    /// The parts asked for but not in force.
    #[must_use]
    pub fn gaps(&self) -> Vec<String> {
        [
            ("filesystem", &self.filesystem),
            ("network", &self.network),
            ("limits", &self.limits),
        ]
        .into_iter()
        .filter(|(_, s)| s.is_gap())
        .map(|(what, s)| s.line(what))
        .collect()
    }

    /// One line per part, for a person.
    #[must_use]
    pub fn report(&self) -> String {
        [
            self.filesystem.line("filesystem"),
            self.network.line("network"),
            self.limits.line("limits"),
            self.environment.line("environment"),
        ]
        .join("\n")
    }
}

/// What the parent hands the helper, as JSON in one argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnterSpec {
    pub fs_mode: FsMode,
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub deny_read: Vec<PathBuf>,
    /// Paths whose writes Seatbelt refuses (macOS); on Linux they are
    /// restored after the run instead ([`protect`]).
    #[serde(default)]
    pub protected: Vec<PathBuf>,
    pub temp: PathBuf,
    pub net_mode: NetMode,
    pub proxy_port: Option<u16>,
    pub limits: Limits,
    pub strict: bool,
    /// The command has its own process group: the helper stays as its
    /// supervisor instead of exec'ing it, so that if the parent dies (even by
    /// SIGKILL, which nothing can forward) the whole group dies with it.
    #[serde(default)]
    pub supervise: bool,
    /// Windows: the run's private SID, granted on its writable roots and
    /// temp directory; the helper restricts the command's token to it.
    #[serde(default)]
    pub sid: Option<String>,
    /// Windows: the parent's process id. The helper watches it and ends the
    /// command's job if the parent dies (there is no parent-death signal).
    #[serde(default)]
    pub parent_pid: Option<u32>,
    /// Windows, network deny: whether this helper starts a second one as the
    /// offline account ([`windows::offline`]), or is that second one.
    #[serde(default)]
    pub offline: Offline,
    /// Windows, network deny: the setup's record, where the first helper
    /// reads the account's password.
    #[serde(default)]
    pub offline_record: Option<PathBuf>,
}

/// The helper's part in running a command as the offline account (Windows,
/// network deny).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Offline {
    /// The command runs as the caller's user.
    #[default]
    No,
    /// This helper starts the second helper as the offline account.
    Launch,
    /// This helper is that second helper, running as the account.
    Inner,
}

/// What the first helper hands the second, in [`INNER_ENV`]
/// (`CreateProcessWithLogonW` takes at most 1024 characters of command line).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct InnerLaunch {
    pub spec: EnterSpec,
    pub program: String,
    pub args: Vec<String>,
}

/// The variable [`InnerLaunch`] travels in; removed before the command
/// starts.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const INNER_ENV: &str = "SBX_INNER_LAUNCH";
/// The helper's argument that says its spec is in [`INNER_ENV`].
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const INNER_ARG: &str = "@env";

/// Call first thing in `main` of any program that embeds this library. When
/// the process was started as the helper it applies the confinement and
/// execs the command, and never returns; otherwise it returns at once.
pub fn enter_if_requested() {
    let mut args = std::env::args_os();
    let _exe = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(ENTER_ARG)) {
        return;
    }
    let rest: Vec<OsString> = args.collect();
    let code = match enter(&rest) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("sbx: {e}");
            e.code
        }
    };
    std::process::exit(code);
}

enum Never {}

#[derive(Debug)]
struct EnterError {
    message: String,
    code: i32,
}

impl std::fmt::Display for EnterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn sandbox_err(message: impl Into<String>) -> EnterError {
    EnterError {
        message: message.into(),
        code: EXIT_SANDBOX,
    }
}

/// The helper: `<spec-json> -- <program> [args...]`, or on Windows the
/// second helper of a network-deny run, [`INNER_ARG`] alone with the rest in
/// [`INNER_ENV`].
fn enter(args: &[OsString]) -> Result<Never, EnterError> {
    #[cfg(windows)]
    if args.len() == 1 && args[0] == INNER_ARG {
        let json = std::env::var(INNER_ENV)
            .map_err(|_| sandbox_err(format!("helper started without {INNER_ENV}")))?;
        // Not for the command, nor anything it starts.
        std::env::remove_var(INNER_ENV);
        let launch: InnerLaunch = serde_json::from_str(&json)
            .map_err(|e| sandbox_err(format!("helper spec does not parse: {e}")))?;
        if launch.spec.offline != Offline::Inner {
            return Err(sandbox_err("helper spec is not an inner one"));
        }
        let args: Vec<OsString> = launch.args.into_iter().map(OsString::from).collect();
        return match windows::enter(&launch.spec, &OsString::from(launch.program), &args) {
            Ok(code) => std::process::exit(code),
            Err((message, code)) => Err(EnterError { message, code }),
        };
    }
    let spec_json = args
        .first()
        .and_then(|a| a.to_str())
        .ok_or_else(|| sandbox_err("helper started without its spec"))?;
    let spec: EnterSpec = serde_json::from_str(spec_json)
        .map_err(|e| sandbox_err(format!("helper spec does not parse: {e}")))?;
    if args.get(1).map(|a| a.as_os_str()) != Some(std::ffi::OsStr::new("--")) {
        return Err(sandbox_err("helper expected `--` before the command"));
    }
    let program = args
        .get(2)
        .ok_or_else(|| sandbox_err("helper started without a command"))?;
    let program_args = &args[3..];
    // Off Unix and Windows nothing in the spec is enforced by the helper: the
    // parent already refused (strict) or reported (best effort) every gap.
    #[cfg(not(any(unix, windows)))]
    let _ = &spec;

    // If the parent dies (SIGKILL included, which nothing can forward), the
    // command must not run on unsupervised. In the caller's process group a
    // parent-death SIGKILL on the command is enough (the caller's group kill
    // reaches the rest); in its own group the supervisor below handles it.
    #[cfg(unix)]
    let parent = {
        // SAFETY: getppid has no preconditions.
        unsafe { libc::getppid() }
    };
    #[cfg(target_os = "linux")]
    {
        let signal = if spec.supervise {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        };
        // SAFETY: prctl with integer arguments.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, signal);
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: getppid has no preconditions.
        if unsafe { libc::getppid() } != parent {
            return Err(sandbox_err("the parent exited before the command started"));
        }
    }

    #[cfg(unix)]
    if let Err(e) = limits::apply(&spec.limits) {
        if spec.strict {
            return Err(sandbox_err(e));
        }
        eprintln!("sbx: limit not applied (best effort): {e}");
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = linux::restrict(&spec) {
        if spec.strict {
            return Err(sandbox_err(format!("Landlock: {e}")));
        }
        eprintln!("sbx: Landlock not applied (best effort): {e}");
    }
    // After Landlock, before exec: refuse terminal input injection, vsock and
    // io_uring whenever anything is confined, and under deny/proxy also the
    // sockets Landlock does not see (UDP, raw, ...). See `seccomp`. Only the
    // socket layer is part of the network policy, so only it is fatal in
    // strict mode; with the filesystem alone, an architecture without a
    // filter is skipped quietly and a kernel refusal is reported.
    #[cfg(target_os = "linux")]
    {
        let network = spec.net_mode != NetMode::Allow;
        let fs = spec.fs_mode != FsMode::Off && seccomp::available();
        if network || fs {
            if let Err(e) = seccomp::install(network) {
                if spec.strict && network {
                    return Err(sandbox_err(format!("seccomp: {e}")));
                }
                eprintln!("sbx: seccomp not applied (best effort): {e}");
            }
        }
    }

    #[cfg(unix)]
    if spec.supervise {
        supervise(program, program_args, parent);
    }
    // No exec on Windows: the helper starts the command confined, in a job,
    // and stays to pass its exit code on.
    #[cfg(windows)]
    {
        match windows::enter(&spec, program, program_args) {
            Ok(code) => std::process::exit(code),
            Err((message, code)) => Err(EnterError { message, code }),
        }
    }
    #[cfg(not(windows))]
    exec(program, program_args)
}

#[cfg(unix)]
static PARENT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Kill the whole process group if the parent is gone. Async-signal-safe.
#[cfg(unix)]
extern "C" fn kill_group_if_orphaned(_sig: libc::c_int) {
    // SAFETY: getppid, getpgrp and kill are async-signal-safe.
    unsafe {
        if libc::getppid() != PARENT.load(std::sync::atomic::Ordering::Relaxed) {
            libc::kill(-libc::getpgrp(), libc::SIGKILL);
        }
    }
}

/// Stay in the command's process group as its supervisor: run it, pass its
/// exit status on, and kill the group if the parent disappears. Signals the
/// parent forwards to the group (SIGINT, SIGTERM, ...) reach the command
/// directly; here they only trigger the orphan check, so the command can
/// still shut down gracefully while the parent is alive.
#[cfg(unix)]
fn supervise(program: &OsString, args: &[OsString], parent: libc::pid_t) -> Never {
    PARENT.store(parent, std::sync::atomic::Ordering::Relaxed);
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: a handler that only calls async-signal-safe functions. A
        // handler (not SIG_IGN) is reset to the default by exec, so the
        // command does not inherit it.
        unsafe {
            libc::signal(
                sig,
                kill_group_if_orphaned as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
    // Where there is no parent-death signal (macOS), notice by polling.
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        kill_group_if_orphaned(0);
    });
    let code = match Command::new(program).args(args).status() {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            status
                .code()
                .or_else(|| status.signal().map(|s| 128 + s))
                .unwrap_or(1)
        }
        Err(e) => {
            eprintln!("sbx: cannot run {}: {e}", Path::new(program).display());
            if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            }
        }
    };
    std::process::exit(code);
}

#[cfg(unix)]
fn exec(program: &OsString, args: &[OsString]) -> Result<Never, EnterError> {
    use std::os::unix::process::CommandExt;
    let err = Command::new(program).args(args).exec();
    Err(EnterError {
        code: if err.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        },
        message: format!("cannot run {}: {err}", Path::new(program).display()),
    })
}

#[cfg(not(any(unix, windows)))]
fn exec(program: &OsString, args: &[OsString]) -> Result<Never, EnterError> {
    // No exec here: run it and pass its exit code on.
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| EnterError {
            code: if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            },
            message: format!("cannot run {}: {e}", Path::new(program).display()),
        })?;
    std::process::exit(status.code().unwrap_or(1));
}

/// A policy ready to run commands.
#[derive(Debug, Clone)]
pub struct Sandbox {
    policy: Policy,
    helper: Option<PathBuf>,
    strict: bool,
    /// The environment the policy's `[env]` rules start from. Default: this
    /// process's own.
    base_env: Option<Vec<(OsString, OsString)>>,
}

/// Everything a running command needs kept alive: the proxy and the private
/// temp directory. Dropped after the command exits.
pub struct Guard {
    proxy: Option<proxy::Proxy>,
    /// Windows: the run's ACL grant on its roots and temp directory, revoked
    /// when dropped. Declared before `temp` so it drops first, while the
    /// temp directory still exists to be revoked.
    #[cfg(windows)]
    _lease: Option<windows::acl::Lease>,
    temp: TempDir,
    pub enforcement: Enforcement,
    protected: Option<protect::Snapshot>,
    /// Whether the command runs in its own process group (only with a
    /// wall-clock limit and a non-terminal stdin); kill `-pid` to stop it all.
    pub own_group: bool,
}

impl Guard {
    /// The proxy's decisions so far (empty without a proxy).
    #[must_use]
    pub fn decisions(&self) -> Vec<proxy::Decision> {
        self.proxy
            .as_ref()
            .map(proxy::Proxy::decisions)
            .unwrap_or_default()
    }

    /// Put back whatever the command changed under the protected paths
    /// (`fs.protect_git`), and say what. Call once the command has exited.
    #[must_use]
    pub fn restore_protected(&self) -> Vec<String> {
        self.protected
            .as_ref()
            .map(protect::Snapshot::restore)
            .unwrap_or_default()
    }

    /// The command's private temp directory.
    #[must_use]
    pub fn temp_dir(&self) -> &Path {
        &self.temp.path
    }
}

/// How a command ended.
#[derive(Debug)]
pub struct Outcome {
    pub status: ExitStatus,
    pub timed_out: bool,
    pub decisions: Vec<proxy::Decision>,
    /// Protected paths the command changed, which were put back.
    pub restored: Vec<String>,
}

impl Outcome {
    /// The exit code a shell would report: the command's, 128 + signal, or
    /// [`EXIT_TIMEOUT`].
    #[must_use]
    pub fn code(&self) -> i32 {
        if self.timed_out {
            return EXIT_TIMEOUT;
        }
        if let Some(c) = self.status.code() {
            return c;
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = self.status.signal() {
                return 128 + sig;
            }
        }
        1
    }
}

impl Sandbox {
    #[must_use]
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            helper: None,
            strict: true,
            base_env: None,
        }
    }

    /// Start the command's environment from `vars` instead of this process's
    /// own, for a caller that filters it first. The policy's `[env]` rules
    /// still apply on top, and the run's own variables (the temp directory,
    /// the proxy) are still added.
    #[must_use]
    pub fn inherit_from(mut self, vars: Vec<(OsString, OsString)>) -> Self {
        self.base_env = Some(vars);
        self
    }

    /// The binary re-executed as the helper. Default: the current executable,
    /// which must then call [`enter_if_requested`].
    #[must_use]
    pub fn helper(mut self, exe: impl Into<PathBuf>) -> Self {
        self.helper = Some(exe.into());
        self
    }

    /// Strict (the default): refuse to run when any part of the policy cannot
    /// be enforced. Off: run with what can be, and say what could not.
    #[must_use]
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// What this machine will enforce of the policy.
    #[must_use]
    pub fn enforcement(&self) -> Enforcement {
        enforcement(&self.policy)
    }

    /// The confined [`Command`] for `program args`, started in `cwd`, and the
    /// guard to keep alive until it exits. The caller sets the pipes and
    /// spawns it; [`Self::spawn`] does all of that.
    ///
    /// # Errors
    /// In strict mode when part of the policy cannot be enforced; when a
    /// writable root is missing; when the proxy or temp dir cannot be made.
    pub fn command(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<(Command, Guard), String> {
        let enforcement = self.enforcement();
        let gaps = enforcement.gaps();
        if self.strict && !gaps.is_empty() {
            return Err(format!(
                "this policy cannot be fully enforced here (run with --best-effort to accept):\n  {}",
                gaps.join("\n  ")
            ));
        }
        let roots = self.policy.resolved_roots(cwd)?;
        #[allow(unused_mut)]
        let mut deny_read = self.policy.resolved_deny_read(cwd, &roots)?;
        // Inside WSL2 a Windows program started from the sandbox would run
        // outside it altogether: hide the interop interpreter and sockets.
        #[cfg(target_os = "linux")]
        let block_interop = self.policy.fs.mode != FsMode::Off
            && self.policy.fs.block_wsl_interop
            && wsl::detected();
        #[cfg(not(target_os = "linux"))]
        let block_interop = false;
        #[cfg(target_os = "linux")]
        if block_interop {
            for p in wsl::interop_paths() {
                if !deny_read.contains(&p) && !roots.iter().any(|r| p.starts_with(r)) {
                    deny_read.push(p);
                }
            }
        }
        let protected = (self.policy.fs.mode == FsMode::WorkspaceWrite
            && self.policy.fs.protect_git)
            .then(|| protect::Snapshot::take(protect::git_paths(&roots)));
        let temp = TempDir::new()?;
        // Windows has no process groups: the helper's job object does that.
        let own_group =
            cfg!(unix) && self.policy.limits.wall_secs.is_some() && !stdin_is_terminal();
        let helper = match &self.helper {
            Some(h) => h.clone(),
            None => std::env::current_exe().map_err(|e| format!("cannot name the helper: {e}"))?,
        };
        // Windows, network deny or proxy: the command runs as the offline
        // account, when `sbx setup-windows` has made it (otherwise the gap
        // was refused above, or accepted with --best-effort).
        #[cfg(windows)]
        let mut offline = if self.policy.network.mode == NetMode::Allow {
            None
        } else {
            windows::setup::ready().as_ref().ok().cloned()
        };
        // Windows: grant the writable roots and the temp directory to a
        // private SID for this run, before the helper restricts the
        // command's token to it; under network deny to the offline account
        // as well, with the current folder and the helper program to read.
        // A grant that fails refuses the run in strict mode; best effort
        // runs it with the writes unconfined (and the network open), and
        // says so.
        #[cfg(windows)]
        let lease = if self.policy.fs.mode == FsMode::Off && offline.is_none() {
            None
        } else {
            let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            let mut grant = roots.clone();
            if self.policy.fs.mode == FsMode::Off {
                // Not confined, but another account: give it the folder it
                // was started in, so it can work there at all.
                grant.push(real(cwd));
            }
            grant.push(temp.path.clone());
            let account = offline.as_ref().map(|ready| {
                let reads = windows::offline_reads(&grant, &real(cwd), &real(&helper));
                windows::acl::OfflineGrant {
                    user: ready.sid.clone(),
                    // PowerShell lists the current folder's parent to start
                    // in it at all.
                    parents: windows::offline_parents(&grant, &reads, &real(cwd)),
                    reads,
                }
            });
            match windows::acl::Lease::acquire(&grant, account) {
                Ok(lease) => Some(lease),
                Err(e) if self.strict => return Err(e),
                Err(e) => {
                    eprintln!("sbx: writes not confined and network not denied (best effort): {e}");
                    offline = None;
                    None
                }
            }
        };

        let proxy = if self.policy.network.mode == NetMode::Proxy {
            let allowlist = proxy::Allowlist::new(
                &self.policy.network.allow,
                self.policy.network.allow_private,
            )?;
            let upstream = proxy::Upstream::from_env(|k| std::env::var(k).ok());
            // As the offline account, the command may only reach the
            // loopback ports the setup's filter permits: listen on one.
            #[cfg(windows)]
            let ports = offline.as_ref().map(|r| r.proxy_ports);
            #[cfg(not(windows))]
            let ports: Option<(u16, u16)> = None;
            let started = match ports {
                Some(range) => proxy::Proxy::start_in(allowlist, upstream, range),
                None => proxy::Proxy::start(allowlist, upstream),
            };
            Some(started.map_err(|e| format!("proxy: {e}"))?)
        } else {
            None
        };

        let mut extra: Vec<(String, String)> = Vec::new();
        let temp_str = temp.path.to_string_lossy().into_owned();
        for k in ["TMPDIR", "TMP", "TEMP"] {
            extra.push((k.into(), temp_str.clone()));
        }
        if let Some(p) = &proxy {
            extra.extend(p.client_env());
        }
        let inherited = match &self.base_env {
            Some(vars) => vars.clone(),
            None => std::env::vars_os().collect(),
        };
        let mut environment = env::compute(&self.policy.env, inherited, &extra);
        if block_interop {
            environment.retain(|(k, _)| k != "WSL_INTEROP");
        }

        let spec = EnterSpec {
            fs_mode: self.policy.fs.mode,
            roots: roots.clone(),
            deny_read,
            protected: protected
                .as_ref()
                .map(|s| s.paths().to_vec())
                .unwrap_or_default(),
            temp: temp.path.clone(),
            net_mode: self.policy.network.mode,
            proxy_port: proxy.as_ref().map(proxy::Proxy::port),
            limits: self.policy.limits.clone(),
            strict: self.strict,
            supervise: own_group,
            #[cfg(windows)]
            sid: lease.as_ref().map(|l| l.sid().to_string()),
            #[cfg(not(windows))]
            sid: None,
            parent_pid: Some(std::process::id()),
            #[cfg(windows)]
            offline: if offline.is_some() {
                Offline::Launch
            } else {
                Offline::No
            },
            #[cfg(not(windows))]
            offline: Offline::No,
            #[cfg(windows)]
            offline_record: offline.as_ref().map(|r| r.record.clone()),
            #[cfg(not(windows))]
            offline_record: None,
        };
        let spec_json = serde_json::to_string(&spec).map_err(|e| e.to_string())?;

        let mut cmd = wrap_platform(&helper, &spec, &roots);
        cmd.arg(ENTER_ARG)
            .arg(spec_json)
            .arg("--")
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(environment);
        if own_group {
            set_own_process_group(&mut cmd);
        }

        Ok((
            cmd,
            Guard {
                proxy,
                #[cfg(windows)]
                _lease: lease,
                temp,
                enforcement,
                protected,
                own_group,
            },
        ))
    }

    /// Run `program args` in `cwd` with the caller's stdio, enforcing the
    /// wall-clock limit, and wait for it.
    ///
    /// # Errors
    /// As [`Self::command`], or when the command cannot be started.
    pub fn run(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Outcome, String> {
        let (mut cmd, guard) = self.command(program, args, cwd)?;
        let wall = self.policy.limits.wall_secs.map(Duration::from_secs);
        let own_group = guard.own_group;
        let child = cmd
            .spawn()
            .map_err(|e| format!("cannot start the helper: {e}"))?;
        if own_group {
            forward_signals_to_group(child.id());
        }
        let (status, timed_out) = wait(child, wall, own_group)?;
        Ok(Outcome {
            status,
            timed_out,
            decisions: guard.decisions(),
            restored: guard.restore_protected(),
        })
    }
}

/// A command's result with what it printed, kept up to a limit.
#[derive(Debug)]
pub struct Captured {
    pub outcome: Outcome,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Bytes printed in all, including what was cut.
    pub stdout_total: u64,
    pub stderr_total: u64,
}

impl Sandbox {
    /// Run `program args` in `cwd` with `stdin` fed in, keep the first
    /// `max_output` bytes of each output stream, and wait for it.
    ///
    /// For a server running commands on someone else's behalf: no signal is
    /// forwarded (the server's own signals are its own), and when the command
    /// has its own process group (a wall-clock limit is set) the whole group
    /// is killed once the command ends, so nothing it left running in the
    /// background outlives the run.
    ///
    /// # Errors
    /// As [`Self::command`], or when the command cannot be started.
    pub fn run_captured(
        &self,
        program: impl AsRef<std::ffi::OsStr>,
        args: &[OsString],
        cwd: &Path,
        stdin: Option<&[u8]>,
        max_output: usize,
    ) -> Result<Captured, String> {
        use std::process::Stdio;
        let (mut cmd, guard) = self.command(program, args, cwd)?;
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let wall = self.policy.limits.wall_secs.map(Duration::from_secs);
        let own_group = guard.own_group;
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start the helper: {e}"))?;
        let out = child.stdout.take().map(|s| drain(s, max_output));
        let err = child.stderr.take().map(|s| drain(s, max_output));
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let input = input.to_vec();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = pipe.write_all(&input);
            });
        }
        let pid = child.id();
        let (status, timed_out) = wait(child, wall, own_group)?;
        if own_group {
            kill_group(pid);
        }
        let collect = |h: Option<std::thread::JoinHandle<(Vec<u8>, u64)>>| {
            h.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        let (stdout, stdout_total) = collect(out);
        let (stderr, stderr_total) = collect(err);
        Ok(Captured {
            outcome: Outcome {
                status,
                timed_out,
                decisions: guard.decisions(),
                restored: guard.restore_protected(),
            },
            stdout,
            stderr,
            stdout_total,
            stderr_total,
        })
    }
}

/// Read a stream to its end on its own thread, keeping the first `max` bytes
/// and counting the rest.
fn drain(
    mut stream: impl std::io::Read + Send + 'static,
    max: usize,
) -> std::thread::JoinHandle<(Vec<u8>, u64)> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut total = 0u64;
        let mut buf = [0u8; 8192];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
            let room = max.saturating_sub(kept.len());
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
        (kept, total)
    })
}

/// Kill a process group the sandbox created, whatever is left in it.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: signalling a process group this crate created.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// `path` as commands expect to see it. On Windows `canonicalize` returns
/// `\\?\C:\...`, which `cmd.exe` refuses as a working folder and which
/// does not accept `/`; this gives back `C:\...`. Elsewhere, unchanged.
#[must_use]
pub fn plain_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(windows::strip_verbatim(&path.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        path
    }
}

/// The enforcement of `policy` on this machine.
#[must_use]
pub fn enforcement(policy: &Policy) -> Enforcement {
    Enforcement {
        filesystem: fs_status(&policy.fs),
        network: net_status(policy.network.mode),
        limits: limits::status(&policy.limits),
        environment: Status::Enforced(environment_summary(&policy.env)),
    }
}

fn environment_summary(e: &policy::Env) -> String {
    let mut parts = vec![match e.inherit {
        Inherit::All => "inherited".to_string(),
        Inherit::None => "only essentials and `pass`".to_string(),
    }];
    if e.strip_agent_session {
        parts.push("agent session stripped".into());
    }
    if e.strip_git_redirects {
        parts.push("git redirects stripped".into());
    }
    if !e.remove.is_empty() {
        parts.push(format!("{} pattern(s) removed", e.remove.len()));
    }
    parts.push("private TMPDIR".into());
    parts.join(", ")
}

#[cfg(target_os = "linux")]
fn fs_status(fs: &policy::Fs) -> Status {
    match linux::fs_status(fs.mode) {
        Status::Enforced(how) if wsl::detected() => Status::Enforced(format!(
            "{how}; WSL: starting Windows programs is blocked (interop interpreter and \
             sockets hidden, WSL_INTEROP removed, vsock refused)"
        )),
        other => other,
    }
}

#[cfg(target_os = "macos")]
fn fs_status(fs: &policy::Fs) -> Status {
    match fs.mode {
        FsMode::Off => Status::NotRequested,
        _ if Path::new(seatbelt::SANDBOX_EXEC).exists() => {
            Status::Enforced("Seatbelt (sandbox-exec)".into())
        }
        _ => Status::NotEnforced(format!("{} was not found", seatbelt::SANDBOX_EXEC)),
    }
}

#[cfg(windows)]
fn fs_status(fs: &policy::Fs) -> Status {
    windows::fs_status(fs)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn fs_status(fs: &policy::Fs) -> Status {
    match fs.mode {
        FsMode::Off => Status::NotRequested,
        _ => {
            Status::NotEnforced("no filesystem sandbox is implemented for this platform yet".into())
        }
    }
}

#[cfg(target_os = "linux")]
fn net_status(mode: NetMode) -> Status {
    linux::net_status(mode)
}

#[cfg(target_os = "macos")]
fn net_status(mode: NetMode) -> Status {
    match mode {
        NetMode::Allow => Status::NotRequested,
        _ if Path::new(seatbelt::SANDBOX_EXEC).exists() => Status::Enforced(match mode {
            NetMode::Proxy => "Seatbelt: outbound only to the proxy (and Unix sockets)".into(),
            _ => "Seatbelt: network denied (Unix sockets allowed)".into(),
        }),
        _ => Status::NotEnforced(format!("{} was not found", seatbelt::SANDBOX_EXEC)),
    }
}

#[cfg(windows)]
fn net_status(mode: NetMode) -> Status {
    if mode == NetMode::Allow {
        // Without asking whether the offline account is set up.
        return Status::NotRequested;
    }
    windows::net_status(mode, windows::setup::ready())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn net_status(mode: NetMode) -> Status {
    match mode {
        NetMode::Allow => Status::NotRequested,
        _ => Status::NotEnforced("no network sandbox is implemented for this platform yet".into()),
    }
}

/// The helper invocation, under `sandbox-exec` on macOS.
#[cfg(target_os = "macos")]
fn wrap_platform(helper: &Path, spec: &EnterSpec, roots: &[PathBuf]) -> Command {
    if spec.fs_mode == FsMode::Off && spec.net_mode == NetMode::Allow {
        return Command::new(helper);
    }
    let mut cmd = Command::new(seatbelt::SANDBOX_EXEC);
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    cmd.arg("-D")
        .arg(format!("TMP={}", real(&spec.temp).display()));
    for (i, root) in roots.iter().enumerate() {
        cmd.arg("-D").arg(format!("WS{i}={}", real(root).display()));
    }
    for (i, hidden) in spec.deny_read.iter().enumerate() {
        cmd.arg("-D")
            .arg(format!("DR{i}={}", real(hidden).display()));
    }
    for (i, kept) in spec.protected.iter().enumerate() {
        cmd.arg("-D").arg(format!("PR{i}={}", real(kept).display()));
    }
    cmd.arg("-p").arg(seatbelt::profile(
        spec.fs_mode,
        roots.len(),
        spec.deny_read.len(),
        spec.protected.len(),
        spec.net_mode,
        spec.proxy_port,
    ));
    cmd.arg(helper);
    cmd
}

#[cfg(not(target_os = "macos"))]
fn wrap_platform(helper: &Path, _spec: &EnterSpec, _roots: &[PathBuf]) -> Command {
    Command::new(helper)
}

/// A command gets its own process group only when it has a wall-clock limit,
/// so the limit can kill everything it started; [`Sandbox::run`] then
/// forwards the termination signals it receives, and the supervisor in the
/// helper kills the group if the parent dies. Skipped when stdin is a
/// terminal: a background process group cannot read the terminal, so an
/// interactive command keeps the caller's group (the limit then kills the
/// command itself).
fn stdin_is_terminal() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty only inspects the descriptor.
        unsafe { libc::isatty(0) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn set_own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

#[cfg(unix)]
static FORWARD_TO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn forward(sig: libc::c_int) {
    let pgid = FORWARD_TO.load(std::sync::atomic::Ordering::Relaxed);
    if pgid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
}

/// Pass SIGINT, SIGTERM, SIGHUP and SIGQUIT on to the command's process
/// group, so a caller that stops sbx stops what sbx started.
fn forward_signals_to_group(pid: u32) {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else { return };
        FORWARD_TO.store(pid, std::sync::atomic::Ordering::Relaxed);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            // SAFETY: installing a handler that only calls kill.
            unsafe {
                libc::signal(
                    sig,
                    forward as extern "C" fn(libc::c_int) as libc::sighandler_t,
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn wait(
    mut child: Child,
    wall: Option<Duration>,
    own_group: bool,
) -> Result<(ExitStatus, bool), String> {
    let Some(wall) = wall else {
        return child.wait().map(|s| (s, false)).map_err(|e| e.to_string());
    };
    let deadline = Instant::now() + wall;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok((status, false));
        }
        if Instant::now() >= deadline {
            kill(&mut child, own_group);
            let status = child.wait().map_err(|e| e.to_string())?;
            return Ok((status, true));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn kill(child: &mut Child, own_group: bool) {
    #[cfg(unix)]
    if own_group {
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: signalling a process group we created.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            return;
        }
    }
    let _ = own_group;
    let _ = child.kill();
}

/// A private temp directory, removed when dropped.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Result<Self, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let name = format!(
                "sbx-{}-{}-{nanos:x}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = base.join(name);
            match create_private_dir(&path) {
                Ok(()) => {
                    // Plain, not `\\?\C:\...`: many tools cannot use that as TEMP.
                    let path = plain_path(std::fs::canonicalize(&path).unwrap_or(path));
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot make a temp directory: {e}")),
            }
        }
        Err("cannot make a unique temp directory".into())
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> EnterSpec {
        EnterSpec {
            fs_mode: FsMode::WorkspaceWrite,
            roots: vec![PathBuf::from("/w")],
            deny_read: Vec::new(),
            protected: Vec::new(),
            temp: PathBuf::from("/t"),
            net_mode: NetMode::Deny,
            proxy_port: None,
            limits: Limits::default(),
            strict: true,
            supervise: false,
            sid: None,
            parent_pid: Some(7),
            offline: Offline::Launch,
            offline_record: Some(PathBuf::from(r"C:\ProgramData\sbx\offline.conf")),
        }
    }

    #[test]
    fn a_spec_without_the_offline_fields_still_parses() {
        let mut json: serde_json::Value = serde_json::to_value(spec()).expect("json");
        assert_eq!(json["offline"], "launch");
        let map = json.as_object_mut().expect("object");
        map.remove("offline");
        map.remove("offline_record");
        let back: EnterSpec = serde_json::from_value(json).expect("parses");
        assert_eq!(back.offline, Offline::No);
        assert_eq!(back.offline_record, None);
    }

    #[test]
    fn the_second_helper_gets_its_spec_and_command_whole() {
        let mut inner = spec();
        inner.offline = Offline::Inner;
        let launch = InnerLaunch {
            spec: inner,
            program: "cmd".into(),
            args: vec!["/c".into(), "echo \"a b\" & exit 3".into(), String::new()],
        };
        let json = serde_json::to_string(&launch).expect("json");
        let back: InnerLaunch = serde_json::from_str(&json).expect("parses");
        assert_eq!(back.spec.offline, Offline::Inner);
        assert_eq!(back.program, "cmd");
        assert_eq!(back.args, launch.args);
        assert_eq!(back.spec.parent_pid, Some(7));
        assert_eq!(INNER_ARG, "@env");
        assert!(!INNER_ENV.is_empty());
    }
}
