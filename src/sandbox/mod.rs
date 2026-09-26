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
//! Network is NOT restricted by any mode here; what each OS does about it is
//! reported by [`per_os`], not enforced.
//!
//! # How, per OS — and where it is honest about the limit
//!
//! - **Linux** — the kernel's own Landlock, applied by `incode --sandbox-exec`
//!   ([`exec`]) to itself before it execs the shell, so the restriction is in
//!   force for the command and everything it starts. Unprivileged; sound.
//! - **macOS** — `sandbox-exec` with a generated Seatbelt profile. Sound; the
//!   tool is marked deprecated by Apple but present and working.
//! - **Windows** — see [`windows`]. Read-only is enforced with a low-integrity
//!   token; workspace-write cannot be done soundly without changing the
//!   workspace's ACL, so it is left OFF with the reason stated, rather than
//!   pretended.
//!
//! When a platform cannot enforce a mode soundly, [`Sandbox::wrap`] returns the
//! command unconfined and [`Sandbox::note`] says so — the caller traces it, so
//! a session that ran unconfined is never mistaken for one that was confined.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
pub mod windows;

/// Names the sandbox mode, ahead of the settings and the permission-mode
/// default — for a run that wants to pin it without editing `admin.json`.
pub const SANDBOX_ENV: &str = "INCORD_SANDBOX";

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

/// The sandbox in force for a session's shell commands: the mode and the
/// directories a `workspace-write` command may write in.
#[derive(Debug, Clone)]
pub struct Sandbox {
    mode: SandboxMode,
    /// The workspace first, then any added directory. Made absolute by the
    /// caller, which is where they are already canonical.
    roots: Vec<PathBuf>,
}

impl Sandbox {
    /// A sandbox in `mode` over `roots` (workspace first).
    #[must_use]
    pub fn new(mode: SandboxMode, roots: Vec<PathBuf>) -> Self {
        Self { mode, roots }
    }

    /// No sandbox: every command runs unconfined.
    #[must_use]
    pub fn off() -> Self {
        Self {
            mode: SandboxMode::Off,
            roots: Vec::new(),
        }
    }

    /// The mode in force.
    #[must_use]
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// Whether this sandbox does anything to a command.
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.mode == SandboxMode::Off
    }

    /// The command to spawn for one shell `command`, confined to this
    /// sandbox where the platform can, and left unconfined where it cannot —
    /// [`Self::note`] says which. The caller still sets the working
    /// directory, the environment and the pipes.
    ///
    /// The `shell` pair is how this platform runs a command string, e.g.
    /// `("cmd", "/C")` or `("sh", "-c")` — used unchanged when the sandbox is
    /// off or cannot apply.
    #[must_use]
    pub fn wrap(&self, shell: (&str, &str), command: &str) -> Command {
        if self.mode == SandboxMode::Off {
            return plain(shell, command);
        }
        self.wrap_confined(shell, command)
            .unwrap_or_else(|| plain(shell, command))
    }

    /// One line for the trace and for `/doctor`: what the sandbox is doing to
    /// this session's commands, and where it cannot.
    #[must_use]
    pub fn note(&self) -> String {
        if self.mode == SandboxMode::Off {
            return "sandbox off: shell commands run unconfined".to_string();
        }
        match self.applies() {
            Some(()) => format!(
                "sandbox {}: shell commands are confined by {}",
                self.mode.as_str(),
                per_os().mechanism
            ),
            None => format!(
                "sandbox {} requested but NOT applied on this platform: {}",
                self.mode.as_str(),
                per_os().cannot
            ),
        }
    }

    /// Whether this platform can confine this mode. `Some(())` when it can.
    fn applies(&self) -> Option<()> {
        #[cfg(target_os = "linux")]
        {
            Some(())
        }
        #[cfg(target_os = "macos")]
        {
            macos::available().then_some(())
        }
        #[cfg(windows)]
        {
            windows::applies(self.mode).then_some(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            None
        }
    }

    /// The confined command, or `None` when this platform cannot confine this
    /// mode (the caller then runs it unconfined).
    #[allow(unused_variables)]
    fn wrap_confined(&self, shell: (&str, &str), command: &str) -> Option<Command> {
        #[cfg(target_os = "linux")]
        {
            Some(linux::wrap(self.mode, &self.roots, command))
        }
        #[cfg(target_os = "macos")]
        {
            macos::wrap(self.mode, &self.roots, shell, command)
        }
        #[cfg(windows)]
        {
            windows::wrap(self.mode, &self.roots, shell, command)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            None
        }
    }
}

/// The plain way this platform runs a command string, unconfined.
fn plain(shell: (&str, &str), command: &str) -> Command {
    let mut cmd = Command::new(shell.0);
    #[cfg(windows)]
    {
        // `cmd /C` does not parse MSVC quoting, so the command is passed
        // verbatim — the same reasoning as `tools::bash`.
        use std::os::windows::process::CommandExt;
        cmd.arg(shell.1).raw_arg(command);
    }
    #[cfg(not(windows))]
    {
        cmd.arg(shell.1).arg(command);
    }
    cmd
}

