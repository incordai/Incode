//! The grant: one inheritable `Modify` ACE per writable root, for one run.
//!
//! Ported from incode's `sandbox::windows::acl`. incode grants once per
//! session and keeps the roots granted across commands; `sbx` runs one
//! command per policy, so each run gets its own SID and its own [`Lease`]:
//! granted when the command is prepared, revoked when the lease drops after
//! it exits. Two runs at once never share a SID, so neither can write the
//! other's roots.
//!
//! # Network deny: the offline account's share
//!
//! When the command runs as the offline account ([`super::offline`]), the
//! token's normal access check is the account's, not the user's, so each root
//! also gets an ACE for the account (`Modify`), and the current folder and
//! the helper program one for reading. Writes still need the run's SID too
//! (the token's restricted check), so a root granted to the account for one
//! run is not writable by another run's command. The account's ACE on a path
//! is one ACE however many runs granted it (a grant merges into it), so a run
//! that ends removes it only when no other live run still lists that path,
//! and leaves read access when another only reads it
//! ([`super::offline::still_needed`]).
//!
//! # A crash leaves the ACE behind: the sweep
//!
//! Every grant is written down before it is made, in
//! `%LOCALAPPDATA%\sbx\grants\<rid>.grant` (the SID and each root, and the
//! account's SID and read paths), and the process holds a named mutex
//! `Global\sbx-grant-<rid>` while the lease lives. Every new lease first
//! sweeps: a record whose mutex no longer exists belongs to a process that
//! died, so its ACEs are revoked and the record deleted. A record whose
//! revoke failed is kept for the next sweep. An ACE a dead run left is
//! harmless meanwhile (only a token carrying that exact SID could use it, and
//! none will be made again), but it is the user's ACL, so it is cleaned.
//!
//! The sweep only ever revokes SIDs with this crate's prefix
//! ([`super::SID_PREFIX`]), and an account ACE only when the record's
//! account SID is the real offline account's, so a forged record can do no
//! more than remove another dead run's ACE.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorControl, ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
};

use super::offline::{still_needed, Need};
use super::token::{last_error, sid, sid_to_string, wide, LocalPtr, OwnedHandle};
use super::{fold_nested, is_session_sid, refuse_broad, session_sid, GrantRecord};

/// The rights granted on a root: `icacls`'s `M` (read, write, execute,
/// delete).
const MODIFY: u32 = 0x0013_01BF;
/// The rights granted on a read path: `icacls`'s `RX`.
const READ_EXECUTE: u32 = 0x0012_00A9;

/// How long a grant or revoke waits for another process changing an ACL at
/// the same moment. Propagation over a very large tree takes tens of
/// seconds; past this the run is refused rather than left hanging.
const LOCK_WAIT_MS: u32 = 120_000;

/// The offline account's part in a run's grant.
#[derive(Debug, Clone)]
pub(crate) struct OfflineGrant {
    /// The account's SID.
    pub user: String,
    /// Paths it is granted for reading only (the current folder when it is
    /// not inside a root, the helper program).
    pub reads: Vec<PathBuf>,
}

