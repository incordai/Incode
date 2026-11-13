//! Windows, natively: a write-restricted token, one ACE per writable root,
//! and a job object.
//!
//! Ported from incode's own Windows sandbox (`src/sandbox/windows.rs` and
//! `windows/acl.rs`, Apache-2.0), whose authors measured it on Windows. What
//! they measured is repeated here where it explains a choice. This crate's
//! port is checked by `tests/escape_windows.rs` on the `windows-latest` CI
//! runner, after `insbx setup-windows` (see the README).
//!
//! # Writes: a write-restricted token
//!
//! `CreateRestrictedToken` with `WRITE_RESTRICTED` and a list of restricting
//! SIDs gives a token that READS anywhere its user can, but WRITES only where
//! the user may AND one of the restricting SIDs is granted. The restricting
//! SIDs are:
//!
//! - a private SID made for this one run
//!   (`S-1-5-21-1111111111-2222222222-4242424242-<random>`), granted on the
//!   run's private temp directory and nowhere else ([`acl`]);
//! - each of this run's writable roots' own SID
//!   (`S-1-5-21-1111111111-2222222222-4141414141-<a>-<b>-<c>-<d>`, from the
//!   SHA-256 of its path, [`root_sid_from_digest`]), whose grant on that root
//!   is kept across runs ([`acl`]);
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
//! security descriptor, though no file's contents or mtime. So a root's grant
//! is made by the first run on it and KEPT: later runs read the root's own
//! ACL to see it is there and walk nothing. `insbx revoke` takes it back
//! exactly. The temp directory's grant is per run: revoked after it, and a
//! run that crashes leaves its ACE for the next run's sweep ([`acl`]).
//!
//! # Reads: `deny_read` is NOT enforced
//!
//! A write-restricted token consults its restricting SIDs only for write
//! access, so a deny ACE for the private SID would not stop a read. Denying
//! the user's own SID would hide the path from the user everywhere, not just
//! from the sandbox; a fully restricted token (not write-restricted) would
//! need every file a toolchain reads granted to one of its SIDs. So
//! `deny_read` is reported not enforced, and `INSBX_BACKEND=auto` sends such a
//! policy to WSL2, where Landlock enforces it.
//!
//! # Network: a second account the firewall cuts off ([`offline`])
//!
//! No token restricts the network (incode measured raw TCP, http and https
//! all working under it). Only an AppContainer without the internet
//! capabilities does, and an AppContainer reads only what is granted to it:
//! the workspace, the user's toolchains and profile would all need granting.
//! Instead, once `insbx setup-windows` has run (elevated), `deny` and `proxy`
//! run the command as a local account, `insbx-offline`, that persistent
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
//! # Git Bash: `msys`, a second account ([`offline`])
//!
//! Git Bash and every msys program fail to start under the write-restricted
//! token: the msys runtime builds its pipes and sections with a DACL that
//! grants only the token's user (msys2-runtime `sec_acl`), a write passes the
//! restricted check only where a restricting SID is granted, and none is
//! (`couldn't create signal pipe, Win32 error 5`, `CreateFileMapping ...,
//! Win32 error 5`; measured). Adding the token's own user SID to the
//! restricting SIDs fixes it (measured), but the offline account's ACEs are
//! kept across runs ([`acl`]), so trusting its SID would open every folder an
//! earlier deny run used (measured: a write into another folder succeeded).
//!
//! So a policy that asks (`[fs] msys = true`, `--msys`) runs as a third kind of
//! identity: the account `insbx-msys`, made by `insbx setup-windows` beside
//! `insbx-offline` with the same firewall filters, whose SID the token trusts
//! ([`token::restricted`]) and which has NO kept ACE anywhere:
//!
//! - its `Modify` on each writable root and `RX` on the reads are granted per
//!   run, by walking the trees, and taken back when the run ends or, after a
//!   crash, by the next run's sweep ([`acl::Lease`]);
//! - one msys run at a time (the mutex `Global\insbx-msys`), since the account
//!   is shared by every run that holds the mutex;
//! - each root carries an inheritable OWNER RIGHTS entry of `READ_CONTROL`
//!   only (kept), so a file the account creates with the default ACL, which it
//!   owns, does not give it an owner's implicit `WRITE_DAC` in a later run (a
//!   file created with its own explicit, protected ACL is not covered, and
//!   `Everyone` is a restricting SID too); the user's own `READ_CONTROL |
//!   WRITE_DAC` entry ([`acl`]) still lets the user re-ACL it. A root whose
//!   ACL passes down a CREATOR OWNER entry, or a broader OWNER RIGHTS one, is
//!   refused;
//! - what a crashed msys run left for the account, and a revoke could not take
//!   back (a root moved or gone, a failed revoke), refuses the next msys run
//!   until `insbx revoke --msys-leftovers` clears it ([`acl::Lease`]).
//!
//! It only means something here, with network `deny` or `proxy` (a run uses an
//! account only then, [`msys_gap`]); on Linux and macOS `msys` is ignored.
//! Without `msys`, use `cmd` or PowerShell, or `INSBX_BACKEND=wsl` for a POSIX
//! shell.

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
pub use acl::{clear_msys_leftovers, kept_folders, revoke_kept, KeptFolder};
#[cfg(windows)]
pub(crate) use helper::enter;

