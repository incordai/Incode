//! Windows, natively: a write-restricted token, one ACE per writable root,
//! and a job object.
//!
//! Ported from incode's own Windows sandbox (`src/sandbox/windows.rs` and
//! `windows/acl.rs`, Apache-2.0), whose authors measured it on Windows. What
//! they measured is repeated here where it explains a choice; nothing in this
//! crate's port has itself run on Windows yet (see the README).
//!
//! # Writes: a write-restricted token
//!
//! `CreateRestrictedToken` with `WRITE_RESTRICTED` and a list of restricting
//! SIDs gives a token that READS anywhere its user can, but WRITES only where
//! the user may AND one of the restricting SIDs is granted. The restricting
//! SIDs are:
//!
//! - a private SID made for this one run
//!   (`S-1-5-21-1111111111-2222222222-4242424242-<random>`), granted on each
//!   writable root and the run's private temp directory and nowhere else
//!   ([`acl`]);
//! - `Everyone`, so `> NUL` still works;
//! - this logon's SID, so the command can use its window station and desktop
//!   and start at all.
//!
//! So a location that grants `Everyone` (or the logon SID) write access stays
//! writable to the command: that is the price of the second and third SID,
//! and the README lists it. incode measured, without elevation: writes to the
//! workspace and to `Documents` refused, `reg add HKCU`, `schtasks /create`
//! and process injection into `explorer` refused, reads and `> NUL` working.
//!
//! `read-only` is the same token with no root granted: only the temp
//! directory, which the policy promises the command in every mode.
//!
//! # The grant
//!
//! NTFS inheritance is written into every object beneath a root, not computed
//! at access time, so granting a root costs time linear in its size (incode
//! measured 2.4-2.6 s each way on 18,673 objects) and touches every object's
//! security descriptor, though no file's contents or mtime. The revoke after
//! the run is exact (an `icacls /save` of the tree was byte-identical before
//! and after). A run that crashes leaves its ACE; the next run sweeps it
//! ([`acl`]).
//!
//! # Reads: `deny_read` is NOT enforced
//!
//! A write-restricted token consults its restricting SIDs only for write
//! access, so a deny ACE for the private SID would not stop a read. Denying
//! the user's own SID would hide the path from the user everywhere, not just
//! from the sandbox; a fully restricted token (not write-restricted) would
//! need every file a toolchain reads granted to one of its SIDs. So
//! `deny_read` is reported not enforced, and `SBX_BACKEND=auto` sends such a
//! policy to WSL2, where Landlock enforces it.
//!
//! # Network: a second account the firewall cuts off ([`offline`])
//!
//! No token restricts the network (incode measured raw TCP, http and https
//! all working under it). Only an AppContainer without the internet
//! capabilities does, and an AppContainer reads only what is granted to it:
//! the workspace, the user's toolchains and profile would all need granting.
//! Instead, once `sbx setup-windows` has run (elevated), `deny` and `proxy`
//! run the command as a local account, `sbx-offline`, that persistent
//! Windows Filtering Platform filters block from the network, as OpenAI
//! Codex's Windows sandbox does. Without that setup they are reported not
//! enforced, and `auto` sends them to WSL2.
//!
//! # Lifetime and limits: a job object
//!
//! The helper starts the command suspended, puts it in a new job object with
//! `KILL_ON_JOB_CLOSE`, then lets it run. Every process the command starts is
//! in the job too, and cannot leave it (breakaway is not allowed). The job's
//! only handle is the helper's, so the whole tree dies when the helper exits:
//! when the command ends, when the parent kills the helper at the wall-clock
//! limit, or when the parent itself dies (the helper watches it). The policy's
//! `memory_mb` becomes the job's committed-memory limit per process and
//! `cpu_secs` its user-mode time limit per process. `file_size_mb` and
//! `open_files` have no Windows equivalent and are reported not enforced.
//!
//! # Known limit: Git Bash
//!
//! Git Bash and every msys program fail to start under the write-restricted
//! token: the msys runtime maps a per-user shared section the token may not
//! map writable, and dies with `CreateFileMapping ..., Win32 error 5`
//! (incode measured). Use `cmd` or PowerShell, or `SBX_BACKEND=wsl` for a
//! POSIX shell.

// The pure parts below are compiled on every platform so their tests run in
// Linux CI; only the Windows build calls them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use crate::policy::{Fs, FsMode, NetMode};
use crate::Status;

#[cfg(windows)]
pub(crate) mod acl;
#[cfg(windows)]
mod helper;
pub mod offline;
#[cfg(windows)]
pub(crate) mod setup;
#[cfg(windows)]
mod token;

#[cfg(windows)]
pub(crate) use helper::enter;

/// `sbx setup-windows` (or `--remove`): what it did, line by line.
///
/// # Errors
/// When not elevated, or a step fails.
#[cfg(windows)]
pub fn setup_windows(args: &offline::SetupArgs) -> Result<Vec<String>, String> {
    setup::run(args)
}

