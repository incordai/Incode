//! Which shell the `Bash` tool runs a command in, chosen once per process.
//!
//! # Why this exists
//!
//! The tool is called `Bash` and models write POSIX shell. On Windows it used
//! to run `cmd /C`, so `mkdir -p .backups && cp a ".backups/a.$(date +%s)"`
//! made a directory called `-p` and a file whose name ended in a literal
//! `$(date +%s)` — and the model concluded it was talking to cmd.exe.
//!
//! # The order, on Windows
//!
//! 1. `INCODE_SHELL`, then `shell` in `admin.json`: a path or a program name
//!    on `PATH`. `bash`, `sh` or any other name is run as a POSIX shell with
//!    `-c`; `pwsh`/`powershell` as PowerShell. `cmd.exe` and the WSL launcher
//!    are refused, and the search carries on — the refusal is traced.
//! 2. Git for Windows' bash: `bin\bash.exe` in a directory above a `git.exe`
//!    on `PATH` (`<Git>\cmd\git.exe` and `<Git>\mingw64\bin\git.exe` both lead
//!    to `<Git>\bin\bash.exe`), then the standard install directories.
//!    NEVER `bash.exe` under the Windows directory: that is WSL, which runs in
//!    another filesystem namespace, so the workspace path means nothing there.
//! 3. PowerShell: `pwsh.exe` on `PATH`, then Windows PowerShell.
//!
//! Never cmd.exe. When nothing is found the tool's description says so and
//! every call fails with that reason.
//!
//! A candidate is taken only once it has answered `--version` (PowerShell: its
//! version table) within [`PROBE_BOUND`], so a file that exists but cannot run
//! is passed over rather than chosen.
//!
//! # Under the Windows sandbox
//!
//! Git Bash cannot start under the write-restricted token the Windows sandbox
//! runs a command with. Measured on this machine: every msys program died at
//! once with `CreateFileMapping S-1-5-21-…-1004.1, Win32 error 5` — the msys
//! runtime maps a per-user shared section that other msys processes of the same
//! user created, and a write-restricted token may not map it writable. So when
//! the token applies (read-only, or a named workspace-write), the command runs
//! in PowerShell, which does run under it and is confined by it, and the tool's
//! description says so ([`describe`]).
//!
//! # Elsewhere
//!
//! macOS and Linux run `sh -c`, as they always have.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Names the shell to use on Windows, ahead of `admin.json` `shell`.
#[cfg(windows)]
const SHELL_ENV: &str = "INCODE_SHELL";

/// How long a candidate has to answer its version probe.
#[cfg(windows)]
const PROBE_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

/// The two kinds of shell the tool can run a command in. PowerShell, and the
/// round trip through the sandbox helper's argv, exist on Windows only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Flavor {
    /// `sh -c`, bash and the like.
    Posix,
    /// `pwsh` or Windows PowerShell.
    PowerShell,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Flavor {
    /// The word the sandbox helper's argv uses.
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::PowerShell => "powershell",
        }
    }

    /// The flavor named by [`Self::as_str`], or `None`.
    #[must_use]
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "posix" => Some(Self::Posix),
            "powershell" => Some(Self::PowerShell),
            _ => None,
        }
    }
}

/// One shell, found and proved to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shell {
    program: PathBuf,
    flavor: Flavor,
    /// What the model is told, e.g. `Git Bash (bash 5.2)`.
    label: String,
}

