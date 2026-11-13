//! The grant: one inheritable `Modify` ACE per writable root, kept across
//! runs, and one per run on its private temp directory.
//!
//! Ported from incode's `sandbox::windows::acl`, which grants once per
//! session and keeps the roots granted across commands.
//!
//! # Writable roots: kept, one SID per folder
//!
//! Granting a folder writes the ACE into every object beneath it (NTFS
//! inheritance is stored, not computed), so it costs time linear in the tree:
//! measured 2.3 s per walk on 17,752 objects, and a run used to walk its
//! roots twice (grant and revoke), five to seven times under network deny.
//! incode drives one `insbx run` per shell command, so every command paid it.
//! Now each root has its own SID, derived from its path ([`root_sid`],
//! [`super::root_sid_from_digest`]), and its grant is KEPT after the run:
//! the first run on a folder walks it once, and every later run only reads
//! the folder's own ACL to see the grant is there ([`passes_down`]). As
//! OpenAI Codex's Windows sandbox does (`codex-rs/windows-sandbox-rs`,
//! `cap.rs`: a capability SID per writable root, persisted).
//!
//! What keeps that confined: the command's token carries the SIDs of THIS
//! run's roots only ([`Lease::root_sids`]). A kept grant on another folder
//! names a SID the token does not hold, so it is not writable, exactly as a
//! folder never granted. `insbx revoke <PATH>|--all` takes kept grants back
//! ([`revoke_kept`]); `insbx grants` lists them ([`kept_folders`]).
//!
//! A file moved INTO a root from elsewhere keeps the ACL it had: no grant's
//! walk re-ACLs it (measured, and so it was with the per-run grant too), so
//! the command cannot write it (it fails closed). `icacls <FILE> /reset`
//! gives it its folder's ACL, the root's ACE included.
//!
//! # The temp directory: one SID per run
//!
//! The run's private temp directory is new and empty, so its grant is
//! cheap, and it stays per run: granted to the run's own SID when the
//! command is prepared, revoked when the [`Lease`] drops after it exits. Two
//! runs at once never share that SID, so neither can write the other's temp
//! directory, even with the same roots.
//!
//! # Network deny: the offline account's share
//!
//! When the command runs as the offline account ([`super::offline`]), the
//! token's normal access check is the account's, not the user's, so each root
//! also gets an ACE for the account (`Modify`), and the current folder and
//! the helper program one for reading. Writes still need the root's own SID
//! too (the token's restricted check), so a root granted to the account for
//! one run is not writable by another run's command unless that run names
//! the same root. The account's ACEs on roots and on a current FOLDER are
//! kept like the roots' own: a later network-deny run may READ a folder an
//! earlier one was granted, never write it. Its ACEs on the temp directory
//! and on a read FILE (the helper program) last the run: one ACE however
//! many runs granted it (a grant merges into it), so a run that ends removes
//! it only when no other live run still lists that path, and leaves read
//! access when another only reads it ([`super::offline::still_needed`]).
//!
//! # Network deny: what the account creates stays the user's to re-ACL
//!
//! A file the command creates under the offline account is OWNED by the
//! account, and the user's `Modify` on it holds no `WRITE_DAC`. A later run's
//! grant reaches a file only by being propagated onto it, which the user may
//! not do there, so that run's command could not write a file an earlier run
//! made (cargo's build cache, the second `cargo test`), and the earlier run's
//! SID could not be revoked from it. So each root also gets one inheritable
//! ACE for the user's own SID, `READ_CONTROL | WRITE_DAC`, which is what
//! owning the file would give ([`keep_acl_rights`]). It is never revoked: it
//! names only the user, on the user's own folder, and a revoke would strip
//! it from those files again. It gives the command nothing: a network-deny
//! command runs as the account, whose token does not hold the user's SID.
//!
//! # Network deny: listing the current folder's parent
//!
//! PowerShell finds its first location by looking the current folder up in
//! its parent, which needs the right to list the parent
//! ([`super::offline_parents`] says why). So the account also gets one ACE
//! on the parent: `FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES |
//! FILE_TRAVERSE | SYNCHRONIZE`, no inheritance flags, written to that one
//! folder through its handle (`SetKernelObjectSecurity`, which, unlike
//! `SetNamedSecurityInfoW` and `SetSecurityInfo`, never walks the children
//! to re-propagate). It shows the command the names in the parent, not
//! their contents, and reaches nothing beneath it. It is shared like the
//! account's other ACEs: removed (that exact ACE, nothing else) when no
//! other live run lists the same parent.
//!
//! # A crash leaves the ACE behind: the sweep
//!
//! Every grant is written down before it is made, in
//! `%LOCALAPPDATA%\insbx\grants\<rid>.grant` (the SID and each root, and the
//! account's SID and read paths), and the process holds a named mutex
//! `Global\insbx-grant-<rid>` while the lease lives. Every new lease first
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
    GetNamedSecurityInfoW, GetSecurityInfo, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
use windows_sys::Win32::Security::{
    AclSizeInformation, AddAccessAllowedAceEx, AddAce, EqualSid, GetAce, GetAclInformation,
    GetLengthSid, GetSecurityDescriptorControl, InitializeAcl, InitializeSecurityDescriptor,
    SetKernelObjectSecurity, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    ACCESS_ALLOWED_ACE, ACE_FLAGS, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_DESCRIPTOR, SE_DACL_AUTO_INHERITED,
    SE_DACL_PROTECTED, SUB_CONTAINERS_AND_OBJECTS_INHERIT, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{READ_CONTROL, WRITE_DAC};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE,
};

use super::offline::{still_needed, Need};
use super::token::{current_user_sid, last_error, sid, sid_to_string, wide, LocalPtr, OwnedHandle};
use super::{
    fold_nested, is_listed, is_session_sid, kept_text, parse_kept, path_key, refuse_broad,
    root_sid_from_digest, session_sid, GrantRecord,
};

