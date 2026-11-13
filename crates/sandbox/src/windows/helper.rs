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
//!
//! An `msys` run (`spec.msys`, [`super::offline`]) is the same, with the
//! second helper logged on as the msys account and its token trusting that
//! account's own SID. The first helper checks the spec's SID against the
//! setup record's ([`launch_offline`]); the second against its own token
//! ([`super::token::restricted`]).

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND,
    ERROR_PATH_NOT_FOUND, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{SetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
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
    GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, GetExitCodeProcess, OpenProcess,
    OpenThread, ResumeThread, TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION,
    PROCESS_SYNCHRONIZE, STARTF_USESTDHANDLES, STARTUPINFOW, THREAD_SET_CONTEXT,
    THREAD_SUSPEND_RESUME,
};

use super::job_flags;
use super::offline::{is_sid, HELPER_PROCESS_SDDL, HELPER_THREAD_SDDL};
use super::token::{
    dacl_of, last_error, restricted, security_descriptor, set_own_default_dacl, wide, OwnedHandle,
};
use super::{
    batch_command_line, command_line, is_batch, is_root_sid, is_session_sid, resolve_program,
    JobLimits,
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
        eprintln!("insbx: job object limits not applied (best effort): {e}");
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
            eprintln!("insbx: restricted token not applied (best effort): {e}");
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
        // An msys run (whose token trusts this account's own SID) never goes
        // on unprotected, whatever the mode.
        if hardening_failure_refuses(spec.strict, spec.msys.is_some()) {
            return Err(sandbox(format!(
                "the helper could not protect itself from the command: {e}"
            )));
        }
        eprintln!("insbx: helper not protected from the command (best effort): {e}");
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

/// Whether a helper that could not lock itself against the command refuses the
/// run: strict mode does, and an msys run ALWAYS does (its command carries the
/// account's own SID, so an unlocked helper is an unrestricted one to hijack),
/// best effort or not. The first helper's own locking (the second's thread and
/// process, before it runs) is belt and braces for any other run, and passes
/// `strict = false`.
fn hardening_failure_refuses(strict: bool, msys: bool) -> bool {
    strict || msys
}

/// Give the kernel object `handle` (a process or a thread, with `WRITE_DAC`)
/// the protected DACL `sddl`.
fn harden_handle(handle: HANDLE, sddl: &str) -> Result<(), String> {
    let sd = security_descriptor(sddl)?;
    // SAFETY: the caller's handle is valid for the call; the DACL lives
    // inside `sd` across it.
    let err = unsafe {
        SetSecurityInfo(
            handle,
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

/// Give this process the DACL [`HELPER_PROCESS_SDDL`] and every thread of it
/// [`HELPER_THREAD_SDDL`]. The process object alone is not enough: the main
/// thread the logon service made has a default DACL that grants the account
/// `GENERIC_ALL`, and an msys command's token carries the account's own SID,
/// so it passes both halves of the check on that thread and could hijack it,
/// then run unrestricted as the account.
///
/// First the process token's default DACL becomes the thread DACL
/// ([`set_own_default_dacl`]), so a thread made AFTER the snapshot below (a
/// loader worker started while the command's process is created) is born
/// locked rather than with the account's default.
fn harden_own_process() -> Result<(), String> {
    set_own_default_dacl(HELPER_THREAD_SDDL)?;
    // SAFETY: the pseudo-handle has every right on this process.
    harden_handle(unsafe { GetCurrentProcess() }, HELPER_PROCESS_SDDL)?;
    harden_own_threads()
}

/// [`HELPER_THREAD_SDDL`] on every thread this process has (found by a
/// Toolhelp snapshot: the ones that exist before the command starts). Every
/// thread is tried; the failures are reported together, never stopping at the
/// first.
fn harden_own_threads() -> Result<(), String> {
    let mut tids = Vec::new();
    // SAFETY: a snapshot handle owned and closed here; the entry is a plain
    // structure with its size set.
    unsafe {
        let pid = GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(last_error("CreateToolhelp32Snapshot"));
        }
        let snapshot = OwnedHandle(snapshot);
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut more = Thread32First(snapshot.0, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                tids.push(entry.th32ThreadID);
            }
            more = Thread32Next(snapshot.0, &mut entry) != 0;
        }
    }
    let failed: Vec<String> = tids
        .into_iter()
        .filter_map(|tid| {
            harden_thread(tid)
                .err()
                .map(|e| format!("thread {tid}: {e}"))
        })
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// [`HELPER_THREAD_SDDL`] on the thread `tid` of this process.
///
/// - The CURRENT thread goes through the `GetCurrentThread` pseudo-handle,
///   which has full access whatever the thread's DACL says: the first helper
///   may have locked this very thread already ([`launch_offline`]), and a
///   locked thread denies even its owner `WRITE_DAC`.
/// - Any other thread is opened for `WRITE_DAC` and locked.
/// - One that cannot be opened for that is fine only if it is ALREADY locked:
///   it also cannot be opened for what hijacking takes
///   ([`thread_opens_for_hijack`]). A thread that is neither lockable nor
///   locked is the failure.
fn harden_thread(tid: u32) -> Result<(), String> {
    // SAFETY: the pseudo-handle needs no closing; a thread handle opened here
    // is owned and closed.
    unsafe {
        if tid == GetCurrentThreadId() {
            return harden_handle(GetCurrentThread(), HELPER_THREAD_SDDL);
        }
        let thread = OpenThread(WRITE_DAC, 0, tid);
        if !thread.is_null() {
            let thread = OwnedHandle(thread);
            return harden_handle(thread.0, HELPER_THREAD_SDDL);
        }
        let err = GetLastError();
        if err == ERROR_ACCESS_DENIED && !thread_opens_for_hijack(tid) {
            return Ok(());
        }
        Err(format!(
            "cannot be locked and is not locked already (OpenThread error {err})"
        ))
    }
}

/// Whether this process can open the thread `tid` for what hijacking it
/// takes: `THREAD_SET_CONTEXT | THREAD_SUSPEND_RESUME`.
fn thread_opens_for_hijack(tid: u32) -> bool {
    // SAFETY: opens a thread by id; a handle returned is closed at once.
    unsafe {
        let h = OpenThread(THREAD_SET_CONTEXT | THREAD_SUSPEND_RESUME, 0, tid);
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
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
    // An msys run logs on as the msys account, and its token trusts that
    // account's SID: the spec's SID must be the record's, so a spec cannot
    // name any other SID for the token to trust.
    let (account, account_sid, password) =
        super::setup::password(record, spec.msys.is_some()).map_err(sandbox)?;
    if let Some(asked) = &spec.msys {
        if *asked != account_sid {
            return Err(sandbox(format!(
                "{asked} is not the msys account's SID in the setup record; the command was not run"
            )));
        }
    }
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
    // call, the password is zeroed when `password` drops just after the last
    // attempt, and every handle returned is owned and closed.
    unsafe {
        // The handle values are copied as they are: they are valid in the
        // second helper because seclogon creates it as this process's child
        // with inheritance, so they must be inheritable here.
        let si = startup_info();
        // Made again only when refused because another run's call was still
        // starting seclogon ([`super::SECLOGON_STARTING`]). A fresh copy of
        // the command line each time: the call may write to it.
        let started = super::start_once_seclogon_is_up(
            || {
                let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
                let mut attempt_line = line.clone();
                let ok = CreateProcessWithLogonW(
                    user.as_ptr(),
                    domain.as_ptr(),
                    password.as_ptr(),
                    LOGON_FLAGS,
                    application.as_ptr(),
                    attempt_line.as_mut_ptr(),
                    CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                    environment.as_ptr().cast(),
                    cwd.as_ptr(),
                    &si,
                    &mut pi,
                );
                if ok == 0 {
                    Err(GetLastError())
                } else {
                    Ok(pi)
                }
            },
            |_| std::thread::sleep(super::SECLOGON_START_WAIT),
        );
        drop(password);
        let pi = match started {
            Ok(pi) => pi,
            Err(err) => return Err(sandbox(logon_failure(err, &account, &exe))),
        };
        let thread = OwnedHandle(pi.hThread);
        let process = OwnedHandle(pi.hProcess);
        if AssignProcessToJobObject(job.0, process.0) == 0 {
            let e = last_error("AssignProcessToJobObject");
            TerminateProcess(process.0, 1);
            return Err(sandbox(format!("the command was not run: {e}")));
        }
        // The second helper's thread and process are the account's, with the
        // default DACLs the logon service gave them. Lock them before the
        // thread first runs (the helper locks every thread of its own again,
        // before it starts the command, which is what holds even if this
        // fails: [`harden_own_process`] says why). An msys run, whose command
        // carries the account's own SID, is refused if it fails, in every mode.
        let locked = harden_handle(thread.0, HELPER_THREAD_SDDL)
            .and_then(|()| harden_handle(process.0, HELPER_PROCESS_SDDL));
        if let (Err(e), true) = (
            &locked,
            hardening_failure_refuses(false, spec.msys.is_some()),
        ) {
            TerminateProcess(process.0, 1);
            return Err(sandbox(format!(
                "the second helper could not be locked against the command, so the msys run \
                 was not started: {e}"
            )));
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
            "{account}'s password no longer matches the one insbx stored; run `insbx setup-windows` \
             as administrator again"
        ),
        1331 => format!("{account} is disabled; run `insbx setup-windows` as administrator again"),
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

/// The token for a confined run, restricted to the run's SID and its roots'
/// kept SIDs. Any SID not of either shape is refused, so a spec cannot point
/// the token at a SID that is granted somewhere real.
fn confining_token(spec: &EnterSpec) -> Result<OwnedHandle, String> {
    let sid = spec
        .sid
        .as_deref()
        .ok_or("the helper was started without the run's SID")?;
    if !is_session_sid(sid) {
        return Err(format!("{sid} is not an insbx run SID"));
    }
    if let Some(bad) = spec.root_sids.iter().find(|s| !is_root_sid(s)) {
        return Err(format!("{bad} is not an insbx root SID"));
    }
    // The msys account's SID, trusted by the token: only in the second
    // helper, which runs as that account (`restricted` checks it is the
    // token's own user), never in a run as the caller.
    if let Some(msys) = &spec.msys {
        if !is_sid(msys) || spec.offline != Offline::Inner {
            return Err(format!("{msys} cannot be trusted by this token"));
        }
    }
    restricted(sid, &spec.root_sids, spec.msys.as_deref())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::token::set_token_default_dacl;
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ADJUST_DEFAULT,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcessToken, PROCESS_CREATE_THREAD, PROCESS_VM_OPERATION, PROCESS_VM_WRITE,
    };

    /// A `cmd.exe` of this user's, suspended: its first thread has not run.
    fn suspended_child() -> PROCESS_INFORMATION {
        suspended_child_as(None)
    }

    /// The same, started with `token` (a primary token) when given.
    fn suspended_child_as(token: Option<HANDLE>) -> PROCESS_INFORMATION {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let application = wide(format!(r"{root}\System32\cmd.exe"));
        let mut line = wide("cmd.exe /d /c exit 0");
        // SAFETY: Win32 process creation with NUL-terminated strings that
        // outlive the call; a zeroed STARTUPINFOW with its size set is valid.
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = match token {
                Some(token) => CreateProcessAsUserW(
                    token,
                    application.as_ptr(),
                    line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    CREATE_SUSPENDED | CREATE_NO_WINDOW,
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
                    0,
                    CREATE_SUSPENDED | CREATE_NO_WINDOW,
                    std::ptr::null(),
                    std::ptr::null(),
                    &si,
                    &mut pi,
                ),
            };
            assert_ne!(ok, 0, "{}", last_error("CreateProcess"));
            pi
        }
    }

    /// A primary-token copy of this process's own, with `sddl` as its default
    /// DACL when given.
    fn own_token_copy(sddl: Option<&str>) -> OwnedHandle {
        // SAFETY: opens and duplicates this process's own token; every
        // handle is owned and closed.
        unsafe {
            let mut own: HANDLE = std::ptr::null_mut();
            let access =
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT;
            assert_ne!(
                OpenProcessToken(GetCurrentProcess(), access, &mut own),
                0,
                "{}",
                last_error("OpenProcessToken")
            );
            let own = OwnedHandle(own);
            let mut copy: HANDLE = std::ptr::null_mut();
            assert_ne!(
                DuplicateTokenEx(
                    own.0,
                    0x000f_01ff,
                    std::ptr::null(),
                    SecurityImpersonation,
                    TokenPrimary,
                    &mut copy
                ),
                0,
                "{}",
                last_error("DuplicateTokenEx")
            );
            let copy = OwnedHandle(copy);
            if let Some(sddl) = sddl {
                set_token_default_dacl(copy.0, sddl).expect("default DACL set");
            }
            copy
        }
    }

    /// Run `f(tid)` while a second thread, `tid`, is parked; then let it end.
    fn with_parked_thread<R>(f: impl FnOnce(u32) -> R) -> R {
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let parked = std::thread::spawn(move || {
            // SAFETY: reads the calling thread's id.
            tid_tx.send(unsafe { GetCurrentThreadId() }).expect("tid");
            let _ = stop_rx.recv();
        });
        let tid = tid_rx.recv().expect("the thread's id");
        let result = f(tid);
        let _ = stop_tx.send(());
        parked.join().expect("the parked thread ends");
        result
    }

    /// Whether this process may open the thread `tid` for what hijacking it
    /// takes (set its context, suspend and resume it); and why not.
    fn can_open_thread(tid: u32) -> (bool, u32) {
        // SAFETY: opens a thread by id; a handle returned is closed at once.
        unsafe {
            let h = OpenThread(THREAD_SET_CONTEXT | THREAD_SUSPEND_RESUME, 0, tid);
            if h.is_null() {
                return (false, GetLastError());
            }
            CloseHandle(h);
            (true, 0)
        }
    }

    fn can_open_process(pid: u32) -> (bool, u32) {
        // SAFETY: opens a process by id; a handle returned is closed at once.
        unsafe {
            let h = OpenProcess(
                PROCESS_VM_WRITE | PROCESS_VM_OPERATION | PROCESS_CREATE_THREAD,
                0,
                pid,
            );
            if h.is_null() {
                return (false, GetLastError());
            }
            CloseHandle(h);
            (true, 0)
        }
    }

    /// **A HARDENED THREAD (AND PROCESS) CANNOT BE OPENED FOR CONTEXT OR
    /// SUSPEND/RESUME, EVEN BY ITS OWN OWNER**, which is who the confined
    /// command is relative to the second helper (the same account). Measured
    /// on a suspended child of this user's: before, the owner opens it; after
    /// [`harden_handle`], `ERROR_ACCESS_DENIED`; the handle the creator holds
    /// keeps working (the creator resumes and ends it).
    #[test]
    fn a_hardened_thread_and_process_cannot_be_opened_even_by_their_owner() {
        let pi = suspended_child();
        let (thread, process) = (OwnedHandle(pi.hThread), OwnedHandle(pi.hProcess));
        // The control: nothing locked yet, the owner opens both.
        assert!(can_open_thread(pi.dwThreadId).0, "control: the thread");
        assert!(can_open_process(pi.dwProcessId).0, "control: the process");

        harden_handle(thread.0, HELPER_THREAD_SDDL).expect("thread hardened");
        harden_handle(process.0, HELPER_PROCESS_SDDL).expect("process hardened");
        assert_eq!(
            can_open_thread(pi.dwThreadId),
            (false, ERROR_ACCESS_DENIED),
            "the thread of the second helper must not open for context or suspend"
        );
        assert_eq!(
            can_open_process(pi.dwProcessId),
            (false, ERROR_ACCESS_DENIED),
            "nor the process for memory or threads"
        );
        // The creator's own handles still work.
        // SAFETY: handles owned above.
        unsafe {
            assert_ne!(
                ResumeThread(thread.0),
                u32::MAX,
                "{}",
                last_error("ResumeThread")
            );
            WaitForSingleObject(process.0, 10_000);
            TerminateProcess(process.0, 0);
        }
    }

    /// **A THREAD THE FIRST HELPER ALREADY LOCKED IS NOT AN ERROR** (the
    /// regression: it denied even its owner `WRITE_DAC`, so hardening every
    /// thread by `OpenThread(WRITE_DAC)` refused every account run). An
    /// unlocked other thread is locked; a locked one, other or the current
    /// one, passes; a thread that cannot be locked and is not locked is the
    /// failure.
    #[test]
    fn hardening_a_thread_that_is_already_locked_is_not_an_error() {
        // An ordinary other thread: opens before, locked after.
        with_parked_thread(|tid| {
            assert!(thread_opens_for_hijack(tid), "control: unlocked opens");
            harden_thread(tid).expect("locks an unlocked thread");
            assert!(!thread_opens_for_hijack(tid), "locked after");
        });
        // Locked by the parent first, as `launch_offline` does: the owner
        // cannot even open it for WRITE_DAC now, and that is fine.
        with_parked_thread(|tid| {
            // SAFETY: opens a thread by id for WRITE_DAC; closed on drop.
            let parent = OwnedHandle(unsafe { OpenThread(WRITE_DAC, 0, tid) });
            assert!(!parent.0.is_null(), "{}", last_error("OpenThread"));
            harden_handle(parent.0, HELPER_THREAD_SDDL).expect("the parent locks it");
            // SAFETY: opens by id; a handle returned is closed at once.
            let again = unsafe { OpenThread(WRITE_DAC, 0, tid) };
            assert!(
                again.is_null(),
                "a locked thread denies its owner WRITE_DAC"
            );
            harden_thread(tid).expect("already locked is not an error");
            assert!(!thread_opens_for_hijack(tid));
        });
        // The CURRENT thread, locked already: the pseudo-handle still works.
        std::thread::spawn(|| {
            // SAFETY: reads the calling thread's id.
            let me = unsafe { GetCurrentThreadId() };
            harden_handle(unsafe { GetCurrentThread() }, HELPER_THREAD_SDDL).expect("locked");
            assert!(!thread_opens_for_hijack(me));
            harden_thread(me).expect("the current thread, locked, is hardened again");
        })
        .join()
        .expect("the thread");
        // Neither lockable nor locked (there is no such thread): the failure.
        let err = harden_thread(0xffff_fff0).expect_err("a thread that is not there");
        assert!(err.contains("not locked already"), "{err}");
    }

    /// **A THREAD MADE AFTER THE SNAPSHOT IS BORN LOCKED** when the helper's
    /// token default DACL is the thread DACL: a child started with such a
    /// token has a first thread the owner cannot open for context or suspend;
    /// with the ordinary token it can (the control).
    #[test]
    fn a_thread_made_under_the_hardened_default_dacl_is_born_locked() {
        let plain = own_token_copy(None);
        let pi = suspended_child_as(Some(plain.0));
        assert!(
            can_open_thread(pi.dwThreadId).0,
            "control: an ordinary token's thread opens"
        );
        // SAFETY: handles from the creation above, ended and closed here.
        unsafe {
            TerminateProcess(pi.hProcess, 0);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }

        let locked = own_token_copy(Some(HELPER_THREAD_SDDL));
        let pi = suspended_child_as(Some(locked.0));
        assert_eq!(
            can_open_thread(pi.dwThreadId),
            (false, ERROR_ACCESS_DENIED),
            "a thread born under the hardened default DACL is locked"
        );
        // SAFETY: as above.
        unsafe {
            TerminateProcess(pi.hProcess, 0);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
    }

    #[test]
    fn a_helper_that_cannot_lock_itself_refuses_an_msys_run_in_every_mode() {
        assert!(hardening_failure_refuses(true, false), "strict");
        assert!(hardening_failure_refuses(false, true), "msys, best effort");
        assert!(hardening_failure_refuses(true, true));
        assert!(
            !hardening_failure_refuses(false, false),
            "best effort, no msys"
        );
    }

    /// Every thread of a process is found: the snapshot sees this process's.
    #[test]
    fn the_thread_snapshot_finds_this_processs_threads() {
        // SAFETY: a snapshot owned and closed here; a plain entry structure.
        unsafe {
            let snapshot = OwnedHandle(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0));
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let pid = GetCurrentProcessId();
            let mut mine = 0;
            let mut more = Thread32First(snapshot.0, &mut entry) != 0;
            while more {
                mine += usize::from(entry.th32OwnerProcessID == pid);
                more = Thread32Next(snapshot.0, &mut entry) != 0;
            }
            assert!(mine >= 1, "the test's own thread is in it");
        }
    }
}
