//! The helper: the calling binary re-executed with [`ENTER_ARG`], which
//! applies the confinement to itself and then becomes the command (exec on
//! Unix; on Windows it starts the command confined and stays to pass its
//! exit code on).
//!
//! The parent side, which decides the policy and starts this process, is
//! [`Sandbox::command`] in the crate root; it hands the helper an
//! [`EnterSpec`] as JSON in its first argument.

use super::*;

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
///
/// The steps: the Windows inner helper ([`enter_inner`]), the arguments
/// ([`helper_args`]), the tie to the parent ([`bind_to_parent`]), the
/// confinement ([`confine`]), then supervise or exec the command.
fn enter(args: &[OsString]) -> Result<Never, EnterError> {
    #[cfg(windows)]
    if args.len() == 1 && args[0] == INNER_ARG {
        return enter_inner();
    }
    let (spec, program, program_args) = helper_args(args)?;
    // Off Unix and Windows nothing in the spec is enforced by the helper: the
    // parent already refused (strict) or reported (best effort) every gap.
    #[cfg(not(any(unix, windows)))]
    let _ = &spec;
    #[cfg(unix)]
    let parent = bind_to_parent(spec.supervise)?;
    #[cfg(unix)]
    confine(&spec)?;

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

/// Windows, the second helper of a network-deny run: the spec, program and
/// arguments come in [`INNER_ENV`], which the command never sees.
#[cfg(windows)]
fn enter_inner() -> Result<Never, EnterError> {
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
    match windows::enter(&launch.spec, &OsString::from(launch.program), &args) {
        Ok(code) => std::process::exit(code),
        Err((message, code)) => Err(EnterError { message, code }),
    }
}

/// `<spec-json> -- <program> [args...]`: the spec, the program and its
/// arguments.
fn helper_args(args: &[OsString]) -> Result<(EnterSpec, &OsString, &[OsString]), EnterError> {
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
    Ok((spec, program, &args[3..]))
}

/// If the parent dies (SIGKILL included, which nothing can forward), the
/// command must not run on unsupervised. In the caller's process group a
/// parent-death SIGKILL on the command is enough (the caller's group kill
/// reaches the rest); in its own group (`supervise`) the supervisor handles
/// it. Returns the parent's pid; a parent already gone refuses the run.
#[cfg(unix)]
fn bind_to_parent(supervise: bool) -> Result<libc::pid_t, EnterError> {
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    #[cfg(target_os = "linux")]
    {
        let signal = if supervise {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        };
        // SAFETY: prctl with integer arguments.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, signal);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = supervise;
    // SAFETY: getppid has no preconditions.
    if unsafe { libc::getppid() } != parent {
        return Err(sandbox_err("the parent exited before the command started"));
    }
    Ok(parent)
}

/// Apply the spec to this process before it execs the command: the resource
/// limits, then (Linux) Landlock, then seccomp. In strict mode a part that
/// does not take refuses the run; best effort says so and goes on.
///
/// Seccomp comes after Landlock, before exec: it refuses terminal input
/// injection, vsock and io_uring whenever anything is confined, and under
/// deny/proxy also the sockets Landlock does not see (UDP, raw, ...). See
/// `seccomp`. Only the socket layer is part of the network policy, so only it
/// is fatal in strict mode; with the filesystem alone, an architecture
/// without a filter is skipped quietly and a kernel refusal is reported.
#[cfg(unix)]
fn confine(spec: &EnterSpec) -> Result<(), EnterError> {
    if let Err(e) = limits::apply(&spec.limits) {
        if spec.strict {
            return Err(sandbox_err(e));
        }
        eprintln!("sbx: limit not applied (best effort): {e}");
    }
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = linux::restrict(spec) {
            if spec.strict {
                return Err(sandbox_err(format!("Landlock: {e}")));
            }
            eprintln!("sbx: Landlock not applied (best effort): {e}");
        }
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
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_json() -> String {
        let spec = EnterSpec {
            fs_mode: FsMode::ReadOnly,
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
            offline: Offline::No,
            offline_record: None,
        };
        serde_json::to_string(&spec).expect("encodes")
    }

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// `<spec-json> -- <program> [args...]`: the spec as the parent wrote
    /// it, then the program and its arguments.
    #[test]
    fn the_helper_reads_its_spec_program_and_arguments() {
        let json = spec_json();
        let args = os(&[&json, "--", "cargo", "build", "--release"]);
        let (spec, program, rest) = helper_args(&args).expect("parses");
        assert_eq!(spec.fs_mode, FsMode::ReadOnly);
        assert_eq!(spec.net_mode, NetMode::Deny);
        assert!(spec.strict);
        assert_eq!(spec.parent_pid, Some(7));
        assert_eq!(program, "cargo");
        assert_eq!(rest, os(&["build", "--release"]).as_slice());
    }

    /// A missing `--`, a missing command, an unreadable spec and no spec at
    /// all are each refused with the sandbox's exit code and their reason.
    #[test]
    fn a_malformed_helper_call_is_refused_with_its_reason() {
        let json = spec_json();
        for (args, reason) in [
            (os(&[]), "without its spec"),
            (os(&["{not json", "--", "true"]), "does not parse"),
            (os(&[&json, "true"]), "expected `--`"),
            (os(&[&json, "--"]), "without a command"),
        ] {
            let err = helper_args(&args)
                .err()
                .unwrap_or_else(|| panic!("{args:?} should be refused"));
            assert_eq!(err.code, EXIT_SANDBOX, "{args:?}");
            assert!(err.message.contains(reason), "{args:?}: {}", err.message);
        }
    }

    /// A process not started as the helper (no `__sbx-enter` first) goes on
    /// at once: this test binary was not, and the call returns.
    #[test]
    fn a_process_not_started_as_the_helper_carries_on() {
        enter_if_requested();
    }
}
