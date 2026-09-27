//! The OS sandbox for a model's shell commands.
//!
//! # What it is for
//!
//! The `Bash` tool runs a command the MODEL chose. The guards catch a direct
//! textual route to a protected file, and the permission mode decides whether
//! the tool may run at all — but neither stops a command that writes outside
//! the workspace through a variable, a glob or a script it wrote first. This
//! module hands the command to the operating system's own confinement so the
//! write is PREVENTED, not merely noticed.
//!
//! # The modes
//!
//! From `admin.json` `sandbox`, or, unset, from the session's permission mode:
//!
//! | permission mode | default sandbox |
//! |---|---|
//! | read-only, plan | `read-only` |
//! | prompt, workspace-write, acceptEdits | `workspace-write` |
//! | danger-full-access, allow | `off` |
//!
//! - **`read-only`** — the command may read anywhere and write nowhere, bar
//!   the null sink and the terminal.
//! - **`workspace-write`** — it may read anywhere and write only inside the
//!   session's roots (the workspace and any `--add-dir`) and the temp
//!   directory.
//! - **`off`** — the command runs unconfined, as it did before this existed.
//!
//! # The network — a separate, opt-in switch
//!
//! `admin.json` `sandbox_network: "allow" | "deny"`, or [`SANDBOX_NETWORK_ENV`].
//! The default is `allow`, which is the behaviour before the switch existed.
//! `deny` is independent of the filesystem mode (it applies under `off` too)
//! and each OS does what it soundly can, [`Sandbox::note`] saying which:
//!
//! - **Linux** — Landlock ABI v4 (Linux 6.7+): every TCP `bind` and `connect`
//!   is refused, loopback included (Landlock matches ports, not addresses). UDP
//!   is not covered. An older kernel is reported as NOT enforced.
//! - **macOS** — the Seatbelt profile denies `network*` and re-allows loopback
//!   (so a local dev server or a test suite still works) and Unix sockets.
//! - **Windows** — NOT enforced, and said so: only an AppContainer can block a
//!   command's network, and an AppContainer cannot read the workspace or the
//!   user's toolchains unless every one of them is granted to it.
//!
//! # How, per OS — and where it is honest about the limit
//!
//! - **Linux** — the kernel's own Landlock, applied by `incode --sandbox-exec`
//!   ([`exec`]) to itself before it execs the shell, so the restriction is in
//!   force for the command and everything it starts. Unprivileged; sound.
//! - **macOS** — `sandbox-exec` with a generated Seatbelt profile. Sound; the
//!   tool is marked deprecated by Apple but present and working.
//! - **Windows** — see [`windows`]. Read-only is enforced with a write-restricted
//!   token. Workspace-write is enforced only when the operator NAMED it
//!   (`sandbox: "workspace-write"` or `INCORD_SANDBOX`), because it adds one ACE
//!   for a private per-session SID to each root's ACL for the session
//!   (`windows::acl`); left to the permission-mode default it stays off, with
//!   the reason stated, rather than pretended.
//!
//! When a platform cannot enforce a mode soundly, [`Sandbox::wrap`] returns the
//! command unconfined and [`Sandbox::note`] says so — the caller traces it, so
//! a session that ran unconfined is never mistaken for one that was confined.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::tools::shell_choice::Shell;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
pub mod windows;

/// Names the sandbox mode, ahead of the settings and the permission-mode
/// default — for a run that wants to pin it without editing `admin.json`.
pub const SANDBOX_ENV: &str = "INCORD_SANDBOX";

/// Names the network setting (`allow` or `deny`), ahead of `admin.json`
/// `sandbox_network`.
pub const SANDBOX_NETWORK_ENV: &str = "INCORD_SANDBOX_NETWORK";

/// Whether a sandboxed command may use the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxNetwork {
    /// Unrestricted — the behaviour before the switch existed, and the default.
    #[default]
    Allow,
    /// Refused where the OS can do it soundly; reported where it cannot.
    Deny,
}

impl SandboxNetwork {
    /// The word the setting and the trace use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    /// The setting named, or `None` for a word nobody uses.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// Everything the operator decided about the sandbox, resolved once for a
/// session and handed to its toolbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// What a command may write.
    pub mode: SandboxMode,
    /// Whether it may use the network.
    pub network: SandboxNetwork,
    /// Whether the operator NAMED the mode (settings or environment) rather
    /// than leaving it to the permission mode. Windows applies workspace-write
    /// only when named: it changes the roots' ACL for the session, which is a
    /// cost the operator has to have asked for.
    pub named: bool,
}

