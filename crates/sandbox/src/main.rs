//! `insbx`: run a command under a sandbox policy.
//!
//! ```text
//! insbx run [options] -- <command> [args...]
//! insbx check [options]          what this machine would enforce of the policy
//! insbx doctor                   what this machine can enforce at all
//! insbx mcp [options]            sandboxes as an MCP server an agent drives
//! ```

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use incord_sandbox::{
    enforcement, policy::Inherit, FsMode, NetMode, Policy, Sandbox, Status, EXIT_SANDBOX,
};

const USAGE: &str = "\
insbx - run any command under one sandbox policy

USAGE:
    insbx run   [OPTIONS] -- <COMMAND> [ARGS...]
    insbx check [OPTIONS]      print what this machine enforces of the policy (JSON with --json)
    insbx doctor               print what this machine can enforce at all
    insbx mcp   [MCP OPTIONS]  serve sandboxes over MCP (stdio): an agent creates one,
                             writes scripts into it, runs them, reads the results
    insbx setup-windows [--read <DIR>]... [--proxy-ports <LO-HI>] | --remove
                             Windows, as administrator, once: network deny and proxy
                             natively (an offline account the firewall cuts off).
                             --read lets that account read a folder (a toolchain);
                             --remove undoes all of it
    insbx grants               Windows: the folders whose write grant is kept, and whether
                             each still carries it
    insbx revoke <DIR>... | --all [--msys-leftovers]
                             Windows: take a folder's kept grant back (the msys account's
                             access too); its next run grants it again. --msys-leftovers
                             clears what a crashed --msys run left that its sweep could not
                             take back (a moved root...), which refuses every --msys run

KEPT GRANTS (Windows):
    A writable root's grant is written into every file beneath it, so it is made by the
    first run on that folder and then KEPT: later runs only check it, instead of walking
    the tree twice per run. Each folder has its own SID, and a run's token carries only
    its own roots' SIDs, so a kept grant lets no other run write there. A file moved into
    a root from elsewhere keeps its old ACL (not writable to the command) until
    `icacls <FILE> /reset` gives it its folder's.

MCP OPTIONS (the ceilings; an agent can ask for less, never more):
        --root <DIR>         where sandboxes live (default: <temp>/insbx-mcp-<pid>)
        --net <MODE>         most network a sandbox may have: deny (default) | proxy | allow
        --allow-host <HOST>  with --net proxy: a host sandboxes may name (repeatable)
        --max-wall <SECS>    longest one run may take (default 600)
    -p, --policy <FILE>      base policy (environment, limits) every sandbox starts from
        --keep               leave sandboxes on disk when the server stops
        --best-effort        run even if part of the policy cannot be enforced here

POLICY (a file, then flags on top of it):
    -p, --policy <FILE>      TOML (or .json) policy file
        --fs <MODE>          off | read-only | workspace-write
    -w, --write <DIR>        writable root (implies --fs workspace-write; repeatable)
        --deny-read <PATH>   hide this path entirely, reads included (repeatable)
        --msys               Windows, with --net deny or proxy: run as the account insbx-msys
                             so Git Bash (msys) works. Costs per-run grants (each root is
                             walked, then taken back) and one such run at a time. Refused
                             with --net allow; ignored on Linux and macOS
        --net <MODE>         allow | deny | proxy
        --allow-host <HOST>  host the proxy lets through (implies --net proxy; repeatable)
        --allow-private      an allowed host may resolve to a private address
        --allow-loopback-port <PORT>
                             this machine's own port the proxy may reach (localhost,
                             127.0.0.1, ::1 only; implies --net proxy; repeatable)
        --env-none          start from an empty environment (essentials kept)
        --pass-env <NAME>    keep this variable with --env-none (repeatable; NAME* for a prefix)
        --unset-env <NAME>   remove this variable (repeatable; NAME* for a prefix)
        --set-env <K=V>      set a variable (repeatable)
        --keep-agent-session keep CLAUDECODE / CLAUDE_CODE_SESSION* (stripped by default)
        --keep-git-redirects keep GIT_DIR / GIT_WORK_TREE / ... (stripped by default)
        --no-protect-git     let the command change .git/hooks and .git/config (by default
                             they are put back after the run: a planted hook would run,
                             unsandboxed, the next time anyone uses git there)
        --wall <SECS>        kill the command (and everything it started) after SECS
        --cpu <SECS>         CPU seconds per process
        --mem <MB>           address space per process
        --fsize <MB>         largest file a process may write
        --nofile <N>         open files per process
        --cwd <DIR>          start the command here (default: the current directory)
        --best-effort        run even if part of the policy cannot be enforced here
    -q, --quiet              no report on stderr