/// Whether the offline account is set up and usable by this user, and if
/// not, why.
///
/// # Errors
/// Why not.
#[cfg(windows)]
pub fn offline_setup() -> Result<offline::Ready, String> {
    setup::ready().clone()
}

/// Every run's private SID starts with this, and the sweep touches no other.
/// The domain part is made up (no real machine or domain has it) and differs
/// from incode's own (`...-3333333333-`), so neither ever revokes the other's
/// grants.
pub const SID_PREFIX: &str = "S-1-5-21-1111111111-2222222222-4242424242-";

/// The private SID for a run, from its random relative ID.
#[must_use]
pub fn session_sid(rid: u32) -> String {
    format!("{SID_PREFIX}{rid}")
}

/// Whether `sid` is one of this crate's run SIDs: the prefix and a non-zero
/// decimal RID, nothing else. The helper refuses any other SID, so a spec
/// cannot point the token at a SID that is granted somewhere real.
#[must_use]
pub fn is_session_sid(sid: &str) -> bool {
    sid.strip_prefix(SID_PREFIX)
        .and_then(|rid| {
            rid.parse::<u32>()
                .ok()
                .filter(|r| *r != 0)
                .map(|r| (rid, r))
        })
        .is_some_and(|(rid, r)| rid == r.to_string())
}

/// The restricted token's default DACL, in SDDL: the user, SYSTEM and the
/// run's SID, all full access. Objects the command creates without a
/// descriptor of their own (its own process and threads) get this, and
/// without the run's SID in it the command could not even open its own
/// process token: the restricted half of the access check would fail.
#[must_use]
pub fn default_dacl_sddl(user_sid: &str, run_sid: &str) -> String {
    format!("D:(A;;GA;;;{user_sid})(A;;GA;;;SY)(A;;GA;;;{run_sid})")
}

/// Why `deny_read` is not enforced natively.
pub const DENY_READ_NOT_ENFORCED: &str =
    "a write-restricted token consults its restricting SIDs only for writes, so no ACE can hide \
     a path from the sandbox alone; use WSL2 (SBX_BACKEND=wsl, or auto), where Landlock does";

/// What native Windows enforces of the filesystem policy.
#[must_use]
pub fn fs_status(fs: &Fs) -> Status {
    if fs.mode == FsMode::Off {
        return Status::NotRequested;
    }
    if !fs.deny_read.is_empty() {
        return Status::NotEnforced(format!(
            "deny_read is not enforced natively on Windows ({DENY_READ_NOT_ENFORCED}); writes \
             would be confined"
        ));
    }
    let writes = match fs.mode {
        FsMode::ReadOnly => "writes only to the run's private temp directory",
        _ => "writes only beneath the writable roots and the run's private temp directory",
    };
    Status::Enforced(format!(
        "write-restricted token: reads anywhere, {writes} (each granted to a private per-run \
         SID, revoked after the run), the null device, and anywhere Everyone may write"
    ))
}

/// What native Windows enforces of the network policy, given whether the
/// offline account is set up ([`offline::net_status`]).
#[must_use]
pub fn net_status(mode: NetMode, setup: &Result<offline::Ready, String>) -> Status {
    offline::net_status(mode, setup)
}

// ---- The grant's bookkeeping (see `acl`). ----

/// What one run granted, written down before it is granted so a crash is
/// swept ([`acl`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GrantRecord {
    /// The run's private SID: `Modify` on each root.
    pub sid: String,
    /// The writable roots and the temp directory.
    pub roots: Vec<PathBuf>,
    /// Network deny ([`offline`]): the offline account's SID, granted
    /// `Modify` on every root too, and read on `reads`.
    pub user: Option<String>,
    /// Paths only the offline account is granted, for reading: the current
    /// folder, the helper program.
    pub reads: Vec<PathBuf>,
    /// Folders the offline account may only list, by one ACE on the folder
    /// itself and nothing beneath it: the current folder's parent
    /// ([`offline_parents`]). Records written before this field have none.
    pub parents: Vec<PathBuf>,
}

impl GrantRecord {
    /// The record's text: `sid`, a `root` per root, then `user`, a `read`
    /// per read path and a `parent` per listed folder when the offline
    /// account takes part.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = format!("sid {}\n", self.sid);
        for root in &self.roots {
            text.push_str(&format!("root {}\n", root.display()));
        }
        if let Some(user) = &self.user {
            text.push_str(&format!("user {user}\n"));
        }
        for read in &self.reads {
            text.push_str(&format!("read {}\n", read.display()));
        }
        for parent in &self.parents {
            text.push_str(&format!("parent {}\n", parent.display()));
        }
        text
    }

    /// A record read back. `None` for one whose SID is not a run SID of this
    /// crate: the sweep must never revoke anyone else's ACE. (Which `user` a
    /// sweep may revoke is checked there, against the real account.)
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut record = Self::default();
        let mut sid = None;
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("sid", v)) => sid = Some(v.to_string()),
                Some(("root", v)) => record.roots.push(PathBuf::from(v)),
                Some(("user", v)) if offline::is_sid(v) => record.user = Some(v.to_string()),
                Some(("read", v)) => record.reads.push(PathBuf::from(v)),
                Some(("parent", v)) => record.parents.push(PathBuf::from(v)),
                _ => {}
            }
        }
        record.sid = sid.filter(|s| is_session_sid(s))?;
        Some(record)
    }
}