impl SandboxPolicy {
    /// No sandbox at all: every command unconfined, the network open.
    pub const OFF: Self = Self {
        mode: SandboxMode::Off,
        network: SandboxNetwork::Allow,
        named: false,
    };
}

/// What a sandboxed command may do to the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Unconfined — the behaviour before the sandbox existed.
    Off,
    /// Read anywhere; write only inside the roots and the temp directory.
    WorkspaceWrite,
    /// Read anywhere; write nowhere but the null sink and the terminal.
    ReadOnly,
}

impl SandboxMode {
    /// The word the setting and the trace use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceWrite => "workspace-write",
            Self::ReadOnly => "read-only",
        }
    }

    /// The mode named in a setting, or `None` for one nobody uses.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "read-only" => Some(Self::ReadOnly),
            _ => None,
        }
    }

    /// The sandbox a permission mode implies when the operator set none: as
    /// tight as the mode, never tighter — a writing session writes to its
    /// workspace, a looking one writes nothing, and the two full-access modes
    /// opt out.
    #[must_use]
    pub fn for_permission_mode(mode: crate::permissions::PermissionMode) -> Self {
        use crate::permissions::PermissionMode as P;
        match mode {
            P::ReadOnly => Self::ReadOnly,
            P::Prompt | P::WorkspaceWrite => Self::WorkspaceWrite,
            P::DangerFullAccess | P::Allow => Self::Off,
        }
    }
}

/// The sandbox in force for a session's shell commands: the policy and the
/// directories a `workspace-write` command may write in.
#[derive(Debug, Clone)]
pub struct Sandbox {
    policy: SandboxPolicy,
    /// The workspace first, then any added directory. Made absolute by the
    /// caller, which is where they are already canonical.
    roots: Vec<PathBuf>,
    /// Windows workspace-write only: the session's private SID and temp
    /// directory, once [`Self::prepared`] has granted the roots to that SID.
    #[cfg(windows)]
    grant: Option<windows::acl::Grant>,
}

/// What a session's sandbox holds for as long as the session lasts. Only
/// Windows workspace-write holds anything — the ACE granted on each root to the
/// session's private SID ([`windows::acl`]) — and dropping this revokes it. Owned
/// by the toolbox, so the grant ends when the session does.
#[derive(Debug, Default)]
pub struct SessionGrant {
    #[cfg(windows)]
    lease: std::sync::Mutex<Option<windows::acl::Lease>>,
}

impl Sandbox {
    /// A sandbox under `policy` over `roots` (workspace first).
    #[must_use]
    pub fn new(policy: SandboxPolicy, roots: Vec<PathBuf>) -> Self {
        Self {
            policy,
            roots,
            #[cfg(windows)]
            grant: None,
        }
    }

    /// No sandbox: every command runs unconfined.
    #[must_use]
    pub fn off() -> Self {
        Self::new(SandboxPolicy::OFF, Vec::new())
    }

    /// The filesystem mode in force.
    #[must_use]
    pub fn mode(&self) -> SandboxMode {
        self.policy.mode
    }