/// `insbx setup-windows` (or `--remove`): what it did, line by line.
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

/// Every writable root's own SID starts with this ([`root_sid_from_digest`]).
/// A different domain part from [`SID_PREFIX`], so the crash sweep, which
/// only ever revokes run SIDs, never touches a kept grant. Every part must
/// fit in 32 bits: Windows stores a larger one as `4294967295`, and the ACE
/// would then name a different SID from the token's.
pub const ROOT_SID_PREFIX: &str = "S-1-5-21-1111111111-2222222222-4141414141-";

/// A writable root's SID, from the SHA-256 of its [`path_key`] (`acl::root_sid`
/// computes the digest): the prefix and the digest's first 16 bytes as four
/// decimal sub-authorities. The same folder always gets the same SID, so its
/// grant can be kept across runs and checked with one read of the folder's
/// own ACL instead of re-made by walking every file beneath it.
///
/// Not a secret, as a run SID is not: a confined command cannot add a SID to
/// its own token (`CreateRestrictedToken` on a restricted token intersects
/// the restricting SIDs), and only insbx's helper builds tokens, with the SIDs
/// of the run's own roots and no others ([`is_root_sid`]).
#[must_use]
pub fn root_sid_from_digest(digest: &[u8; 32]) -> String {
    let part =
        |i: usize| u32::from_le_bytes([digest[i], digest[i + 1], digest[i + 2], digest[i + 3]]);
    format!(
        "{ROOT_SID_PREFIX}{}-{}-{}-{}",
        part(0),
        part(4),
        part(8),
        part(12)
    )
}

/// Whether `sid` has the shape of a root SID: the prefix and exactly four
/// decimal sub-authorities, written as Windows writes them. The helper
/// refuses any other in a spec's `root_sids`.
#[must_use]
pub fn is_root_sid(sid: &str) -> bool {
    let Some(rest) = sid.strip_prefix(ROOT_SID_PREFIX) else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| p.parse::<u32>().is_ok_and(|n| n.to_string() == *p))
}

/// The kept-grants list (`%LOCALAPPDATA%\insbx\kept`): one folder per line,
/// for `insbx grants` and `insbx revoke --all`. Only a listing: whether a
/// folder is granted is read from its own ACL, never from here, so an edited
/// list can neither grant nor hide a grant. Blank lines and a second spelling
/// of the same folder ([`path_key`]) are dropped.
#[must_use]
pub fn parse_kept(text: &str) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let key = path_key(line);
        if !seen.contains(&key) {
            seen.push(key);
            out.push(PathBuf::from(line));
        }
    }
    out
}

/// [`parse_kept`]'s file, written back.
#[must_use]
pub fn kept_text(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| format!("{}\n", p.display())).collect()
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
     a path from the sandbox alone; use WSL2 (INSBX_BACKEND=wsl, or auto), where Landlock does";

