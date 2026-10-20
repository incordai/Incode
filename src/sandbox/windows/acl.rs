//! Windows workspace-write: one ACE per root, for the session.
//!
//! A write-restricted token writes only where one of its restricting SIDs is
//! granted. So a session that asked for workspace-write gets a private SID
//! (`S-1-5-21-1111111111-2222222222-3333333333-<random>`, one per incode
//! process), and each root it writes in gets ONE inheritable `Modify` ACE for
//! that SID — added on the first sandboxed command, removed when the last
//! lease on the root is dropped. The command's temp directory is the session's
//! own, created with the same ACE, because the user's `%TEMP%` is not granted.
//!
//! # What the measurements said (a scratch copy of incode's tree, 18,673 objects)
//!
//! - Grant 2.41–2.49 s, revoke 2.40–2.58 s, three runs each (6.0 s the first,
//!   cold). Linear in the number of objects: NTFS inheritance is WRITTEN into
//!   every descendant, not computed at access time, so every object's security
//!   descriptor changes — but no file's contents or mtime, and `git
//!   diff-files` stays clean.
//! - Propagation does not follow a junction out of the tree: the junction
//!   itself got the ACE, the directory it points at did not.
//! - The revoke is exact: `icacls /save` of the whole tree after it was
//!   byte-identical to the one before the grant.
//!
//! # A crash leaves the ACE behind — the sweep
//!
//! Every grant is written down first, in `<data dir>/sandbox-grants/<rid>.grant`
//! (the SID, the temp directory, each root), and the process holds a named
//! mutex `Global\incode-sandbox-<rid>` for as long as it holds any grant. The
//! first grant a later process makes sweeps: a record whose mutex no longer
//! exists belongs to a process that died, so its ACEs are revoked, its temp
//! directory removed and the record deleted. A record whose revoke failed is
//! kept for the next sweep. The ACE a dead session leaves is harmless in the
//! meantime — only a token carrying that exact SID can use it, and none will
//! ever be made again — but it is the operator's ACL, so it is cleaned.
//!
//! # What is refused
//!
//! A drive root, and any directory that holds the user profile: the grant
//! would reach every file under it, which is both slow and no confinement at
//! all. The command is then NOT run — the operator named this sandbox, so
//! running unconfined instead would be the silent failure the module exists
//! to prevent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

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
    CreateMutexW, OpenMutexW, ReleaseMutex, WaitForSingleObject,
};

use super::imp::{last_error, sid, wide, LocalPtr};

/// Every session SID starts with this — the authority of incode's fixed
/// read-only SID ([`super::imp::INCODE_SID`]), with a random last RID.
pub(crate) const SID_PREFIX: &str = "S-1-5-21-1111111111-2222222222-3333333333-";

/// The rights granted: `icacls`'s `M` — read, write, execute and delete.
/// Spelled numerically because the named constants live in a windows-sys
/// feature this crate does not enable.
const MODIFY: u32 = 0x0013_01BF;

/// `SYNCHRONIZE`, the least right that proves a named mutex exists.
const SYNCHRONIZE: u32 = 0x0010_0000;

/// How long a grant or revoke waits for another incode process changing an
/// ACL at the same moment. A propagation over a very large tree takes tens of
/// seconds; past this the command is refused rather than left hanging.
const LOCK_WAIT_MS: u32 = 120_000;

/// The records' directory, under the data directory.
const RECORDS: &str = "sandbox-grants";

/// What a sandboxed command needs from the session's grant: the SID its token
/// is restricted to, and the temp directory it may write in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) sid: String,
    pub(crate) temp: PathBuf,
}

/// One holder's claim on a set of granted roots. Dropping it gives each root
/// back; the last claim on a root revokes its ACE, and the last claim in the
/// process ends the session's SID, temp directory and record.
#[derive(Debug)]
pub(crate) struct Lease {
    roots: Vec<PathBuf>,
    grant: Grant,
}