/// The rights granted on a root: `icacls`'s `M` (read, write, execute,
/// delete).
const MODIFY: u32 = 0x0013_01BF;
/// The rights granted on a read path: `icacls`'s `RX`.
const READ_EXECUTE: u32 = 0x0012_00A9;
/// The rights on the current folder's parent: `FILE_LIST_DIRECTORY |
/// FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE`. Names and
/// attributes of what is in it; no file's contents, no ACL, no write.
const LIST_ONLY: u32 = 0x0010_00A1;
/// The rights the user keeps beneath a root the offline account writes to:
/// what owning a file gives, reading and changing its ACL.
const ACL_RIGHTS: u32 = READ_CONTROL | WRITE_DAC;

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
    /// Folders it may only list, by an ACE on the folder alone: the current
    /// folder's parent ([`super::offline_parents`]).
    pub parents: Vec<PathBuf>,
}

impl OfflineGrant {
    /// The share a run may give the account, for [`Lease::acquire`]: a read
    /// folder that would reach far past a workspace refuses the run, and a
    /// read path or parent this process may not re-ACL is left out.
    fn grantable(self, profile: Option<&str>) -> Result<Self, String> {
        for read in self.reads.iter().filter(|r| r.is_dir()) {
            refuse_broad(&read.to_string_lossy(), profile).map_err(|e| {
                format!(
                    "network deny runs the command as the offline account, which must \
                     be granted the current folder; {e} (run from a project folder)"
                )
            })?;
        }
        // Only what this process may re-ACL: anything else (an insbx.exe under
        // Program Files) is the system's, readable by `Users` already or not
        // this run's to open up. If it is not readable, starting the command
        // says so.
        let reads = self
            .reads
            .into_iter()
            .filter(|r| may_change_acl(r))
            .collect();
        // The same for a parent: one this process may not re-ACL is not the
        // user's (a folder of the system's), and is listable by `Users`
        // already or not this run's to open up.
        let parents = self
            .parents
            .into_iter()
            .filter(|p| p.is_dir() && may_change_acl(p))
            .collect();
        Ok(Self {
            user: self.user,
            reads,
            parents,
        })
    }
}

/// Refuse the run when a root may not be granted, for [`Lease::acquire`]: it
/// would reach far past a workspace ([`refuse_broad`]), or it is not a
/// folder. Checked for every root before anything is granted.
fn refuse_ungrantable(roots: &[PathBuf], profile: Option<&str>) -> Result<(), String> {
    for root in roots {
        refuse_broad(&root.to_string_lossy(), profile)
            .map_err(|e| format!("workspace-write {e}"))?;
        if !root.is_dir() {
            return Err(format!(
                "workspace-write cannot grant {}: not a directory",
                root.display()
            ));
        }
    }
    Ok(())
}

/// One run's grant: its roots' kept grants, checked or made, and its temp
/// directory's own. Dropping it revokes what was the run's alone (the temp
/// directory, a read file, a listed parent); the kept grants stay.
pub(crate) struct Lease {
    record: GrantRecord,
    path: PathBuf,
    /// The per-run paths a grant was attempted on, so far: what dropping the
    /// lease takes back. The temp directory only, as a root.
    roots: Vec<PathBuf>,
    reads: Vec<PathBuf>,
    parents: Vec<PathBuf>,
    /// The kept SIDs of this run's writable roots ([`root_sid`]), for the
    /// command's token.
    root_sids: Vec<String>,
    /// The roots whose grant this lease had to make (walking their trees);
    /// a root granted before is only read, and is not here.
    walked: Vec<PathBuf>,
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
    /// Make sure each of `roots` (the writable roots) carries its kept grant,
    /// granting the ones that do not yet; grant `temp` (the run's private
    /// temp directory) to a new private SID; and with `offline`, the offline
    /// account's share as well. The crash sweep runs first.
    ///
    /// # Errors
    /// When a root may not or could not be granted. What this call granted
    /// for the run alone is not left behind on failure; a root whose grant
    /// failed part-way has it taken off again ([`keep_root`]), so the next
    /// run tries again instead of trusting it.
    ///
    /// The steps: the roots are checked ([`refuse_ungrantable`]) and folded,
    /// the account's share narrowed ([`OfflineGrant::grantable`]) and its
    /// read paths split into folders (kept) and files (this run's), the
    /// sweep run and the run's grant written down, then the temp directory
    /// granted ([`Lease::grant_roots`]), the roots' kept grants checked or
    /// made ([`Lease::keep_roots`]), the account's read paths
    /// ([`Lease::grant_reads`]) and its parents ([`Lease::grant_parents`]).
    /// A step that fails returns with the lease, which drops and so takes
    /// back what the steps before it listed.
    ///
    /// Called by `crate::Sandbox::windows_lease` (lib.rs), once per run.
    pub(crate) fn acquire(
        roots: &[PathBuf],
        temp: &Path,
        offline: Option<OfflineGrant>,
    ) -> Result<Self, String> {
        let profile = std::env::var("USERPROFILE").ok();
        refuse_ungrantable(roots, profile.as_deref())?;
        refuse_state_dir(roots)?;
        let roots = fold_nested(roots);
        let offline = offline
            .map(|o| o.grantable(profile.as_deref()))
            .transpose()?;
        let (user, reads, parents) = match offline {
            Some(o) => (Some(o.user), o.reads, o.parents),
            None => (None, Vec::new(), Vec::new()),
        };
        let (kept_reads, reads): (Vec<PathBuf>, Vec<PathBuf>) =
            reads.into_iter().partition(|r| r.is_dir());
        let dir = records_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        sweep(&dir);

        let (rid, alive) = claim_rid(&dir)?;
        let temp = vec![temp.to_path_buf()];
        let mut lease = Self {
            record: GrantRecord {
                sid: session_sid(rid),
                roots: temp.clone(),
                user,
                reads: reads.clone(),
                parents: parents.clone(),
            },
            path: dir.join(format!("{rid}.grant")),
            roots: Vec::new(),
            reads: Vec::new(),
            parents: Vec::new(),
            root_sids: Vec::new(),
            walked: Vec::new(),
            _alive: alive,
        };
        // Written down BEFORE anything is granted, so a crash at any point
        // after this is swept.
        std::fs::write(&lease.path, lease.record.to_text())
            .map_err(|e| format!("{}: {e}", lease.path.display()))?;
        lease.grant_roots(&temp)?;
        lease.keep_roots(&roots)?;
        if let Some(user) = lease.record.user.clone() {
            for read in &kept_reads {
                keep_read(read, &user).map_err(|e| {
                    format!(
                        "could not let the offline account read {}: {e}",
                        read.display()
                    )
                })?;
            }
            lease.grant_reads(&reads, &user)?;
            lease.grant_parents(&parents, &user);
        }
        Ok(lease)
    }