EXIT:
    the command's own code; 124 when --wall killed it; 125 when insbx refused or failed;
    126/127 when the command could not be run / found.
";

fn main() -> ExitCode {
    incord_sandbox::enter_if_requested();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // On Windows: natively, or forwarded into WSL2 when the policy needs
    // what only the Linux sandbox enforces (see `incord_sandbox::backend`).
    #[cfg(windows)]
    if let Some(code) = windows_backend::dispatch(&args) {
        return code;
    }
    match real_main(args) {
        Ok(code) => exit_with(code),
        Err(e) => {
            eprintln!("insbx: {e}");
            ExitCode::from(EXIT_SANDBOX as u8)
        }
    }
}

/// The command's exit code as this process's own. Windows codes are 32
/// bits (a crash is `0xC0000005`, negative as an `i32`): clamping one into
/// 0..=255 could turn a crash into success, so they are passed on whole.
fn exit_with(code: i32) -> ExitCode {
    #[cfg(windows)]
    if !(0..=255).contains(&code) {
        std::process::exit(code);
    }
    ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

/// `insbx.exe` on Windows: run natively, or forward to the Linux `insbx` inside
/// WSL2, where Landlock and seccomp do the confining.
#[cfg(windows)]
mod windows_backend {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::{Command, ExitCode, Stdio};

    use incord_sandbox::backend::{choose, Backend, Choice, WSL_SETUP};
    use incord_sandbox::enforcement;
    use incord_sandbox::wslpath::{to_wsl, translate_args};

    /// Where the installer puts the Linux `insbx` inside WSL.
    const IN_WSL: &str = "$HOME/.local/bin/insbx";

    /// `Some(exit)` when this invocation was forwarded (or refused); `None`
    /// to run it natively.
    pub fn dispatch(args: &[OsString]) -> Option<ExitCode> {
        let backend = match Backend::from_env(|k| std::env::var(k).ok()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("insbx: {e}");
                return Some(ExitCode::from(125));
            }
        };
        let sub = args.first().and_then(|a| a.to_str()).unwrap_or("");
        if backend == Backend::Wsl
            && !matches!(
                sub,
                "" | "-h"
                    | "--help"
                    | "help"
                    | "-V"
                    | "--version"
                    | "setup-windows"
                    | "grants"
                    | "revoke"
            )
        {
            return Some(forward(args));
        }
        // What native would leave unenforced. A command line that does not
        // parse runs natively, which reports the mistake.
        let (gaps, best_effort) = match sub {
            "run" | "check" => {
                let o = super::parse(&args[1..]).ok()?;
                (enforcement(&o.policy).gaps(), o.best_effort)
            }
            "mcp" => {
                let (_, c) = super::parse_mcp(&args[1..]).ok()?;
                (enforcement(&c.widest_policy()).gaps(), c.best_effort)
            }
            _ => return None,
        };
        match choose(backend, &gaps, best_effort, wsl_ready) {
            Choice::Native => None,
            Choice::Wsl => Some(forward(args)),
            // `check` is a report: say what native enforces (exit 1 for the
            // gaps) and why WSL was not used.
            Choice::Refuse(why) if sub == "check" => {
                eprintln!("insbx: {why}");
                None
            }
            Choice::Refuse(why) => {
                eprintln!("insbx: {why}");
                Some(ExitCode::from(125))
            }
        }
    }

    fn wsl_exe() -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join("System32").join("wsl.exe")
    }

    /// Whether forwarding would work: `wsl.exe` exists and the Linux `insbx`
    /// is installed in the distribution. Starts WSL, so asked only when it
    /// matters.
    pub fn wsl_ready() -> Result<(), String> {
        let exe = wsl_exe();
        if !exe.is_file() {
            return Err("wsl.exe was not found: WSL is not installed".into());
        }
        let mut cmd = Command::new(&exe);
        if let Some(distro) = std::env::var_os("INSBX_WSL_DISTRO") {
            cmd.arg("-d").arg(distro);
        }
        let status = cmd
            .args(["-e", "sh", "-c", &format!("[ -x \"{IN_WSL}\" ]")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(_) => Err(format!(
                "the Linux insbx is not installed inside WSL at {IN_WSL}, or WSL has no Linux \
                 distribution"
            )),
            Err(e) => Err(format!("wsl.exe cannot start: {e}")),
        }
    }

    /// The Windows lines of `insbx doctor`: the backend, and whether WSL2
    /// forwarding is there for what native cannot enforce.
    pub fn doctor() {
        match Backend::from_env(|k| std::env::var(k).ok()) {
            Ok(b) => println!(
                "  [info] backend        INSBX_BACKEND={}: {}",
                b.as_str(),
                match b {
                    Backend::Auto => "native when it enforces the whole policy, else WSL2",
                    Backend::Native => "always native",
                    Backend::Wsl => "always WSL2",
                }
            ),
            Err(e) => println!("  [no  ] backend        {e}"),
        }
        match incord_sandbox::windows::offline_setup() {
            Ok(r) => println!(
                "  [ok  ] offline user   {} and {} (--msys) and their firewall filters are set up \
                 (insbx setup-windows): network deny and proxy run natively (proxy ports {}-{})",
                incord_sandbox::windows::offline::ACCOUNT,
                incord_sandbox::windows::offline::MSYS_ACCOUNT,
                r.proxy_ports.0,
                r.proxy_ports.1
            ),
            Err(why) => println!(
                "  [no  ] offline user   {why}; for network deny natively, run `insbx setup-windows` \
                 as administrator"
            ),
        }
        match wsl_ready() {
            Ok(()) => println!(
                "  [ok  ] WSL2           forwarding available: network deny/proxy and deny_read \
                 are enforced there (run `INSBX_BACKEND=wsl insbx doctor` for its report)"
            ),
            Err(why) => println!("  [no  ] WSL2           {why}; to have it, {WSL_SETUP}"),
        }
    }

    pub fn forward(args: &[OsString]) -> ExitCode {
        match run(args) {
            Ok(code) => super::exit_with(code),
            Err(e) => {
                eprintln!("insbx: {e}");
                ExitCode::from(125)
            }
        }
    }

    fn run(args: &[OsString]) -> Result<i32, String> {
        let args: Vec<String> = args
            .iter()
            .map(|a| {
                a.to_str()
                    .map(str::to_string)
                    .ok_or("an argument is not valid UTF-8")
            })
            .collect::<Result<_, _>>()?;
        let args = translate_args(&args)?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let cwd = to_wsl(&cwd.display().to_string()).ok_or_else(|| {
            format!(
                "the current folder {} has no path inside WSL; run from a drive folder",
                cwd.display()
            )
        })?;
        let mut cmd = Command::new(wsl_exe());
        if let Some(distro) = std::env::var_os("INSBX_WSL_DISTRO") {
            cmd.arg("-d").arg(distro);
        }
        cmd.arg("--cd")
            .arg(&cwd)
            .arg("-e")
            .arg("sh")
            .arg("-c")
            .arg(format!(
                "[ -x \"{IN_WSL}\" ] || {{ echo 'insbx is not installed inside WSL' >&2; exit 126; }}; \
                 exec \"{IN_WSL}\" \"$@\""
            ))
            .arg("insbx")
            .args(&args);
        let status = cmd
            .status()
            .map_err(|e| format!("cannot start wsl.exe ({e}); {WSL_SETUP}"))?;
        let code = status.code().unwrap_or(1);
        // wsl.exe itself fails with -1 (no WSL, no distribution); the
        // command's own codes pass through.
        if code < 0 || code == 126 {
            eprintln!(
                "insbx: forwarding to WSL2 failed; {WSL_SETUP}. Or set INSBX_BACKEND=native to run \
                 natively (writes, environment and limits confined; network and deny_read not)."
            );
        }
        Ok(code)
    }
}

