//! The helper on Windows: build the token, start the command under it in a
//! job object, wait, and pass its exit code on.
//!
//! Why a helper rather than the parent calling `CreateProcessAsUserW`
//! itself: the parent hands its caller a ready `std::process::Command`
//! ([`crate::Sandbox::command`]) whose pipes the caller chooses, and only
//! `std` can spawn that. The helper is that `Command`; it inherits the pipes
//! as its own standard handles and passes them straight to the command, the
//! way incode's `--sandbox-exec` helper does. Windows has no `exec`, so the
//! helper stays, as the command's supervisor: it holds the job's only handle
//! and watches the parent, so when either the helper or the parent goes, the
//! whole tree goes with it.
//!
//! # Network deny: two helpers
//!
//! When the network is denied ([`super::offline`]), the command must run as
//! the offline account, and an unprivileged process can only start a process
//! as another user with `CreateProcessWithLogonW`, which takes a password,
//! not a token. So this helper (the user's, [`Offline::Launch`]) makes the
//! job, starts a second copy of itself as the account, suspended, puts it in
//! the job and lets it run. The second helper ([`Offline::Inner`]) builds the
//! write-restricted token from its own token, as below, starts the command
//! (in the job already, as its child), and passes its exit code back. Its
//! spec and the command travel in an environment variable, not on its
//! command line: `CreateProcessWithLogonW` takes at most 1024 characters
//! there.

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{
    GetLastError, SetHandleInformation, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{SetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, CreateProcessWithLogonW, GetCurrentProcess,
    GetExitCodeProcess, OpenProcess, ResumeThread, TerminateProcess, WaitForMultipleObjects,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE,
    PROCESS_INFORMATION, PROCESS_SYNCHRONIZE, STARTF_USESTDHANDLES, STARTUPINFOW,
};

use super::job_flags;
use super::offline::HELPER_PROCESS_SDDL;
use super::token::{dacl_of, last_error, restricted, security_descriptor, wide, OwnedHandle};
use super::{
    batch_command_line, command_line, is_batch, is_session_sid, resolve_program, JobLimits,
};
use crate::policy::FsMode;
use crate::{EnterSpec, InnerLaunch, Offline, ENTER_ARG, EXIT_SANDBOX, INNER_ARG, INNER_ENV};

/// `dwLogonFlags` for `CreateProcessWithLogonW`: no profile loaded (see
/// [`super::offline`] for why).
const LOGON_FLAGS: u32 = 0;

// The flags are spelled in the pure module for its tests; check them here.
const _: () = {
    assert!(job_flags::KILL_ON_JOB_CLOSE == JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
    assert!(job_flags::DIE_ON_UNHANDLED_EXCEPTION == JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION);
    assert!(job_flags::PROCESS_MEMORY == JOB_OBJECT_LIMIT_PROCESS_MEMORY);
    assert!(job_flags::PROCESS_TIME == JOB_OBJECT_LIMIT_PROCESS_TIME);
};

/// An error and the exit code it ends the helper with.
type Failure = (String, i32);

fn sandbox(message: impl Into<String>) -> Failure {
    (message.into(), EXIT_SANDBOX)
}

/// Run `program args` confined as `spec` says; its exit code.
///
/// # Errors
/// When the parent is gone, the token or job cannot be made in strict mode,
/// or the command cannot be found (127) or started (126).
pub(crate) fn enter(
    spec: &EnterSpec,
    program: &OsString,
    args: &[OsString],
) -> Result<i32, Failure> {
    if spec.offline == Offline::Inner {
        return inner(spec, program, args);
    }
    // Watch the parent from before the command starts: if it is already
    // gone, nothing should run unsupervised.
    let parent = match spec.parent_pid {
        Some(pid) => {
            // SAFETY: opening a process by id for SYNCHRONIZE only.
            let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            if h.is_null() {
                return Err(sandbox("the parent exited before the command started"));
            }
            Some(OwnedHandle(h))
        }
        None => None,
    };

    let job = job(&JobLimits::from_limits(&spec.limits)).or_else(|e| {
        if spec.strict {
            return Err(sandbox(format!("job object: {e}")));
        }
        eprintln!("sbx: job object limits not applied (best effort): {e}");
        job(&JobLimits::from_limits(&crate::policy::Limits::default()))
            .map_err(|e| sandbox(format!("job object: {e}")))
    })?;

    let child = if spec.offline == Offline::Launch {
        launch_offline(spec, program, args, &job)?
    } else {
        let token = token_for(spec)?;
        let (application, mut line) = command_for(program, args)?;
        start(token.as_ref(), &application, &mut line, Some(&job))?
    };

    // Wait for the command, or for the parent to disappear.
    let mut handles: Vec<HANDLE> = vec![child.0];
    if let Some(p) = &parent {
        handles.push(p.0);
    }
    // SAFETY: valid handles owned above, alive for the call.
    let waited =
        unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
    if waited != WAIT_OBJECT_0 {
        // The parent died (or the wait failed): the tree goes with it.
        // SAFETY: the job handle is ours.
        unsafe { TerminateJobObject(job.0, 1) };
        return Ok(1);
    }
    let mut code = 0u32;
    // SAFETY: the process handle is ours.
    if unsafe { GetExitCodeProcess(child.0, &mut code) } == 0 {
        return Err(sandbox(last_error("GetExitCodeProcess")));
    }
    // Whatever the command left running ends with it, as on Linux when the
    // run has its own process group; closing the job would do the same.
    // SAFETY: the job handle is ours.
    unsafe { TerminateJobObject(job.0, 1) };
    Ok(code as i32)
}

/// The write-restricted token the command runs under, from this process's
/// own; `None` when the filesystem is not confined (or, best effort, when
/// the token could not be made).
fn token_for(spec: &EnterSpec) -> Result<Option<OwnedHandle>, Failure> {
    if spec.fs_mode == FsMode::Off {
        return Ok(None);
    }
    match confining_token(spec) {
        Ok(t) => Ok(Some(t)),
        Err(e) if spec.strict => Err(sandbox(format!(
            "the {} sandbox could not be applied, so the command was not run: {e}",
            spec.fs_mode.as_str()
        ))),
        Err(e) => {
            eprintln!("sbx: restricted token not applied (best effort): {e}");
            Ok(None)
        }
    }
}

/// The second helper, running as the offline account inside the first
/// helper's job: confine and start the command, wait, pass its code on.
/// Nothing to watch: if the first helper goes, its job takes this down.
fn inner(spec: &EnterSpec, program: &OsString, args: &[OsString]) -> Result<i32, Failure> {
    // The command runs as this same account: keep it from opening this
    // (unrestricted) process to write into it.
    if let Err(e) = harden_own_process() {
        if spec.strict {
            return Err(sandbox(format!(
                "the helper could not protect itself from the command: {e}"
            )));
        }
        eprintln!("sbx: helper not protected from the command (best effort): {e}");
    }
    let token = token_for(spec)?;
    let (application, mut line) = command_for(program, args)?;
    let child = start(token.as_ref(), &application, &mut line, None)?;
    // SAFETY: the process handle is ours.
    if unsafe { WaitForSingleObject(child.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(sandbox(last_error("WaitForSingleObject")));
    }
    let mut code = 0u32;
    // SAFETY: the process handle is ours.
    if unsafe { GetExitCodeProcess(child.0, &mut code) } == 0 {
        return Err(sandbox(last_error("GetExitCodeProcess")));
    }
    Ok(code as i32)
}

/// Give this process the DACL [`HELPER_PROCESS_SDDL`].
fn harden_own_process() -> Result<(), String> {
    let sd = security_descriptor(HELPER_PROCESS_SDDL)?;
    // SAFETY: the pseudo-handle has every right on this process; the DACL
    // lives inside `sd` across the call.
    let err = unsafe {
        SetSecurityInfo(
            GetCurrentProcess(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl_of(&sd)?,
            std::ptr::null(),
        )
    };
    if err != 0 {
        return Err(format!("SetSecurityInfo failed with error {err}"));
    }
    Ok(())
}

/// Start the second helper as the offline account, suspended, put it in
/// `job`, and let it run. Its spec and the command go in [`INNER_ENV`].
fn launch_offline(
    spec: &EnterSpec,
    program: &OsString,
    args: &[OsString],
    job: &OwnedHandle,
) -> Result<OwnedHandle, Failure> {
    let record = spec
        .offline_record
        .as_ref()
        .ok_or_else(|| sandbox("the helper was started without the offline account's record"))?;
    let (account, password) = super::setup::password(record).map_err(sandbox)?;
    let text = |s: &OsString| {
        s.to_str()
            .map(str::to_string)
            .ok_or_else(|| sandbox("under network deny every argument must be valid Unicode"))
    };
    let mut inner_spec = spec.clone();
    inner_spec.offline = Offline::Inner;
    inner_spec.parent_pid = None;
    inner_spec.offline_record = None;
    let launch = InnerLaunch {
        spec: inner_spec,
        program: text(program)?,
        args: args.iter().map(text).collect::<Result<_, _>>()?,
    };
    let json = serde_json::to_string(&launch).map_err(|e| sandbox(e.to_string()))?;
    let exe =
        std::env::current_exe().map_err(|e| sandbox(format!("cannot name the helper: {e}")))?;
    let exe = super::strip_verbatim(&exe.to_string_lossy());
    let mut line = command_line(&[
        exe.encode_utf16().collect(),
        ENTER_ARG.encode_utf16().collect(),
        INNER_ARG.encode_utf16().collect(),
    ]);
    line.push(0);
    // This helper is single-threaded here: set, copy the block, unset.
    std::env::set_var(INNER_ENV, &json);
    let environment = environment_block();
    std::env::remove_var(INNER_ENV);
    let environment = environment.map_err(sandbox)?;
    let cwd = std::env::current_dir().map_err(|e| sandbox(format!("no current folder: {e}")))?;
    let cwd = wide(super::strip_verbatim(&cwd.to_string_lossy()));
    let application = wide(&exe);
    let (user, domain) = (wide(&account), wide("."));

    // SAFETY: Win32 process creation; every pointer passed outlives the
    // call, the password is zeroed when `password` drops just after it, and
    // every handle returned is owned and closed.
    unsafe {
        // The handle values are copied as they are: they are valid in the
        // second helper because seclogon creates it as this process's child
        // with inheritance, so they must be inheritable here.
        let si = startup_info();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessWithLogonW(
            user.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON_FLAGS,
            application.as_ptr(),
            line.as_mut_ptr(),
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &si,
            &mut pi,
        );
        let err = GetLastError();
        drop(password);
        if ok == 0 {
            return Err(sandbox(logon_failure(err, &account, &exe)));
        }
        let thread = OwnedHandle(pi.hThread);
        let process = OwnedHandle(pi.hProcess);
        if AssignProcessToJobObject(job.0, process.0) == 0 {
            let e = last_error("AssignProcessToJobObject");
            TerminateProcess(process.0, 1);
            return Err(sandbox(format!("the command was not run: {e}")));
        }
        if ResumeThread(thread.0) == u32::MAX {
            let e = last_error("ResumeThread");
            TerminateProcess(process.0, 1);
            return Err(sandbox(e));
        }
        Ok(process)
    }
}

/// A `STARTUPINFOW` that hands this helper's own standard handles (the
/// pipes or console its caller chose) to the process it starts, made
/// inheritable so they are valid there.
fn startup_info() -> STARTUPINFOW {
    // SAFETY: reads this process's standard handles and marks them
    // inheritable; a zeroed STARTUPINFOW is a valid value.
    unsafe {
        let std_handles = [
            GetStdHandle(STD_INPUT_HANDLE),
            GetStdHandle(STD_OUTPUT_HANDLE),
            GetStdHandle(STD_ERROR_HANDLE),
        ];
        for h in std_handles {
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = std_handles[0];
        si.hStdOutput = std_handles[1];
        si.hStdError = std_handles[2];
        si
    }
}

/// Why `CreateProcessWithLogonW` failed, and what to do.
fn logon_failure(err: u32, account: &str, exe: &str) -> String {
    let why = match err {
        1326 => format!(
            "{account}'s password no longer matches the one sbx stored; run `sbx setup-windows` \
             as administrator again"
        ),
        1331 => format!("{account} is disabled; run `sbx setup-windows` as administrator again"),
        1385 => format!("a policy denies {account} local logon (\"Allow log on locally\")"),
        267 => format!("{account} cannot use the current folder"),
        5 => format!("access denied (can {account} read {exe}?)"),
        1058 => "the Secondary Logon service (seclogon) is disabled".to_string(),
        _ => format!("Win32 error {err}"),
    };
    format!("the command could not be started as {account}, so it was not run: {why}")
}

/// This process's environment block, as `CreateProcess` takes it.
fn environment_block() -> Result<Vec<u16>, String> {
    // SAFETY: reads the block up to its double NUL, then frees it.
    unsafe {
        let p = GetEnvironmentStringsW();
        if p.is_null() {
            return Err(last_error("GetEnvironmentStringsW"));
        }
        let mut len = 0usize;
        while !(*p.add(len) == 0 && *p.add(len + 1) == 0) {
            len += 1;
        }
        let block = std::slice::from_raw_parts(p, len + 2).to_vec();
        FreeEnvironmentStringsW(p);
        Ok(block)
    }
}

/// The token for a confined run, restricted to the run's SID.
fn confining_token(spec: &EnterSpec) -> Result<OwnedHandle, String> {
    let sid = spec
        .sid
        .as_deref()
        .ok_or("the helper was started without the run's SID")?;
    if !is_session_sid(sid) {
        return Err(format!("{sid} is not an sbx run SID"));
    }
    restricted(sid)
}

/// A job that kills everything in it when its last handle closes, with the
/// policy's per-process limits. Not inheritable: the only handle is ours.
fn job(limits: &JobLimits) -> Result<OwnedHandle, String> {
    // SAFETY: no security attributes, no name; the handle is owned.
    let job = OwnedHandle(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
    if job.0.is_null() {
        return Err(last_error("CreateJobObjectW"));
    }
    // SAFETY: a plain-data structure, zero is a valid value for every field.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = limits.flags;
    info.BasicLimitInformation.PerProcessUserTimeLimit = limits.process_user_time;
    info.ProcessMemoryLimit = limits.process_memory;
    // SAFETY: `info` is the structure the class names, at its size.
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error("SetInformationJobObject"));
    }
    Ok(job)
}

fn to_wide(s: &OsString) -> Vec<u16> {
    s.encode_wide().collect()
}

/// The program to start and its command line (NUL-terminated), found the way
/// [`resolve_program`] says. A batch file runs through `cmd.exe`.
fn command_for(program: &OsString, args: &[OsString]) -> Result<(Vec<u16>, Vec<u16>), Failure> {
    let name = program.to_string_lossy();
    let path = std::env::var("PATH").ok();
    let pathext = std::env::var("PATHEXT").ok();
    let resolved = resolve_program(&name, path.as_deref(), pathext.as_deref(), &|p| {
        std::path::Path::new(p).is_file()
    })
    .ok_or_else(|| (format!("cannot run {name}: not found"), 127))?;
    let mut argv: Vec<Vec<u16>> = vec![to_wide(program)];
    argv.extend(args.iter().map(to_wide));
    let (application, mut line) = if is_batch(&resolved) {
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            format!(r"{root}\System32\cmd.exe")
        });
        argv[0] = resolved.encode_utf16().collect();
        let wide_comspec: Vec<u16> = comspec.encode_utf16().collect();
        (comspec, batch_command_line(&wide_comspec, &argv))
    } else {
        (resolved, command_line(&argv))
    };
    line.push(0);
    let application: Vec<u16> = application.encode_utf16().chain(Some(0)).collect();
    Ok((application, line))
}

/// Start the command suspended (under `token` when given), with this
/// helper's standard handles, put it in `job` (without one it is already in
/// this helper's), then let it run.
fn start(
    token: Option<&OwnedHandle>,
    application: &[u16],
    line: &mut [u16],
    job: Option<&OwnedHandle>,
) -> Result<OwnedHandle, Failure> {
    // SAFETY: Win32 process creation; every pointer passed outlives the call
    // and every handle returned is owned and closed.
    unsafe {
        // The helper's own standard handles are the pipes (or console) its
        // caller chose: hand them straight to the command.
        let si = startup_info();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        // Suspended, so it is in the job before it runs a single
        // instruction (or starts a single child).
        let ok = match token {
            Some(t) => CreateProcessAsUserW(
                t.0,
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            ),
            None => CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            ),
        };
        if ok == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            let name = String::from_utf16_lossy(&application[..application.len() - 1]);
            let code = if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND {
                127
            } else {
                126
            };
            return Err((format!("cannot run {name}: Win32 error {err}"), code));
        }
        let thread = OwnedHandle(pi.hThread);
        let process = OwnedHandle(pi.hProcess);
        if let Some(job) = job {
            if AssignProcessToJobObject(job.0, process.0) == 0 {
                let e = last_error("AssignProcessToJobObject");
                TerminateProcess(process.0, 1);
                return Err(sandbox(format!("the command was not run: {e}")));
            }
        }
        if ResumeThread(thread.0) == u32::MAX {
            let e = last_error("ResumeThread");
            TerminateProcess(process.0, 1);
            return Err(sandbox(e));
        }
        Ok(process)
    }
}