impl Lease {
    /// Grant `roots` (each not already granted in this process) to the
    /// process's session SID, starting the session — and sweeping what a
    /// crashed one left — on first use.
    ///
    /// # Errors
    ///
    /// When a root may not or could not be granted. Nothing this call granted
    /// is left behind on failure.
    pub(crate) fn acquire(roots: &[PathBuf]) -> Result<Self, String> {
        let wanted = wanted(roots)?;
        let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        if state.is_none() {
            *state = Some(Session::start()?);
        }
        let Some(session) = state.as_mut() else {
            return Err("the sandbox session could not be started".to_string());
        };
        let mut taken = Vec::new();
        for root in &wanted {
            if let Err(e) = session.take(root) {
                session.give_back(&taken);
                if session.is_idle() {
                    if let Some(done) = state.take() {
                        done.finish();
                    }
                }
                return Err(e);
            }
            taken.push(root.clone());
        }
        Ok(Self {
            roots: taken,
            grant: Grant {
                sid: session.sid.clone(),
                temp: session.temp.clone(),
            },
        })
    }

    /// Whether this lease already covers exactly `roots`.
    pub(crate) fn covers(&self, roots: &[PathBuf]) -> bool {
        wanted(roots).is_ok_and(|w| w == self.roots)
    }

    /// The SID and temp directory a command under this lease runs with.
    pub(crate) fn grant(&self) -> Grant {
        self.grant.clone()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        let idle = match state.as_mut() {
            Some(session) => {
                session.give_back(&self.roots);
                session.is_idle()
            }
            None => false,
        };
        if idle {
            if let Some(done) = state.take() {
                done.finish();
            }
        }
    }
}

/// The process's session, while any lease is held.
static STATE: Mutex<Option<Session>> = Mutex::new(None);

struct Session {
    sid: String,
    temp: PathBuf,
    record: PathBuf,
    /// The named mutex that tells a later sweep this session is alive, kept
    /// as the handle's value (a raw pointer is not `Send`, and this lives in a
    /// static) and closed when the session drops.
    alive: isize,
    /// Each granted root and how many leases hold it.
    held: BTreeMap<PathBuf, usize>,
    /// Roots whose revoke failed: kept in the record for the next sweep.
    stuck: Vec<PathBuf>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the handle CreateMutexW returned, owned by this session and
        // closed once, here.
        unsafe { CloseHandle(self.alive as HANDLE) };
    }
}

impl Session {
    /// A new session SID, after sweeping what a dead process left.
    fn start() -> Result<Self, String> {
        let dir = crate::admin::AdminConfig::default_dir().join(RECORDS);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        sweep(&dir);

        let rid = random_rid();
        let sid = format!("{SID_PREFIX}{rid}");
        // SAFETY: a NUL-terminated name; no security attributes, so the handle
        // is not inherited by the commands (which would keep a dead session
        // looking alive).
        let alive = unsafe { CreateMutexW(std::ptr::null(), 0, wide(alive_name(rid)).as_ptr()) };
        if alive.is_null() {
            return Err(last_error("CreateMutexW"));
        }

        let temp = dir.join(format!("{rid}.tmp"));
        let record = dir.join(format!("{rid}.grant"));
        let session = Self {
            sid,
            temp: temp.clone(),
            record,
            alive: alive as isize,
            held: BTreeMap::new(),
            stuck: Vec::new(),
        };
        // Written down BEFORE anything is granted, so a crash at any point
        // after this is swept.
        session.write_record()?;
        std::fs::create_dir_all(&temp).map_err(|e| format!("{}: {e}", temp.display()))?;
        change(&temp, &session.sid, Change::Grant)?;
        Ok(session)
    }

    /// One more lease on `root`, granting it if none held it.
    fn take(&mut self, root: &Path) -> Result<(), String> {
        let count = self.held.get(root).copied().unwrap_or(0);
        if count == 0 {
            self.held.insert(root.to_path_buf(), 0);
            self.write_record()?;
            let began = Instant::now();
            if let Err(e) = change(root, &self.sid, Change::Grant) {
                // A propagation can fail part-way (a file the user may not
                // re-ACL); take back whatever landed.
                let _ = change(root, &self.sid, Change::Revoke);
                self.held.remove(root);
                let _ = self.write_record();
                return Err(e);
            }
            crate::trace!(
                "sandbox",
                "granted {} to this session's SID in {:.2}s",
                root.display(),
                began.elapsed().as_secs_f64()
            );
        }
        self.held.insert(root.to_path_buf(), count + 1);
        Ok(())
    }