struct Options {
    policy: Policy,
    cwd: PathBuf,
    best_effort: bool,
    quiet: bool,
    json: bool,
    command: Vec<OsString>,
}

fn real_main(args: Vec<OsString>) -> Result<i32, String> {
    let Some(sub) = args.first().and_then(|a| a.to_str()) else {
        print!("{USAGE}");
        return Ok(2);
    };
    match sub {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(0)
        }
        "-V" | "--version" => {
            println!("insbx {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "doctor" => {
            doctor();
            Ok(0)
        }
        "check" => {
            let o = parse(&args[1..])?;
            let e = enforcement(&o.policy);
            if o.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&e).map_err(|e| e.to_string())?
                );
            } else {
                println!("{}", e.report());
            }
            Ok(if e.gaps().is_empty() { 0 } else { 1 })
        }
        "run" => run(parse(&args[1..])?),
        "mcp" => mcp(&args[1..]),
        "setup-windows" => setup_windows(&args[1..]),
        "grants" => grants(),
        "revoke" => revoke(&args[1..]),
        other => Err(format!("unknown command `{other}` (try `insbx --help`)")),
    }
}

fn run(o: Options) -> Result<i32, String> {
    let (program, rest) = o
        .command
        .split_first()
        .ok_or("nothing to run: give the command after `--`")?;
    let sandbox = Sandbox::new(o.policy).strict(!o.best_effort);
    let e = sandbox.enforcement();
    if !o.quiet {
        for gap in e.gaps() {
            eprintln!("insbx: {gap}");
        }
    }
    let outcome = sandbox.run(program, rest, &o.cwd)?;
    if !o.quiet {
        for d in outcome.decisions.iter().filter(|d| !d.allowed) {
            eprintln!("insbx: network refused {} ({})", d.target, d.reason);
        }
        for r in &outcome.restored {
            eprintln!("insbx: protected path changed by the command, {r}");
        }
        if outcome.timed_out {
            eprintln!("insbx: killed after the wall-clock limit");
        }
    }
    Ok(outcome.code())
}