/// A path as Windows compares it: `\\?\` dropped (`canonicalize` adds it),
/// `/` as `\`, no trailing separator, lower case. Windows paths are
/// case-insensitive, and a refusal must not be dodged by spelling.
#[must_use]
pub fn path_key(path: &str) -> String {
    let p = path.replace('/', "\\");
    let p = if let Some(unc) = p.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(rest) = p.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        p
    };
    let trimmed = p.trim_end_matches('\\');
    // `C:\` keeps its separator: `C:` alone means "the current folder on C".
    let p = if trimmed.len() == 2 && trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    };
    p.to_lowercase()
}

/// Whether `outer` is `inner` or one of its ancestors.
#[must_use]
pub fn holds(outer: &str, inner: &str) -> bool {
    let (o, i) = (path_key(outer), path_key(inner));
    if o == i {
        return true;
    }
    let o = if o.ends_with('\\') {
        o
    } else {
        format!("{o}\\")
    };
    i.starts_with(&o)
}

/// `\\?\C:\x` as `C:\x`, for paths handed to commands (a `\\?\` `TEMP`
/// breaks many tools). Anything else is returned as it was.
#[must_use]
pub fn strip_verbatim(path: &str) -> String {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        Some(rest) if rest.starts_with("UNC\\") => format!(r"\\{}", &rest[4..]),
        _ => path.to_string(),
    }
}

/// Refuse a root whose grant would reach far past a workspace: a drive root,
/// a share's root, or a folder that holds the user profile. The grant would
/// reach every file beneath it, which is both slow and no confinement at all.
///
/// # Errors
/// When the root is one of those, or not absolute.
pub fn refuse_broad(root: &str, profile: Option<&str>) -> Result<(), String> {
    let key = path_key(root);
    let absolute = key.as_bytes().get(1) == Some(&b':') || key.starts_with(r"\\");
    if !absolute {
        return Err(format!("{root}: not an absolute path"));
    }
    let is_drive_root = key.len() == 3 && key.ends_with(":\\");
    let is_share_root = key.starts_with(r"\\") && key[2..].split('\\').count() <= 2;
    if is_drive_root || is_share_root {
        return Err(format!(
            "will not grant {root}: a drive or share root is not a workspace"
        ));
    }
    if profile.is_some_and(|p| holds(root, p)) {
        return Err(format!("will not grant {root}: it holds the user profile"));
    }
    Ok(())
}

/// The roots worth granting: deduplicated, in a stable order, and a root
/// inside another dropped (it inherits the outer one's ACE).
#[must_use]
pub fn fold_nested(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots {
        let s = r.to_string_lossy();
        if !out
            .iter()
            .any(|o| path_key(&o.to_string_lossy()) == path_key(&s))
        {
            out.push(r.clone());
        }
    }
    out.sort_by_key(|p| path_key(&p.to_string_lossy()));
    let all = out.clone();
    out.retain(|r| {
        !all.iter().any(|other| {
            let (o, i) = (other.to_string_lossy(), r.to_string_lossy());
            path_key(&o) != path_key(&i) && holds(&o, &i)
        })
    });
    out
}

/// What the offline account must also be able to read for a run whose
/// roots (writable roots, temp directory) are `roots`: the folder the
/// command starts in, and the helper program it is started through, each
/// unless a root already holds it (and so grants it `Modify`).
#[must_use]
pub fn offline_reads(roots: &[PathBuf], cwd: &Path, helper: &Path) -> Vec<PathBuf> {
    let held = |p: &Path| {
        roots
            .iter()
            .any(|r| holds(&r.to_string_lossy(), &p.to_string_lossy()))
    };
    [cwd, helper]
        .into_iter()
        .filter(|p| !held(p))
        .map(Path::to_path_buf)
        .collect()
}

