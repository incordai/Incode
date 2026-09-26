//! Windows confinement with a write-restricted token.
//!
//! # What is enforced, and what is left off — measured on this machine
//!
//! A restricted token created with `WRITE_RESTRICTED` and a set of restricting
//! SIDs lets a process READ anywhere but WRITE only where every restricting SID
//! is granted. incode uses it for the **read-only** mode: the command may read
//! any file and write none (bar the null device, which `Everyone` still
//! reaches). Measured without elevation: a write to the workspace and a write
//! to `Documents` were both refused, `reg add HKCU`, `schtasks /create` and
//! process injection into `explorer` were refused, while reads and `> NUL`
//! still worked.
//!
//! **workspace-write is deliberately NOT applied on Windows.** The only sound
//! way to let a write-restricted command write inside the workspace is to add
//! an ACE for incode's private SID to the workspace's real DACL before each
//! command and remove it after — which, measured on a 28,000-file tree, costs
//! about 2.5 s to grant and 2.5 s to revoke, mutates the operator's own files,
//! and leaves the ACE behind if incode is killed mid-command. Doing that
//! silently on every shell command is the "safety with a cost the operator did
//! not agree to" this project avoids, so a workspace-write session runs
//! unconfined here and [`super::Sandbox::note`] says so. A read-only session —
//! `/plan`, or `sandbox: "read-only"` — is confined.
//!
//! **Network is not restricted by either token.** Measured: raw TCP, http and
//! Python's https all succeeded under the token; only an AppContainer (heavy
//! setup, everything must be granted read access) blocks the network, so it is
//! not used.
//!
//! # How it reaches a command
//!
//! [`wrap`] returns a `Command` that runs `incode --sandbox-exec read-only --
//! <command>`. incode spawns THAT through its ordinary shell machinery, so the
//! pipes, the live output, the timeout and the job all work unchanged; the
//! helper ([`run_helper`]) builds the token and launches `cmd /S /C <command>`
//! with `CreateProcessAsUserW`, inheriting the helper's own std handles (which
//! are incode's pipes) and joining incode's job, then waits and exits with the
//! command's code.

use std::path::PathBuf;
use std::process::Command;

use super::{PerOs, SandboxMode};

/// Whether Windows confines `mode`. Read-only is enforced; workspace-write is
/// left off (see the module note), and off is not a thing to apply.
pub(super) fn applies(mode: SandboxMode) -> bool {
    mode == SandboxMode::ReadOnly
}