/// `insbx grants`: every folder whose write grant is kept
/// (`incord_sandbox::windows::kept_folders`), one per line, `granted` or
/// `missing` (its ACE is gone: the next run grants it again).
fn grants() -> Result<i32, String> {
    #[cfg(windows)]
    {
        let kept = incord_sandbox::windows::kept_folders()?;
        if kept.is_empty() {
            println!("insbx grants: none kept");
        }
        for k in kept {
            let state = if k.granted { "granted" } else { "missing" };
            println!("{state:<8} {}", strip_verbatim(&k.path));
        }
        Ok(0)
    }
    #[cfg(not(windows))]
    {
        Err("kept grants are Windows only: Landlock and Seatbelt grant nothing on disk".into())
    }
}

/// `insbx revoke <DIR>... | --all [--msys-leftovers]`: take kept grants back
/// (`incord_sandbox::windows::revoke_kept`), the offline and msys accounts'
/// too when set up; with `--msys-leftovers`, clear what a crashed msys run
/// left (`incord_sandbox::windows::clear_msys_leftovers`). Every folder is
/// tried; the exit code is 1 when any failed.
fn revoke(args: &[OsString]) -> Result<i32, String> {
    let mut words: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().ok_or("an argument is not valid Unicode"))
        .collect::<Result<_, _>>()?;
    let leftovers = words.contains(&"--msys-leftovers");
    words.retain(|w| *w != "--msys-leftovers");
    if words.is_empty() && !leftovers {
        return Err("revoke what? give folders, or --all, or --msys-leftovers".into());
    }
    #[cfg(windows)]
    {
        use incord_sandbox::windows;
        let setup = windows::offline_setup().ok();
        let accounts: Vec<String> = setup
            .iter()
            .flat_map(|r| [r.sid.clone(), r.msys_sid.clone()])
            .collect();
        let mut failed = false;
        if leftovers {
            let setup = setup.as_ref().ok_or(
                "--msys-leftovers needs the setup (`insbx setup-windows`): see `insbx doctor`",
            )?;
            for line in windows::clear_msys_leftovers(&setup.msys_sid)? {
                println!("insbx revoke: {line}");
            }
        }
        let folders: Vec<PathBuf> = if words.is_empty() {
            Vec::new()
        } else if words == ["--all"] {
            windows::kept_folders()?
                .into_iter()
                .map(|k| k.path)
                .collect()
        } else if words.contains(&"--all") {
            return Err("--all takes no folders".into());
        } else {
            words
                .iter()
                .map(|w| std::fs::canonicalize(w).map_err(|e| format!("{w}: {e}")))
                .collect::<Result<_, _>>()?
        };
        for folder in folders {
            match windows::revoke_kept(&folder, &accounts) {
                Ok(()) => println!("insbx revoke: {}", strip_verbatim(&folder)),
                Err(e) => {
                    eprintln!("insbx revoke: {}: {e}", strip_verbatim(&folder));
                    failed = true;
                }
            }
        }
        Ok(i32::from(failed))
    }
    #[cfg(not(windows))]
    {
        Err("kept grants are Windows only: Landlock and Seatbelt grant nothing on disk".into())
    }
}