/// Why a policy's `msys = true` cannot be honoured natively with network
/// `allow`, or `None` when it can (or is not asked for). `msys` runs as the
/// `insbx-msys` account, and a run uses an account only under network deny or
/// proxy ([`offline`]); with `allow` the command would run as the caller, where
/// Git Bash does not start (see "Known limit: Git Bash" above). Called by
/// `status::enforcement` on Windows, where it makes the filesystem a gap, so
/// strict mode refuses the run and `--best-effort` runs without `msys`.
#[must_use]
pub fn msys_gap(fs: &Fs, net: NetMode) -> Option<Status> {
    (fs.msys && net == NetMode::Allow).then(|| {
        Status::NotEnforced(
            "msys = true needs network deny or proxy: Git Bash runs as the account insbx-msys, \
             and a run uses an account only when the network is denied (with `allow` it runs as \
             you, where Git Bash does not start); use --net deny or --net proxy"
                .to_string(),
        )
    })
}

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
    let msys = if fs.msys {
        "; msys: the command runs as insbx-msys, granted per run (roots and reads walked and \
         taken back), one such run at a time, Git Bash works"
    } else {
        ""
    };
    Status::Enforced(format!(
        "write-restricted token: reads anywhere, {writes} (each root granted to its own SID, \
         kept for later runs until `insbx revoke`; the temp directory to a per-run SID, revoked \
         after the run), the null device, and anywhere Everyone may write{msys}"
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
    /// An `msys` run ([`offline`]): the writable roots the `msys` account
    /// (`user`) is granted `Modify` on for this run alone, taken back when it
    /// ends or by the sweep. The offline account's own ACE on a root is kept,
    /// so it is not listed here; the msys account has none to keep.
    pub modify: Vec<PathBuf>,
    /// Which folder each `modify` path was when it was granted (the same
    /// length as `modify`; `None` where unknown). A path is only a name: a
    /// root moved aside and a new folder made under the old name must not be
    /// taken for the folder that holds the grant ([`FileId`]).
    pub modify_ids: Vec<Option<FileId>>,
}

/// Which file or folder a path named when it was recorded: the volume's serial
/// number and the object's file index (`GetFileInformationByHandle`), which
/// stay with the object when it is renamed or moved on its volume and differ
/// for a new folder of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub volume: u32,
    pub index: u64,
}

impl FileId {
    /// `<volume as 8 hex digits>:<index as 16>`.
    #[must_use]
    pub fn to_text(self) -> String {
        format!("{:08x}:{:016x}", self.volume, self.index)
    }

    /// [`Self::to_text`] read back.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (volume, index) = text.trim().split_once(':')?;
        Some(Self {
            volume: u32::from_str_radix(volume, 16).ok()?,
            index: u64::from_str_radix(index, 16).ok()?,
        })
    }
}

impl GrantRecord {
    /// Add a per-run writable root with what it was ([`FileId`]).
    pub fn push_modify(&mut self, path: PathBuf, id: Option<FileId>) {
        self.modify_ids.resize(self.modify.len(), None);
        self.modify.push(path);
        self.modify_ids.push(id);
    }

    /// What `modify[i]` was when it was recorded, when it is known.
    #[must_use]
    pub fn modify_id(&self, i: usize) -> Option<FileId> {
        self.modify_ids.get(i).copied().flatten()
    }