/// One run's grant on its roots. Dropping it revokes them.
pub(crate) struct Lease {
    record: GrantRecord,
    path: PathBuf,
    /// The roots and read paths a grant was attempted on, so far: what
    /// dropping the lease takes back.
    roots: Vec<PathBuf>,
    reads: Vec<PathBuf>,
    /// The named mutex that tells a later sweep this run is alive.
    _alive: OwnedHandle,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("record", &self.record)
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// Grant `roots` (the writable roots and the run's temp directory) to a
    /// new private SID, and with `offline` to the offline account as well,
    /// after sweeping what a crashed run left.
    ///
    /// # Errors
    /// When a root may not or could not be granted. Nothing this call granted
    /// is left behind on failure.
    pub(crate) fn acquire(
        roots: &[PathBuf],
        offline: Option<OfflineGrant>,
    ) -> Result<Self, String> {
        let profile = std::env::var("USERPROFILE").ok();
        for root in roots {
            refuse_broad(&root.to_string_lossy(), profile.as_deref())
                .map_err(|e| format!("workspace-write {e}"))?;
            if !root.is_dir() {
                return Err(format!(
                    "workspace-write cannot grant {}: not a directory",
                    root.display()
                ));
            }
        }
        let roots = fold_nested(roots);
        let (user, reads) = match offline {
            Some(o) => {
                for read in o.reads.iter().filter(|r| r.is_dir()) {
                    refuse_broad(&read.to_string_lossy(), profile.as_deref()).map_err(|e| {
                        format!(
                            "network deny runs the command as the offline account, which must \
                             be granted the current folder; {e} (run from a project folder)"
                        )
                    })?;
                }
                // Only what this process may re-ACL: anything else (an
                // sbx.exe under Program Files) is the system's, readable by
                // `Users` already or not this run's to open up. If it is not
                // readable, starting the command says so.
                let reads = o.reads.into_iter().filter(|r| may_change_acl(r)).collect();
                (Some(o.user), reads)
            }
            None => (None, Vec::new()),
        };
        let dir = records_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        sweep(&dir);

        let (rid, alive) = claim_rid(&dir)?;
        let mut lease = Self {
            record: GrantRecord {
                sid: session_sid(rid),
                roots: roots.clone(),
                user,
                reads: reads.clone(),
            },
            path: dir.join(format!("{rid}.grant")),
            roots: Vec::new(),
            reads: Vec::new(),
            _alive: alive,
        };
        // Written down BEFORE anything is granted, so a crash at any point
        // after this is swept.
        std::fs::write(&lease.path, lease.record.to_text())
            .map_err(|e| format!("{}: {e}", lease.path.display()))?;
        for root in &roots {
            // Listed first: a propagation can fail part-way (a file the user
            // may not re-ACL), and dropping the lease takes back whatever
            // landed.
            lease.roots.push(root.clone());
            let granted =
                change(root, &lease.record.sid, Change::Grant(MODIFY)).and_then(|()| match &lease
                    .record
                    .user
                {
                    Some(user) => change(root, user, Change::Grant(MODIFY)),
                    None => Ok(()),
                });
            if let Err(e) = granted {
                return Err(format!(
                    "workspace-write could not grant {}: {e}",
                    root.display()
                ));
            }
        }
        if let Some(user) = lease.record.user.clone() {
            for read in &reads {
                lease.reads.push(read.clone());
                change(read, &user, Change::Grant(READ_EXECUTE)).map_err(|e| {
                    format!(
                        "could not let the offline account read {}: {e}",
                        read.display()
                    )
                })?;
            }
        }
        Ok(lease)
    }

    /// The SID the command's token is restricted to.
    pub(crate) fn sid(&self) -> &str {
        &self.record.sid
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut stuck = GrantRecord {
            sid: self.record.sid.clone(),
            user: self.record.user.clone(),
            ..GrantRecord::default()
        };
        // One lock for the whole revoke, so another run's grant cannot slip
        // between deciding an account ACE is unused and removing it.
        let _lock = AclLock::take();
        for root in self.roots.iter().filter(|r| r.exists()) {
            // (The temp directory may already be gone.)
            if let Err(e) = change(root, &self.record.sid, Change::Revoke) {
                eprintln!(
                    "sbx: could not revoke the sandbox's access to {} ({e}); the next run \
                     sweeps it",
                    root.display()
                );
                stuck.roots.push(root.clone());
            }
        }
        if let Some(user) = &self.record.user {
            let others = live_account_grants(&self.path);
            for (path, was_root) in self
                .roots
                .iter()
                .map(|r| (r, true))
                .chain(self.reads.iter().map(|r| (r, false)))
                .filter(|(p, _)| p.exists())
            {
                if let Err(e) = release_account(path, user, &others) {
                    eprintln!(
                        "sbx: could not revoke the offline account's access to {} ({e}); the \
                         next run sweeps it",
                        path.display()
                    );
                    if was_root {
                        stuck.roots.push(path.clone());
                    } else {
                        stuck.reads.push(path.clone());
                    }
                }
            }
        }
        if stuck.roots.is_empty() && stuck.reads.is_empty() {
            let _ = std::fs::remove_file(&self.path);
        } else {
            let _ = std::fs::write(&self.path, stuck.to_text());
        }
    }
}

/// Take back the account's ACE on `path`, as far as no other live run
/// (`others`) still needs it.
fn release_account(
    path: &Path,
    user: &str,
    others: &[(Vec<PathBuf>, Vec<PathBuf>)],
) -> Result<(), String> {
    match still_needed(path, others) {
        Need::Modify => Ok(()),
        Need::Read => {
            change(path, user, Change::Revoke)?;
            change(path, user, Change::Grant(READ_EXECUTE))
        }
        Need::None => change(path, user, Change::Revoke),
    }
}