impl Shell {
    /// A shell rebuilt from what the sandbox helper was handed. The label is
    /// not needed there. Windows only, as that helper is.
    #[must_use]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn from_parts(program: PathBuf, flavor: Flavor) -> Self {
        Self {
            program,
            flavor,
            label: String::new(),
        }
    }

    /// The executable.
    #[must_use]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    /// POSIX or PowerShell.
    #[must_use]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn flavor(&self) -> Flavor {
        self.flavor
    }

    /// The command that runs `script` in this shell, unconfined. The caller
    /// sets the working directory, the environment and the pipes.
    #[must_use]
    pub(crate) fn command(&self, script: &str) -> Command {
        let mut cmd = Command::new(&self.program);
        #[cfg(windows)]
        {
            // Every argument is quoted here, whole, rather than by `arg`:
            // `arg` leaves an argument without spaces unquoted, and the msys
            // runtime GLOBS an unquoted argument from a Windows parent.
            use std::os::windows::process::CommandExt;
            for piece in self.windows_args(script) {
                cmd.raw_arg(piece);
            }
        }
        #[cfg(not(windows))]
        {
            cmd.arg("-c").arg(script);
        }
        cmd
    }

    /// The whole Windows command line that runs `script`, program first, for a
    /// launcher that takes one string (`CreateProcessAsUserW`).
    #[cfg(windows)]
    #[must_use]
    pub(crate) fn command_line(&self, script: &str) -> String {
        let mut line = quote_windows(&self.program.to_string_lossy());
        for piece in self.windows_args(script) {
            line.push(' ');
            line.push_str(&piece);
        }
        line
    }

    /// The argv that runs `script`, program first — for a launcher that
    /// takes the program as an argument (`sandbox-exec`).
    #[cfg(target_os = "macos")]
    #[must_use]
    pub(crate) fn argv(&self, script: &str) -> Vec<std::ffi::OsString> {
        vec![
            self.program.clone().into_os_string(),
            "-c".into(),
            script.into(),
        ]
    }

    /// The arguments after the program, each already quoted for a Windows
    /// command line.
    #[cfg(windows)]
    fn windows_args(&self, script: &str) -> Vec<String> {
        // Only the script, last, is quoted: the flags before it never need it.
        let mut args = self.args(script);
        if let Some(last) = args.last_mut() {
            *last = quote_windows(last);
        }
        args
    }

    /// The arguments that run `script` in this shell, unquoted, for a launcher
    /// that quotes them itself (`sbx.exe`). On Windows, PowerShell gets
    /// `-Command` and ONE argument: it parses its command line by the C
    /// runtime's rules, so the script arrives as written (measured with
    /// quotes, backslashes, `&|<>^%` and non-ASCII). Not `-EncodedCommand`:
    /// with it, Windows PowerShell writes its error stream as CLIXML. Output
    /// is UTF-8 (the OEM code page otherwise), and progress is off.
    #[must_use]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn args(&self, script: &str) -> Vec<String> {
        match self.flavor {
            Flavor::Posix => vec!["-c".to_string(), script.to_string()],
            Flavor::PowerShell => {
                let whole = format!(
                    "$ProgressPreference='SilentlyContinue'; \
                     [Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {script}"
                );
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    whole,
                ]
            }
        }
    }
}

/// `arg` quoted for a Windows command line, always inside double quotes: a
/// quote is escaped with a backslash, and the backslashes before a quote (or
/// before the closing one) are doubled — the rules both the C runtime and the
/// msys runtime parse.
#[cfg(windows)]
fn quote_windows(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The shell this process runs the tool's commands in, found once: the
/// operator's choice, Git Bash, PowerShell — or why there is none.
fn chosen() -> &'static Result<Shell, String> {
    static CHOSEN: OnceLock<Result<Shell, String>> = OnceLock::new();
    CHOSEN.get_or_init(|| {
        #[cfg(windows)]
        {
            let configured = crate::admin::AdminConfig::load_default().shell;
            let found = choose(&Probe {
                env: &|name| std::env::var_os(name),
                configured: configured.as_deref(),
                is_file: &|path| path.is_file(),
                version: &probe_version,
            });
            match &found {
                Ok(shell) => crate::trace!(
                    "tool",
                    "the Bash tool runs in {} ({})",
                    shell.label,
                    shell.program.display()
                ),
                Err(why) => crate::trace!("tool", "the Bash tool has no shell: {why}"),
            }
            found
        }
        #[cfg(not(windows))]
        {
            Ok(Shell {
                program: PathBuf::from("sh"),
                flavor: Flavor::Posix,
                label: "sh".to_string(),
            })
        }
    })
}