/// A path as the user wrote it: `canonicalize`'s `\\?\` dropped.
#[cfg(windows)]
fn strip_verbatim(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map_or(s.clone(), str::to_string)
}

/// `insbx setup-windows`: see [`incord_sandbox::windows::offline`].
fn setup_windows(args: &[OsString]) -> Result<i32, String> {
    let args: Vec<String> = args
        .iter()
        .map(|a| {
            a.to_str()
                .map(str::to_string)
                .ok_or_else(|| "an argument is not valid Unicode".to_string())
        })
        .collect::<Result<_, _>>()?;
    let parsed = incord_sandbox::windows::offline::parse_setup_args(&args)?;
    #[cfg(windows)]
    {
        for line in incord_sandbox::windows::setup_windows(&parsed)? {
            println!("insbx setup-windows: {line}");
        }
        if parsed.remove {
            println!("insbx setup-windows: removed; network deny now needs WSL2 again");
        } else {
            println!(
                "insbx setup-windows: done; `insbx run --net deny ...` now runs natively (see `insbx doctor`)"
            );
        }
        Ok(0)
    }
    #[cfg(not(windows))]
    {
        let _ = parsed;
        Err(
            "setup-windows is for Windows only: it sets up network deny for the native \
             Windows sandbox"
                .to_string(),
        )
    }
}

fn mcp(args: &[OsString]) -> Result<i32, String> {
    let (root, mut ceilings) = parse_mcp(args)?;
    let root = root
        .unwrap_or_else(|| std::env::temp_dir().join(format!("insbx-mcp-{}", std::process::id())));
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    ceilings.root =
        incord_sandbox::plain_path(std::fs::canonicalize(&root).map_err(|e| e.to_string())?);
    let stdin = std::io::stdin();
    incord_sandbox::server::Server::new(ceilings)
        .serve(stdin.lock(), std::io::stdout().lock())
        .map_err(|e| format!("mcp: {e}"))?;
    Ok(0)
}