    /// Check or make each root's kept grant ([`keep_root`]), for
    /// [`Lease::acquire`], collecting the SIDs the command's token carries.
    /// Stops at the first that fails.
    fn keep_roots(&mut self, roots: &[PathBuf]) -> Result<(), String> {
        for root in roots {
            let (sid, walked) = keep_root(root, self.record.user.as_deref())
                .map_err(|e| format!("workspace-write could not grant {}: {e}", root.display()))?;
            self.root_sids.push(sid);
            if walked {
                self.walked.push(root.clone());
            }
        }
        Ok(())
    }

    /// The roots whose grant this lease made, walking their trees.
    #[cfg(test)]
    fn walked(&self) -> &[PathBuf] {
        &self.walked
    }

    /// Grant each root in turn ([`grant_root`]), for [`Lease::acquire`],
    /// stopping at the first that fails.
    fn grant_roots(&mut self, roots: &[PathBuf]) -> Result<(), String> {
        for root in roots {
            // Listed first: a propagation can fail part-way (a file the user
            // may not re-ACL), and dropping the lease takes back whatever
            // landed.
            self.roots.push(root.clone());
            grant_root(root, &self.record.sid, self.record.user.as_deref())
                .map_err(|e| format!("workspace-write could not grant {}: {e}", root.display()))?;
        }
        Ok(())
    }

    /// Let the offline account `user` read each of `reads`, for
    /// [`Lease::acquire`], stopping at the first that fails.
    fn grant_reads(&mut self, reads: &[PathBuf], user: &str) -> Result<(), String> {
        for read in reads {
            self.reads.push(read.clone());
            grant_account(read, user, READ_EXECUTE).map_err(|e| {
                format!(
                    "could not let the offline account read {}: {e}",
                    read.display()
                )
            })?;
        }
        Ok(())
    }

    /// Let the offline account `user` list each of `parents`, for
    /// [`Lease::acquire`]. Not a confinement matter: without it the run goes
    /// on, and only PowerShell's starting location suffers, so a failure is
    /// said and the next parent tried.
    fn grant_parents(&mut self, parents: &[PathBuf], user: &str) {
        for parent in parents {
            self.parents.push(parent.clone());
            if let Err(e) = set_list(parent, user, true) {
                eprintln!(
                    "insbx: could not let the offline account list {} ({e}); PowerShell \
                     may start at the drive root instead of the current folder",
                    parent.display()
                );
            }
        }
    }

    /// The run's own SID, granted on its temp directory: the token's first
    /// restricting SID, and the one its default DACL names.
    pub(crate) fn sid(&self) -> &str {
        &self.record.sid
    }

    /// The kept SIDs of this run's writable roots: the token's other
    /// restricting SIDs, and the only root SIDs it carries.
    pub(crate) fn root_sids(&self) -> &[String] {
        &self.root_sids
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
                    "insbx: could not revoke the sandbox's access to {} ({e}); the next run \
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
                        "insbx: could not revoke the offline account's access to {} ({e}); the \
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
            for parent in self.parents.iter().filter(|p| p.exists()) {
                if let Err(e) = release_list(parent, user, &others) {
                    eprintln!(
                        "insbx: could not take back the offline account's listing of {} ({e}); \
                         the next run sweeps it",
                        parent.display()
                    );
                    stuck.parents.push(parent.clone());
                }
            }
        }
        if stuck.roots.is_empty() && stuck.reads.is_empty() && stuck.parents.is_empty() {
            let _ = std::fs::remove_file(&self.path);
        } else {
            let _ = std::fs::write(&self.path, stuck.to_text());
        }
    }
}

/// Take back the account's ACE on `path`, as far as no other live run
/// (`others`) still needs it.
fn release_account(path: &Path, user: &str, others: &Others) -> Result<(), String> {
    let _lock = AclLock::take()?;
    match still_needed(path, &others.grants) {
        Need::Modify => return Ok(()),
        Need::Read => {
            change(path, user, Change::Revoke)?;
            change(path, user, Change::Grant(READ_EXECUTE))?;
        }
        Need::None => change(path, user, Change::Revoke)?,
    }
    // The revoke took every ACE of the account's, a listing ACE another live
    // run put on this folder too: put that back.
    if is_listed(path, &others.parents) {
        set_list(path, user, true)?;
    }
    Ok(())
}

/// Take back the account's listing ACE on `parent`, unless another live run
/// lists it too. Only that exact ACE goes: an ACE of another run's that
/// grants the folder as a root or read path stays.
fn release_list(parent: &Path, user: &str, others: &Others) -> Result<(), String> {
    if is_listed(parent, &others.parents) {
        return Ok(());
    }
    set_list(parent, user, false).map(|_| ())
}