/// PowerShell, found once, for the commands the Windows sandbox confines.
#[cfg(windows)]
fn powershell() -> &'static Result<Shell, String> {
    static FOUND: OnceLock<Result<Shell, String>> = OnceLock::new();
    FOUND.get_or_init(|| {
        let env = |name: &str| std::env::var_os(name);
        find_powershell(&Probe {
            env: &env,
            configured: None,
            is_file: &|path| path.is_file(),
            version: &probe_version,
        })
        .ok_or_else(|| "PowerShell was not found".to_string())
    })
}

/// The shell a command confined by `sandbox` runs in — or why there is none.
///
/// # Errors
///
/// No usable shell was found; the text says where it looked.
pub(crate) fn for_sandbox(sandbox: &crate::sandbox::Sandbox) -> Result<Shell, String> {
    let shell = chosen().clone();
    #[cfg(windows)]
    {
        if !sandbox.posix_shell_runs() && shell.as_ref().is_ok_and(|s| s.flavor == Flavor::Posix) {
            return powershell().clone().map_err(|why| {
                format!(
                    "Git Bash cannot start under this session's Windows sandbox, and {why}, so \
                     the command was not run"
                )
            });
        }
    }
    #[cfg(not(windows))]
    let _ = sandbox;
    shell.map_err(|why| format!("no shell to run the command in: {why}"))
}

/// The sentence the `Bash` tool's description carries about its shell, for a
/// session whose commands run under `sandbox` — so the model writes the syntax
/// that shell reads.
#[must_use]
pub(crate) fn describe(sandbox: &crate::sandbox::Sandbox) -> String {
    match for_sandbox(sandbox) {
        Ok(shell) => sentence(&shell, chosen().as_ref().ok()),
        Err(why) => format!("No shell is available: {why}. Every call fails and says so."),
    }
}

/// What the model is told about `shell`; `preferred` is the process's own
/// choice, which differs only when the sandbox forced PowerShell.
fn sentence(shell: &Shell, preferred: Option<&Shell>) -> String {
    match shell.flavor {
        Flavor::Posix if cfg!(windows) => format!(
            "Commands run in {} on Windows: write POSIX shell (`mkdir -p a/b`, \
             `$(date +%Y%m%d%H%M%S)`, `&&`), with paths as `C:/Users/...` or `/c/Users/...`. \
             Git Bash rewrites an argument that looks like a path for a Windows program, so \
             write a Windows program's `/switch` as `//switch`.",
            shell.label
        ),
        Flavor::Posix => format!(
            "Commands run in {} (`sh -c`): write POSIX shell.",
            shell.label
        ),
        Flavor::PowerShell => {
            let why = if preferred.is_some_and(|p| p.flavor == Flavor::Posix) {
                " because Git Bash cannot start under this session's Windows sandbox"
            } else {
                " because no Git Bash was found"
            };
            format!(
                "Commands run in {}, not bash,{why}: write PowerShell \
                 (`New-Item -ItemType Directory -Force a/b`, \
                 `Get-Date -Format yyyyMMddHHmmss`, `;` between commands).",
                shell.label
            )
        }
    }
}

/// Where the search looks, as functions, so a test can hand it a machine.
#[cfg(windows)]
struct Probe<'a> {
    env: &'a dyn Fn(&str) -> Option<std::ffi::OsString>,
    /// `shell` from `admin.json`.
    configured: Option<&'a str>,
    is_file: &'a dyn Fn(&Path) -> bool,
    /// The version `program` reports, `Some("")` when it ran but named none,
    /// `None` when it did not run.
    version: &'a dyn Fn(&Path, Flavor) -> Option<String>,
}

/// The shell to use, in the order the module doc gives.
#[cfg(windows)]
fn choose(probe: &Probe<'_>) -> Result<Shell, String> {
    let named = (probe.env)(SHELL_ENV)
        .map(|v| (v.to_string_lossy().trim().to_string(), SHELL_ENV))
        .filter(|(v, _)| !v.is_empty())
        .or_else(|| {
            probe
                .configured
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| (v.to_string(), "admin.json `shell`"))
        });
    let mut refused = Vec::new();
    if let Some((name, from)) = named {
        match named_shell(probe, &name) {
            Ok(shell) => return Ok(shell),
            Err(why) => {
                crate::trace!("tool", "{from} names `{name}`, which was not used: {why}");
                refused.push(format!("{from} `{name}` was not used ({why})"));
            }
        }
    }
    if let Some(shell) = find_git_bash(probe) {
        return Ok(shell);
    }
    if let Some(shell) = find_powershell(probe) {
        return Ok(shell);
    }
    refused.push(
        "no Git for Windows bash (beside a git.exe on PATH or under Program Files) and no \
         PowerShell was found"
            .to_string(),
    );
    Err(refused.join("; "))
}