/// The folders the offline account must be able to list for the run's
/// current folder `cwd`: every folder above it, up to (not including) a
/// drive or share root or `profiles` (the folder holding the user profiles,
/// `C:\Users`, which every user may list already and which an elevated run
/// must not re-ACL), less any a root or read folder already holds.
///
/// Why: Windows PowerShell 5.1 sets its first location by normalising the
/// process's current directory, and it looks up EVERY component in its
/// parent (`Directory.EnumerateFiles`/`EnumerateDirectories`) to get its
/// exact case, which needs `FILE_LIST_DIRECTORY` on each folder above. The
/// offline account has none under the user's profile, the lookup throws
/// `UnauthorizedAccessException`, startup swallows it, and PowerShell is
/// left at the drive root: `Set-Content rel.txt` writes `C:\rel.txt`.
/// Measured on GitHub's windows-latest: with only the immediate parent
/// listable an explicit `Set-Location` to the current folder still failed
/// "Access is denied"; the chain above it (`Temp`, `Local`, `AppData`, the
/// profile) was not listable. Traversal is `SeChangeNotifyPrivilege`'s (kept
/// in the command's token); each ACE is list-only and inherits to nothing.
#[must_use]
pub fn offline_parents(
    roots: &[PathBuf],
    reads: &[PathBuf],
    cwd: &Path,
    profiles: Option<&Path>,
) -> Vec<PathBuf> {
    let stop = profiles.map(|p| path_key(&p.to_string_lossy()));
    let mut out = Vec::new();
    let mut here = cwd.to_string_lossy().into_owned();
    while let Some(parent) = parent_of(&here) {
        // Absolute and not a drive or share root.
        if refuse_broad(&parent, None).is_err() {
            break;
        }
        if stop.as_deref() == Some(path_key(&parent).as_str()) {
            break;
        }
        let held = roots
            .iter()
            .chain(reads)
            .any(|r| holds(&r.to_string_lossy(), &parent));
        if !held {
            out.push(PathBuf::from(&parent));
        }
        here = parent;
    }
    out
}

/// The folder holding the user profiles (`C:\Users`): the parent of
/// `USERPROFILE`. Where [`offline_parents`] stops.
#[must_use]
pub fn profiles_dir() -> Option<PathBuf> {
    let profile = std::env::var("USERPROFILE").ok()?;
    parent_of(&profile).map(PathBuf::from)
}

/// Whether `path` is one of `parents` (as Windows compares paths): another
/// live run still needs the account to list it.
#[must_use]
pub fn is_listed(path: &Path, parents: &[PathBuf]) -> bool {
    let key = path_key(&path.to_string_lossy());
    parents
        .iter()
        .any(|p| path_key(&p.to_string_lossy()) == key)
}

/// `path`'s parent folder, Windows-style on every platform (the pure parts
/// are tested on Linux, where `Path::parent` does not split on `\`): the
/// text before the last separator, a bare `C:` given back its `\`. `None`
/// for a path with no separator.
fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let cut = trimmed.rfind(['\\', '/'])?;
    let parent = &trimmed[..cut];
    if parent.is_empty() || parent.ends_with(['\\', '/']) {
        // `\\server` and the like: no folder to grant.
        return None;
    }
    Some(if parent.ends_with(':') {
        format!("{parent}\\")
    } else {
        parent.to_string()
    })
}

// ---- Starting the command: the program and its command line. ----

/// The extensions tried when a program is named without one.
pub const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Find `program` the way a shell would, without the current directory:
/// a name with a separator is a path (relative to the command's folder);
/// a bare name is searched in each `PATH` folder. Without one of `PATHEXT`'s
/// extensions, each is tried in turn (so `npm` finds `npm.cmd`). `exists`
/// is asked about each candidate.
///
/// Deliberately not `CreateProcess`'s own search, which looks in the
/// helper's folder and the current folder first: a `cmd.exe` planted in the
/// workspace would be run in place of the real one.
#[must_use]
pub fn resolve_program(
    program: &str,
    path: Option<&str>,
    pathext: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let exts: Vec<String> = pathext
        .filter(|p| !p.trim().is_empty())
        .unwrap_or(DEFAULT_PATHEXT)
        .split(';')
        .map(str::trim)
        .filter(|e| e.starts_with('.') && e.len() > 1)
        .map(str::to_ascii_lowercase)
        .collect();
    let file_name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let has_known_ext = file_name
        .rfind('.')
        .is_some_and(|i| exts.contains(&file_name[i..].to_ascii_lowercase()));
    let candidates = |base: &str| -> Vec<String> {
        let mut c = Vec::new();
        if has_known_ext {
            c.push(base.to_string());
        } else {
            c.extend(exts.iter().map(|e| format!("{base}{e}")));
            c.push(base.to_string());
        }
        c
    };
    if program.contains(['\\', '/', ':']) {
        return candidates(program).into_iter().find(|c| exists(c));
    }
    for dir in path.unwrap_or("").split(';') {
        let dir = dir.trim().trim_matches('"').trim_end_matches(['\\', '/']);
        if dir.is_empty() {
            continue;
        }
        if let Some(found) = candidates(&format!("{dir}\\{program}"))
            .into_iter()
            .find(|c| exists(c))
        {
            return Some(found);
        }
    }
    None
}

