//! `sbx`: run a command under a sandbox policy.
//!
//! ```text
//! sbx run [options] -- <command> [args...]
//! sbx check [options]          what this machine would enforce of the policy
//! sbx doctor                   what this machine can enforce at all
//! ```

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use incord_sandbox::{
    enforcement, policy::Inherit, FsMode, NetMode, Policy, Sandbox, Status, EXIT_SANDBOX,
};

const USAGE: &str = "\
sbx - run any command under one sandbox policy

USAGE:
    sbx run   [OPTIONS] -- <COMMAND> [ARGS...]
    sbx check [OPTIONS]      print what this machine enforces of the policy (JSON with --json)
    sbx doctor               print what this machine can enforce at all

POLICY (a file, then flags on top of it):
    -p, --policy <FILE>      TOML (or .json) policy file
        --fs <MODE>          off | read-only | workspace-write
    -w, --write <DIR>        writable root (implies --fs workspace-write; repeatable)
        --deny-read <PATH>   hide this path entirely, reads included (repeatable)
        --net <MODE>         allow | deny | proxy
        --allow-host <HOST>  host the proxy lets through (implies --net proxy; repeatable)
        --allow-private      an allowed host may resolve to a private address
        --env-none           start from an empty environment (essentials kept)
        --pass-env <NAME>    keep this variable with --env-none (repeatable; NAME* for a prefix)
        --unset-env <NAME>   remove this variable (repeatable; NAME* for a prefix)
        --set-env <K=V>      set a variable (repeatable)
        --keep-agent-session keep CLAUDECODE / CLAUDE_CODE_SESSION* (stripped by default)
        --keep-git-redirects keep GIT_DIR / GIT_WORK_TREE / ... (stripped by default)
        --wall <SECS>        kill the command (and everything it started) after SECS
        --cpu <SECS>         CPU seconds per process
        --mem <MB>           address space per process
        --fsize <MB>         largest file a process may write
        --nofile <N>         open files per process
        --cwd <DIR>          start the command here (default: the current directory)
        --best-effort        run even if part of the policy cannot be enforced here
    -q, --quiet              no report on stderr

EXIT:
    the command's own code; 124 when --wall killed it; 125 when sbx refused or failed;
    126/127 when the command could not be run / found.
";

fn main() -> ExitCode {
    incord_sandbox::enter_if_requested();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match real_main(args) {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1)),
        Err(e) => {
            eprintln!("sbx: {e}");
            ExitCode::from(EXIT_SANDBOX as u8)
        }
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
            println!("sbx {}", env!("CARGO_PKG_VERSION"));
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
        other => Err(format!("unknown command `{other}` (try `sbx --help`)")),
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
            eprintln!("sbx: {gap}");
        }
    }
    let outcome = sandbox.run(program, rest, &o.cwd)?;
    if !o.quiet {
        for d in outcome.decisions.iter().filter(|d| !d.allowed) {
            eprintln!("sbx: network refused {} ({})", d.target, d.reason);
        }
        if outcome.timed_out {
            eprintln!("sbx: killed after the wall-clock limit");
        }
    }
    Ok(outcome.code())
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
        let takes_value = matches!(
            arg,
            "-p" | "--policy"
                | "--fs"
                | "-w"
                | "--write"
                | "--deny-read"
                | "--net"
                | "--allow-host"
                | "--pass-env"
                | "--unset-env"
                | "--set-env"
                | "--wall"
                | "--cpu"
                | "--mem"
                | "--fsize"
                | "--nofile"
                | "--cwd"
        );
        let value = if takes_value {
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
        let v = value.unwrap_or_default();
        let num = |v: &str| {
            v.parse::<u64>()
                .map_err(|_| format!("{flag}: `{v}` is not a number"))
        };
        match flag.as_str() {
            "--fs" => {
                policy.fs.mode = FsMode::parse(&v).ok_or(format!("--fs: unknown mode `{v}`"))?;
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
            "--net" => {
                policy.network.mode =
                    NetMode::parse(&v).ok_or(format!("--net: unknown mode `{v}`"))?;
            }
            "--allow-host" => {
                policy.network.mode = NetMode::Proxy;
                policy.network.allow.push(v);
            }
            "--allow-private" => policy.network.allow_private = true,
            "--env-none" => policy.env.inherit = Inherit::None,
            "--pass-env" => policy.env.pass.push(v),
            "--unset-env" => policy.env.remove.push(v),
            "--set-env" => {
                let (k, val) = v
                    .split_once('=')
                    .ok_or(format!("--set-env: expected K=V, got `{v}`"))?;
                policy.env.set.insert(k.to_string(), val.to_string());
            }
            "--keep-agent-session" => policy.env.strip_agent_session = false,
            "--keep-git-redirects" => policy.env.strip_git_redirects = false,
            "--wall" => policy.limits.wall_secs = Some(num(&v)?),
            "--cpu" => policy.limits.cpu_secs = Some(num(&v)?),
            "--mem" => policy.limits.memory_mb = Some(num(&v)?),
            "--fsize" => policy.limits.file_size_mb = Some(num(&v)?),
            "--nofile" => policy.limits.open_files = Some(num(&v)?),
            other => return Err(format!("unknown option `{other}` (try `sbx --help`)")),
        }
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
    println!(
        "sbx {} on {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    let show = |what: &str, s: &Status| match s {
        Status::Enforced(how) => println!("  [ok  ] {what:<14} {how}"),
        Status::NotEnforced(why) => println!("  [no  ] {what:<14} {why}"),
        Status::NotRequested => {}
    };
    show("filesystem", &full.filesystem);
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
}