/// What every other live run granted the offline account: (roots, reads).
fn live_account_grants(own: &Path) -> Vec<(Vec<PathBuf>, Vec<PathBuf>)> {
    let Ok(entries) = std::fs::read_dir(records_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != own)
        .filter_map(|p| Some((record_rid(&p)?, p)))
        .filter(|(rid, _)| is_alive(*rid))
        .filter_map(|(_, p)| GrantRecord::parse(&std::fs::read_to_string(p).ok()?))
        .filter(|r| r.user.is_some())
        .map(|r| (r.roots, r.reads))
        .collect()
}

/// Let the offline account read `path` (a folder and all beneath it, or one
/// file), for `sbx setup-windows --read`.
///
/// # Errors
/// When the ACL cannot be read or written.
pub(crate) fn grant_read(path: &Path, user: &str) -> Result<(), String> {
    change(path, user, Change::Grant(READ_EXECUTE))
}

/// Remove every ACE for `user` from `path` (and what it propagated).
///
/// # Errors
/// When the ACL cannot be read or written.
pub(crate) fn revoke(path: &Path, user: &str) -> Result<(), String> {
    change(path, user, Change::Revoke)
}

/// Whether this process may change `path`'s DACL: it opens for `WRITE_DAC`
/// (as its owner, an administrator, or by an explicit grant).
fn may_change_acl(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const WRITE_DAC: u32 = 0x0004_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .access_mode(WRITE_DAC)
        .share_mode(7) // read, write, delete: never in anyone's way
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS) // folders open too
        .open(path)
        .is_ok()
}

/// The SID that owns `path`.
///
/// # Errors
/// When its security descriptor cannot be read.
pub(crate) fn owner_of(path: &Path) -> Result<String, String> {
    let mut owner: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: the owner points into `sd`, which is owned and freed only
    // after the owner has been read.
    unsafe {
        let err = GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        if err != 0 {
            return Err(format!(
                "reading the owner of {} failed with error {err}",
                path.display()
            ));
        }
        let sd = LocalPtr(sd);
        let text = sid_to_string(owner);
        drop(sd);
        text
    }
}

/// Where grant records live: beside the user's other per-machine data, never
/// inside anything a command is granted (a root holding the profile is
/// refused).
fn records_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("sbx").join("grants"))
        .unwrap_or_else(|| std::env::temp_dir().join("sbx-grants"))
}

/// A fresh RID nobody is using, and the liveness mutex that holds it.
fn claim_rid(dir: &Path) -> Result<(u32, OwnedHandle), String> {
    for attempt in 0..64u32 {
        let rid = random_rid(attempt);
        if is_alive(rid) || dir.join(format!("{rid}.grant")).exists() {
            continue;
        }
        // SAFETY: a NUL-terminated name; no security attributes, so the
        // handle is not inherited by the command (which would keep a dead
        // run looking alive).
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide(alive_name(rid)).as_ptr()) };
        if handle.is_null() {
            return Err(last_error("CreateMutexW"));
        }
        return Ok((rid, OwnedHandle(handle)));
    }
    Err("could not find an unused sandbox SID".to_string())
}

/// A random, non-zero RID. Uniqueness is what matters, not secrecy: a
/// process cannot add a SID to its own token, so knowing it gains nothing.
fn random_rid(salt: u32) -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.write_u32(salt);
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let rid = (h.finish() & 0x7fff_ffff) as u32;
    rid.max(1)
}

/// The RID a record file is named by.
fn record_rid(path: &Path) -> Option<u32> {
    if path.extension().is_none_or(|e| e != "grant") {
        return None;
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok())
}