/// Whether a resolved program is a batch file, which only `cmd.exe` runs.
#[must_use]
pub fn is_batch(program: &str) -> bool {
    let p = program.to_ascii_lowercase();
    p.ends_with(".bat") || p.ends_with(".cmd")
}

/// Append `arg` to a command line, quoted so that `CommandLineToArgvW` (and
/// the C runtime) read it back as exactly one argument: in quotes when it is
/// empty or holds a space, tab, newline or quote; a quote escaped with a
/// backslash; backslashes doubled only where they precede a quote.
pub fn append_arg(line: &mut Vec<u16>, arg: &[u16]) {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    if !line.is_empty() {
        line.push(u16::from(b' '));
    }
    let plain = !arg.is_empty()
        && !arg.iter().any(|&c| {
            c == QUOTE || c == u16::from(b' ') || c == u16::from(b'\t') || c == 10 || c == 11
        });
    if plain {
        line.extend_from_slice(arg);
        return;
    }
    line.push(QUOTE);
    let mut backslashes = 0usize;
    for &c in arg {
        if c == BACKSLASH {
            backslashes += 1;
            continue;
        }
        if c == QUOTE {
            line.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
        } else {
            line.extend(std::iter::repeat_n(BACKSLASH, backslashes));
        }
        backslashes = 0;
        line.push(c);
    }
    line.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    line.push(QUOTE);
}

/// The whole command line for `argv` (UTF-16, no terminating NUL).
#[must_use]
pub fn command_line(argv: &[Vec<u16>]) -> Vec<u16> {
    let mut line = Vec::new();
    for a in argv {
        append_arg(&mut line, a);
    }
    line
}

/// The command line that runs a batch file: `cmd.exe /d /s /c "<line>"`.
/// `/d` skips AutoRun commands from the registry; `/s` makes `cmd` strip
/// exactly the outer quotes. The batch file's arguments are then parsed by
/// `cmd` itself (`%`, `^`, `&` keep their meaning there), which is why this
/// is only used for `.bat`/`.cmd` programs; either way it runs confined.
#[must_use]
pub fn batch_command_line(comspec: &[u16], argv: &[Vec<u16>]) -> Vec<u16> {
    let mut line = Vec::new();
    append_arg(&mut line, comspec);
    for flag in ["/d", "/s", "/c"] {
        line.push(u16::from(b' '));
        line.extend(flag.encode_utf16());
    }
    line.extend(" \"".encode_utf16());
    line.extend(command_line(argv));
    line.push(u16::from(b'"'));
    line
}

// ---- The job's limits. ----

/// `JOB_OBJECT_LIMIT_*` values, spelled here so the flag arithmetic is
/// testable everywhere; the Windows build checks them against `windows-sys`.
pub mod job_flags {
    pub const PROCESS_TIME: u32 = 0x0000_0002;
    pub const PROCESS_MEMORY: u32 = 0x0000_0100;
    pub const DIE_ON_UNHANDLED_EXCEPTION: u32 = 0x0000_0400;
    pub const KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
}

/// What the job object is set to, from the policy's limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    /// `JOB_OBJECT_LIMIT_*` flags.
    pub flags: u32,
    /// Committed memory per process, in bytes.
    pub process_memory: usize,
    /// User-mode CPU time per process, in 100 ns units.
    pub process_user_time: i64,
}

