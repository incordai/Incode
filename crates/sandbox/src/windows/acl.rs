//! The grant: one inheritable `Modify` ACE per writable root, for one run.
//!
//! Ported from incode's `sandbox::windows::acl`. incode grants once per
//! session and keeps the roots granted across commands; `sbx` runs one
//! command per policy, so each run gets its own SID and its own [`Lease`]:
//! granted when the command is prepared, revoked when the lease drops after
//! it exits. Two runs at once never share a SID, so neither can write the
//! other's roots.
//!
//! # A crash leaves the ACE behind: the sweep
//!
//! Every grant is written down before it is made, in
//! `%LOCALAPPDATA%\sbx\grants\<rid>.grant` (the SID and each root), and the
//! process holds a named mutex `Global\sbx-grant-<rid>` while the lease
//! lives. Every new lease first sweeps: a record whose mutex no longer exists
//! belongs to a process that died, so its ACEs are revoked and the record
//! deleted. A record whose revoke failed is kept for the next sweep. An ACE a
//! dead run left is harmless meanwhile (only a token carrying that exact SID
//! could use it, and none will be made again), but it is the user's ACL, so
//! it is cleaned.
//!
//! The sweep only ever revokes SIDs with this crate's prefix
//! ([`super::SID_PREFIX`]), so a forged record can do no more than remove
//! another dead run's ACE.

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
    GetSecurityDescriptorControl, ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
    PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
};

use super::token::{last_error, sid, wide, LocalPtr, OwnedHandle};
use super::{fold_nested, is_session_sid, parse_record, record_text, refuse_broad, session_sid};

/// The rights granted: `icacls`'s `M` (read, write, execute, delete).
const MODIFY: u32 = 0x0013_01BF;

/// How long a grant or revoke waits for another process changing an ACL at
/// the same moment. Propagation over a very large tree takes tens of
/// seconds; past this the run is refused rather than left hanging.
const LOCK_WAIT_MS: u32 = 120_000;

/// One run's grant on its roots. Dropping it revokes them.
pub(crate) struct Lease {
    sid: String,
    record: PathBuf,
    roots: Vec<PathBuf>,
    /// The named mutex that tells a later sweep this run is alive.
    _alive: OwnedHandle,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("sid", &self.sid)
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// Grant `roots` (the writable roots and the run's temp directory) to a
    /// new private SID, after sweeping what a crashed run left.
    ///
    /// # Errors
    /// When a root may not or could not be granted. Nothing this call granted
    /// is left behind on failure.
    pub(crate) fn acquire(roots: &[PathBuf]) -> Result<Self, String> {
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
        let dir = records_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        sweep(&dir);

        let (rid, alive) = claim_rid(&dir)?;
        let mut lease = Self {
            sid: session_sid(rid),
            record: dir.join(format!("{rid}.grant")),
            roots: Vec::new(),
            _alive: alive,
        };
        // Written down BEFORE anything is granted, so a crash at any point
        // after this is swept.
        std::fs::write(&lease.record, record_text(&lease.sid, roots.iter()))
            .map_err(|e| format!("{}: {e}", lease.record.display()))?;
        for root in &roots {
            if let Err(e) = change(root, &lease.sid, Change::Grant) {
                // A propagation can fail part-way (a file the user may not
                // re-ACL); take back whatever landed. Dropping `lease`
                // revokes the roots granted before this one.
                let _ = change(root, &lease.sid, Change::Revoke);
                return Err(format!(
                    "workspace-write could not grant {}: {e}",
                    root.display()
                ));
            }
            lease.roots.push(root.clone());
        }
        Ok(lease)
    }

    /// The SID the command's token is restricted to.
    pub(crate) fn sid(&self) -> &str {
        &self.sid
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut stuck = Vec::new();
        for root in &self.roots {
            if !root.exists() {
                continue; // the temp directory, already removed
            }
            if let Err(e) = change(root, &self.sid, Change::Revoke) {
                eprintln!(
                    "sbx: could not revoke the sandbox's access to {} ({e}); the next run \
                     sweeps it",
                    root.display()
                );
                stuck.push(root.clone());
            }
        }
        if stuck.is_empty() {
            let _ = std::fs::remove_file(&self.record);
        } else {
            let _ = std::fs::write(&self.record, record_text(&self.sid, stuck.iter()));
        }
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

/// Revoke what dead runs left: every record whose liveness mutex is gone.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "grant") {
            continue;
        }
        let Some(rid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if is_alive(rid) {
            continue;
        }
        let Some((sid, roots)) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| parse_record(&t))
        else {
            continue;
        };
        if !is_session_sid(&sid) {
            continue;
        }
        let mut clean = true;
        for root in roots.iter().filter(|r| r.exists()) {
            if change(root, &sid, Change::Revoke).is_err() {
                clean = false;
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
    Grant,
    Revoke,
}

/// Add (or remove) the SID's inheritable `Modify` ACE on `dir`, propagating
/// to everything beneath it. The DACL's protection is kept as it was.
/// Serialised across processes, because two read-modify-writes of one DACL
/// at once would lose one.
fn change(dir: &Path, sid_text: &str, change: Change) -> Result<(), String> {
    let _lock = AclLock::take()?;
    let trustee = sid(sid_text)?;
    let path = wide(dir);
    // SAFETY: every pointer passed lives to the end of the block (`path`,
    // `trustee`, the returned descriptor and ACL, freed by their owners);
    // each call's result is checked.
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let err = GetNamedSecurityInfoW(
            path.as_ptr(),
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
                dir.display()
            ));
        }
        let descriptor = LocalPtr(descriptor);
        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        if GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) == 0 {
            return Err(last_error("GetSecurityDescriptorControl"));
        }
        let (mode, rights, inheritance) = match change {
            Change::Grant => (GRANT_ACCESS, MODIFY, SUB_CONTAINERS_AND_OBJECTS_INHERIT),
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
            path.as_ptr(),
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
                dir.display()
            ));
        }
    }
    Ok(())
}

/// The cross-process lock around an ACL change, released on drop.
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
