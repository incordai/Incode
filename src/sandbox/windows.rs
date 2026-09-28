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
//! **workspace-write is applied only when the operator NAMED it** (`sandbox:
//! "workspace-write"` in `admin.json`, or `INCODE_SANDBOX`), through a
//! per-SESSION grant ([`acl`]): the first sandboxed command of the session adds
//! ONE inheritable ACE for a private per-process SID to each root's DACL, every
//! command runs under a write-restricted token whose restricting SIDs are that
//! SID, `Everyone` and the logon SID — so it writes only where the SID was
//! granted — and the ACE is removed when the session ends. A session that
//! crashed leaves its ACE behind; the next grant sweeps it first.
//!
//! Measured on a 18,673-object copy of incode's own tree (the scratch copy,
//! never a real project): grant 2.41–2.49 s and revoke 2.40–2.58 s over three
//! runs (6.0 s the first, cold); every object received the inherited ACE (that
//! is how NTFS inheritance works — it is written, not computed at access
//! time), but no file's contents or mtime changed and `git diff-files` stayed
//! clean; propagation did not follow a junction out of the tree; and the
//! `icacls /save` of the whole tree after the revoke was byte-identical to the
//! one before. Once per session that is a cost worth paying; per command (the
//! only route found before) it was not, which is why this was left off. Left
//! to the permission-mode default, workspace-write still runs unconfined here
//! and [`super::Sandbox::note`] says so. A read-only session — `/plan`, or
//! `sandbox: "read-only"` — is confined as before.
//!
//! **Network is not restricted by any token.** Measured: raw TCP, http and
//! Python's https all succeeded under the token. Only an AppContainer without
//! the internet capabilities blocks it, and an AppContainer reads only what is
//! granted to it or to `ALL APPLICATION PACKAGES` — measured here: `System32`
//! and Git's `cmd` are, but the workspace (`D:\Projects\…`), `~\.cargo\bin`
//! and the whole user profile are not. Granting all of them read access for a
//! session would be a far larger ACL change than the workspace grant, so
//! `sandbox_network: "deny"` is NOT enforced on Windows, and the note says so.
//!
//! # How it reaches a command
//!
//! [`wrap`] returns a `Command` that runs `incode --sandbox-exec <mode>
//! [--sid <SID> --temp <dir>] --shell <exe> --shell-kind <kind> -- <command>`.
//! incode spawns THAT through its ordinary shell machinery, so the pipes, the
//! live output, the timeout and the job all work unchanged; the helper
//! ([`run_helper`]) builds the token and launches the named shell on the
//! command with `CreateProcessAsUserW`, inheriting the helper's own std handles
//! (which are incode's pipes) and joining incode's job, then waits and exits
//! with the command's code.
//!
//! The shell is PowerShell whenever the token applies: Git Bash cannot start
//! under it (see [`super::Sandbox::posix_shell_runs`]).

use std::path::PathBuf;
use std::process::Command;

use super::{PerOs, SandboxMode, SandboxPolicy};
use crate::tools::shell_choice::Shell;

pub(super) mod acl;

/// Why `sandbox_network: "deny"` is not enforced here — the note's wording.
pub(super) const NETWORK_NOT_ENFORCED: &str =
    "Windows has no sound way to block it for a command: only an AppContainer can, and an \
     AppContainer cannot read the workspace or the user's toolchains unless each is granted \
     to it";

/// Whether Windows workspace-write must grant the roots first: only when the
/// operator named the mode.
pub(super) fn needs_grant(policy: SandboxPolicy) -> bool {
    policy.mode == SandboxMode::WorkspaceWrite && policy.named
}

/// Whether Windows confines this policy's filesystem mode. Read-only always;
/// workspace-write when it was named AND the session's grant is in hand; off
/// is not a thing to apply.
pub(super) fn applies(policy: SandboxPolicy, granted: bool) -> bool {
    match policy.mode {
        SandboxMode::ReadOnly => true,
        SandboxMode::WorkspaceWrite => needs_grant(policy) && granted,
        SandboxMode::Off => false,
    }
}

/// The command that runs `command` under the sandbox: incode re-invoked as the
/// `--sandbox-exec` helper, carrying the session's grant for workspace-write.
/// `None` for a mode not confined here (workspace-write without a grant, off),
/// or when incode cannot name its own binary — the caller then runs the
/// command unconfined, [`super::Sandbox::note`] having said so.
pub(super) fn wrap(
    mode: SandboxMode,
    grant: Option<&acl::Grant>,
    shell: &Shell,
    command: &str,
) -> Option<Command> {
    let args = helper_args(mode, grant, shell, command)?;
    let exe = std::env::current_exe().ok()?;
    let mut cmd = Command::new(exe);
    cmd.args(args);
    Some(cmd)
}