/// Grant one root, for [`Lease::grant_roots`]: `Modify` to the run's SID
/// and, with the offline account `user`, to the account as well, with the
/// user's own ACL rights kept beneath it ([`keep_acl_rights`]). Stops at the
/// first that fails.
fn grant_root(root: &Path, sid_text: &str, user: Option<&str>) -> Result<(), String> {
    change(root, sid_text, Change::Grant(MODIFY))?;
    if let Some(user) = user {
        grant_account(root, user, MODIFY)?;
        keep_acl_rights(root)?;
    }
    Ok(())
}

/// The kept SID of the writable root `root`: [`super::root_sid_from_digest`]
/// of the SHA-256 of its [`path_key`] (case and spelling folded, so
/// `C:\W` and `c:/w/` are one root), hashed by Windows' own CNG.
///
/// # Errors
/// When the hash cannot be computed.
pub(crate) fn root_sid(root: &Path) -> Result<String, String> {
    let key = path_key(&root.to_string_lossy());
    let mut digest = [0u8; 32];
    let input = key.as_bytes();
    let len = u32::try_from(input.len()).map_err(|_| "the path is too long to hash")?;
    // SAFETY: the SHA-256 pseudo-handle needs no open or close; the input
    // and output buffers are valid for the lengths passed.
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null(),
            0,
            input.as_ptr(),
            len,
            digest.as_mut_ptr(),
            32,
        )
    };
    if status != 0 {
        return Err(format!("BCryptHash failed with status {status:#x}"));
    }
    Ok(root_sid_from_digest(&digest))
}

/// Make sure `root` carries its kept grant, for [`Lease::keep_roots`]:
/// `Modify` to [`root_sid`], and with the offline account `user`, `Modify`
/// to the account and the user's own ACL rights beneath ([`keep_acl_rights`]).
/// Each is read off the folder's own ACL first ([`passes_down`]) and made
/// only when missing, so a root granted before costs one ACL read, not a
/// walk of its tree. The SID, and whether a grant had to be made (a walk).
///
/// A grant that fails part-way (a file beneath the user may not re-ACL) has
/// the root's ACE taken off again, so the next run walks and reports it
/// again instead of finding the ACE on the folder and trusting a tree only
/// part granted. Under the ACL lock throughout, so two runs granting the
/// same new folder do not both walk it.
fn keep_root(root: &Path, user: Option<&str>) -> Result<(String, bool), String> {
    let sid = root_sid(root)?;
    let _lock = AclLock::take()?;
    let mut walked = false;
    if !passes_down(root, &sid, MODIFY)? {
        walked = true;
        if let Err(e) = change(root, &sid, Change::Grant(MODIFY)) {
            let _ = change(root, &sid, Change::Revoke);
            return Err(e);
        }
    }
    if let Some(user) = user {
        if !passes_down(root, user, MODIFY)? {
            walked = true;
            if let Err(e) = grant_account(root, user, MODIFY) {
                let _ = change(root, user, Change::Revoke);
                return Err(e);
            }
        }
        walked |= keep_acl_rights(root)?;
    }
    remember_kept(root);
    Ok((sid, walked))
}

/// Make sure the offline account `user` may read the folder `dir` (the
/// current folder of a network-deny run), kept like a root's grant: made
/// once, checked by one ACL read after that.
fn keep_read(dir: &Path, user: &str) -> Result<(), String> {
    let _lock = AclLock::take()?;
    if !passes_down(dir, user, READ_EXECUTE)? {
        if let Err(e) = grant_account(dir, user, READ_EXECUTE) {
            let _ = change(dir, user, Change::Revoke);
            return Err(e);
        }
    }
    remember_kept(dir);
    Ok(())
}

/// Refuse a root that holds insbx's own state (`%LOCALAPPDATA%\insbx`: the
/// crash records and the kept list), for [`Lease::acquire`]: a command
/// that could write there could forge a record for the sweep.
fn refuse_state_dir(roots: &[PathBuf]) -> Result<(), String> {
    let Some(state) = records_dir().parent().map(Path::to_path_buf) else {
        return Ok(());
    };
    let state_key = path_key(&state.to_string_lossy());
    for root in roots {
        let key = path_key(&root.to_string_lossy());
        let holds = state_key == key
            || state_key.starts_with(&format!("{key}\\"))
            || key.starts_with(&format!("{state_key}\\"));
        if holds {
            return Err(format!(
                "workspace-write refuses {}: it holds insbx's own records ({})",
                root.display(),
                state.display()
            ));
        }
    }
    Ok(())
}

/// The kept-grants list's file: `%LOCALAPPDATA%\insbx\kept`.
fn kept_file() -> PathBuf {
    records_dir()
        .parent()
        .map_or_else(|| PathBuf::from("insbx-kept"), |d| d.join("kept"))
}

/// Add `path` to the kept list, unless it is there already. Only for
/// listing ([`super::parse_kept`]); a failure to write it is said, not
/// fatal: the grant itself is in the folder's ACL.
fn remember_kept(path: &Path) {
    let file = kept_file();
    let mut kept = std::fs::read_to_string(&file)
        .map(|t| parse_kept(&t))
        .unwrap_or_default();
    let key = path_key(&path.to_string_lossy());
    if kept.iter().any(|k| path_key(&k.to_string_lossy()) == key) {
        return;
    }
    kept.push(path.to_path_buf());
    if let Err(e) = std::fs::write(&file, kept_text(&kept)) {
        eprintln!(
            "insbx: could not add {} to the kept-grants list {} ({e}); `insbx grants` will \
             not show it",
            path.display(),
            file.display()
        );
    }
}

/// One kept folder, for `insbx grants`.
#[derive(Debug, Clone)]
pub struct KeptFolder {
    pub path: PathBuf,
    /// Whether the folder still carries its root SID's grant.
    pub granted: bool,
}