    /// The record's text: `sid`, a `root` per root, then `user`, a `read`
    /// per read path, a `parent` per listed folder and a `modify` per
    /// per-run writable root when an account takes part.
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
        for (i, path) in self.modify.iter().enumerate() {
            text.push_str(&format!("modify {}\n", path.display()));
            if let Some(id) = self.modify_id(i) {
                text.push_str(&format!("id {}\n", id.to_text()));
            }
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
                Some(("modify", v)) => record.push_modify(PathBuf::from(v), None),
                // The identity of the `modify` path just above it.
                Some(("id", v)) => {
                    if let (Some(last), Some(id)) = (record.modify_ids.last_mut(), FileId::parse(v))
                    {
                        *last = Some(id);
                    }
                }
                _ => {}
            }
        }
        record.modify_ids.resize(record.modify.len(), None);
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
/// command starts in, the helper program it is started through, and the
/// program it was asked to run (`program`, as [`resolve_program`] found it,
/// when it did), each unless a root already holds it (and so grants it
/// `Modify`). The program is the one the caller named, so letting the
/// account read it shows the command nothing it was not given: without it
/// a program under the user's profile (Claude Code's `~/.local/bin/claude.exe`)
/// could not even start, measured, unless `insbx setup-windows --read` had
/// opened its whole folder. A file the user may not re-ACL (`cmd.exe` in
/// System32) is left out later, by `acl::OfflineGrant::grantable`: it is
/// readable to `Users` already.
#[must_use]
pub fn offline_reads(
    roots: &[PathBuf],
    cwd: &Path,
    helper: &Path,
    program: Option<&Path>,
) -> Vec<PathBuf> {
    let held = |p: &Path| {
        roots
            .iter()
            .any(|r| holds(&r.to_string_lossy(), &p.to_string_lossy()))
    };
    let mut reads: Vec<PathBuf> = Vec::new();
    for p in [Some(cwd), Some(helper), program].into_iter().flatten() {
        let key = path_key(&p.to_string_lossy());
        if !held(p) && !reads.iter().any(|r| path_key(&r.to_string_lossy()) == key) {
            reads.push(p.to_path_buf());
        }
    }
    reads
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

// ---- Starting as the offline account. ----

/// `ERROR_SERVICE_ALREADY_RUNNING` (Win32 error 1056).
///
/// # The root cause this exists for
///
/// A network-deny run starts its command as the offline account through
/// `CreateProcessWithLogonW`, and that call is served by the Secondary Logon
/// service (seclogon), which is demand-start: it is not running until the
/// first such call starts it. When two runs make that first call at the same
/// time, the one whose call arrives while the other's start of seclogon is
/// still in progress is refused with this error instead of waiting for it —
/// and its command is not run. Observed on the `windows-latest` runner
/// (`insbx: the command could not be started as insbx-offline … Win32 error
/// 1056`), in the escape tests that start several network-deny runs at once
/// right after `setup-windows`; the race is inferred from that, not
/// reproduced on a machine where seclogon was already up.
pub const SECLOGON_STARTING: u32 = 1056;

/// How many more times a start refused while seclogon was starting is made.
pub const SECLOGON_RETRIES: u32 = 5;

/// How long to let seclogon finish starting before the start is made again.
pub const SECLOGON_START_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// Make the start `attempt` until it succeeds or fails with anything but
/// [`SECLOGON_STARTING`] — the refusal while seclogon is still starting — at
/// most `1 + SECLOGON_RETRIES` times, calling `wait(n)` before attempt
/// `n + 1`. Its last result: any other error, or seclogon still not up after
/// every wait, is the caller's to report.
///
/// Called by `windows::helper::launch_offline` around
/// `CreateProcessWithLogonW`, the one start that goes through seclogon.
pub fn start_once_seclogon_is_up<T>(
    mut attempt: impl FnMut() -> Result<T, u32>,
    mut wait: impl FnMut(u32),
) -> Result<T, u32> {
    let mut waited = 0;
    loop {
        match attempt() {
            Err(SECLOGON_STARTING) if waited < SECLOGON_RETRIES => {
                waited += 1;
                wait(waited);
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Limits;

    /// **A START REFUSED WHILE SECLOGON WAS STILL STARTING IS MADE AGAIN**,
    /// a bounded number of times; any other refusal is reported at once.
    #[test]
    fn a_start_refused_while_seclogon_starts_is_made_again_and_nothing_else_is() {
        // Refused twice while seclogon came up, then started.
        let mut answers = vec![
            Err(SECLOGON_STARTING),
            Err(SECLOGON_STARTING),
            Ok("started"),
        ];
        answers.reverse();
        let mut waits = Vec::new();
        let got = start_once_seclogon_is_up(
            || answers.pop().expect("asked too often"),
            |n| waits.push(n),
        );
        assert_eq!(got, Ok("started"));
        assert_eq!(waits, [1, 2]);

        // Any other refusal is not retried: a wrong password is wrong again.
        let mut calls = 0;
        let got: Result<(), u32> = start_once_seclogon_is_up(
            || {
                calls += 1;
                Err(1326)
            },
            |_| panic!("waited for a refusal that is not seclogon starting"),
        );
        assert_eq!((got, calls), (Err(1326), 1));

        // seclogon never up: 1 + SECLOGON_RETRIES starts, then the error stands.
        let mut calls = 0;
        let got: Result<(), u32> = start_once_seclogon_is_up(
            || {
                calls += 1;
                Err(SECLOGON_STARTING)
            },
            |_| {},
        );
        assert_eq!((got, calls), (Err(SECLOGON_STARTING), SECLOGON_RETRIES + 1));
    }

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
            // incode's own prefix: never swept or used by insbx.
            "S-1-5-21-1111111111-2222222222-3333333333-77",
        ] {
            assert!(!is_session_sid(bad), "{bad}");
        }
    }

    #[test]
    fn a_root_sid_is_four_sub_authorities_from_the_digest_and_nothing_else_passes() {
        let mut digest = [0u8; 32];
        digest[0] = 1; // 1 little-endian
        digest[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        digest[12] = 7;
        digest[31] = 0xff; // past the 16 bytes used: no effect
        let sid = root_sid_from_digest(&digest);
        assert_eq!(sid, format!("{ROOT_SID_PREFIX}1-4294967295-0-7"));
        assert!(is_root_sid(&sid));
        assert!(!is_session_sid(&sid), "a root SID is never a run SID");
        for bad in [
            ROOT_SID_PREFIX.to_string(),
            format!("{ROOT_SID_PREFIX}1-2-3"),
            format!("{ROOT_SID_PREFIX}1-2-3-4-5"),
            format!("{ROOT_SID_PREFIX}1-2-3-04"),
            format!("{ROOT_SID_PREFIX}1-2-3-4294967296"),
            format!("{ROOT_SID_PREFIX}1-2-3-x"),
            session_sid(77),
            "S-1-1-0".to_string(),
            "S-1-5-21-1111111111-2222222222-3333333333-1-2-3-4".to_string(),
        ] {
            assert!(!is_root_sid(&bad), "{bad}");
        }
        // Different digests, different SIDs.
        digest[8] = 1;
        assert_ne!(root_sid_from_digest(&digest), sid);
    }

    /// Windows stores a sub-authority above `u32::MAX` as `4294967295`, so a
    /// prefix with one would name a different SID on disk than in the token
    /// (it did: `4343434343`).
    #[test]
    fn every_part_of_both_sid_prefixes_fits_in_32_bits() {
        for prefix in [SID_PREFIX, ROOT_SID_PREFIX] {
            for part in prefix.trim_end_matches('-').split('-').skip(1) {
                assert!(part.parse::<u32>().is_ok(), "{prefix}: {part}");
            }
        }
    }

    #[test]
    fn the_kept_list_drops_blank_lines_and_second_spellings() {
        let text = "C:\\w\\a\n\n  \nc:/W/A/\nD:\\b c\n";
        let kept = parse_kept(text);
        assert_eq!(
            kept,
            vec![PathBuf::from(r"C:\w\a"), PathBuf::from(r"D:\b c")]
        );
        assert_eq!(parse_kept(&kept_text(&kept)), kept);
        assert!(parse_kept("").is_empty());
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
            reads: vec![
                PathBuf::from(r"C:\proj"),
                PathBuf::from(r"C:\bin\insbx.exe"),
            ],
            parents: vec![PathBuf::from(r"C:\Users\me\src")],
            modify: Vec::new(),
            modify_ids: Vec::new(),
        };
        assert!(offline.to_text().ends_with("parent C:\\Users\\me\\src\n"));
        assert_eq!(GrantRecord::parse(&offline.to_text()), Some(offline));
        // An msys run lists the roots its account holds for this run alone.
        let mut msys = GrantRecord {
            sid: session_sid(10),
            roots: vec![PathBuf::from(r"C:\tmp\run")],
            user: Some("S-1-5-21-1-2-3-1010".into()),
            reads: vec![PathBuf::from(r"C:\proj")],
            ..GrantRecord::default()
        };
        let id = FileId {
            volume: 0xdead_beef,
            index: 0x0001_0000_0000_002a,
        };
        msys.push_modify(PathBuf::from(r"C:\w\a"), Some(id));
        msys.push_modify(PathBuf::from(r"D:\b c"), None);
        assert!(
            msys.to_text()
                .ends_with("modify C:\\w\\a\nid deadbeef:000100000000002a\nmodify D:\\b c\n"),
            "{}",
            msys.to_text()
        );
        assert_eq!(GrantRecord::parse(&msys.to_text()), Some(msys.clone()));
        assert_eq!(msys.modify_id(0), Some(id));
        assert_eq!(msys.modify_id(1), None);
        assert_eq!(msys.modify_id(7), None);
        // A record without identities (or a bad one) still parses: unknown.
        let old = GrantRecord::parse(&format!(
            "sid {}\nmodify C:\\w\nid nonsense\nmodify D:\\x\n",
            session_sid(11)
        ))
        .expect("parses");
        assert_eq!(old.modify.len(), 2);
        assert_eq!(old.modify_ids, [None, None]);
        assert_eq!(FileId::parse(&id.to_text()), Some(id));
        assert_eq!(FileId::parse("zz:1"), None);
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
            "sid {}\nroot C:\\w\nuser S-1-5-21-1-2-3-1009\nread C:\\bin\\insbx.exe\n",
            session_sid(7)
        );
        let record = GrantRecord::parse(&text).expect("parses");
        assert_eq!(record.reads, [PathBuf::from(r"C:\bin\insbx.exe")]);
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
        assert_eq!(strip_verbatim(r"\\?\C:\t\insbx-1"), r"C:\t\insbx-1");
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
        assert!(refuse_broad(r"\\?\C:\Users\someone\AppData\Local\Temp\insbx-1", profile).is_ok());
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
            PathBuf::from(r"C:\t\insbx-1"),
        ];
        let helper = Path::new(r"C:\tools\insbx.exe");
        assert_eq!(
            offline_reads(&roots, Path::new(r"C:\w\proj\sub"), helper, None),
            [PathBuf::from(r"C:\tools\insbx.exe")]
        );
        assert_eq!(
            offline_reads(
                &roots,
                Path::new(r"C:\w"),
                Path::new(r"c:\W\PROJ\target\insbx.exe"),
                None
            ),
            [PathBuf::from(r"C:\w")]
        );
        assert_eq!(
            offline_reads(&[], Path::new(r"C:\w"), helper, None),
            [PathBuf::from(r"C:\w"), PathBuf::from(r"C:\tools\insbx.exe")]
        );
    }

    #[test]
    fn the_offline_account_also_reads_the_program_it_was_asked_to_run() {
        let roots = [PathBuf::from(r"C:\w\proj")];
        let helper = Path::new(r"C:\tools\insbx.exe");
        let claude = Path::new(r"C:\Users\me\.local\bin\claude.exe");
        assert_eq!(
            offline_reads(&roots, Path::new(r"C:\w\proj"), helper, Some(claude)),
            [PathBuf::from(r"C:\tools\insbx.exe"), claude.to_path_buf()]
        );
        // Inside a root already: nothing more to grant.
        let built = Path::new(r"c:\W\proj\target\tool.exe");
        assert_eq!(
            offline_reads(&roots, Path::new(r"C:\w\proj"), helper, Some(built)),
            [PathBuf::from(r"C:\tools\insbx.exe")]
        );
        // The helper itself named as the program: once.
        assert_eq!(
            offline_reads(&roots, Path::new(r"C:\w\proj"), helper, Some(helper)),
            [PathBuf::from(r"C:\tools\insbx.exe")]
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

    /// `msys` with network `allow` is a gap (the command would run as the
    /// caller, not an account); with deny or proxy, or not asked for, it is
    /// not, and the filesystem line says an msys run is in force.
    #[test]
    fn msys_needs_an_account_so_a_network_mode_that_uses_one() {
        let mut fs = Fs::default();
        assert!(msys_gap(&fs, NetMode::Allow).is_none(), "not asked for");
        fs.msys = true;
        for net in [NetMode::Deny, NetMode::Proxy] {
            assert!(msys_gap(&fs, net).is_none(), "{net:?}");
        }
        let gap = msys_gap(&fs, NetMode::Allow).expect("a gap");
        assert!(
            matches!(&gap, Status::NotEnforced(s) if s.contains("--net deny") && s.contains("insbx-msys")),
            "{gap:?}"
        );
        assert!(gap.is_gap());
        fs.mode = FsMode::WorkspaceWrite;
        assert!(matches!(fs_status(&fs), Status::Enforced(s) if s.contains("insbx-msys")));
        fs.msys = false;
        assert!(matches!(fs_status(&fs), Status::Enforced(s) if !s.contains("msys")));
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
            msys_sid: "S-1-5-21-1-1010".into(),
            proxy_ports: offline::DEFAULT_PROXY_PORTS,
            record: PathBuf::from(r"C:\ProgramData\insbx\offline.conf"),
        });
        assert!(!net_status(NetMode::Deny, &ready).is_gap());
        assert!(!net_status(NetMode::Proxy, &ready).is_gap());
    }
}