/// The helper's arguments after the binary: `--sandbox-exec <mode>`, the grant
/// for workspace-write, the shell, then `-- <command>` as ONE argument. `None`
/// for a mode this platform does not confine without more than it was given.
fn helper_args(
    mode: SandboxMode,
    grant: Option<&acl::Grant>,
    shell: &Shell,
    command: &str,
) -> Option<Vec<String>> {
    let mut args = vec!["--sandbox-exec".to_string(), mode.as_str().to_string()];
    match (mode, grant) {
        (SandboxMode::ReadOnly, _) => {}
        (SandboxMode::WorkspaceWrite, Some(grant)) => {
            args.push("--sid".to_string());
            args.push(grant.sid.clone());
            args.push("--temp".to_string());
            args.push(grant.temp.to_string_lossy().into_owned());
        }
        _ => return None,
    }
    args.push("--shell".to_string());
    args.push(shell.program().to_string_lossy().into_owned());
    args.push("--shell-kind".to_string());
    args.push(shell.flavor().as_str().to_string());
    args.push("--".to_string());
    args.push(command.to_string());
    Some(args)
}

/// What the Windows sandbox can and cannot do, for the report.
pub(super) fn per_os() -> PerOs {
    PerOs {
        mechanism: "a write-restricted token (workspace-write through a per-session ACE on each \
                    root)",
        cannot: "workspace-write changes each root's ACL for the session, so it is applied only \
                 when named (`sandbox: \"workspace-write\"` or INCODE_SANDBOX) and its grant was \
                 made",
        network: "not enforced (a token cannot block it; only an AppContainer can, and one cannot \
                  read the workspace or the user's toolchains without granting them to it)",
    }
}

/// The `--sandbox-exec` helper on Windows: build the write-restricted token and
/// run `command` under it, returning the command's exit code. Read-only
/// restricts to incode's fixed SID (granted nowhere); workspace-write to the
/// session's SID in `grant`, with `TEMP`/`TMP` pointed at the session's own
/// granted temp directory. Off runs the command through the plain shell. The
/// command runs in `shell`.
///
/// # Errors
///
/// When the command could not be started at all, or workspace-write came
/// without its grant. A command that ran and failed returns its own non-zero
/// code, not an error.
pub(crate) fn run_helper(
    mode: SandboxMode,
    grant: Option<(String, PathBuf)>,
    shell: &Shell,
    command: &str,
) -> anyhow::Result<i32> {
    let sid = match (mode, grant) {
        (SandboxMode::Off, _) => return plain(shell, command),
        (SandboxMode::ReadOnly, _) => imp::INCODE_SID.to_string(),
        (SandboxMode::WorkspaceWrite, Some((sid, temp))) => {
            if !sid.starts_with(acl::SID_PREFIX) {
                anyhow::bail!(
                    "--sid {sid} is not an incode session SID, so the command was not run"
                );
            }
            // The command's temp is the session's own granted directory: the
            // user's %TEMP% is not granted, and too many tools fail without
            // one. Set here, in the helper, because the caller may clear the
            // environment it hands over. Single-threaded: `main` runs this
            // before any thread starts.
            std::env::set_var("TEMP", &temp);
            std::env::set_var("TMP", &temp);
            sid
        }
        (SandboxMode::WorkspaceWrite, None) => anyhow::bail!(
            "workspace-write needs the session's grant (--sid, --temp), so the command was not run"
        ),
    };
    match imp::run_restricted(&shell.command_line(command), &sid) {
        Ok(code) => Ok(code),
        Err(e) => {
            // A sandbox that could not be built must not silently run the
            // command UNCONFINED — that is the failure mode this exists to
            // avoid. Report it; the caller shows it as the command's error.
            Err(anyhow::anyhow!(
                "the {} sandbox could not be applied, so the command was not run: {e}",
                mode.as_str()
            ))
        }
    }
}

/// Run `command` through the plain shell, unconfined, returning its code.
fn plain(shell: &Shell, command: &str) -> anyhow::Result<i32> {
    let status = shell.command(command).status()?;
    Ok(status.code().unwrap_or(-1))
}