/// Revoke what dead runs left: every record whose liveness mutex is gone.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // The real offline account's SID, looked up once and only if a dead
    // record names an account.
    let mut account: Option<Option<String>> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(rid) = record_rid(&path) else {
            continue;
        };
        if is_alive(rid) {
            continue;
        }
        let Some(record) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| GrantRecord::parse(&t))
        else {
            continue;
        };
        if !is_session_sid(&record.sid) {
            continue;
        }
        let Ok(_lock) = AclLock::take() else {
            return;
        };
        let mut clean = true;
        for root in record.roots.iter().filter(|r| r.exists()) {
            if change(root, &record.sid, Change::Revoke).is_err() {
                clean = false;
            }
        }
        if let Some(user) = &record.user {
            let real = account.get_or_insert_with(super::setup::account_sid);
            // Only the real account's ACEs; a record naming anyone else is
            // not acted on (and an account since removed left only ACEs for
            // a SID nothing can carry any more).
            if real.as_deref() == Some(user.as_str()) {
                let others = live_account_grants(&path);
                for p in record
                    .roots
                    .iter()
                    .chain(&record.reads)
                    .filter(|p| p.exists())
                {
                    if release_account(p, user, &others).is_err() {
                        clean = false;
                    }
                }
            }
        }
        if clean {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether run `rid` is still alive: its liveness mutex exists. Any answer
/// but "no such mutex" counts as alive, so a sweep never revokes a live
/// run's grant on a doubt.
fn is_alive(rid: u32) -> bool {
    // SAFETY: a NUL-terminated name; the handle, if any, is closed at once.
    unsafe {
        let handle = OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            0,
            wide(alive_name(rid)).as_ptr(),
        );
        if !handle.is_null() {
            CloseHandle(handle);
            return true;
        }
        GetLastError() != ERROR_FILE_NOT_FOUND
    }
}

fn alive_name(rid: u32) -> String {
    format!("Global\\sbx-grant-{rid}")
}

#[derive(Clone, Copy)]
enum Change {
    /// An ACE with these rights, inherited by everything beneath a folder.
    Grant(u32),
    Revoke,
}

/// Add (or remove) the SID's ACE on `path`, propagating to everything
/// beneath a folder (a file gets a plain ACE). The DACL's protection is kept
/// as it was. Serialised across processes, because two read-modify-writes
/// of one DACL at once would lose one.
fn change(path: &Path, sid_text: &str, change: Change) -> Result<(), String> {
    let _lock = AclLock::take()?;
    let trustee = sid(sid_text)?;
    let wide_path = wide(path);
    let folder = path.is_dir();
    // SAFETY: every pointer passed lives to the end of the block
    // (`wide_path`, `trustee`, the returned descriptor and ACL, freed by
    // their owners); each call's result is checked.
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let err = GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        );
        if err != 0 {
            return Err(format!(
                "reading the ACL of {} failed with error {err}",
                path.display()
            ));
        }
        let descriptor = LocalPtr(descriptor);
        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        if GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) == 0 {
            return Err(last_error("GetSecurityDescriptorControl"));
        }
        let inherit: ACE_FLAGS = if folder {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        let (mode, rights, inheritance) = match change {
            Change::Grant(rights) => (GRANT_ACCESS, rights, inherit),
            Change::Revoke => (REVOKE_ACCESS, 0, NO_INHERITANCE),
        };
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: rights,
            grfAccessMode: mode,
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: trustee.0.cast(),
            },
        };
        let mut updated: *mut ACL = std::ptr::null_mut();
        let err = SetEntriesInAclW(1, &entry, dacl, &mut updated);
        if err != 0 {
            return Err(format!("SetEntriesInAclW failed with error {err}"));
        }
        let updated = LocalPtr(updated.cast());
        let protection = if control & SE_DACL_PROTECTED != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        let err = SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | protection,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            updated.0.cast::<ACL>(),
            std::ptr::null(),
        );
        drop((updated, descriptor));
        if err != 0 {
            return Err(format!(
                "setting the ACL of {} failed with error {err}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// The cross-process lock around an ACL change, released on drop. A
/// Windows mutex is recursive for the thread that holds it, so a caller may
/// hold it across several [`change`]s, which each take it again.
struct AclLock(HANDLE);

impl AclLock {
    fn take() -> Result<Self, String> {
        // SAFETY: a NUL-terminated name; the handle is owned by the guard.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, wide("Global\\sbx-acl").as_ptr());
            if handle.is_null() {
                return Err(last_error("CreateMutexW"));
            }
            let waited = WaitForSingleObject(handle, LOCK_WAIT_MS);
            if waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED {
                CloseHandle(handle);
                return Err(
                    "another process held the ACL lock for two minutes; not waiting longer"
                        .to_string(),
                );
            }
            Ok(Self(handle))
        }
    }
}

impl Drop for AclLock {
    fn drop(&mut self) {
        // SAFETY: owned, held, released and closed once.
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}