    /// Whether this sandbox does anything to a command.
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.policy.mode == SandboxMode::Off && self.policy.network == SandboxNetwork::Allow
    }

    /// This sandbox made ready for a command: whatever the session must hold
    /// for it is taken from `session` first. Only Windows workspace-write needs
    /// anything — its roots granted to the session's SID, once per root per
    /// session — and everything else passes through unchanged.
    ///
    /// # Errors
    ///
    /// When the grant could not be made. The command must then NOT run: a
    /// sandbox the operator named, silently not applied, is the failure this
    /// module exists to prevent.
    pub fn prepared(self, session: &SessionGrant) -> Result<Self, String> {
        #[cfg(windows)]
        {
            let mut this = self;
            if windows::needs_grant(this.policy) {
                this.grant = Some(session.hold(&this.roots)?);
            }
            Ok(this)
        }
        #[cfg(not(windows))]
        {
            let _ = session;
            Ok(self)
        }
    }

    /// The command to spawn for one shell `command`, confined to this
    /// sandbox where the platform can, and left unconfined where it cannot —
    /// [`Self::note`] says which. The caller still sets the working
    /// directory, the environment and the pipes.
    ///
    /// `shell` is the Bash tool's shell ([`crate::tools::shell_choice`]) —
    /// used unchanged when the sandbox is off or cannot apply.
    #[must_use]
    pub(crate) fn wrap(&self, shell: &Shell, command: &str) -> Command {
        if self.is_off() {
            return plain(shell, command);
        }
        self.wrap_confined(shell, command)
            .unwrap_or_else(|| plain(shell, command))
    }

    /// One line for the trace and for `/doctor`: what the sandbox is doing to
    /// this session's commands, and where it cannot — the filesystem first,
    /// then the network when `deny` was asked for.
    #[must_use]
    pub fn note(&self) -> String {
        let mode = self.policy.mode;
        let files = if mode == SandboxMode::Off {
            "sandbox off: shell commands run unconfined".to_string()
        } else if self.applies() {
            format!(
                "sandbox {}: shell commands are confined by {}",
                mode.as_str(),
                per_os().mechanism
            )
        } else {
            format!(
                "sandbox {} requested but NOT applied on this platform: {}",
                mode.as_str(),
                per_os().cannot
            )
        };
        if self.policy.network == SandboxNetwork::Allow {
            return files;
        }
        match network_support() {
            Ok(how) => format!("{files}; network denied by {how}"),
            Err(why) => format!("{files}; network deny requested but NOT enforced: {why}"),
        }
    }

    /// Whether a POSIX shell's runtime can start under this sandbox. False
    /// only on Windows when the write-restricted token applies (read-only, or
    /// a named workspace-write): Git Bash's msys runtime maps a per-user
    /// shared section the token may not map writable, and every msys program
    /// dies at once with `CreateFileMapping ..., Win32 error 5` (measured).
    /// Decided from the policy alone, so it can be asked before any grant.
    #[must_use]
    pub fn posix_shell_runs(&self) -> bool {
        #[cfg(windows)]
        {
            !windows::applies(self.policy, windows::needs_grant(self.policy))
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    /// Whether this platform confines this filesystem mode.
    fn applies(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            true
        }
        #[cfg(target_os = "macos")]
        {
            macos::available()
        }
        #[cfg(windows)]
        {
            windows::applies(self.policy, self.grant.is_some())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            false
        }
    }

    /// The confined command, or `None` when this platform confines nothing
    /// of this policy (the caller then runs it unconfined).
    fn wrap_confined(&self, shell: &Shell, command: &str) -> Option<Command> {
        #[cfg(target_os = "linux")]
        {
            let _ = shell;
            Some(linux::wrap(self.policy, &self.roots, command))
        }
        #[cfg(target_os = "macos")]
        {
            macos::wrap(self.policy, &self.roots, shell, command)
        }
        #[cfg(windows)]
        {
            windows::wrap(self.policy.mode, self.grant.as_ref(), shell, command)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = (shell, command);
            None
        }
    }
}

#[cfg(windows)]
impl SessionGrant {
    /// The session's grant over `roots`: taken on first use, widened when a
    /// root is added mid-session (the new lease is taken BEFORE the old one is
    /// dropped, so a root in both is never revoked and re-granted).
    fn hold(&self, roots: &[PathBuf]) -> Result<windows::acl::Grant, String> {
        let mut held = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lease) = held.as_ref() {
            if lease.covers(roots) {
                return Ok(lease.grant());
            }
        }
        let lease = windows::acl::Lease::acquire(roots)?;
        let grant = lease.grant();
        *held = Some(lease);
        Ok(grant)
    }
}

/// Whether this platform can refuse a command's network, and by what — or why
/// not. Asked only when `deny` was set.
fn network_support() -> Result<&'static str, &'static str> {
    #[cfg(target_os = "linux")]
    {
        if linux::network_enforceable() {
            Ok("Landlock ABI v4 (every TCP bind and connect, loopback included; UDP is not covered)")
        } else {
            Err("this kernel's Landlock predates ABI v4 (Linux 6.7), the first to cover the network")
        }
    }
    #[cfg(target_os = "macos")]
    {
        if macos::available() {
            Ok("the Seatbelt profile (loopback and Unix sockets stay allowed)")
        } else {
            Err("sandbox-exec was not found")
        }
    }
    #[cfg(windows)]
    {
        Err(windows::NETWORK_NOT_ENFORCED)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err("this platform has no sandbox")
    }
}

/// The plain way `shell` runs a command string, unconfined.
fn plain(shell: &Shell, command: &str) -> Command {
    shell.command(command)
}