/// The shell the operator named, or why it cannot be used.
#[cfg(windows)]
fn named_shell(probe: &Probe<'_>, name: &str) -> Result<Shell, String> {
    let path = if name.contains(['\\', '/']) {
        PathBuf::from(name)
    } else {
        on_path(probe, name).ok_or_else(|| format!("`{name}` is not on PATH"))?
    };
    if is_under_windows_dir(probe, &path) && stem(&path) == "bash" {
        return Err("that is the WSL launcher, which runs in another filesystem namespace".into());
    }
    let flavor = match stem(&path).as_str() {
        "cmd" => return Err("cmd.exe is not a POSIX shell".into()),
        "pwsh" | "powershell" => Flavor::PowerShell,
        _ => Flavor::Posix,
    };
    if !(probe.is_file)(&path) {
        return Err(format!("{} does not exist", path.display()));
    }
    let version = (probe.version)(&path, flavor)
        .ok_or_else(|| format!("{} did not answer its version probe", path.display()))?;
    let label = match flavor {
        Flavor::PowerShell => powershell_label(&path, &version),
        Flavor::Posix => with_version(&stem(&path), &version),
    };
    Ok(Shell {
        program: path,
        flavor,
        label,
    })
}

/// Git for Windows' `bin\bash.exe`, found from a `git.exe` on `PATH` or in the
/// standard install directories — never under the Windows directory.
#[cfg(windows)]
fn find_git_bash(probe: &Probe<'_>) -> Option<Shell> {
    let mut candidates = Vec::new();
    if let Some(path) = (probe.env)("PATH") {
        for dir in std::env::split_paths(&path) {
            if (probe.is_file)(&dir.join("git.exe")) {
                // <Git>\cmd, <Git>\bin, <Git>\mingw64\bin: bash is in <Git>\bin.
                for up in dir.ancestors().take(3) {
                    candidates.push(up.join("bin").join("bash.exe"));
                }
            }
        }
    }
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(root) = (probe.env)(var) {
            candidates.push(PathBuf::from(root).join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Some(local) = (probe.env)("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    candidates
        .into_iter()
        .filter(|path| !is_under_windows_dir(probe, path))
        .find_map(|path| {
            if !(probe.is_file)(&path) {
                return None;
            }
            let version = (probe.version)(&path, Flavor::Posix)?;
            let label = if version.is_empty() {
                "Git Bash".to_string()
            } else {
                format!("Git Bash (bash {version})")
            };
            Some(Shell {
                program: path,
                flavor: Flavor::Posix,
                label,
            })
        })
}

/// `pwsh.exe` on `PATH`, then Windows PowerShell where Windows keeps it, then
/// `powershell.exe` on `PATH`.
#[cfg(windows)]
fn find_powershell(probe: &Probe<'_>) -> Option<Shell> {
    let mut candidates: Vec<PathBuf> = on_path(probe, "pwsh.exe").into_iter().collect();
    candidates.push(
        windows_dir(probe)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
    );
    candidates.extend(on_path(probe, "powershell.exe"));
    candidates.into_iter().find_map(|path| {
        if !(probe.is_file)(&path) {
            return None;
        }
        let version = (probe.version)(&path, Flavor::PowerShell)?;
        let label = powershell_label(&path, &version);
        Some(Shell {
            program: path,
            flavor: Flavor::PowerShell,
            label,
        })
    })
}

/// `PowerShell 7.4` or `Windows PowerShell 5.1`.
#[cfg(windows)]
fn powershell_label(path: &Path, version: &str) -> String {
    let name = if stem(path) == "pwsh" {
        "PowerShell"
    } else {
        "Windows PowerShell"
    };
    with_version(name, version)
}

/// `name` followed by `version` when there is one.
#[cfg(windows)]
fn with_version(name: &str, version: &str) -> String {
    if version.is_empty() {
        name.to_string()
    } else {
        format!("{name} {version}")
    }
}

/// The first `name` on `PATH`, with `.exe` added when it has no extension.
#[cfg(windows)]
fn on_path(probe: &Probe<'_>, name: &str) -> Option<PathBuf> {
    let file = if Path::new(name).extension().is_some() {
        name.to_string()
    } else {
        format!("{name}.exe")
    };
    let path = (probe.env)("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&file))
        .find(|candidate| (probe.is_file)(candidate))
}

/// The Windows directory, from `SystemRoot`.
#[cfg(windows)]
fn windows_dir(probe: &Probe<'_>) -> PathBuf {
    (probe.env)("SystemRoot")
        .or_else(|| (probe.env)("windir"))
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
}

/// Whether `path` is inside the Windows directory — where `bash.exe` is the
/// WSL launcher.
#[cfg(windows)]
fn is_under_windows_dir(probe: &Probe<'_>, path: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let dir = norm(&windows_dir(probe));
    let dir = dir.trim_end_matches('\\');
    let path = norm(path);
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('\\'))
}

/// A file's stem, lowercased.
#[cfg(windows)]
fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Run `program`'s version probe, bounded: `Some(version)` (`""` when it named
/// none) when it ran and succeeded.
#[cfg(windows)]
fn probe_version(program: &Path, flavor: Flavor) -> Option<String> {
    let mut cmd = Command::new(program);
    match flavor {
        Flavor::Posix => {
            cmd.arg("--version");
        }
        Flavor::PowerShell => {
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ]);
        }
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::agent::process_tree::prepare(&mut cmd);
    let child = cmd.spawn().ok()?;
    let out = crate::tools::bash::wait_with_timeout(child, PROBE_BOUND).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(version_in(flavor, &String::from_utf8_lossy(&out.stdout)))
}

/// `major.minor` from a version probe's output: bash's `version 5.2.37(1)`,
/// PowerShell's `5.1.26100.4202`. Empty when there is none.
#[cfg(windows)]
fn version_in(flavor: Flavor, text: &str) -> String {
    let from = match flavor {
        Flavor::Posix => text.find("version ").map_or("", |at| &text[at + 8..]),
        Flavor::PowerShell => text.trim_start(),
    };
    let digits: String = from
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.split('.').take(2).collect::<Vec<_>>().join(".")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    /// A machine: environment variables, the files on it, and which of them
    /// answer their version probe with what.
    struct Machine {
        env: HashMap<&'static str, String>,
        files: Vec<String>,
        versions: HashMap<String, String>,
        configured: Option<&'static str>,
    }

    impl Machine {
        fn new() -> Self {
            let mut env = HashMap::new();
            env.insert("SystemRoot", r"C:\Windows".to_string());
            Self {
                env,
                files: Vec::new(),
                versions: HashMap::new(),
                configured: None,
            }
        }

        /// A file that exists and answers `version`.
        fn runs(mut self, path: &str, version: &str) -> Self {
            self.files.push(path.to_lowercase());
            self.versions
                .insert(path.to_lowercase(), version.to_string());
            self
        }

        /// A file that exists but does not run.
        fn broken(mut self, path: &str) -> Self {
            self.files.push(path.to_lowercase());
            self
        }

        fn env(mut self, name: &'static str, value: &str) -> Self {
            self.env.insert(name, value.to_string());
            self
        }

        fn choose(&self) -> Result<Shell, String> {
            let env = |name: &str| self.env.get(name).map(OsString::from);
            let is_file = |p: &Path| self.files.contains(&p.to_string_lossy().to_lowercase());
            let version = |p: &Path, _: Flavor| {
                self.versions
                    .get(&p.to_string_lossy().to_lowercase())
                    .cloned()
            };
            choose(&Probe {
                env: &env,
                configured: self.configured,
                is_file: &is_file,
                version: &version,
            })
        }
    }

    const GIT_BASH: &str = r"C:\Program Files\Git\bin\bash.exe";
    const WSL_BASH: &str = r"C:\Windows\System32\bash.exe";
    const WIN_PS: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";

    /// **THE OPERATOR'S CHOICE WINS**: `INCODE_SHELL` over `admin.json`, both
    /// over Git Bash; a bare name is looked up on `PATH`.
    #[test]
    fn the_override_wins_env_first_then_settings() {
        let machine = Machine::new()
            .runs(GIT_BASH, "5.2")
            .runs(r"C:\msys64\usr\bin\bash.exe", "5.3")
            .runs(r"C:\tools\pwsh.exe", "7.4")
            .env("PATH", r"C:\tools")
            .env("ProgramFiles", r"C:\Program Files");
        let mut settings = machine;
        settings.configured = Some("pwsh");
        let chosen = settings.choose().expect("a shell");
        assert_eq!(chosen.flavor, Flavor::PowerShell);
        assert_eq!(chosen.label, "PowerShell 7.4");

        let env = settings.env(SHELL_ENV, r"C:\msys64\usr\bin\bash.exe");
        let chosen = env.choose().expect("a shell");
        assert_eq!(chosen.program, PathBuf::from(r"C:\msys64\usr\bin\bash.exe"));
        assert_eq!(chosen.flavor, Flavor::Posix);
        assert_eq!(chosen.label, "bash 5.3");
    }

    /// **GIT BASH IS FOUND** from the `git.exe` on `PATH` — through `cmd\`
    /// and through `mingw64\bin\` — and from Program Files when git is not on
    /// `PATH` at all.
    #[test]
    fn git_bash_is_found_beside_git_or_under_program_files() {
        for git_dir in [
            r"D:\Apps\Git\cmd",
            r"D:\Apps\Git\mingw64\bin",
            r"D:\Apps\Git\bin",
        ] {
            let machine = Machine::new()
                .runs(r"D:\Apps\Git\bin\bash.exe", "5.2")
                .broken(&format!(r"{git_dir}\git.exe"))
                .runs(WIN_PS, "5.1")
                .env("PATH", git_dir);
            let chosen = machine.choose().expect("a shell");
            assert_eq!(
                chosen.program,
                PathBuf::from(r"D:\Apps\Git\bin\bash.exe"),
                "from {git_dir}"
            );
            assert_eq!(chosen.label, "Git Bash (bash 5.2)");
        }
        let machine = Machine::new()
            .runs(GIT_BASH, "5.2")
            .runs(WIN_PS, "5.1")
            .env("ProgramFiles", r"C:\Program Files");
        assert_eq!(
            machine.choose().expect("a shell").program,
            PathBuf::from(GIT_BASH)
        );
    }

    /// **WSL'S BASH IS NEVER USED**: not found through `PATH` (git beside it
    /// or not), and refused when named — the search carries on to PowerShell.
    #[test]
    fn the_wsl_launcher_is_refused() {
        let machine = Machine::new()
            .runs(WSL_BASH, "5.1")
            .broken(r"C:\Windows\System32\git.exe")
            .runs(WIN_PS, "5.1")
            .env("PATH", r"C:\Windows\System32")
            .env(SHELL_ENV, WSL_BASH);
        let chosen = machine.choose().expect("a shell");
        assert_eq!(chosen.program, PathBuf::from(WIN_PS), "{chosen:?}");
        // cmd.exe named is refused too.
        let machine = Machine::new()
            .runs(r"C:\Windows\System32\cmd.exe", "")
            .runs(WIN_PS, "5.1")
            .env(SHELL_ENV, r"C:\Windows\System32\cmd.exe");
        assert_eq!(
            machine.choose().expect("a shell").flavor,
            Flavor::PowerShell
        );
    }

    /// **POWERSHELL WHEN THERE IS NO GIT BASH** — pwsh on `PATH` first — and
    /// a bash that exists but does not run is passed over.
    #[test]
    fn powershell_is_the_fallback() {
        let machine = Machine::new()
            .broken(GIT_BASH)
            .runs(WIN_PS, "5.1")
            .env("ProgramFiles", r"C:\Program Files");
        let chosen = machine.choose().expect("a shell");
        assert_eq!(chosen.flavor, Flavor::PowerShell);
        assert_eq!(chosen.label, "Windows PowerShell 5.1");
        let machine = Machine::new()
            .runs(WIN_PS, "5.1")
            .runs(r"C:\Program Files\PowerShell\7\pwsh.exe", "7.4")
            .env("PATH", r"C:\Program Files\PowerShell\7");
        assert_eq!(machine.choose().expect("a shell").label, "PowerShell 7.4");
    }

    /// **NOTHING FOUND IS SAID PLAINLY**, including the override that was
    /// named and why it was not used — never a silent cmd.exe.
    #[test]
    fn no_shell_at_all_says_why() {
        let machine = Machine::new()
            .runs(r"C:\Windows\System32\cmd.exe", "")
            .env(SHELL_ENV, r"C:\nowhere\bash.exe");
        let why = machine.choose().expect_err("nothing to run");
        assert!(why.contains(r"C:\nowhere\bash.exe"), "{why}");
        assert!(why.contains("does not exist"), "{why}");
        assert!(why.contains("no Git for Windows bash"), "{why}");
    }

    /// Versions are read as major.minor from each probe's own output.
    #[test]
    fn versions_are_read_from_the_probe() {
        assert_eq!(
            version_in(
                Flavor::Posix,
                "GNU bash, version 5.2.37(1)-release (x86_64-pc-msys)\n"
            ),
            "5.2"
        );
        assert_eq!(version_in(Flavor::PowerShell, "5.1.26100.4202\r\n"), "5.1");
        assert_eq!(version_in(Flavor::Posix, "BusyBox v1.36"), "");
    }

    /// On this machine: the description names the shell in use, and a
    /// session whose commands the Windows token confines runs PowerShell and
    /// says so — Git Bash cannot start under that token.
    #[test]
    fn the_description_names_the_shell_in_use() {
        use crate::sandbox::{Sandbox, SandboxMode, SandboxNetwork, SandboxPolicy};
        let off = Sandbox::off();
        let shell = for_sandbox(&off).expect("this machine has a shell");
        let said = describe(&off);
        assert!(said.contains(&shell.label), "{said}");

        let read_only = Sandbox::new(
            SandboxPolicy {
                mode: SandboxMode::ReadOnly,
                network: SandboxNetwork::Allow,
                named: false,
                engine: crate::sandbox::SandboxEngine::Builtin,
            },
            Vec::new(),
        );
        let confined = for_sandbox(&read_only).expect("PowerShell");
        assert_eq!(confined.flavor, Flavor::PowerShell);
        let said = describe(&read_only);
        assert!(
            said.contains(&confined.label) && said.contains("not bash"),
            "{said}"
        );
    }

    /// PowerShell gets the script as written — quotes, backslashes and
    /// cmd's metacharacters intact — prints UTF-8 rather than the console
    /// code page, and reports an error as text, not CLIXML.
    #[test]
    fn powershell_runs_a_script_intact() {
        let ps = powershell().clone().expect("PowerShell is on Windows");
        let out = ps
            .command(
                "Write-Output \"it's \"\"quoted\"\" \u{e9}\u{4e2d}\"; Write-Output 'a\\b&|<>^%'",
            )
            .output()
            .expect("runs");
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            ["it's \"quoted\" \u{e9}\u{4e2d}", "a\\b&|<>^%"],
            "{text}"
        );
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let failed = ps
            .command("Get-Item no-such-file-here")
            .output()
            .expect("runs");
        let err = String::from_utf8_lossy(&failed.stderr);
        assert!(!failed.status.success());
        assert!(
            err.contains("no-such-file-here") && !err.contains("CLIXML"),
            "{err}"
        );
    }

    /// Every argument reaches the program as written — through the quoting
    /// the msys runtime and the C runtime both parse.
    #[test]
    fn quoting_keeps_quotes_and_backslashes() {
        assert_eq!(quote_windows("a b"), r#""a b""#);
        assert_eq!(quote_windows(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_windows(r"end\"), r#""end\\""#);
        assert_eq!(quote_windows(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote_windows(r"a\b"), r#""a\b""#);
        assert_eq!(quote_windows(""), r#""""#);
    }
}