/// Every folder the kept list names that still exists, each with whether
/// its grant is still on it. Folders that are gone are dropped from the
/// list.
///
/// # Errors
/// When the list cannot be read.
pub fn kept_folders() -> Result<Vec<KeptFolder>, String> {
    let _lock = AclLock::take()?;
    let file = kept_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let all = parse_kept(&text);
    let live: Vec<PathBuf> = all.iter().filter(|p| p.exists()).cloned().collect();
    if live.len() != all.len() {
        let _ = std::fs::write(&file, kept_text(&live));
    }
    Ok(live
        .into_iter()
        .map(|path| {
            let granted = root_sid(&path)
                .and_then(|sid| passes_down(&path, &sid, MODIFY))
                .unwrap_or(false);
            KeptFolder { path, granted }
        })
        .collect())
}

/// Take `path`'s kept grants back: its root SID's ACE, and with the offline
/// account `user`, the account's ACEs on it; then drop it from the kept
/// list. The user's own ACL rights stay ([`keep_acl_rights`] says why). A
/// run that is using the folder right now loses its writes there. The next
/// run with it as a root grants the whole tree again (files moved in keep
/// their own ACL even then; `icacls <FILE> /reset` fixes one).
///
/// # Errors
/// When an ACE cannot be removed; the folder stays listed then.
pub fn revoke_kept(path: &Path, user: Option<&str>) -> Result<(), String> {
    let _lock = AclLock::take()?;
    let key = path_key(&path.to_string_lossy());
    if path.exists() {
        change(path, &root_sid(path)?, Change::Revoke)?;
        if let Some(user) = user {
            change(path, user, Change::Revoke)?;
        }
    }
    let file = kept_file();
    let kept = std::fs::read_to_string(&file)
        .map(|t| parse_kept(&t))
        .unwrap_or_default();
    let left: Vec<PathBuf> = kept
        .into_iter()
        .filter(|k| path_key(&k.to_string_lossy()) != key)
        .collect();
    std::fs::write(&file, kept_text(&left)).map_err(|e| format!("{}: {e}", file.display()))
}

/// Grant the account `rights` on `path`, inherited beneath a folder. A
/// listing ACE already on the folder (another run's parent) is taken off
/// first and put back after: `SetEntriesInAclW` may merge a grant into an
/// ACE the trustee already has, and merged into that one the grant would
/// not be inherited.
fn grant_account(path: &Path, user: &str, rights: u32) -> Result<(), String> {
    let _lock = AclLock::take()?;
    let listed = path.is_dir() && set_list(path, user, false)?;
    change(path, user, Change::Grant(rights))?;
    if listed {
        set_list(path, user, true)?;
    }
    Ok(())
}

/// Let the user keep reading and changing the ACL of everything beneath
/// `root`, the files the offline account creates included, so a later run's
/// grant and revoke reach them (see the module docs). Added once: a root
/// that already passes these rights down to the user is left alone, which
/// spares a second walk of a large tree on every run. Whether it was added.
fn keep_acl_rights(root: &Path) -> Result<bool, String> {
    let user = current_user_sid()?;
    if passes_down(root, &user, ACL_RIGHTS)? {
        return Ok(false);
    }
    change(root, &user, Change::Grant(ACL_RIGHTS)).map(|()| true)
}