/// What this OS's sandbox can and cannot do, for the report.
pub struct PerOs {
    /// The mechanism, named.
    pub mechanism: &'static str,
    /// Why a mode it cannot do is left off.
    pub cannot: &'static str,
    /// What `sandbox_network: "deny"` does here.
    pub network: &'static str,
}

/// The current platform's sandbox capabilities, for `/doctor` and the docs.
#[must_use]
pub fn per_os() -> PerOs {
    #[cfg(target_os = "linux")]
    {
        PerOs {
            mechanism: "Landlock (the kernel's own unprivileged filesystem confinement)",
            cannot: "an old kernel without Landlock leaves the command unconfined",
            network: "deny refuses every TCP bind and connect (loopback included) on Landlock \
                      ABI v4+ (Linux 6.7+); UDP is not covered; an older kernel is reported \
                      as not enforced",
        }
    }
    #[cfg(target_os = "macos")]
    {
        PerOs {
            mechanism: "sandbox-exec (Seatbelt)",
            cannot: "sandbox-exec was not found",
            network: "deny refuses network* in the profile, keeping loopback and Unix sockets",
        }
    }
    #[cfg(windows)]
    {
        windows::per_os()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        PerOs {
            mechanism: "nothing (no sandbox is implemented for this platform)",
            cannot: "this platform has no sandbox",
            network: "not restricted",
        }
    }
}

/// The macOS Seatbelt profile: allow all, then — unless the filesystem mode is
/// off — deny every write and re-allow the temp directory, `WS0..WSn` and the
/// sinks; the last matching rule wins, so each deny is stated before its
/// allows. With no writable roots it is read-only. With the network denied it
/// then denies `network*` and re-allows loopback (a local dev server, a test
/// suite that binds 127.0.0.1) and Unix-domain sockets (ssh-agent, a local
/// daemon) — a decision, stated in the docs: this blocks reaching the outside,
/// not local IPC.
///
/// Here, not in [`macos`], so it is compiled and tested on every platform: the
/// profile string is the load-bearing part, and a machine that cannot run
/// `sandbox-exec` can still check it is well formed. The network filter forms
/// (`local ip`, `remote ip "localhost:*"`, `unix-socket (path-regex …)`) are
/// the ones shipping Seatbelt profiles use; they have NOT been run on a Mac
/// from this change.
#[must_use]
pub fn seatbelt_profile(mode: SandboxMode, root_count: usize, network: SandboxNetwork) -> String {
    let mut profile = String::from("(version 1) (allow default)");
    if mode != SandboxMode::Off {
        let mut allow = String::from("(subpath (param \"TMP\"))");
        for i in 0..root_count {
            allow.push_str(&format!(" (subpath (param \"WS{i}\"))"));
        }
        profile.push_str(&format!(
            " (deny file-write* (subpath \"/\")) \
             (allow file-write* {allow} \
             (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") \
             (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/[0-9]+$\") \
             (regex #\"^/dev/ttys[0-9]+$\"))"
        ));
    }
    if network == SandboxNetwork::Deny {
        profile.push_str(
            " (deny network*) \
             (allow network-bind (local ip \"localhost:*\")) \
             (allow network-inbound (local ip \"localhost:*\")) \
             (allow network-outbound (remote ip \"localhost:*\")) \
             (allow network-bind (local unix-socket (path-regex #\"^/\"))) \
             (allow network-outbound (remote unix-socket (path-regex #\"^/\")))",
        );
    }
    profile
}

/// The temp directory a `workspace-write` command may also write in, resolved
/// and made real. Kept beside the roots because a great many tools write
/// there and denying it breaks them.
#[must_use]
pub fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir();
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// The `--sandbox-exec` helper: apply this platform's confinement to THIS
/// process, then replace it with the shell command. Called from `main` before
/// any thread starts, because a restriction that binds one thread only (early
/// Landlock) must bind the process that will run the command.
///
/// Never returns on success — it execs. On Windows there is no exec, so this
/// mode is not used there ([`windows::wrap`] launches a confined child
/// instead); on any platform where it cannot apply, it runs the command
/// unconfined rather than refusing, so a command still runs.
///
/// # Errors
///
/// When the arguments are malformed, or the command could not be started.
#[cfg(unix)]
pub fn exec(args: &[String]) -> anyhow::Result<std::convert::Infallible> {
    let parsed = ExecArgs::parse(args)?;
    #[cfg(target_os = "linux")]
    linux::restrict(parsed.mode, parsed.network, &parsed.roots);
    // exec replaces this image, so the restriction above stays in force.
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&parsed.command);
    Err(cmd.exec()).map_err(|e| anyhow::anyhow!("could not run the sandboxed command: {e}"))
}