    /// Give back one lease on each of `roots`, revoking a root nobody holds.
    fn give_back(&mut self, roots: &[PathBuf]) {
        for root in roots {
            let Some(count) = self.held.get_mut(root) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count > 0 {
                continue;
            }
            self.held.remove(root);
            let began = Instant::now();
            match change(root, &self.sid, Change::Revoke) {
                Ok(()) => crate::trace!(
                    "sandbox",
                    "revoked {} from this session's SID in {:.2}s",
                    root.display(),
                    began.elapsed().as_secs_f64()
                ),
                Err(e) => {
                    crate::trace!(
                        "sandbox",
                        "could not revoke {} ({e}); the next session sweeps it",
                        root.display()
                    );
                    self.stuck.push(root.clone());
                }
            }
        }
        let _ = self.write_record();
    }

    fn is_idle(&self) -> bool {
        self.held.is_empty()
    }

    /// The session is over: its temp directory goes, and its record too unless
    /// a revoke failed (then the next sweep retries it). The liveness mutex is
    /// closed as `self` drops.
    fn finish(self) {
        if let Err(e) = std::fs::remove_dir_all(&self.temp) {
            crate::trace!(
                "sandbox",
                "could not remove {} ({e}); the next session sweeps it",
                self.temp.display()
            );
            return;
        }
        if self.stuck.is_empty() {
            let _ = std::fs::remove_file(&self.record);
        }
    }

    fn write_record(&self) -> Result<(), String> {
        let roots = self.held.keys().chain(self.stuck.iter());
        let text = record_text(&self.sid, &self.temp, roots);
        std::fs::write(&self.record, text).map_err(|e| format!("{}: {e}", self.record.display()))
    }
}

/// The record's contents: one `key value` per line.
fn record_text<'a>(sid: &str, temp: &Path, roots: impl Iterator<Item = &'a PathBuf>) -> String {
    let mut text = format!("sid {sid}\ntemp {}\n", temp.display());
    for root in roots {
        text.push_str(&format!("root {}\n", root.display()));
    }
    text
}

/// A record read back: the SID, the temp directory and the roots. `None` for
/// one that is not an incode session's (its SID lacks [`SID_PREFIX`]), which
/// the sweep then leaves alone.
fn parse_record(text: &str) -> Option<(String, Option<PathBuf>, Vec<PathBuf>)> {
    let mut sid = None;
    let mut temp = None;
    let mut roots = Vec::new();
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("sid", v)) => sid = Some(v.to_string()),
            Some(("temp", v)) => temp = Some(PathBuf::from(v)),
            Some(("root", v)) => roots.push(PathBuf::from(v)),
            _ => {}
        }
    }
    let sid = sid.filter(|s| s.starts_with(SID_PREFIX) && s.len() > SID_PREFIX.len())?;
    Some((sid, temp, roots))
}

/// Revoke what dead sessions left: every record whose process's liveness
/// mutex is gone.
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
        let Some((sid, temp, roots)) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| parse_record(&t))
        else {
            continue;
        };
        let mut clean = true;
        for root in &roots {
            if !root.exists() {
                continue;
            }
            if let Err(e) = change(root, &sid, Change::Revoke) {
                crate::trace!(
                    "sandbox",
                    "sweep: could not revoke {} ({e})",
                    root.display()
                );
                clean = false;
            }
        }
        if let Some(temp) = temp.filter(|t| t.exists()) {
            if std::fs::remove_dir_all(&temp).is_err() {
                clean = false;
            }
        }
        if clean {
            let _ = std::fs::remove_file(&path);
            crate::trace!(
                "sandbox",
                "swept the grant a dead session left on {} root(s)",
                roots.len()
            );
        }
    }
}

/// Whether the session `rid` is still running: its liveness mutex exists.
/// Any answer but "no such mutex" counts as alive, so a sweep never revokes a
/// live session's grant on a doubt.
fn is_alive(rid: u32) -> bool {
    // SAFETY: a NUL-terminated name; the handle, if any, is closed at once.
    unsafe {
        let handle = OpenMutexW(SYNCHRONIZE, 0, wide(alive_name(rid)).as_ptr());
        if !handle.is_null() {
            CloseHandle(handle);
            return true;
        }
        GetLastError() != ERROR_FILE_NOT_FOUND
    }
}

fn alive_name(rid: u32) -> String {
    format!("Global\\incode-sandbox-{rid}")
}