/// `insbx mcp`'s options: the sandboxes' root (if given) and the ceilings.
fn parse_mcp(
    args: &[OsString],
) -> Result<(Option<PathBuf>, incord_sandbox::server::Ceilings), String> {
    use incord_sandbox::server::Ceilings;
    let mut root: Option<PathBuf> = None;
    let mut ceilings = Ceilings::new(PathBuf::new());
    let mut i = 0;
    let value = |i: usize| -> Result<String, String> {
        args.get(i)
            .and_then(|v| v.to_str())
            .map(str::to_string)
            .ok_or_else(|| "a value is missing after an option".to_string())
    };
    while i < args.len() {
        let flag = args[i].to_str().unwrap_or("");
        match flag {
            "--root" => {
                i += 1;
                root = Some(PathBuf::from(value(i)?));
            }
            "--net" => {
                i += 1;
                let v = value(i)?;
                ceilings.network =
                    NetMode::parse(&v).ok_or(format!("--net: unknown mode `{v}`"))?;
            }
            "--allow-host" => {
                i += 1;
                ceilings.hosts.push(value(i)?);
            }
            "--max-wall" => {
                i += 1;
                let v = value(i)?;
                ceilings.max_wall_secs = v
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(format!("--max-wall: `{v}` is not a positive number"))?;
            }
            "-p" | "--policy" => {
                i += 1;
                ceilings.base = Policy::load(&PathBuf::from(value(i)?))?;
            }
            "--keep" => ceilings.keep = true,
            "--best-effort" => ceilings.best_effort = true,
            other => return Err(format!("insbx mcp: unknown option `{other}`")),
        }
        i += 1;
    }
    if !ceilings.hosts.is_empty() && ceilings.network == NetMode::Deny {
        ceilings.network = NetMode::Proxy;
    }
    Ok((root, ceilings))
}

fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut policy: Option<Policy> = None;
    let mut flags: Vec<(String, Option<String>)> = Vec::new();
    let mut command = Vec::new();
    let mut cwd = None;
    let (mut best_effort, mut quiet, mut json) = (false, false, false);

    let mut i = 0;
    while i < args.len() {
        let arg = args[i]
            .to_str()
            .ok_or_else(|| format!("argument {i} is not valid UTF-8"))?;
        if arg == "--" {
            command = args[i + 1..].to_vec();
            break;
        }
        let value = if takes_value(arg) {
            i += 1;
            Some(
                args.get(i)
                    .and_then(|v| v.to_str())
                    .ok_or_else(|| format!("{arg} needs a value"))?
                    .to_string(),
            )
        } else {
            None
        };
        match arg {
            "-p" | "--policy" => {
                policy = Some(Policy::load(&PathBuf::from(value.unwrap_or_default()))?);
            }
            "--cwd" => cwd = value.map(PathBuf::from),
            "--best-effort" => best_effort = true,
            "-q" | "--quiet" => quiet = true,
            "--json" => json = true,
            _ => flags.push((arg.to_string(), value)),
        }
        i += 1;
    }

    // Flags apply on top of the file, whatever order they were given in.
    let mut policy = policy.unwrap_or_default();
    for (flag, value) in flags {
        apply_flag(&mut policy, &flag, value.unwrap_or_default())?;
    }

    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir().map_err(|e| format!("no current directory: {e}"))?,
    };
    Ok(Options {
        policy,
        cwd,
        best_effort,
        quiet,
        json,
        command,
    })
}

/// Whether `arg` is an `insbx run`/`check` option followed by a value.
fn takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-p" | "--policy"
            | "--fs"
            | "-w"
            | "--write"
            | "--deny-read"
            | "--net"
            | "--allow-host"
            | "--allow-loopback-port"
            | "--pass-env"
            | "--unset-env"
            | "--set-env"
            | "--wall"
            | "--cpu"
            | "--mem"
            | "--fsize"
            | "--nofile"
            | "--cwd"
    )
}

/// One policy flag applied on top of the policy file: the filesystem,
/// network, environment or limits option it names, or an error for one that
/// is none of them.
fn apply_flag(policy: &mut Policy, flag: &str, v: String) -> Result<(), String> {
    let handled = apply_fs_flag(policy, flag, &v)?
        || apply_net_flag(policy, flag, &v)?
        || apply_env_flag(policy, flag, &v)?
        || apply_limit_flag(policy, flag, &v)?;
    if handled {
        Ok(())
    } else {
        Err(format!("unknown option `{flag}` (try `insbx --help`)"))
    }
}

/// `--fs`, `-w`/`--write`, `--deny-read`, `--no-protect-git`, `--msys`;
/// `Ok(false)` for any other flag.
fn apply_fs_flag(policy: &mut Policy, flag: &str, v: &str) -> Result<bool, String> {
    match flag {
        "--fs" => {
            policy.fs.mode = FsMode::parse(v).ok_or(format!("--fs: unknown mode `{v}`"))?;
        }
        "-w" | "--write" => {
            policy.fs.mode = FsMode::WorkspaceWrite;
            policy.fs.write.push(PathBuf::from(v));
        }
        "--deny-read" => {
            if policy.fs.mode == FsMode::Off {
                policy.fs.mode = FsMode::ReadOnly;
            }
            policy.fs.deny_read.push(PathBuf::from(v));
        }
        "--no-protect-git" => policy.fs.protect_git = false,
        "--msys" => policy.fs.msys = true,
        _ => return Ok(false),
    }
    Ok(true)
}