/// The command that runs `command` under the read-only sandbox: incode
/// re-invoked as the `--sandbox-exec` helper. `None` for a mode Windows does
/// not confine, or when incode cannot name its own binary — the caller then
/// runs the command unconfined, [`super::Sandbox::note`] having said so.
pub(super) fn wrap(
    mode: SandboxMode,
    _roots: &[PathBuf],
    shell: (&str, &str),
    command: &str,
) -> Option<Command> {
    if !applies(mode) {
        return None;
    }
    // `cmd /C` is the only shell this path launches; a caller asking for a
    // different one is not confined here rather than run unconfined-looking.
    if shell.0 != "cmd" {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--sandbox-exec")
        .arg(mode.as_str())
        .arg("--")
        .arg(command);
    Some(cmd)
}

/// What the Windows sandbox can and cannot do, for the report.
pub(super) fn per_os() -> PerOs {
    PerOs {
        mechanism: "a write-restricted token (read-only mode only)",
        cannot: "workspace-write needs a per-command change to the workspace's real ACL, so it is \
                 left unconfined on Windows",
        network: "not restricted (a token cannot block it; only an AppContainer can)",
    }
}

/// The `--sandbox-exec` helper on Windows: build the write-restricted token and
/// run `command` under it, returning the command's exit code. Only read-only is
/// enforced; any other mode runs the command through the plain shell, so a
/// command still runs.
///
/// # Errors
///
/// When the command could not be started at all. A command that ran and failed
/// returns its own non-zero code, not an error.
pub fn run_helper(mode: SandboxMode, command: &str) -> anyhow::Result<i32> {
    if mode != SandboxMode::ReadOnly {
        // Not confined here; run it plainly so the operator still gets a result.
        return plain(command);
    }
    match imp::run_restricted(command) {
        Ok(code) => Ok(code),
        Err(e) => {
            // A sandbox that could not be built must not silently run the
            // command UNCONFINED — that is the failure mode this exists to
            // avoid. Report it; the caller shows it as the command's error.
            Err(anyhow::anyhow!(
                "the read-only sandbox could not be applied, so the command was not run: {e}"
            ))
        }
    }
}

/// Run `command` through the plain shell, unconfined, returning its code.
fn plain(command: &str) -> anyhow::Result<i32> {
    use std::os::windows::process::CommandExt;
    let status = Command::new("cmd")
        .arg("/S")
        .arg("/C")
        .raw_arg(format!("\"{command}\""))
        .status()?;
    Ok(status.code().unwrap_or(-1))
}

mod imp {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    };
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, GetSecurityDescriptorDacl, GetTokenInformation, SetTokenInformation,
        TokenDefaultDacl, TokenLogonSid, TokenUser, DISABLE_MAX_PRIVILEGE, SID_AND_ATTRIBUTES,
        TOKEN_ACCESS_MASK, TOKEN_DEFAULT_DACL, TOKEN_GROUPS, TOKEN_USER, WRITE_RESTRICTED,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
        ResumeThread, WaitForSingleObject, CREATE_SUSPENDED, INFINITE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    /// Everyone, S-1-1-0.
    const EVERYONE: &str = "S-1-1-0";
    /// incode's own private restricting SID. Fixed, so re-runs are idempotent
    /// and only incode's tokens ever carry it. A high, otherwise-unused RID
    /// under the NT-authority non-unique range.
    const INCODE_SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-4242";
    /// `TOKEN_ALL_ACCESS`, spelled numerically for the same reason.
    const TOKEN_ALL: TOKEN_ACCESS_MASK = 0x000f_01ff;

    /// A wide, NUL-terminated string for a Win32 W-API.
    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// A SID pointer freed on drop with `LocalFree` (how
    /// `ConvertStringSidToSidW` allocates it).
    struct LocalSid(*mut core::ffi::c_void);
    impl Drop for LocalSid {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from a Local* allocator and is freed once.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// Build a SID from its string form.
    fn sid(s: &str) -> Result<LocalSid, String> {
        let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `wide(s)` is NUL-terminated; `&mut out` is the `*mut PSID` the
        // API writes the owned SID pointer into.
        let ok = unsafe { ConvertStringSidToSidW(wide(s).as_ptr(), &mut out) };
        if ok == 0 || out.is_null() {
            return Err(format!("could not build the SID {s}"));
        }
        Ok(LocalSid(out))
    }

    /// Run `command` under a write-restricted token, returning its exit code.
    pub(super) fn run_restricted(command: &str) -> Result<i32, String> {
        // SAFETY: the whole sequence is Win32 handle work; each call is checked
        // and every handle this owns is closed. The pointers handed to the API
        // outlive the calls that read them (the SIDs and the array live to the
        // end of the function).
        unsafe {
            let process = GetCurrentProcess();
            let mut base: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_ALL, &mut base) == 0 {
                return Err(last_error("OpenProcessToken"));
            }
            let base = OwnedHandle(base);

            // The three restricting SIDs: incode's private one, Everyone (so
            // `> NUL` still works) and this logon's SID (so the shell and the
            // tools it starts can run at all).
            let incode = sid(INCODE_SID)?;
            let everyone = sid(EVERYONE)?;
            let logon = logon_sid(base.0)?;
            let restricting = [
                SID_AND_ATTRIBUTES {
                    Sid: incode.0.cast(),
                    Attributes: 0,
                },
                SID_AND_ATTRIBUTES {
                    Sid: everyone.0.cast(),
                    Attributes: 0,
                },
                SID_AND_ATTRIBUTES {
                    Sid: logon.0.cast(),
                    Attributes: 0,
                },
            ];

            let mut restricted: HANDLE = std::ptr::null_mut();
            if CreateRestrictedToken(
                base.0,
                DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                restricting.len() as u32,
                restricting.as_ptr(),
                &mut restricted,
            ) == 0
            {
                return Err(last_error("CreateRestrictedToken"));
            }
            let restricted = OwnedHandle(restricted);
            // Keep the SID allocations alive until the token is built.
            drop((incode, everyone, logon));

            set_default_dacl(base.0, restricted.0)?;
            launch(restricted.0, command)
        }
    }

    /// Give the restricted token a default DACL that lets it open its own
    /// process — without one the child's own `OpenProcessToken` fails.
    unsafe fn set_default_dacl(base: HANDLE, token: HANDLE) -> Result<(), String> {
        let user = token_user_sid(base)?;
        // The user, SYSTEM and incode's private SID, all full access.
        let sddl = format!("D:(A;;GA;;;{user})(A;;GA;;;SY)(A;;GA;;;{INCODE_SID})");
        let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(&sddl).as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
            || sd.is_null()
        {
            return Err(last_error(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            ));
        }
        let sd = LocalPtr(sd);
        let mut present = 0i32;
        let mut dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
        let mut defaulted = 0i32;
        if GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) == 0 {
            return Err(last_error("GetSecurityDescriptorDacl"));
        }
        let mut info = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
        if SetTokenInformation(
            token,
            TokenDefaultDacl,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        ) == 0
        {
            return Err(last_error("SetTokenInformation(TokenDefaultDacl)"));
        }
        Ok(())
    }

    /// The user SID of `token`, as a string.
    unsafe fn token_user_sid(token: HANDLE) -> Result<String, String> {
        let buf = token_information(token, TokenUser)?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        sid_to_string(user.User.Sid)
    }

    /// The logon SID of `token`, kept alive as an owned copy.
    unsafe fn logon_sid(token: HANDLE) -> Result<LocalSid, String> {
        let buf = token_information(token, TokenLogonSid)?;
        let groups = &*buf.as_ptr().cast::<TOKEN_GROUPS>();
        if groups.GroupCount == 0 {
            return Err("the token has no logon SID".to_string());
        }
        // Round-trip through the string form so the SID outlives `buf`.
        let text = sid_to_string(groups.Groups.as_ptr().read().Sid)?;
        sid(&text)
    }

    /// Read one variable-length token information class into a byte buffer.
    unsafe fn token_information(token: HANDLE, class: i32) -> Result<Vec<u8>, String> {
        let mut len = 0u32;
        // First call sizes the buffer; it "fails" with ERROR_INSUFFICIENT_BUFFER.
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len);
        if len == 0 {
            return Err(last_error("GetTokenInformation(size)"));
        }
        let mut buf = vec![0u8; len as usize];
        if GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(last_error("GetTokenInformation"));
        }
        Ok(buf)
    }

    /// A SID as its string form.
    unsafe fn sid_to_string(sid: *mut core::ffi::c_void) -> Result<String, String> {
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        let mut out: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid.cast(), &mut out) == 0 || out.is_null() {
            return Err(last_error("ConvertSidToStringSidW"));
        }
        let mut len = 0usize;
        while *out.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
        LocalFree(out.cast());
        Ok(text)
    }

    /// Launch `cmd /S /C <command>` under `token`, inheriting this process's std
    /// handles (which are incode's pipes), and wait for it.
    unsafe fn launch(token: HANDLE, command: &str) -> Result<i32, String> {
        // Inherit the helper's own std handles: they are the pipes incode gave
        // it, so the command's output reaches incode unchanged.
        let stdin = GetStdHandle(STD_INPUT_HANDLE);
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        let stderr = GetStdHandle(STD_ERROR_HANDLE);
        for h in [stdin, stdout, stderr] {
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = stdin;
        si.hStdOutput = stdout;
        si.hStdError = stderr;

        // `/S /C "<command>"`: /S strips the outer quotes and runs the rest
        // verbatim, so a command with its own quotes is untouched.
        let mut line = wide(&format!("cmd.exe /S /C \"{command}\""));
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessAsUserW(
            token,
            std::ptr::null(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // inherit handles
            CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(), // cwd: inherit the helper's, which incode set to the workspace
            &si,
            &mut pi,
        ) == 0
        {
            return Err(last_error("CreateProcessAsUserW"));
        }
        let _thread = OwnedHandle(pi.hThread);
        let child = OwnedHandle(pi.hProcess);
        // Suspended so it could have been put in a job first; incode already
        // holds the helper in its job, and a child joins it, so resuming is
        // enough here.
        ResumeThread(pi.hThread);
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 0u32;
        if GetExitCodeProcess(pi.hProcess, &mut code) == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        drop((child, _thread));
        Ok(code as i32)
    }

    /// A handle closed once on drop.
    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: owned, closed once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// A `LocalAlloc`-owned pointer freed once on drop.
    struct LocalPtr(*mut core::ffi::c_void);
    impl Drop for LocalPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: from a Local* allocator, freed once.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// The last Win32 error, named by the call that produced it.
    fn last_error(call: &str) -> String {
        // SAFETY: reads the calling thread's error value; no arguments.
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        format!("{call} failed with error {code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only is confined on Windows; workspace-write and off are not
    /// applied here.
    #[test]
    fn only_read_only_is_applied_on_windows() {
        assert!(applies(SandboxMode::ReadOnly));
        assert!(!applies(SandboxMode::WorkspaceWrite));
        assert!(!applies(SandboxMode::Off));
    }

    /// wrap re-invokes incode as the helper for read-only, and declines the
    /// modes it does not confine.
    #[test]
    fn wrap_builds_the_helper_only_for_read_only() {
        let cmd = wrap(SandboxMode::ReadOnly, &[], ("cmd", "/C"), "echo hi").expect("read-only");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--sandbox-exec");
        assert_eq!(args[1], "read-only");
        assert_eq!(args.last().map(String::as_str), Some("echo hi"));
        assert!(wrap(SandboxMode::WorkspaceWrite, &[], ("cmd", "/C"), "echo hi").is_none());
    }
}