pub(super) mod imp {
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
    /// incode's own private restricting SID for READ-ONLY. Fixed, and granted
    /// nowhere, so a token restricted to it writes nothing. A high,
    /// otherwise-unused RID under the NT-authority non-unique range; the
    /// workspace-write session SIDs share its prefix ([`super::acl::SID_PREFIX`]).
    pub(in crate::sandbox) const INCODE_SID: &str =
        "S-1-5-21-1111111111-2222222222-3333333333-4242";
    /// `TOKEN_ALL_ACCESS`, spelled numerically for the same reason.
    const TOKEN_ALL: TOKEN_ACCESS_MASK = 0x000f_01ff;

    /// A wide, NUL-terminated string for a Win32 W-API.
    pub(in crate::sandbox) fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// A SID pointer freed on drop with `LocalFree` (how
    /// `ConvertStringSidToSidW` allocates it).
    pub(in crate::sandbox) struct LocalSid(pub(in crate::sandbox) *mut core::ffi::c_void);
    impl Drop for LocalSid {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from a Local* allocator and is freed once.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// Build a SID from its string form.
    pub(in crate::sandbox) fn sid(s: &str) -> Result<LocalSid, String> {
        let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `wide(s)` is NUL-terminated; `&mut out` is the `*mut PSID` the
        // API writes the owned SID pointer into.
        let ok = unsafe { ConvertStringSidToSidW(wide(s).as_ptr(), &mut out) };
        if ok == 0 || out.is_null() {
            return Err(format!("could not build the SID {s}"));
        }
        Ok(LocalSid(out))
    }

    /// Run `command_line` (the shell and its arguments, whole) under a
    /// write-restricted token restricted to `private` (plus `Everyone` and the
    /// logon SID), returning its exit code. The token writes only where a
    /// restricting SID is granted: for read-only `private` is granted nowhere;
    /// for workspace-write it is the session SID, granted on the roots.
    pub(super) fn run_restricted(command_line: &str, private: &str) -> Result<i32, String> {
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

            // The three restricting SIDs: the private one, Everyone (so
            // `> NUL` still works) and this logon's SID (so the shell and the
            // tools it starts can run at all).
            let incode = sid(private)?;
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

            set_default_dacl(base.0, restricted.0, private)?;
            launch(restricted.0, command_line)
        }
    }