/// `--net`, `--allow-host`, `--allow-private`, `--allow-loopback-port`;
/// `Ok(false)` for any other flag.
fn apply_net_flag(policy: &mut Policy, flag: &str, v: &str) -> Result<bool, String> {
    match flag {
        "--net" => {
            policy.network.mode = NetMode::parse(v).ok_or(format!("--net: unknown mode `{v}`"))?;
        }
        "--allow-host" => {
            policy.network.mode = NetMode::Proxy;
            policy.network.allow.push(v.to_string());
        }
        "--allow-private" => policy.network.allow_private = true,
        "--allow-loopback-port" => {
            let port: u16 = v
                .parse()
                .ok()
                .filter(|p| *p != 0)
                .ok_or(format!("--allow-loopback-port: `{v}` is not a port"))?;
            policy.network.mode = NetMode::Proxy;
            policy.network.loopback_ports.push(port);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// `--env-none`, `--pass-env`, `--unset-env`, `--set-env`,
/// `--keep-agent-session`, `--keep-git-redirects`; `Ok(false)` for any other
/// flag.
fn apply_env_flag(policy: &mut Policy, flag: &str, v: &str) -> Result<bool, String> {
    match flag {
        "--env-none" => policy.env.inherit = Inherit::None,
        "--pass-env" => policy.env.pass.push(v.to_string()),
        "--unset-env" => policy.env.remove.push(v.to_string()),
        "--set-env" => {
            let (k, val) = v
                .split_once('=')
                .ok_or(format!("--set-env: expected K=V, got `{v}`"))?;
            policy.env.set.insert(k.to_string(), val.to_string());
        }
        "--keep-agent-session" => policy.env.strip_agent_session = false,
        "--keep-git-redirects" => policy.env.strip_git_redirects = false,
        _ => return Ok(false),
    }
    Ok(true)
}

/// `--wall`, `--cpu`, `--mem`, `--fsize`, `--nofile`; `Ok(false)` for any
/// other flag.
fn apply_limit_flag(policy: &mut Policy, flag: &str, v: &str) -> Result<bool, String> {
    let num = || {
        v.parse::<u64>()
            .map_err(|_| format!("{flag}: `{v}` is not a number"))
    };
    match flag {
        "--wall" => policy.limits.wall_secs = Some(num()?),
        "--cpu" => policy.limits.cpu_secs = Some(num()?),
        "--mem" => policy.limits.memory_mb = Some(num()?),
        "--fsize" => policy.limits.file_size_mb = Some(num()?),
        "--nofile" => policy.limits.open_files = Some(num()?),
        _ => return Ok(false),
    }
    Ok(true)
}

fn doctor() {
    let probe = |fs, net| {
        let mut p = Policy::default();
        p.fs.mode = fs;
        p.network.mode = net;
        p.limits.wall_secs = Some(1);
        p.limits.cpu_secs = Some(1);
        enforcement(&p)
    };
    let full = probe(FsMode::WorkspaceWrite, NetMode::Proxy);
    let deny = probe(FsMode::Off, NetMode::Deny);
    let hidden = {
        let mut p = Policy::default();
        p.fs.mode = FsMode::ReadOnly;
        p.fs.deny_read.push(PathBuf::from("hidden"));
        enforcement(&p)
    };
    println!(
        "insbx {} on {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    let show = |what: &str, s: &Status| match s {
        Status::Enforced(how) => println!("  [ok  ] {what:<14} {how}"),
        Status::NotEnforced(why) => println!("  [no  ] {what:<14} {why}"),
        Status::NotRequested => {}
    };
    show("filesystem", &full.filesystem);
    show("deny_read", &hidden.filesystem);
    show("network deny", &deny.network);
    show("network proxy", &full.network);
    show("limits", &full.limits);
    show("environment", &full.environment);
    match incord_sandbox::proxy::Upstream::from_env(|k| std::env::var(k).ok()) {
        Some(_) => println!(
            "  [note] proxy          this process sits behind an HTTP proxy; allowed requests \
             are chained through it, so the private-address check is that proxy's to make"
        ),
        None => println!("  [ok  ] proxy          connects directly; allowed names that resolve to private addresses are refused"),
    }
    #[cfg(windows)]
    windows_backend::doctor();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// `--allow-loopback-port` names this machine's port, puts the network
    /// behind the proxy, repeats, and leaves `allow_private` off.
    #[test]
    fn a_loopback_port_flag_sets_the_port_and_the_proxy() {
        let o = parse(&args(&[
            "--allow-loopback-port",
            "5173",
            "--allow-loopback-port",
            "8080",
            "--",
            "true",
        ]))
        .expect("parses");
        assert_eq!(o.policy.network.mode, NetMode::Proxy);
        assert_eq!(o.policy.network.loopback_ports, [5173, 8080]);
        assert!(o.policy.network.allow.is_empty());
        assert!(!o.policy.network.allow_private);
        assert_eq!(o.command, args(&["true"]));
    }

    /// Each kind of flag lands in its part of the policy, on top of the
    /// defaults; a bad value and an unknown flag are refused by name.
    #[test]
    fn each_policy_flag_lands_where_it_belongs() {
        let o = parse(&args(&[
            "-w",
            "/w",
            "--deny-read",
            "/secret",
            "--no-protect-git",
            "--msys",
            "--allow-host",
            "crates.io",
            "--allow-private",
            "--env-none",
            "--pass-env",
            "KEEP",
            "--set-env",
            "CI=1",
            "--keep-git-redirects",
            "--wall",
            "30",
            "--nofile",
            "64",
            "--",
            "true",
        ]))
        .expect("parses");
        let p = &o.policy;
        assert_eq!(p.fs.mode, FsMode::WorkspaceWrite);
        assert_eq!(p.fs.write, [PathBuf::from("/w")]);
        assert_eq!(p.fs.deny_read, [PathBuf::from("/secret")]);
        assert!(!p.fs.protect_git);
        assert!(p.fs.msys);
        assert_eq!(p.network.mode, NetMode::Proxy);
        assert_eq!(p.network.allow, ["crates.io"]);
        assert!(p.network.allow_private);
        assert_eq!(p.env.inherit, Inherit::None);
        assert_eq!(p.env.pass, ["KEEP"]);
        assert_eq!(p.env.set.get("CI").map(String::as_str), Some("1"));
        assert!(!p.env.strip_git_redirects);
        assert!(
            p.env.strip_agent_session,
            "a flag not given keeps its default"
        );
        assert_eq!(p.limits.wall_secs, Some(30));
        assert_eq!(p.limits.open_files, Some(64));

        for (bad, reason) in [
            (&["--wall", "soon"][..], "is not a number"),
            (&["--fs", "everything"][..], "unknown mode"),
            (&["--set-env", "NOEQUALS"][..], "expected K=V"),
            (&["--frobnicate"][..], "unknown option `--frobnicate`"),
        ] {
            let mut words = bad.to_vec();
            words.extend(["--", "true"]);
            let err = parse(&args(&words))
                .err()
                .unwrap_or_else(|| panic!("{bad:?} should be refused"));
            assert!(err.contains(reason), "{bad:?}: {err}");
        }
    }

    /// Port 0, a word and a number past 65535 are refused, as is the flag
    /// with no value.
    #[test]
    fn a_loopback_port_that_is_not_a_port_is_refused() {
        for bad in ["0", "web", "70000", "-1"] {
            let err = parse(&args(&["--allow-loopback-port", bad, "--", "true"]))
                .err()
                .unwrap_or_else(|| panic!("`{bad}` should be refused"));
            assert!(err.contains("is not a port"), "{bad}: {err}");
        }
        assert!(parse(&args(&["--allow-loopback-port"])).is_err());
    }
}