/// A random, non-zero session RID that is not the read-only SID's 4242.
fn random_rid() -> u32 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let rid = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if rid == 0 || rid == 4242 {
        0x8000_0001
    } else {
        rid
    }
}

/// The roots worth granting, in a stable order: each checked, and a root
/// inside another dropped (it inherits the outer one's ACE).
fn wanted(roots: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        refuse_broad(root, profile.as_deref())?;
        out.push(root.clone());
    }
    out.sort();
    out.dedup();
    let all = out.clone();
    out.retain(|r| !all.iter().any(|other| other != r && r.starts_with(other)));
    Ok(out)
}

/// Refuse a root whose grant would reach far past a workspace: a drive root,
/// or a directory that holds the user profile.
fn refuse_broad(root: &Path, profile: Option<&Path>) -> Result<(), String> {
    if !root.is_absolute() || root.parent().is_none() {
        return Err(format!(
            "workspace-write will not grant {}: a drive root is not a workspace",
            root.display()
        ));
    }
    if profile.is_some_and(|p| p.starts_with(root)) {
        return Err(format!(
            "workspace-write will not grant {}: it holds the user profile",
            root.display()
        ));
    }
    if !root.is_dir() {
        return Err(format!(
            "workspace-write cannot grant {}: not a directory",
            root.display()
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Change {
    Grant,
    Revoke,
}

/// Add (or remove) the SID's inheritable `Modify` ACE on `dir`, propagating to
/// everything beneath it. The DACL's protection is kept as it was. Serialised
/// across incode processes, because two read-modify-writes of one DACL at once
/// would lose one.
fn change(dir: &Path, sid_text: &str, change: Change) -> Result<(), String> {
    let _lock = AclLock::take()?;
    let trustee_sid = sid(sid_text)?;
    let path = wide(dir);
    // SAFETY: every pointer passed lives to the end of the block (`path`,
    // `trustee_sid`, the returned descriptor and ACL, which are freed by their
    // owners); each call's result is checked.
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
                ptstrName: trustee_sid.0.cast(),
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
            let handle = CreateMutexW(
                std::ptr::null(),
                0,
                wide("Global\\incode-sandbox-acl").as_ptr(),
            );
            if handle.is_null() {
                return Err(last_error("CreateMutexW"));
            }
            let waited = WaitForSingleObject(handle, LOCK_WAIT_MS);
            if waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED {
                CloseHandle(handle);
                return Err(
                    "another incode process held the ACL lock for two minutes; not waiting longer"
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
    use super::super::imp::OwnedHandle;
    use super::*;

    /// A record round-trips, and one whose SID is not an incode session's is
    /// not read back — the sweep must never revoke someone else's ACE.
    #[test]
    fn a_record_round_trips_and_a_foreign_sid_is_ignored() {
        let roots = [PathBuf::from(r"C:\w\a"), PathBuf::from(r"D:\b c")];
        let sid = format!("{SID_PREFIX}123");
        let text = record_text(&sid, Path::new(r"C:\d\123.tmp"), roots.iter());
        let (back, temp, got) = parse_record(&text).expect("parses");
        assert_eq!(back, sid);
        assert_eq!(temp, Some(PathBuf::from(r"C:\d\123.tmp")));
        assert_eq!(got, roots);

        assert!(parse_record("sid S-1-1-0\nroot C:\\w\n").is_none());
        assert!(parse_record(&format!("sid {SID_PREFIX}\n")).is_none());
        assert!(parse_record("root C:\\w\n").is_none());
    }

    /// A drive root and a directory holding the profile are refused; a root
    /// inside another is dropped, since it inherits the outer ACE.
    #[test]
    fn broad_roots_are_refused_and_nested_ones_folded() {
        let profile = Path::new(r"C:\Users\someone");
        assert!(refuse_broad(Path::new(r"C:\"), Some(profile)).is_err());
        assert!(refuse_broad(Path::new(r"C:\Users"), Some(profile)).is_err());
        assert!(refuse_broad(Path::new(r"C:\Users\someone"), Some(profile)).is_err());
        assert!(refuse_broad(Path::new("relative"), Some(profile)).is_err());

        let dir = tempfile::tempdir().expect("temp dir");
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).expect("mkdir");
        assert!(refuse_broad(dir.path(), Some(profile)).is_ok());
        let folded = wanted(&[inner.clone(), dir.path().to_path_buf(), inner]).expect("ok");
        assert_eq!(folded, [dir.path().to_path_buf()]);
    }

    /// A session RID is never the read-only SID's, and never zero.
    #[test]
    fn a_session_rid_is_never_the_read_only_one() {
        for _ in 0..64 {
            let rid = random_rid();
            assert_ne!(rid, 0);
            assert_ne!(rid, 4242);
        }
    }

    // ---- Live, opt-in: they change real ACLs, so they run only on a scratch
    // tree named by `INCODE_SANDBOX_LIVE_DIR` (with `INCODE_SANDBOX_LIVE_BIN`
    // the built incode.exe, `INCODE_SANDBOX_LIVE_OUTSIDE` a scratch directory
    // outside the tree, and `INCODE_DIR` pointing the records at scratch
    // too). Run with `--ignored --test-threads=1`.

    struct Live {
        dir: PathBuf,
        bin: PathBuf,
        outside: PathBuf,
    }

    fn live() -> Option<Live> {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Some(Live {
            dir: var("INCODE_SANDBOX_LIVE_DIR")?,
            bin: var("INCODE_SANDBOX_LIVE_BIN")?,
            outside: var("INCODE_SANDBOX_LIVE_OUTSIDE")?,
        })
    }

    /// Every ACL under `dir`, as `icacls /save` writes it.
    fn snapshot(dir: &Path, to: &Path) -> Vec<u8> {
        let status = std::process::Command::new("icacls")
            .arg(dir)
            .arg("/save")
            .arg(to)
            .args(["/t", "/c", "/q"])
            .status()
            .expect("icacls runs");
        assert!(status.success(), "icacls /save failed");
        std::fs::read(to).expect("snapshot")
    }

    /// Whether `sid` appears in `path`'s own ACL.
    fn acl_names(path: &Path, sid: &str) -> bool {
        let out = std::process::Command::new("icacls")
            .arg(path)
            .output()
            .expect("icacls runs");
        String::from_utf8_lossy(&out.stdout).contains(sid)
    }

    /// Run `command` through the real helper under `grant`, as `wrap` would.
    fn run(live: &Live, grant: &Grant, command: &str) -> (bool, String) {
        run_as(
            live,
            crate::sandbox::SandboxMode::WorkspaceWrite,
            Some(grant),
            command,
        )
    }

    /// Run `command` through the real helper in `mode`, in the shell the
    /// `Bash` tool picks for that sandbox — as `wrap` would.
    fn run_as(
        live: &Live,
        mode: crate::sandbox::SandboxMode,
        grant: Option<&Grant>,
        command: &str,
    ) -> (bool, String) {
        let policy = crate::sandbox::SandboxPolicy {
            mode,
            network: crate::sandbox::SandboxNetwork::Allow,
            named: true,
            engine: crate::sandbox::SandboxEngine::Builtin,
        };
        let shell = crate::tools::shell_choice::for_sandbox(&crate::sandbox::Sandbox::new(
            policy,
            vec![live.dir.clone()],
        ))
        .expect("a shell for the sandbox");
        eprintln!(
            "shell: {} ({:?})",
            shell.program().display(),
            shell.flavor()
        );
        let args = super::super::helper_args(mode, grant, &shell, command).expect("args");
        let out = std::process::Command::new(&live.bin)
            .args(args)
            .current_dir(&live.dir)
            .output()
            .expect("helper runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    /// A workspace-write SESSION, live: writes inside the tree and to the
    /// session's temp are allowed; writes to Documents and to a directory
    /// outside, and re-ACLing a file outside, are refused; and after the
    /// session the whole tree's ACLs are byte-identical to before.
    #[test]
    #[ignore = "live: changes ACLs on the scratch tree named by INCODE_SANDBOX_LIVE_DIR"]
    fn live_a_session_grants_confines_and_restores() {
        let Some(live) = live() else {
            eprintln!("INCODE_SANDBOX_LIVE_* not set; skipped");
            return;
        };
        let snaps = live.outside.join("snapshots");
        std::fs::create_dir_all(&snaps).expect("snapshots dir");
        let before = snapshot(&live.dir, &snaps.join("session_before.txt"));

        let session = crate::sandbox::SessionGrant::default();
        let policy = crate::sandbox::SandboxPolicy {
            mode: crate::sandbox::SandboxMode::WorkspaceWrite,
            network: crate::sandbox::SandboxNetwork::Allow,
            named: true,
            engine: crate::sandbox::SandboxEngine::Builtin,
        };
        let began = Instant::now();
        let sandbox = crate::sandbox::Sandbox::new(policy, vec![live.dir.clone()])
            .prepared(&session)
            .expect("granted");
        eprintln!("GRANT {:.2}s", began.elapsed().as_secs_f64());
        let grant = sandbox.grant.clone().expect("a grant");
        assert!(sandbox.note().contains("confined by"), "{}", sandbox.note());
        assert!(acl_names(&live.dir, &grant.sid), "the root carries the ACE");

        // The commands are PowerShell: the shell the Bash tool runs under this
        // token (Git Bash cannot start under it).
        // Inside: allowed.
        let inside = live.dir.join("live_inside.txt");
        let (ok, out) = run(
            &live,
            &grant,
            &format!("Set-Content -Path \"{}\" -Value inside", inside.display()),
        );
        eprintln!("inside: ok={ok} {out}");
        assert!(ok && inside.exists(), "a write inside the workspace: {out}");

        // Documents: refused, and nothing lands.
        let profile = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .expect("profile");
        let documents = profile
            .join("Documents")
            .join("incode_sandbox_live_probe.txt");
        let (_, out) = run(
            &live,
            &grant,
            &format!("Set-Content -Path \"{}\" -Value x", documents.display()),
        );
        eprintln!("documents: {out}");
        let landed = documents.exists();
        if landed {
            let _ = std::fs::remove_file(&documents);
        }
        assert!(!landed, "a write to Documents was NOT refused: {out}");

        // Outside the tree: refused.
        let outside = live.outside.join("live_outside.txt");
        let (_, out) = run(
            &live,
            &grant,
            &format!("Set-Content -Path \"{}\" -Value x", outside.display()),
        );
        eprintln!("outside: {out}");
        assert!(!outside.exists(), "a write outside was NOT refused: {out}");

        // Re-ACLing a file outside the tree: refused (WRITE_DAC is a write).
        let victim = live.outside.join("victim.txt");
        std::fs::write(&victim, "x").expect("victim");
        let (ok, out) = run(
            &live,
            &grant,
            &format!("icacls \"{}\" /grant '*S-1-1-0:F'", victim.display()),
        );
        eprintln!("re-acl outside: ok={ok} {out}");
        assert!(
            !acl_names(&victim, "Everyone"),
            "the command re-ACLed a file outside: {out}"
        );

        // The session's temp: named by %TEMP%, and writable.
        let (ok, out) = run(
            &live,
            &grant,
            "Write-Output $env:TEMP; Set-Content -Path \"$env:TEMP\\live_temp.txt\" -Value t",
        );
        eprintln!("temp: ok={ok} {out}");
        assert!(
            ok && out.contains(&grant.temp.display().to_string()),
            "{out}"
        );
        assert!(grant.temp.join("live_temp.txt").exists(), "{out}");

        // Deleting inside: allowed.
        let (ok, out) = run(
            &live,
            &grant,
            &format!("Remove-Item \"{}\"", inside.display()),
        );
        assert!(ok && !inside.exists(), "a delete inside: {out}");

        let began = Instant::now();
        drop(sandbox);
        drop(session);
        eprintln!("REVOKE {:.2}s", began.elapsed().as_secs_f64());
        assert!(!grant.temp.exists(), "the session temp is removed");
        let after = snapshot(&live.dir, &snaps.join("session_after.txt"));
        assert!(before == after, "the tree's ACLs differ after the session");
        eprintln!("ACL snapshot identical: {} bytes", after.len());
    }

    /// A READ-ONLY command, live, in the shell the Bash tool picks for it: it
    /// reads the workspace, and every write — inside the workspace, outside
    /// it, to Documents — is refused and lands nowhere. No ACL is changed.
    #[test]
    #[ignore = "live: runs the built helper named by INCODE_SANDBOX_LIVE_BIN"]
    fn live_read_only_confines_the_bash_tools_shell() {
        let Some(live) = live() else {
            eprintln!("INCODE_SANDBOX_LIVE_* not set; skipped");
            return;
        };
        let ro = crate::sandbox::SandboxMode::ReadOnly;
        let readable = live.dir.join("live_ro_readable.txt");
        std::fs::write(&readable, "readable-by-the-command").expect("fixture");

        let (ok, out) = run_as(
            &live,
            ro,
            None,
            &format!("Get-Content \"{}\"", readable.display()),
        );
        eprintln!("read: ok={ok} {out}");
        assert!(ok && out.contains("readable-by-the-command"), "{out}");

        let profile = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .expect("profile");
        for target in [
            live.dir.join("live_ro_inside.txt"),
            live.outside.join("live_ro_outside.txt"),
            profile.join("Documents").join("incode_ro_live_probe.txt"),
        ] {
            let (_, out) = run_as(
                &live,
                ro,
                None,
                &format!("Set-Content -Path \"{}\" -Value x", target.display()),
            );
            let landed = target.exists();
            if landed {
                let _ = std::fs::remove_file(&target);
            }
            eprintln!("write {}: landed={landed}", target.display());
            assert!(
                !landed,
                "a read-only write landed at {}: {out}",
                target.display()
            );
        }
        let _ = std::fs::remove_file(&readable);
    }

    /// The crash half: a child process takes a grant and is killed. The ACE
    /// it left is on the tree; the next grant in this process sweeps it; and
    /// once this session ends the tree's ACLs are byte-identical to before.
    #[test]
    #[ignore = "live: changes ACLs on the scratch tree named by INCODE_SANDBOX_LIVE_DIR"]
    fn live_a_crashed_session_is_swept() {
        use std::io::BufRead;
        let Some(live) = live() else {
            eprintln!("INCODE_SANDBOX_LIVE_* not set; skipped");
            return;
        };
        let snaps = live.outside.join("snapshots");
        std::fs::create_dir_all(&snaps).expect("snapshots dir");
        let before = snapshot(&live.dir, &snaps.join("crash_before.txt"));

        let mut child = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args([
                "--ignored",
                "--exact",
                "sandbox::windows::acl::tests::live_crash_child",
                "--nocapture",
            ])
            .env("INCODE_SANDBOX_LIVE_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("child");
        let stdout = child.stdout.take().expect("stdout");
        let mut child_sid = None;
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.expect("line");
            if let Some(sid) = line.strip_prefix("GRANTED ") {
                child_sid = Some(sid.trim().to_string());
                break;
            }
        }
        let child_sid = child_sid.expect("the child granted");
        child.kill().expect("killed"); // TerminateProcess: no destructor runs
        let _ = child.wait();
        assert!(
            acl_names(&live.dir, &child_sid),
            "the killed session's ACE is still on the root"
        );
        eprintln!("left behind by the killed child: {child_sid}");

        let began = Instant::now();
        let lease = Lease::acquire(std::slice::from_ref(&live.dir)).expect("granted after a sweep");
        eprintln!("SWEEP+GRANT {:.2}s", began.elapsed().as_secs_f64());
        assert!(
            !acl_names(&live.dir, &child_sid),
            "the sweep removed the dead ACE"
        );
        drop(lease);
        let after = snapshot(&live.dir, &snaps.join("crash_after.txt"));
        assert!(before == after, "the tree's ACLs differ after the sweep");
        eprintln!("ACL snapshot identical after sweep: {} bytes", after.len());
    }

    /// The child the crash test kills: takes a grant, says so, and waits.
    #[test]
    #[ignore = "live: run only as the child of live_a_crashed_session_is_swept"]
    fn live_crash_child() {
        if std::env::var_os("INCODE_SANDBOX_LIVE_CHILD").is_none() {
            return;
        }
        let live = live().expect("live env");
        let lease = Lease::acquire(std::slice::from_ref(&live.dir)).expect("granted");
        println!("GRANTED {}", lease.grant().sid);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(120));
        drop(lease);
    }

    /// A liveness name nobody created reads as dead; one this process holds
    /// reads as alive.
    #[test]
    fn liveness_follows_the_named_mutex() {
        let rid = random_rid();
        assert!(!is_alive(rid), "nobody holds {rid}");
        // SAFETY: a NUL-terminated name; closed by the guard.
        let held = OwnedHandle(unsafe {
            CreateMutexW(std::ptr::null(), 0, wide(alive_name(rid)).as_ptr())
        });
        assert!(!held.0.is_null());
        assert!(is_alive(rid));
        drop(held);
        assert!(!is_alive(rid), "closed: gone");
    }
}