    /// Give the restricted token a default DACL that lets it open its own
    /// process — without one the child's own `OpenProcessToken` fails.
    unsafe fn set_default_dacl(base: HANDLE, token: HANDLE, private: &str) -> Result<(), String> {
        let user = token_user_sid(base)?;
        // The user, SYSTEM and the private restricting SID, all full access.
        let sddl = format!("D:(A;;GA;;;{user})(A;;GA;;;SY)(A;;GA;;;{private})");
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

    /// Launch `command_line` under `token`, inheriting this process's std
    /// handles (which are incode's pipes), and wait for it.
    unsafe fn launch(token: HANDLE, command_line: &str) -> Result<i32, String> {
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

        // The shell's own line, every argument quoted by `Shell`.
        let mut line = wide(command_line);
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
    pub(in crate::sandbox) struct OwnedHandle(pub(in crate::sandbox) HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: owned, closed once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// A `LocalAlloc`-owned pointer freed once on drop.
    pub(in crate::sandbox) struct LocalPtr(pub(in crate::sandbox) *mut core::ffi::c_void);
    impl Drop for LocalPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: from a Local* allocator, freed once.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// The last Win32 error, named by the call that produced it.
    pub(in crate::sandbox) fn last_error(call: &str) -> String {
        // SAFETY: reads the calling thread's error value; no arguments.
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        format!("{call} failed with error {code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sandbox::SandboxNetwork;

    fn policy(mode: SandboxMode, named: bool) -> SandboxPolicy {
        SandboxPolicy {
            mode,
            network: SandboxNetwork::Allow,
            named,
        }
    }

    fn grant() -> acl::Grant {
        acl::Grant {
            sid: format!("{}77", acl::SID_PREFIX),
            temp: PathBuf::from(r"C:\t\77.tmp"),
        }
    }

    /// Read-only is always confined. Workspace-write is confined only when the
    /// operator NAMED it and its grant is in hand — the permission-mode
    /// default does not change anyone's ACL. Off is not applied.
    #[test]
    fn workspace_write_applies_only_when_named_and_granted() {
        assert!(applies(policy(SandboxMode::ReadOnly, false), false));
        assert!(!applies(policy(SandboxMode::WorkspaceWrite, false), false));
        assert!(!applies(policy(SandboxMode::WorkspaceWrite, false), true));
        assert!(!applies(policy(SandboxMode::WorkspaceWrite, true), false));
        assert!(applies(policy(SandboxMode::WorkspaceWrite, true), true));
        assert!(!applies(policy(SandboxMode::Off, true), true));
        assert!(needs_grant(policy(SandboxMode::WorkspaceWrite, true)));
        assert!(!needs_grant(policy(SandboxMode::WorkspaceWrite, false)));
        assert!(!needs_grant(policy(SandboxMode::ReadOnly, true)));
    }

    /// wrap re-invokes incode as the helper for read-only, and for
    /// workspace-write only with the session's grant — carrying its SID and
    /// temp directory — and declines everything else.
    #[test]
    fn wrap_builds_the_helper_for_read_only_and_granted_workspace_write() {
        let args_of = |cmd: Command| -> Vec<String> {
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let ps = Shell::from_parts(
            PathBuf::from(r"C:\ps\powershell.exe"),
            crate::tools::shell_choice::Flavor::PowerShell,
        );
        let read = args_of(wrap(SandboxMode::ReadOnly, None, &ps, "echo hi").expect("read-only"));
        assert_eq!(
            read,
            [
                "--sandbox-exec",
                "read-only",
                "--shell",
                r"C:\ps\powershell.exe",
                "--shell-kind",
                "powershell",
                "--",
                "echo hi"
            ]
        );

        let grant = grant();
        let write = args_of(
            wrap(
                SandboxMode::WorkspaceWrite,
                Some(&grant),
                &ps,
                "echo  \"hi\"",
            )
            .expect("granted"),
        );
        assert_eq!(
            write,
            [
                "--sandbox-exec",
                "workspace-write",
                "--sid",
                grant.sid.as_str(),
                "--temp",
                r"C:\t\77.tmp",
                "--shell",
                r"C:\ps\powershell.exe",
                "--shell-kind",
                "powershell",
                "--",
                "echo  \"hi\""
            ]
        );
        assert!(wrap(SandboxMode::WorkspaceWrite, None, &ps, "echo hi").is_none());
        assert!(wrap(SandboxMode::Off, Some(&grant), &ps, "echo hi").is_none());
    }

    /// Git Bash cannot start under the token, so a POSIX shell is said to
    /// run only where the token does not apply: off, and workspace-write left
    /// to the permission-mode default.
    #[test]
    fn a_posix_shell_runs_only_where_the_token_does_not_apply() {
        let with = |mode, named| crate::sandbox::Sandbox::new(policy(mode, named), Vec::new());
        assert!(with(SandboxMode::Off, false).posix_shell_runs());
        assert!(with(SandboxMode::WorkspaceWrite, false).posix_shell_runs());
        assert!(!with(SandboxMode::WorkspaceWrite, true).posix_shell_runs());
        assert!(!with(SandboxMode::ReadOnly, false).posix_shell_runs());
    }

    /// The helper parses back exactly what wrap wrote, and refuses — rather
    /// than running unconfined — workspace-write without a grant, half a
    /// grant, or a SID that is not an incode session's.
    #[test]
    fn the_helper_refuses_workspace_write_without_a_proper_grant() {
        // Every argv names its shell, as `wrap` writes it.
        let s = |v: &[&str]| {
            let mut all: Vec<String> = v[..v.len() - 2].iter().map(|a| (*a).to_string()).collect();
            all.extend(
                [
                    "--shell",
                    r"C:\ps\powershell.exe",
                    "--shell-kind",
                    "powershell",
                ]
                .map(String::from),
            );
            all.extend(v[v.len() - 2..].iter().map(|a| (*a).to_string()));
            all
        };
        let err = crate::sandbox::run_windows_helper(&s(&["workspace-write", "--", "echo hi"]))
            .expect_err("no grant");
        assert!(err.to_string().contains("was not run"), "{err}");
        let err = crate::sandbox::run_windows_helper(&s(&[
            "workspace-write",
            "--sid",
            &format!("{}77", acl::SID_PREFIX),
            "--",
            "echo hi",
        ]))
        .expect_err("half a grant");
        assert!(err.to_string().contains("together"), "{err}");
        let err = crate::sandbox::run_windows_helper(&s(&[
            "workspace-write",
            "--sid",
            "S-1-1-0",
            "--temp",
            r"C:\t",
            "--",
            "echo hi",
        ]))
        .expect_err("a foreign SID");
        assert!(
            err.to_string().contains("not an incode session SID"),
            "{err}"
        );
        // No shell named: refused, never a default one.
        let err =
            crate::sandbox::run_windows_helper(&["read-only", "--", "echo hi"].map(String::from))
                .expect_err("no shell");
        assert!(err.to_string().contains("--shell"), "{err}");
    }
}