/// What this OS's sandbox can and cannot do, for the report.
pub struct PerOs {
    /// The mechanism, named.
    pub mechanism: &'static str,
    /// Why a mode it cannot do is left off.
    pub cannot: &'static str,
    /// What it does about network (reported, not necessarily enforced).
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
            network: "not restricted (Landlock does not cover the network)",
        }
    }
    #[cfg(target_os = "macos")]
    {
        PerOs {
            mechanism: "sandbox-exec (Seatbelt)",
            cannot: "sandbox-exec was not found",
            network: "not restricted by the profile",
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

/// The macOS Seatbelt profile: allow all, then deny every write, then re-allow
/// the temp directory, `WS0..WSn` and the sinks — the last matching rule wins,
/// so the deny is stated before the allows. With no writable roots it is
/// read-only. Here, not in [`macos`], so it is compiled and tested on every
/// platform: the profile string is the load-bearing part, and a machine that
/// cannot run `sandbox-exec` can still check it is well formed.
#[must_use]
pub fn seatbelt_profile(root_count: usize) -> String {
    let mut allow = String::from("(subpath (param \"TMP\"))");
    for i in 0..root_count {
        allow.push_str(&format!(" (subpath (param \"WS{i}\"))"));
    }
    format!(
        "(version 1) (allow default) (deny file-write* (subpath \"/\")) \
         (allow file-write* {allow} \
         (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") \
         (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/[0-9]+$\") \
         (regex #\"^/dev/ttys[0-9]+$\"))"
    )
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
    linux::restrict(parsed.mode, &parsed.roots);
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
/// # Errors
///
/// When the arguments are malformed, or the sandbox could not be built.
#[cfg(windows)]
pub fn run_windows_helper(args: &[String]) -> anyhow::Result<i32> {
    let mode = args
        .first()
        .and_then(|m| SandboxMode::parse(m))
        .ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs a mode first"))?;
    // Everything after `--` is the command, as one argument (wrap passed it
    // whole), so multiple spaces and quotes survive.
    let command = args
        .iter()
        .position(|a| a == "--")
        .and_then(|at| args.get(at + 1))
        .ok_or_else(|| anyhow::anyhow!("--sandbox-exec needs `-- <command>`"))?;
    windows::run_helper(mode, command)
}

/// The parsed argv of `--sandbox-exec <mode> [--root <dir>]... -- <command>`.
#[cfg(unix)]
struct ExecArgs {
    mode: SandboxMode,
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
        let mut roots = Vec::new();
        let mut command = None;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--root" => roots.push(PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--root needs a directory"))?,
                )),
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
            roots,
            command,
        })
    }
}

/// The argv that runs `command` under this session's sandbox through the
/// `--sandbox-exec` helper: the incode binary, the mode, each root, then the
/// command. Used by the platforms whose confinement is applied in-process
/// (Linux); shared here so the shape is written once.
#[cfg(unix)]
fn helper_argv(exe: &Path, mode: SandboxMode, roots: &[PathBuf], command: &str) -> Vec<String> {
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        "--sandbox-exec".to_string(),
        mode.as_str().to_string(),
    ];
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
        let cmd = sandbox.wrap(("sh", "-c"), "echo hi");
        assert!(sandbox.note().contains("off"));
        // The program is the shell itself, not a wrapper.
        assert_eq!(cmd.get_program().to_string_lossy(), "sh");
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

    /// The note names what confined the command, or says plainly that the
    /// requested mode was not applied — never silent.
    #[test]
    fn the_note_says_whether_the_sandbox_applied() {
        let roots = vec![std::env::temp_dir()];
        let sandbox = Sandbox::new(SandboxMode::ReadOnly, roots);
        let note = sandbox.note();
        assert!(note.contains("read-only"), "{note}");
        assert!(
            note.contains("confined by") || note.contains("NOT applied"),
            "{note}"
        );
    }

    /// Workspace-write names every root as a `WS<n>` parameter and allows each
    /// in the Seatbelt profile; read-only names none, so the profile allows
    /// only the temp directory and the sinks. Run on every platform.
    #[test]
    fn the_seatbelt_profile_allows_the_roots_only_in_workspace_write() {
        let write = seatbelt_profile(2);
        assert!(write.contains("(subpath (param \"WS0\"))"), "{write}");
        assert!(write.contains("(subpath (param \"WS1\"))"), "{write}");
        assert!(
            write.contains("(deny file-write* (subpath \"/\"))"),
            "{write}"
        );
        assert!(write.contains("(allow default)"), "{write}");

        let read = seatbelt_profile(0);
        assert!(
            !read.contains("WS0"),
            "read-only allows no workspace: {read}"
        );
        assert!(read.contains("(subpath (param \"TMP\"))"), "{read}");
        assert!(read.contains("/dev/null"), "{read}");
    }

    #[cfg(unix)]
    #[test]
    fn the_helper_argv_carries_the_mode_the_roots_and_the_command() {
        let argv = helper_argv(
            Path::new("/usr/bin/incode"),
            SandboxMode::WorkspaceWrite,
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
        let dashes = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(argv[dashes + 1], "echo hi");
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
        assert_eq!(parsed.roots, [PathBuf::from("/ws")]);
        assert_eq!(parsed.command, "echo hi");
        assert!(ExecArgs::parse(&["bogus".to_string()]).is_err());
    }
}