/// The `--sandbox-exec` helper on Windows: build the write-restricted token and
/// run the command under it, returning its exit code — [`windows::run_helper`].
/// There is no exec on Windows, so this launches a confined child and waits.
///
/// The argv is `<mode> [--sid <SID> --temp <dir>] --shell <exe> --shell-kind
/// <posix|powershell> -- <command>`: the SID and the temp directory are the
/// session's grant, and only workspace-write has one; the shell is the one
/// [`crate::tools::shell_choice`] chose for the command.
///
/// # Errors
///
/// When the arguments are malformed, or the sandbox could not be built.
#[cfg(windows)]
pub fn run_windows_helper(args: &[String]) -> anyhow::Result<i32> {
    let mut it = args.iter();
    let mode = it
        .next()
        .and_then(|m| SandboxMode::parse(m))
        .ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs a mode first"))?;
    let mut sid = None;
    let mut temp = None;
    let mut program = None;
    let mut kind = None;
    let mut command = None;
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--sid" => sid = Some(value("--sid")?),
            "--temp" => temp = Some(PathBuf::from(value("--temp")?)),
            "--shell" => program = Some(PathBuf::from(value("--shell")?)),
            "--shell-kind" => kind = Some(value("--shell-kind")?),
            // The command is ONE argument after `--` (wrap passed it whole),
            // so multiple spaces and quotes survive.
            "--" => {
                command = it.next().cloned();
                break;
            }
            other => anyhow::bail!("unexpected argument to --sandbox-exec: {other}"),
        }
    }
    let command = command.ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs `-- <command>`"))?;
    let grant = match (sid, temp) {
        (Some(sid), Some(temp)) => Some((sid, temp)),
        (None, None) => None,
        _ => anyhow::bail!("--sandbox-exec needs --sid and --temp together"),
    };
    let flavor = kind
        .as_deref()
        .and_then(crate::tools::shell_choice::Flavor::parse)
        .ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs --shell-kind posix|powershell"))?;
    let program = program.ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs --shell <exe>"))?;
    windows::run_helper(mode, grant, &Shell::from_parts(program, flavor), &command)
}

/// The parsed argv of `--sandbox-exec <mode> [--network <allow|deny>]
/// [--root <dir>]... -- <command>`.
#[cfg(unix)]
struct ExecArgs {
    mode: SandboxMode,
    network: SandboxNetwork,
    roots: Vec<PathBuf>,
    command: String,
}

#[cfg(unix)]
impl ExecArgs {
    fn parse(args: &[String]) -> anyhow::Result<Self> {
        let mut it = args.iter();
        let mode = it
            .next()
            .and_then(|m| SandboxMode::parse(m))
            .ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs a mode first"))?;
        let mut network = SandboxNetwork::Allow;
        let mut roots = Vec::new();
        let mut command = None;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--root" => roots.push(PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--root needs a directory"))?,
                )),
                "--network" => {
                    network = it
                        .next()
                        .and_then(|n| SandboxNetwork::parse(n))
                        .ok_or_else(|| anyhow::anyhow!("--network needs allow or deny"))?;
                }
                "--" => {
                    command = Some(it.cloned().collect::<Vec<_>>().join(" "));
                    break;
                }
                other => anyhow::bail!("unexpected argument to --sandbox-exec: {other}"),
            }
        }
        let command =
            command.ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs `-- <command>`"))?;
        Ok(Self {
            mode,
            network,
            roots,
            command,
        })
    }
}