/// Whether `path`'s DACL allows `sid_text` all of `rights` on everything
/// created beneath it: an allow ACE inherited by both files and folders.
fn passes_down(path: &Path, sid_text: &str, rights: u32) -> Result<bool, String> {
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const TO_FILES_AND_FOLDERS: u8 = 0x1 | 0x2; // OBJECT_ | CONTAINER_INHERIT_ACE
    let trustee = sid(sid_text)?;
    // SAFETY: the DACL points into `descriptor`, freed only at the end of
    // the block; each ACE read is inside the DACL `GetAce` returned it from,
    // and only its header and fixed part are read; each call is checked.
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let err = GetNamedSecurityInfoW(
            wide(path).as_ptr(),
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
        let _descriptor = LocalPtr(descriptor);
        if dacl.is_null() {
            // No DACL: everyone may do anything already.
            return Ok(true);
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        if GetAclInformation(
            dacl,
            (&raw mut info).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        ) == 0
        {
            return Err(last_error("GetAclInformation"));
        }
        for index in 0..info.AceCount {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, index, &mut ace) == 0 {
                return Err(last_error("GetAce"));
            }
            let header = *ace.cast::<ACE_HEADER>();
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE
                || header.AceFlags & TO_FILES_AND_FOLDERS != TO_FILES_AND_FOLDERS
            {
                continue;
            }
            let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
            if (*allowed).Mask & rights == rights
                && EqualSid((&raw mut (*allowed).SidStart).cast(), trustee.0) != 0
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// What every other live run granted the offline account.
#[derive(Default)]
struct Others {
    /// Each run's (roots, reads).
    grants: Vec<(Vec<PathBuf>, Vec<PathBuf>)>,
    /// Every folder some run lets it list.
    parents: Vec<PathBuf>,
}

/// What every other live run granted the offline account.
fn live_account_grants(own: &Path) -> Others {
    let mut others = Others::default();
    let Ok(entries) = std::fs::read_dir(records_dir()) else {
        return others;
    };
    for record in entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != own)
        .filter_map(|p| Some((record_rid(&p)?, p)))
        .filter(|(rid, _)| is_alive(*rid))
        .filter_map(|(_, p)| GrantRecord::parse(&std::fs::read_to_string(p).ok()?))
        .filter(|r| r.user.is_some())
    {
        others.grants.push((record.roots, record.reads));
        others.parents.extend(record.parents);
    }
    others
}

/// Let the offline account read `path` (a folder and all beneath it, or one
/// file), for `insbx setup-windows --read`.
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
        .map(|d| PathBuf::from(d).join("insbx").join("grants"))
        .unwrap_or_else(|| std::env::temp_dir().join("insbx-grants"))
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
                for p in record.parents.iter().filter(|p| p.exists()) {
                    if release_list(p, user, &others).is_err() {
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
    format!("Global\\insbx-grant-{rid}")
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

/// Add (`add`) or remove the account's listing ACE on the folder `path`
/// alone: an allow ACE of exactly [`LIST_ONLY`] with no ACE flags. Whether
/// the DACL changed (`false`: the ACE was there already, or not there to
/// remove).
///
/// The DACL is read and written through one handle with the low-level
/// `SetKernelObjectSecurity`, which stores the DACL on this one object and,
/// unlike `SetNamedSecurityInfoW` and `SetSecurityInfo`, does not walk the
/// folder's children to re-propagate inheritance (slow on a big folder, and
/// a failure on any child the user may not re-ACL). Nothing needs
/// propagating: the ACE inherits to nothing, and every other ACE (inherited
/// ones included, with their flags) and the DACL's protection and
/// auto-inherited bits are written back as they were. The new ACE goes after
/// the explicit deny ACEs, where an allow ACE belongs in canonical order.
/// Removing takes off only an ACE that matches exactly, so no other grant of
/// the account's on the folder (as another run's root or read path) is
/// touched.
fn set_list(path: &Path, sid_text: &str, add: bool) -> Result<bool, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const ACCESS_DENIED_OBJECT_ACE_TYPE: u8 = 6;
    const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
    const ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE: u8 = 12;
    const INHERITED_ACE: u8 = 0x10;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    let _lock = AclLock::take()?;
    let trustee = sid(sid_text)?;
    let folder = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .share_mode(7) // read, write, delete: never in anyone's way
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS) // a folder opens too
        .open(path)
        .map_err(|e| format!("opening {} to change its ACL: {e}", path.display()))?;
    let handle: HANDLE = folder.as_raw_handle();
    // SAFETY: every pointer passed lives to the end of the block (`folder`'s
    // handle, `trustee`, the returned descriptor and the DACL inside it, the
    // new ACL's buffer and the descriptor built on the stack); each ACE read
    // is inside the DACL `GetAce` returned it from, and is read only as far
    // as its header says; each call's result is checked.
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let err = GetSecurityInfo(
            handle,
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
        if dacl.is_null() {
            // No DACL: everyone may do anything already.
            return Ok(false);
        }
        let mut control: u16 = 0;
        let mut sd_revision: u32 = 0;
        if GetSecurityDescriptorControl(descriptor.0, &mut control, &mut sd_revision) == 0 {
            return Err(last_error("GetSecurityDescriptorControl"));
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        if GetAclInformation(
            dacl,
            (&raw mut info).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        ) == 0
        {
            return Err(last_error("GetAclInformation"));
        }
        // ACCESS_ALLOWED_ACE ends in the SID's first DWORD.
        let ace_size =
            std::mem::size_of::<ACCESS_ALLOWED_ACE>() as u32 - 4 + GetLengthSid(trustee.0);
        let size = info.AclBytesInUse + if add { ace_size } else { 0 };
        let mut buffer = vec![0u32; size.div_ceil(4) as usize];
        let new: *mut ACL = buffer.as_mut_ptr().cast();
        let revision = u32::from((*dacl).AclRevision).max(ACL_REVISION);
        if InitializeAcl(new, size, revision) == 0 {
            return Err(last_error("InitializeAcl"));
        }
        let mut changed = false;
        let mut placed = !add;
        for index in 0..info.AceCount {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, index, &mut ace) == 0 {
                return Err(last_error("GetAce"));
            }
            let header = *ace.cast::<ACE_HEADER>();
            let ours = header.AceType == ACCESS_ALLOWED_ACE_TYPE
                && header.AceFlags == 0
                && (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask == LIST_ONLY
                && EqualSid(
                    (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast(),
                    trustee.0,
                ) != 0;
            if ours {
                if add {
                    return Ok(false);
                }
                changed = true;
                continue;
            }
            let explicit_deny = header.AceFlags & INHERITED_ACE == 0
                && matches!(
                    header.AceType,
                    ACCESS_DENIED_ACE_TYPE
                        | ACCESS_DENIED_OBJECT_ACE_TYPE
                        | ACCESS_DENIED_CALLBACK_ACE_TYPE
                        | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
                );
            if !placed && !explicit_deny {
                if AddAccessAllowedAceEx(new, revision, 0, LIST_ONLY, trustee.0) == 0 {
                    return Err(last_error("AddAccessAllowedAceEx"));
                }
                placed = true;
                changed = true;
            }
            if AddAce(new, revision, u32::MAX, ace, u32::from(header.AceSize)) == 0 {
                return Err(last_error("AddAce"));
            }
        }
        if !placed {
            if AddAccessAllowedAceEx(new, revision, 0, LIST_ONLY, trustee.0) == 0 {
                return Err(last_error("AddAccessAllowedAceEx"));
            }
            changed = true;
        }
        if !changed {
            return Ok(false);
        }
        let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
        let psd: *mut core::ffi::c_void = (&raw mut sd).cast();
        if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) == 0 {
            return Err(last_error("InitializeSecurityDescriptor"));
        }
        if SetSecurityDescriptorDacl(psd, 1, new, 0) == 0 {
            return Err(last_error("SetSecurityDescriptorDacl"));
        }
        let kept = SE_DACL_AUTO_INHERITED | SE_DACL_PROTECTED;
        if SetSecurityDescriptorControl(psd, kept, control & kept) == 0 {
            return Err(last_error("SetSecurityDescriptorControl"));
        }
        if SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, psd) == 0 {
            return Err(format!(
                "setting the ACL of {}: {}",
                path.display(),
                last_error("SetKernelObjectSecurity")
            ));
        }
        drop(descriptor);
    }
    Ok(true)
}

/// The cross-process lock around an ACL change, released on drop. A
/// Windows mutex is recursive for the thread that holds it, so a caller may
/// hold it across several [`change`]s, which each take it again.
struct AclLock(HANDLE);

impl AclLock {
    fn take() -> Result<Self, String> {
        // SAFETY: a NUL-terminated name; the handle is owned by the guard.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, wide("Global\\insbx-acl").as_ptr());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `path`'s owner and ACL, as Windows reports them, for a failure
    /// message: what a test's precondition actually found, so a run on
    /// another machine (an elevated CI runner) says why it differs.
    fn acl_of(path: &Path) -> String {
        // Without `PSModulePath`: Windows PowerShell started from a
        // PowerShell 7 shell (a CI step) inherits 7's module path, cannot
        // load `Get-Acl`'s module and prints nothing.
        let run = |program: &str, args: &[&std::ffi::OsStr]| {
            std::process::Command::new(program)
                .env_remove("PSModulePath")
                .args(args)
                .output()
                .map(|o| {
                    format!(
                        "{} {}",
                        String::from_utf8_lossy(&o.stdout).trim(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    )
                })
                .unwrap_or_else(|e| format!("{program}: {e}"))
        };
        let owner = run(
            "powershell",
            &[
                "-NoProfile".as_ref(),
                "-Command".as_ref(),
                format!("(Get-Acl -LiteralPath '{}').Owner", path.display()).as_ref(),
            ],
        );
        format!("owner: {owner}\n{}", run("icacls", &[path.as_os_str()]))
    }

    /// A folder with the ACL a data drive gives (`D:\Projects\...`): the
    /// user reaches it through `Authenticated Users`' `Modify`, with no ACE
    /// of the user's own passing anything down. The profile's folders (the
    /// temp folder included) pass the user full control, which hides the
    /// case.
    fn data_drive_folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let set = std::process::Command::new("icacls")
            .arg(dir.path())
            .args([
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
                "*S-1-5-11:(OI)(CI)M",
                "*S-1-5-32-545:(OI)(CI)RX",
            ])
            .output()
            .expect("icacls");
        assert!(
            set.status.success(),
            "{}",
            String::from_utf8_lossy(&set.stdout)
        );
        // None of the user's own: an elevated process (a CI runner) gives a
        // new folder an explicit ACE for its user, which `/inheritance:r`
        // does not take off and which would pass the user everything.
        remove_grants(dir.path(), &[&current_user_sid().expect("the user's SID")]);
        dir
    }

    /// Take every allow ACE for each of `sids` off `path`, explicit ones
    /// included — what `/grant:r` of other SIDs leaves in place.
    fn remove_grants(path: &Path, sids: &[&str]) {
        for sid in sids {
            let removed = std::process::Command::new("icacls")
                .arg(path)
                .args(["/remove:g", &format!("*{sid}")])
                .output()
                .expect("icacls");
            assert!(
                removed.status.success(),
                "{}",
                String::from_utf8_lossy(&removed.stdout)
            );
        }
    }

    /// **THE USER KEEPS READING AND CHANGING THE ACL OF WHAT IS CREATED
    /// BENEATH A ROOT**, so a later run's grant reaches a file the offline
    /// account owns; and the ACE is added once, not on every run.
    #[test]
    fn the_user_keeps_acl_rights_beneath_a_root_and_they_are_added_once() {
        let dir = data_drive_folder();
        let user = current_user_sid().expect("the user's SID");
        assert!(
            !passes_down(dir.path(), &user, ACL_RIGHTS).expect("reads the ACL"),
            "a data-drive folder starts without it (user {user}, elevated: {}):\n{}",
            super::super::token::is_elevated(),
            acl_of(dir.path())
        );

        assert!(keep_acl_rights(dir.path()).expect("kept"), "added");
        assert!(passes_down(dir.path(), &user, ACL_RIGHTS).expect("reads the ACL"));
        // What is created beneath it afterwards inherits the right.
        let file = dir.path().join("made-later.txt");
        std::fs::write(&file, "x").expect("write");
        let file_acl = std::process::Command::new("icacls")
            .arg(&file)
            .output()
            .expect("icacls");
        let file_acl = String::from_utf8_lossy(&file_acl.stdout);
        let name = std::env::var("USERNAME").expect("USERNAME");
        assert!(
            file_acl
                .lines()
                .any(|l| l.contains(&format!("\\{name}:")) && l.contains("WDAC")),
            "{file_acl}"
        );

        assert!(
            !keep_acl_rights(dir.path()).expect("kept"),
            "a root that already passes it down is left alone"
        );
    }

    /// **A GRANT THAT FAILS PART-WAY IS NOT TRUSTED, AND THE RUN'S OWN GRANT
    /// IS TAKEN BACK**: the first root gets its kept grant, the second
    /// refuses (its ACL may be read but not changed: an `OWNER RIGHTS` ACE
    /// takes the owner's implicit `WRITE_DAC` away), so the run is refused.
    /// The first root's grant is kept, as after any run; the second carries
    /// no root SID for a later run to trust; the lease the failure drops
    /// revokes the run's own SID from its temp directory, and its record goes.
    #[test]
    fn a_grant_that_fails_part_way_is_not_trusted_and_the_runs_own_is_taken_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Folded in this order: `a-` before `b-`.
        let granted = dir.path().join("a-granted");
        let locked = dir.path().join("b-locked");
        let temp = dir.path().join("t");
        std::fs::create_dir(&granted).expect("granted root");
        std::fs::create_dir(&locked).expect("locked root");
        std::fs::create_dir(&temp).expect("temp");
        let beneath = granted.join("made-before.txt");
        std::fs::write(&beneath, "x").expect("write");
        // An elevated process (a CI runner) leaves explicit full-control ACEs
        // for its user and for Administrators on a new folder; either one
        // would let it re-ACL the root, which is what must fail here. Taken
        // off BEFORE the lock below: after it, nobody here may change the ACL.
        let user = current_user_sid().expect("the user's SID");
        remove_grants(&locked, &[&user, "S-1-5-32-544"]);
        let set = std::process::Command::new("icacls")
            .arg(&locked)
            .args([
                "/inheritance:r",
                "/grant:r",
                "*S-1-3-4:(OI)(CI)R",
                "*S-1-5-11:(OI)(CI)M",
            ])
            .output()
            .expect("icacls");
        assert!(
            set.status.success(),
            "{}",
            String::from_utf8_lossy(&set.stdout)
        );

        let err = match Lease::acquire(&[granted.clone(), locked.clone()], &temp, None) {
            Err(err) => err,
            Ok(_) => panic!(
                "the locked root refused nothing (elevated: {}):\n{}",
                super::super::token::is_elevated(),
                acl_of(&locked)
            ),
        };
        assert!(
            err.starts_with(&format!(
                "workspace-write could not grant {}:",
                locked.display()
            )),
            "{err}"
        );

        let granted_sid = root_sid(&granted).expect("sid");
        for path in [&granted, &beneath] {
            let acl = acl_of(path);
            assert!(
                acl.contains(&granted_sid),
                "kept: {}: {acl}",
                path.display()
            );
        }
        let locked_acl = acl_of(&locked);
        assert!(
            !locked_acl.contains(&root_sid(&locked).expect("sid")),
            "no grant to trust on the locked root: {locked_acl}"
        );
        let temp_acl = acl_of(&temp);
        assert!(
            !temp_acl.contains(super::super::SID_PREFIX),
            "the run's SID was revoked from its temp: {temp_acl}"
        );
        let left = std::fs::read_dir(records_dir())
            .expect("records")
            .flatten()
            .filter_map(|e| GrantRecord::parse(&std::fs::read_to_string(e.path()).ok()?))
            .any(|r| r.roots.contains(&temp));
        assert!(!left, "the run's record was removed");
        revoke_kept(&granted, None).expect("revoke");
    }

    /// **A KEPT GRANT IS MADE ONCE, THEN ONLY READ; REVOKE TAKES IT BACK**:
    /// the first lease walks the root (a file beneath gets the root's ACE);
    /// the second only reads the root's ACL and walks nothing. `revoke_kept`
    /// takes the ACE off the root and its files and drops the root from the
    /// kept list; the next lease walks again. A file MOVED in keeps the ACL
    /// it had through all of it (no walk re-ACLs it, kept grant or not), and
    /// `icacls /reset` gives it the folder's.
    #[test]
    fn a_kept_grant_is_made_once_then_only_read_and_revoke_takes_it_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&root).expect("root");
        std::fs::create_dir(&outside).expect("outside");
        let beneath = root.join("before.txt");
        std::fs::write(&beneath, "x").expect("write");
        let sid = root_sid(&root).expect("sid");
        let temp = |n: &str| {
            let t = dir.path().join(n);
            std::fs::create_dir(&t).expect("temp");
            t
        };

        let first = Lease::acquire(std::slice::from_ref(&root), &temp("t1"), None).expect("first");
        assert_eq!(first.root_sids(), std::slice::from_ref(&sid));
        assert_eq!(
            first.walked(),
            std::slice::from_ref(&root),
            "the first walks"
        );
        drop(first);
        assert!(acl_of(&root).contains(&sid), "kept on the root");
        assert!(acl_of(&beneath).contains(&sid), "and beneath it");
        let listed = kept_folders().expect("list");
        assert!(
            listed.iter().any(|k| k.path == root && k.granted),
            "listed as granted: {listed:?}"
        );

        let moved = root.join("moved.txt");
        std::fs::write(outside.join("moved.txt"), "y").expect("write");
        std::fs::rename(outside.join("moved.txt"), &moved).expect("move in");
        assert!(!acl_of(&moved).contains(&sid), "a moved file keeps its ACL");

        let second =
            Lease::acquire(std::slice::from_ref(&root), &temp("t2"), None).expect("second");
        assert_eq!(second.root_sids(), std::slice::from_ref(&sid));
        assert!(second.walked().is_empty(), "the second only reads the ACL");
        drop(second);
        assert!(!acl_of(&moved).contains(&sid), "{}", acl_of(&moved));

        revoke_kept(&root, None).expect("revoke");
        for path in [&root, &beneath] {
            let acl = acl_of(path);
            assert!(!acl.contains(&sid), "revoked: {}: {acl}", path.display());
        }
        assert!(
            !kept_folders().expect("list").iter().any(|k| k.path == root),
            "no longer listed"
        );

        let third = Lease::acquire(std::slice::from_ref(&root), &temp("t3"), None).expect("third");
        assert_eq!(
            third.walked(),
            std::slice::from_ref(&root),
            "revoked: walks again"
        );
        drop(third);
        assert!(
            acl_of(&beneath).contains(&sid),
            "granted again, the whole tree: {}",
            acl_of(&beneath)
        );
        // The moved file: not reached by the walk either; `icacls /reset`
        // rebuilds its ACL from its folder's.
        assert!(!acl_of(&moved).contains(&sid), "{}", acl_of(&moved));
        let reset = std::process::Command::new("icacls")
            .arg(&moved)
            .arg("/reset")
            .output()
            .expect("icacls");
        assert!(
            reset.status.success(),
            "{}",
            String::from_utf8_lossy(&reset.stdout)
        );
        assert!(acl_of(&moved).contains(&sid), "reset: {}", acl_of(&moved));
        revoke_kept(&root, None).expect("cleanup");
    }

    /// **A ROOT HOLDING INSBX'S OWN RECORDS IS REFUSED**: a command that
    /// could write there could forge a record for the sweep.
    #[test]
    fn a_root_holding_insbx_state_is_refused() {
        let state = records_dir();
        let parent = state.parent().expect("state dir").to_path_buf();
        for root in [parent, state] {
            let err = refuse_state_dir(std::slice::from_ref(&root)).expect_err("refused");
            assert!(err.contains("holds insbx's own records"), "{err}");
        }
        let elsewhere = tempfile::tempdir().expect("tempdir");
        assert!(refuse_state_dir(&[elsewhere.path().to_path_buf()]).is_ok());
    }
}