impl JobLimits {
    /// Always `KILL_ON_JOB_CLOSE` (the tree dies with the helper) and
    /// `DIE_ON_UNHANDLED_EXCEPTION` (a crash ends the process instead of
    /// leaving it waiting on an error-report dialog nobody sees); memory and
    /// CPU when the policy sets them.
    #[must_use]
    pub fn from_limits(limits: &crate::policy::Limits) -> Self {
        const MB: u64 = 1024 * 1024;
        let mut flags = job_flags::KILL_ON_JOB_CLOSE | job_flags::DIE_ON_UNHANDLED_EXCEPTION;
        let mut process_memory = 0;
        let mut process_user_time = 0;
        if let Some(mb) = limits.memory_mb {
            flags |= job_flags::PROCESS_MEMORY;
            process_memory = usize::try_from(mb.saturating_mul(MB)).unwrap_or(usize::MAX);
        }
        if let Some(secs) = limits.cpu_secs {
            flags |= job_flags::PROCESS_TIME;
            process_user_time = i64::try_from(secs.saturating_mul(10_000_000)).unwrap_or(i64::MAX);
        }
        Self {
            flags,
            process_memory,
            process_user_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Limits;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn line(args: &[&str]) -> String {
        String::from_utf16(&command_line(
            &args.iter().map(|a| w(a)).collect::<Vec<_>>(),
        ))
        .expect("utf16")
    }

    #[test]
    fn session_sids_are_recognised_and_nothing_else() {
        let sid = session_sid(77);
        assert_eq!(sid, "S-1-5-21-1111111111-2222222222-4242424242-77");
        assert!(is_session_sid(&sid));
        for bad in [
            "S-1-1-0",
            SID_PREFIX,
            &format!("{SID_PREFIX}0"),
            &format!("{SID_PREFIX}007"),
            &format!("{SID_PREFIX}12-5"),
            &format!("{SID_PREFIX}99999999999"),
            // incode's own prefix: never swept or used by sbx.
            "S-1-5-21-1111111111-2222222222-3333333333-77",
        ] {
            assert!(!is_session_sid(bad), "{bad}");
        }
    }

    #[test]
    fn the_default_dacl_names_the_user_system_and_the_run() {
        assert_eq!(
            default_dacl_sddl("S-1-5-21-1-2-3-1001", &session_sid(9)),
            "D:(A;;GA;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1111111111-2222222222-4242424242-9)"
        );
    }

    #[test]
    fn a_record_round_trips_and_a_foreign_sid_is_ignored() {
        let roots = vec![PathBuf::from(r"C:\w\a"), PathBuf::from(r"D:\b c")];
        let plain = GrantRecord {
            sid: session_sid(123),
            roots: roots.clone(),
            ..GrantRecord::default()
        };
        let text = plain.to_text();
        assert_eq!(
            text,
            format!("sid {}\nroot C:\\w\\a\nroot D:\\b c\n", session_sid(123))
        );
        assert_eq!(GrantRecord::parse(&text), Some(plain));
        let offline = GrantRecord {
            sid: session_sid(9),
            roots,
            user: Some("S-1-5-21-1-2-3-1009".into()),
            reads: vec![PathBuf::from(r"C:\proj"), PathBuf::from(r"C:\bin\sbx.exe")],
            parents: vec![PathBuf::from(r"C:\Users\me\src")],
        };
        assert!(offline.to_text().ends_with("parent C:\\Users\\me\\src\n"));
        assert_eq!(GrantRecord::parse(&offline.to_text()), Some(offline));
        assert!(GrantRecord::parse("sid S-1-1-0\nroot C:\\w\n").is_none());
        assert!(GrantRecord::parse(&format!("sid {SID_PREFIX}\n")).is_none());
        assert!(GrantRecord::parse("root C:\\w\n").is_none());
        // A malformed user is dropped, not trusted.
        let odd = GrantRecord::parse(&format!("sid {}\nuser everyone\n", session_sid(5)));
        assert_eq!(odd.and_then(|r| r.user), None);
    }

    #[test]
    fn a_record_from_before_the_parent_lines_still_parses() {
        let text = format!(
            "sid {}\nroot C:\\w\nuser S-1-5-21-1-2-3-1009\nread C:\\bin\\sbx.exe\n",
            session_sid(7)
        );
        let record = GrantRecord::parse(&text).expect("parses");
        assert_eq!(record.reads, [PathBuf::from(r"C:\bin\sbx.exe")]);
        assert!(record.parents.is_empty());
        // And written back, it is the same text: nothing new appears.
        assert_eq!(record.to_text(), text);
    }

    #[test]
    fn the_account_lists_every_folder_above_the_current_one_up_to_the_profiles() {
        let none: [PathBuf; 0] = [];
        let users = Some(Path::new(r"C:\Users"));
        let ws = Path::new(r"C:\Users\me\AppData\Local\Temp\.tmpAb\ws");
        // Every folder above, stopping before `C:\Users`.
        assert_eq!(
            offline_parents(&[ws.to_path_buf()], &none, ws, users),
            [
                PathBuf::from(r"C:\Users\me\AppData\Local\Temp\.tmpAb"),
                PathBuf::from(r"C:\Users\me\AppData\Local\Temp"),
                PathBuf::from(r"C:\Users\me\AppData\Local"),
                PathBuf::from(r"C:\Users\me\AppData"),
                PathBuf::from(r"C:\Users\me"),
            ]
        );
        // As `canonicalize` spells it (the stop compares the same way), and
        // with a trailing separator.
        assert_eq!(
            offline_parents(&none, &none, Path::new(r"\\?\C:\Users\me\src\proj\"), users),
            [
                PathBuf::from(r"\\?\C:\Users\me\src"),
                PathBuf::from(r"\\?\C:\Users\me"),
            ]
        );
        // Outside the profiles: up to the drive root, which is never listed.
        assert_eq!(
            offline_parents(&none, &none, Path::new(r"D:\work\team\proj"), users),
            [PathBuf::from(r"D:\work\team"), PathBuf::from(r"D:\work")]
        );
        // A folder at the top of a drive or share: nothing above to list.
        for top in [
            r"C:\proj",
            r"\\?\C:\proj",
            r"D:\",
            r"\\srv\share\proj",
            r"\\?\UNC\srv\share\proj",
            r"\\srv\share",
            "proj",
        ] {
            assert_eq!(
                offline_parents(&none, &none, Path::new(top), users),
                none,
                "{top}"
            );
        }
        // Deeper on a share, the share's folders are listed.
        assert_eq!(
            offline_parents(&none, &none, Path::new(r"\\srv\share\team\proj"), users),
            [PathBuf::from(r"\\srv\share\team")]
        );
        // A folder a root or read folder holds is listable already; the ones
        // above it still need the ACE.
        let sub = Path::new(r"C:\w\proj\sub");
        assert_eq!(
            offline_parents(&[PathBuf::from(r"C:\W\Proj")], &none, sub, users),
            [PathBuf::from(r"C:\w")]
        );
        assert_eq!(
            offline_parents(&[PathBuf::from(r"C:\w")], &none, sub, users),
            none
        );
        assert_eq!(
            offline_parents(&none, &[PathBuf::from(r"c:\w\proj")], sub, users),
            [PathBuf::from(r"C:\w")]
        );
        // A root beside it, or inside the current folder, does not count.
        assert_eq!(
            offline_parents(
                &[PathBuf::from(r"C:\w\proj\subway"), sub.join("x")],
                &none,
                sub,
                users
            ),
            [PathBuf::from(r"C:\w\proj"), PathBuf::from(r"C:\w")]
        );
    }

    #[test]
    fn a_listed_parent_is_kept_while_another_run_lists_it() {
        let others = [PathBuf::from(r"C:\Users\me\src"), PathBuf::from(r"D:\x")];
        assert!(is_listed(Path::new(r"c:\users\ME\src\"), &others));
        assert!(is_listed(Path::new(r"\\?\C:\Users\me\src"), &others));
        assert!(!is_listed(Path::new(r"C:\Users\me\src\proj"), &others));
        assert!(!is_listed(Path::new(r"C:\Users\me"), &others));
        assert!(!is_listed(Path::new(r"C:\Users\me\src"), &[]));
    }

    #[test]
    fn paths_compare_the_way_windows_does() {
        assert_eq!(path_key(r"\\?\C:\Work\App\"), r"c:\work\app");
        assert_eq!(path_key("C:/Work"), r"c:\work");
        assert_eq!(path_key(r"\\?\C:\"), r"c:\");
        assert_eq!(path_key(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert!(holds(r"C:\Users", r"c:\users\me\x"));
        assert!(holds(r"\\?\C:\Users\Me", r"C:\Users\me"));
        assert!(!holds(r"C:\Users\me", r"C:\Users\meta"));
        assert!(holds(r"C:\", r"C:\anything"));
        assert_eq!(strip_verbatim(r"\\?\C:\t\sbx-1"), r"C:\t\sbx-1");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\s\x"), r"\\srv\s\x");
        assert_eq!(strip_verbatim(r"C:\t"), r"C:\t");
    }

    #[test]
    fn broad_roots_are_refused_and_nested_ones_folded() {
        let profile = Some(r"C:\Users\someone");
        for broad in [
            r"C:\",
            r"\\?\C:\",
            r"C:\Users",
            r"\\?\c:\users\SOMEONE",
            r"\\server\share",
            "relative",
        ] {
            assert!(refuse_broad(broad, profile).is_err(), "{broad}");
        }
        assert!(refuse_broad(r"C:\Users\someone\proj", profile).is_ok());
        assert!(refuse_broad(r"\\?\C:\Users\someone\AppData\Local\Temp\sbx-1", profile).is_ok());
        assert!(refuse_broad(r"\\server\share\proj", profile).is_ok());
        let folded = fold_nested(&[
            PathBuf::from(r"C:\w\a\inner"),
            PathBuf::from(r"C:\w\a"),
            PathBuf::from(r"c:\W\A"),
            PathBuf::from(r"D:\t"),
        ]);
        assert_eq!(folded, [PathBuf::from(r"C:\w\a"), PathBuf::from(r"D:\t")]);
    }

    #[test]
    fn the_offline_account_reads_the_current_folder_and_helper_unless_granted_already() {
        let roots = [
            PathBuf::from(r"\\?\C:\w\proj"),
            PathBuf::from(r"C:\t\sbx-1"),
        ];
        let helper = Path::new(r"C:\tools\sbx.exe");
        assert_eq!(
            offline_reads(&roots, Path::new(r"C:\w\proj\sub"), helper),
            [PathBuf::from(r"C:\tools\sbx.exe")]
        );
        assert_eq!(
            offline_reads(
                &roots,
                Path::new(r"C:\w"),
                Path::new(r"c:\W\PROJ\target\sbx.exe")
            ),
            [PathBuf::from(r"C:\w")]
        );
        assert_eq!(
            offline_reads(&[], Path::new(r"C:\w"), helper),
            [PathBuf::from(r"C:\w"), PathBuf::from(r"C:\tools\sbx.exe")]
        );
    }

    #[test]
    fn arguments_are_quoted_for_command_line_to_argv() {
        assert_eq!(line(&["cmd", "/c", "echo"]), "cmd /c echo");
        assert_eq!(line(&["a b", ""]), r#""a b" """#);
        assert_eq!(line(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(line(&[r"C:\dir\"]), r"C:\dir\");
        assert_eq!(line(&[r"C:\my dir\"]), r#""C:\my dir\\""#);
        assert_eq!(line(&[r#"a\"b"#]), r#""a\\\"b""#);
        assert_eq!(line(&[r"a\\b c"]), r#""a\\b c""#);
        assert_eq!(line(&["tab\there"]), "\"tab\there\"");
    }

    #[test]
    fn a_batch_file_runs_through_cmd_with_its_line_quoted_whole() {
        let l = batch_command_line(
            &w(r"C:\Windows\system32\cmd.exe"),
            &[w(r"C:\n\npm.cmd"), w("run"), w("a b")],
        );
        assert_eq!(
            String::from_utf16(&l).expect("utf16"),
            r#"C:\Windows\system32\cmd.exe /d /s /c "C:\n\npm.cmd run "a b"""#
        );
        assert!(is_batch(r"C:\n\NPM.CMD"));
        assert!(is_batch("x.bat"));
        assert!(!is_batch("x.exe"));
    }

    #[test]
    fn programs_are_found_on_path_with_pathext_and_never_in_the_current_folder() {
        let files = [
            r"C:\Windows\System32\cmd.exe",
            r"C:\node\npm.cmd",
            r"C:\py\python.exe",
            r"C:\tools\tool",
            r".\local.exe",
        ];
        let exists = |p: &str| files.iter().any(|f| f.eq_ignore_ascii_case(p));
        let path = Some(r"C:\Windows\System32;C:\node\;;C:\py;C:\tools");
        let find = |p: &str| resolve_program(p, path, None, &exists);
        assert_eq!(find("cmd").as_deref(), Some(r"C:\Windows\System32\cmd.exe"));
        assert_eq!(
            find("CMD.EXE").as_deref(),
            Some(r"C:\Windows\System32\CMD.EXE")
        );
        assert_eq!(find("npm").as_deref(), Some(r"C:\node\npm.cmd"));
        assert_eq!(find("python").as_deref(), Some(r"C:\py\python.exe"));
        assert_eq!(find("tool").as_deref(), Some(r"C:\tools\tool"));
        assert_eq!(find("local"), None, "the current folder is not searched");
        assert_eq!(find(r".\local").as_deref(), Some(r".\local.exe"));
        assert_eq!(find("missing"), None);
        assert_eq!(
            resolve_program("npm", path, Some(".EXE"), &exists),
            None,
            "PATHEXT decides the extensions"
        );
    }

    #[test]
    fn job_limits_follow_the_policy() {
        let base = job_flags::KILL_ON_JOB_CLOSE | job_flags::DIE_ON_UNHANDLED_EXCEPTION;
        let none = JobLimits::from_limits(&Limits::default());
        assert_eq!(none.flags, base);
        let both = JobLimits::from_limits(&Limits {
            memory_mb: Some(256),
            cpu_secs: Some(3),
            wall_secs: Some(9),
            ..Limits::default()
        });
        assert_eq!(
            both.flags,
            base | job_flags::PROCESS_MEMORY | job_flags::PROCESS_TIME
        );
        assert_eq!(both.process_memory, 256 * 1024 * 1024);
        assert_eq!(both.process_user_time, 30_000_000);
        let huge = JobLimits::from_limits(&Limits {
            cpu_secs: Some(u64::MAX),
            ..Limits::default()
        });
        assert_eq!(huge.process_user_time, i64::MAX);
    }

    #[test]
    fn statuses_say_what_windows_enforces() {
        let mut fs = Fs::default();
        assert_eq!(fs_status(&fs), Status::NotRequested);
        fs.mode = FsMode::WorkspaceWrite;
        assert!(matches!(fs_status(&fs), Status::Enforced(s) if s.contains("write-restricted")));
        fs.deny_read.push(PathBuf::from("secret"));
        assert!(matches!(fs_status(&fs), Status::NotEnforced(s) if s.contains("deny_read")));
        let missing = Err("the offline account is not set up".to_string());
        assert_eq!(net_status(NetMode::Allow, &missing), Status::NotRequested);
        assert!(net_status(NetMode::Deny, &missing).is_gap());
        assert!(net_status(NetMode::Proxy, &missing).is_gap());
        let ready = Ok(offline::Ready {
            sid: "S-1-5-21-1-1009".into(),
            proxy_ports: offline::DEFAULT_PROXY_PORTS,
            record: PathBuf::from(r"C:\ProgramData\sbx\offline.conf"),
        });
        assert!(!net_status(NetMode::Deny, &ready).is_gap());
        assert!(!net_status(NetMode::Proxy, &ready).is_gap());
    }
}