/// The argv that runs `command` under this session's sandbox through the
/// `--sandbox-exec` helper: the incode binary, the mode, the network setting
/// when it is `deny`, each root, then the command. Used by the platforms whose
/// confinement is applied in-process (Linux); shared here so the shape is
/// written once.
#[cfg(unix)]
fn helper_argv(exe: &Path, policy: SandboxPolicy, roots: &[PathBuf], command: &str) -> Vec<String> {
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        "--sandbox-exec".to_string(),
        policy.mode.as_str().to_string(),
    ];
    if policy.network == SandboxNetwork::Deny {
        argv.push("--network".to_string());
        argv.push(policy.network.as_str().to_string());
    }
    for root in roots {
        argv.push("--root".to_string());
        argv.push(root.to_string_lossy().into_owned());
    }
    argv.push("--".to_string());
    argv.push(command.to_string());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default sandbox is as tight as the permission mode and never
    /// tighter: a looking session writes nothing, a writing one writes its
    /// workspace, and the full-access modes opt out.
    #[test]
    fn the_default_sandbox_matches_the_permission_mode() {
        use crate::permissions::PermissionMode as P;
        assert_eq!(
            SandboxMode::for_permission_mode(P::ReadOnly),
            SandboxMode::ReadOnly
        );
        assert_eq!(
            SandboxMode::for_permission_mode(P::WorkspaceWrite),
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            SandboxMode::for_permission_mode(P::Prompt),
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            SandboxMode::for_permission_mode(P::DangerFullAccess),
            SandboxMode::Off
        );
        assert_eq!(SandboxMode::for_permission_mode(P::Allow), SandboxMode::Off);
    }

    /// Off runs the command through the platform's plain shell, unchanged.
    #[test]
    fn off_runs_the_plain_shell() {
        let sandbox = Sandbox::off();
        assert!(sandbox.is_off());
        let shell = Shell::from_parts(
            PathBuf::from("sh"),
            crate::tools::shell_choice::Flavor::Posix,
        );
        let cmd = sandbox.wrap(&shell, "echo hi");
        assert!(sandbox.note().contains("off"));
        // The program is the shell itself, not a wrapper.
        assert_eq!(cmd.get_program().to_string_lossy(), "sh");
        assert!(sandbox.posix_shell_runs());
    }

    /// A mode round-trips through its word, and an unknown one is refused.
    #[test]
    fn a_mode_round_trips_and_an_unknown_one_is_none() {
        for mode in [
            SandboxMode::Off,
            SandboxMode::WorkspaceWrite,
            SandboxMode::ReadOnly,
        ] {
            assert_eq!(SandboxMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SandboxMode::parse("sometimes"), None);
    }

    fn policy(mode: SandboxMode, network: SandboxNetwork) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            network,
            named: true,
        }
    }

    /// The note names what confined the command, or says plainly that the
    /// requested mode was not applied — never silent.
    #[test]
    fn the_note_says_whether_the_sandbox_applied() {
        let roots = vec![std::env::temp_dir()];
        let sandbox = Sandbox::new(policy(SandboxMode::ReadOnly, SandboxNetwork::Allow), roots);
        let note = sandbox.note();
        assert!(note.contains("read-only"), "{note}");
        assert!(
            note.contains("confined by") || note.contains("NOT applied"),
            "{note}"
        );
        assert!(!note.contains("network"), "allow says nothing: {note}");
    }

    /// The network setting round-trips through its word; the default is
    /// `allow`, which is the behaviour before it existed.
    #[test]
    fn the_network_setting_round_trips_and_defaults_to_allow() {
        for network in [SandboxNetwork::Allow, SandboxNetwork::Deny] {
            assert_eq!(SandboxNetwork::parse(network.as_str()), Some(network));
        }
        assert_eq!(SandboxNetwork::parse("sometimes"), None);
        assert_eq!(SandboxNetwork::default(), SandboxNetwork::Allow);
        assert_eq!(SandboxPolicy::OFF.network, SandboxNetwork::Allow);
    }

    /// `deny` is never silent: the note says it was enforced and by what, or
    /// that it was NOT — and it applies with the filesystem mode off too, so
    /// that sandbox is not "off" any more.
    #[test]
    fn a_network_deny_is_reported_whether_or_not_it_is_enforced() {
        let sandbox = Sandbox::new(policy(SandboxMode::Off, SandboxNetwork::Deny), Vec::new());
        assert!(!sandbox.is_off(), "a network deny is something to apply");
        let note = sandbox.note();
        assert!(
            note.contains("network denied by")
                || note.contains("network deny requested but NOT enforced"),
            "{note}"
        );
        #[cfg(windows)]
        assert!(note.contains("NOT enforced"), "Windows cannot: {note}");
    }

    /// Workspace-write names every root as a `WS<n>` parameter and allows each
    /// in the Seatbelt profile; read-only names none, so the profile allows
    /// only the temp directory and the sinks. Run on every platform.
    #[test]
    fn the_seatbelt_profile_allows_the_roots_only_in_workspace_write() {
        let write = seatbelt_profile(SandboxMode::WorkspaceWrite, 2, SandboxNetwork::Allow);
        assert!(write.contains("(subpath (param \"WS0\"))"), "{write}");
        assert!(write.contains("(subpath (param \"WS1\"))"), "{write}");
        assert!(
            write.contains("(deny file-write* (subpath \"/\"))"),
            "{write}"
        );
        assert!(write.contains("(allow default)"), "{write}");
        assert!(!write.contains("network"), "allow adds no rule: {write}");

        let read = seatbelt_profile(SandboxMode::ReadOnly, 0, SandboxNetwork::Allow);
        assert!(
            !read.contains("WS0"),
            "read-only allows no workspace: {read}"
        );
        assert!(read.contains("(subpath (param \"TMP\"))"), "{read}");
        assert!(read.contains("/dev/null"), "{read}");
    }

    /// With the network denied, the profile denies `network*` and then
    /// re-allows only loopback and Unix sockets — the allows AFTER the deny,
    /// because the last matching rule wins. With the filesystem off it keeps no
    /// write rule at all.
    #[test]
    fn the_seatbelt_profile_denies_the_network_but_keeps_loopback() {
        let profile = seatbelt_profile(SandboxMode::Off, 0, SandboxNetwork::Deny);
        let deny = profile.find("(deny network*)").expect("the deny is there");
        for allow in [
            "(allow network-outbound (remote ip \"localhost:*\"))",
            "(allow network-bind (local ip \"localhost:*\"))",
            "(allow network-inbound (local ip \"localhost:*\"))",
            "(allow network-outbound (remote unix-socket (path-regex #\"^/\")))",
        ] {
            let at = profile
                .find(allow)
                .unwrap_or_else(|| panic!("{allow} in {profile}"));
            assert!(at > deny, "{allow} must follow the deny: {profile}");
        }
        assert!(!profile.contains("file-write"), "fs off: {profile}");
        // Nothing re-allows a remote address that is not loopback.
        assert!(!profile.contains("(remote ip \"*:*\")"), "{profile}");
        assert_eq!(profile.matches('(').count(), profile.matches(')').count());
    }

    #[cfg(unix)]
    #[test]
    fn the_helper_argv_carries_the_mode_the_roots_and_the_command() {
        let argv = helper_argv(
            Path::new("/usr/bin/incode"),
            policy(SandboxMode::WorkspaceWrite, SandboxNetwork::Allow),
            &[PathBuf::from("/ws"), PathBuf::from("/extra")],
            "echo hi",
        );
        assert_eq!(argv[0], "/usr/bin/incode");
        assert_eq!(argv[1], "--sandbox-exec");
        assert_eq!(argv[2], "workspace-write");
        assert!(argv.windows(2).any(|w| w[0] == "--root" && w[1] == "/ws"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--root" && w[1] == "/extra"));
        assert!(
            !argv.iter().any(|a| a == "--network"),
            "allow is the default"
        );
        let dashes = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(argv[dashes + 1], "echo hi");

        let denied = helper_argv(
            Path::new("/usr/bin/incode"),
            policy(SandboxMode::Off, SandboxNetwork::Deny),
            &[],
            "echo hi",
        );
        assert!(denied
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == "deny"));
        let parsed = ExecArgs::parse(&denied[2..]).expect("parses back");
        assert_eq!(parsed.network, SandboxNetwork::Deny);
        assert_eq!(parsed.mode, SandboxMode::Off);
    }

    #[cfg(unix)]
    #[test]
    fn the_helper_args_parse_back() {
        let parsed = ExecArgs::parse(&[
            "read-only".to_string(),
            "--root".to_string(),
            "/ws".to_string(),
            "--".to_string(),
            "echo".to_string(),
            "hi".to_string(),
        ])
        .expect("valid");
        assert_eq!(parsed.mode, SandboxMode::ReadOnly);
        assert_eq!(parsed.network, SandboxNetwork::Allow);
        assert_eq!(parsed.roots, [PathBuf::from("/ws")]);
        assert_eq!(parsed.command, "echo hi");
        assert!(ExecArgs::parse(&["bogus".to_string()]).is_err());
        assert!(ExecArgs::parse(&[
            "off".to_string(),
            "--network".to_string(),
            "sometimes".to_string()
        ])
        .is_err());
    }
}
