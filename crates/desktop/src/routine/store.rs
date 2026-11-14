//! The routines file: one JSON document, written atomically.
//!
//! # Where it lives
//!
//! One file, like Atticus's `skills.json`. Its path is, in order: the
//! `routines` setting of [`crate::config::Config`] (`--config`), the
//! `INCU_ROUTINES` environment variable, else `routines.json` under
//! `incord-desktop` in the platform's per-user data folder:
//!
//! | OS | default |
//! |---|---|
//! | Windows | `%APPDATA%\incord-desktop\routines.json` |
//! | macOS | `~/Library/Application Support/incord-desktop/routines.json` |
//! | Linux | `$XDG_DATA_HOME` or `~/.local/share`, then `incord-desktop/routines.json` |
//!
//! It is per user and not per project: a routine is about the user's desktop,
//! not a repository, and `incu` is a standalone binary with no workspace.
//!
//! # Safe against a crash and against a bad file
//!
//! [`Store::save`] writes a temporary file beside the real one, flushes it to
//! disk and renames it over the real one, so a reader sees the old file or the
//! new one and never half of either. A file that exists but cannot be read as
//! routines is REFUSED ([`Store::load`]), not read as empty: reading it as empty
//! and then saving would destroy every routine in it.
//!
//! The file holds the text a routine types, literally, EXCEPT what goes into a
//! password or other secure field and anything a recording kept only as a
//! placeholder: those are parameters (`{{name}}`, see [`super::params`]) that the
//! caller of `routine_run` supplies, and are never written here. So that the
//! person can still see everything the file does hold, on Unix it is created
//! readable by its owner only.
//!
//! # Never locked out by its own size
//!
//! [`Store::load`] refuses a file over [`MAX_FILE_BYTES`], so [`Store::save`]
//! refuses to write one: a file that can be written but not read again would
//! lose every routine in it. [`MAX_ROUTINES`] times the most one routine may take
//! ([`super::MAX_ROUTINE_BYTES`]) is held under that bound by a test.
//!
//! # Two servers, one file
//!
//! A change is load, change, save. [`Store::update`] holds an exclusive OS lock
//! ([`super::oslock`], on a lock file beside the routines file) across all three,
//! so two servers (or
//! `incu routine keep`) that change it at once each keep the other's change
//! instead of the later save writing over the earlier. The OS releases the lock
//! when its holder exits or crashes, so there is no stale lock to take over.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::oslock::OsLock;
use super::words::{key, word_set};
use super::{day_of, Routine, Step, MAX_ROUTINES, MAX_ROUTINE_BYTES};

/// The environment variable that names the routines file when the config does
/// not.
pub const ROUTINES_ENV: &str = "INCU_ROUTINES";

/// The most of a routines file that is read or written, in bytes. A file over
/// this was not written by this crate.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// How long a change waits for another's lock before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The document on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routines {
    /// The format, for a later change to refuse an older file by name.
    pub version: u32,
    /// Every routine, oldest first.
    pub routines: Vec<Routine>,
}

impl Default for Routines {
    fn default() -> Self {
        Self {
            version: 1,
            routines: Vec::new(),
        }
    }
}

/// What a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// A routine with the same request was there and was replaced.
    pub replaced: bool,
    /// The saved routine is kept by the person (it never expires).
    pub taught: bool,
    /// The replaced routine HAD been kept and the new one, with other steps, is
    /// not: what the person reviewed and kept is not what is saved now, so it
    /// has to be reviewed and kept again.
    pub unkept: bool,
}

impl Routines {
    /// The routines still alive at `now_ms`, in order.
    pub fn live(&self, now_ms: i64) -> impl Iterator<Item = &Routine> {
        self.routines.iter().filter(move |r| !r.is_expired(now_ms))
    }

    /// Drop the expired routines; how many went.
    pub fn prune(&mut self, now_ms: i64) -> usize {
        let before = self.routines.len();
        self.routines.retain(|r| !r.is_expired(now_ms));
        before - self.routines.len()
    }

    /// The live routine `phrase` asks for.
    #[must_use]
    pub fn matching(&self, phrase: &str, now_ms: i64) -> Option<&Routine> {
        if word_set(phrase).is_empty() {
            return None;
        }
        let want = key(phrase);
        self.live(now_ms).find(|r| key(&r.phrase) == want)
    }

    /// Mark the routine `phrase` asks for as taught by the person (it never
    /// expires) or not; whether there was one.
    ///
    /// Only the person does this (`incu routine keep`); no tool takes it as an
    /// argument, so an agent cannot exempt what it saved from expiry.
    pub fn set_taught(&mut self, phrase: &str, taught: bool, now_ms: i64) -> bool {
        self.prune(now_ms);
        let wanted = key(phrase);
        if word_set(phrase).is_empty() {
            return false;
        }
        match self.routines.iter_mut().find(|r| key(&r.phrase) == wanted) {
            Some(routine) => {
                routine.taught = taught;
                // Un-keeping starts the thirty days from now, not from long ago.
                if !taught {
                    routine.last_used_ms = now_ms;
                }
                true
            }
            None => false,
        }
    }

    /// Learn `steps` under `phrase`, replacing the routine that asks the same.
    ///
    /// A redo after a failed replay replaces the old routine. **It does not
    /// keep the person's "kept" flag unless the steps are the same ones**: the
    /// person kept what they had read, and a replacement with other steps is
    /// something nobody has read. It is un-kept and `Saved::unkept` says so; only
    /// `incu routine keep` sets the flag again. (`taught` is for the person's own
    /// code and tests: no tool passes it.)
    ///
    /// # Errors
    ///
    /// The file already holds [`MAX_ROUTINES`] routines and this one is new.
    pub fn learn(
        &mut self,
        phrase: &str,
        steps: Vec<Step>,
        taught: bool,
        now_ms: i64,
    ) -> Result<Saved, String> {
        self.prune(now_ms);
        let wanted = key(phrase);
        let at = self.routines.iter().position(|r| key(&r.phrase) == wanted);
        let was_taught = at.is_some_and(|i| self.routines[i].taught);
        let same_steps = at.is_some_and(|i| self.routines[i].steps == steps);
        let keeps = was_taught && same_steps;
        if at.is_none() && self.routines.len() >= MAX_ROUTINES {
            return Err(format!(
                "the routines file holds {MAX_ROUTINES} routines already; routine_forget \
                 one first"
            ));
        }
        let routine = Routine {
            phrase: phrase.trim().to_string(),
            steps,
            date: day_of(now_ms),
            last_used_ms: now_ms,
            taught: taught || keeps,
        };
        let size = routine.size_bytes();
        if size > MAX_ROUTINE_BYTES {
            return Err(format!(
                "this routine is {size} bytes, over the {MAX_ROUTINE_BYTES} one routine may \
                 take; save a shorter task (fewer or shorter typed texts)"
            ));
        }
        let taught = routine.taught;
        match at {
            Some(i) => self.routines[i] = routine,
            None => self.routines.push(routine),
        }
        Ok(Saved {
            replaced: at.is_some(),
            taught,
            unkept: was_taught && !taught,
        })
    }

    /// Forget the routine `phrase` asks for; whether there was one.
    pub fn forget(&mut self, phrase: &str, now_ms: i64) -> bool {
        self.prune(now_ms);
        let wanted = key(phrase);
        if wanted.is_empty() {
            return false;
        }
        let before = self.routines.len();
        self.routines.retain(|r| key(&r.phrase) != wanted);
        self.routines.len() < before
    }

    /// Note that the routine `phrase` asks for ran to the end at `now_ms`.
    pub fn used(&mut self, phrase: &str, now_ms: i64) {
        let wanted = key(phrase);
        if let Some(r) = self.routines.iter_mut().find(|r| key(&r.phrase) == wanted) {
            r.last_used_ms = now_ms;
        }
    }
}

/// The routines file at a path.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// A store at `path`. Nothing is read or made until it is used.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Where the file is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the pictures of the last replay are kept: a folder beside the
    /// routines file.
    #[must_use]
    pub fn pictures_dir(&self) -> PathBuf {
        self.path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("last-replay")
    }

    /// Change the routines: load, `change`, save, with the lock held across all
    /// three so that no other change lands in between and is lost.
    ///
    /// `change` answering `Err` saves nothing and is the error of the call.
    ///
    /// # Errors
    ///
    /// The lock could not be had within ten seconds, the file could not be loaded
    /// or saved, or `change` refused.
    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut Routines) -> Result<T, String>,
    ) -> Result<T, String> {
        let _held = self.lock()?;
        let mut file = self.load()?;
        let out = change(&mut file)?;
        self.save(&file)?;
        Ok(out)
    }

    /// The lock file's path: the routines file's name and `.lock`.
    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map_or_else(|| "routines".into(), std::ffi::OsString::from);
        name.push(".lock");
        self.path.with_file_name(name)
    }

    /// Take the lock: an exclusive OS lock on the lock file, waited for up to
    /// [`LOCK_WAIT`].
    ///
    /// **An OS lock and not the existence of a file**, because a file that
    /// exists says nothing about whether its maker is alive: the old scheme had
    /// to guess at a dead holder by the age of its file, and a holder merely
    /// slower than the guess was robbed, letting two writers in. The OS holds
    /// this lock for exactly as long as the holder's handle is open, and releases
    /// it the instant the holder exits or dies, so there is nothing to take over
    /// and no clock to read. The file itself is never deleted (deleting it would
    /// let two processes lock two different files of one name).
    fn lock(&self) -> Result<Lock, String> {
        let path = self.lock_path();
        if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot make the folder for {}: {e}", path.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("cannot open the lock {}: {e}", path.display()))?;
        let mut held = OsLock::new(file);
        held.acquire_within(LOCK_WAIT).map_err(|e| match e {
            None => format!(
                "another change to the routines is in progress ({} is held); try again in a \
                 moment",
                path.display()
            ),
            Some(e) => format!("cannot lock the routines ({}): {e}", path.display()),
        })?;
        Ok(Lock { _held: held })
    }

    /// The default path: `$INCU_ROUTINES`, else the per-user data folder.
    /// `None` when neither can be found (no home folder).
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(ROUTINES_ENV).filter(|p| !p.is_empty()) {
            return Some(PathBuf::from(path));
        }
        data_dir().map(|dir| dir.join("incord-desktop").join("routines.json"))
    }

    /// The routines in the file; none if there is no file yet.
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read, is over 4 MiB or is not a routines
    /// document. It is never read as empty: saving after that would erase it.
    pub fn load(&self) -> Result<Routines, String> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Routines::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", self.path.display())),
        };
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!(
                "{} is {} bytes, over the {MAX_FILE_BYTES} a routines file may be; it was \
                 not written by this program and has been left alone",
                self.path.display(),
                meta.len()
            ));
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("cannot read {}: {e}", self.path.display()))?;
        let file: Routines = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("{} is not a routines file: {e}", self.path.display()))?;
        if file.version != 1 {
            return Err(format!(
                "{} is version {}, which this program does not read",
                self.path.display(),
                file.version
            ));
        }
        Ok(file)
    }

    /// Write `routines` to the file, atomically.
    ///
    /// # Errors
    ///
    /// The folder cannot be made, or the file cannot be written, flushed or
    /// renamed into place. The old file is untouched in every one of those.
    pub fn save(&self, routines: &Routines) -> Result<(), String> {
        let fail = |what: &str, e: std::io::Error| {
            format!(
                "cannot save routines to {}: {what}: {e}",
                self.path.display()
            )
        };
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| fail("making the folder", e))?;
        let json = serde_json::to_string_pretty(routines)
            .map_err(|e| format!("cannot encode the routines: {e}"))?;
        // The bound `load` holds the file to. A file written over it could not be
        // read again, and every routine in it would be locked out.
        if json.len() as u64 > MAX_FILE_BYTES {
            return Err(format!(
                "not saved: the routines file would be {} bytes, over the {MAX_FILE_BYTES} \
                 it may be, and could not be read again. Nothing was written; routine_forget \
                 a routine you no longer need",
                json.len()
            ));
        }
        let temp = dir.join(format!(
            ".{}.tmp-{}",
            self.path
                .file_name()
                .map_or("routines".into(), |n| n.to_string_lossy()),
            std::process::id()
        ));
        let written = write_private(&temp, json.as_bytes());
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(fail("writing", e));
        }
        std::fs::rename(&temp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            fail("replacing the file", e)
        })
    }
}

/// The lock, held until it is dropped (its file closes, and the OS lets go), or
/// the process ends, which the OS counts the same.
struct Lock {
    _held: OsLock,
}

/// Write and flush a new file readable by its owner only (on Unix).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The per-user data folder, per platform.
fn data_dir() -> Option<PathBuf> {
    let home = || {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".local").join("share")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::DAY_MS;
    use serde_json::json;
    use std::time::Instant;

    fn steps() -> Vec<Step> {
        vec![Step::new("key", json!({"keys": ["ctrl", "n"]}))]
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("incu-routines-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn learning_a_phrase_twice_replaces_the_routine_and_only_the_same_steps_stay_kept() {
        let mut file = Routines::default();
        let first = file.learn("open notes", steps(), true, 1_000).unwrap();
        assert_eq!(
            first,
            Saved {
                replaced: false,
                taught: true,
                unkept: false
            }
        );
        // The same steps again (nothing the person has not read): still kept.
        let again = file.learn("Notes, open", steps(), false, 1_500).unwrap();
        assert_eq!(
            again,
            Saved {
                replaced: true,
                taught: true,
                unkept: false
            }
        );
        let newer = vec![Step::new("key", json!({"keys": ["ctrl", "t"]}))];
        // The same words in another order are the same request. Other steps are
        // not what the person kept: the replacement is not kept, and says so.
        let second = file
            .learn("Notes, open", newer.clone(), false, 2_000)
            .unwrap();
        assert_eq!(
            second,
            Saved {
                replaced: true,
                taught: false,
                unkept: true
            }
        );
        assert_eq!(file.routines.len(), 1);
        assert_eq!(file.routines[0].steps, newer);
        assert!(!file.routines[0].taught);
        assert_eq!(file.routines[0].last_used_ms, 2_000);
        // And it expires like any other now.
        assert!(file.matching("open notes", 40 * DAY_MS).is_none());
    }

    #[test]
    fn an_expired_routine_is_neither_listed_nor_matched_and_is_pruned_on_a_write() {
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        file.learn("open mail", steps(), true, 0).unwrap();
        let late = 31 * DAY_MS;
        let alive: Vec<_> = file.live(late).map(|r| r.phrase.as_str()).collect();
        assert_eq!(alive, ["open mail"]);
        assert!(file.matching("open notes", late).is_none());
        assert!(file.matching("open mail", late).is_some());
        assert_eq!(file.routines.len(), 2, "reading does not rewrite the file");
        file.learn("open calendar", steps(), false, late).unwrap();
        let kept: Vec<_> = file.routines.iter().map(|r| r.phrase.as_str()).collect();
        assert_eq!(kept, ["open mail", "open calendar"]);
    }

    #[test]
    fn using_a_routine_starts_its_thirty_days_again() {
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        file.used("notes open", 20 * DAY_MS);
        assert!(file.matching("open notes", 49 * DAY_MS).is_some());
        assert!(file.matching("open notes", 51 * DAY_MS).is_none());
    }

    #[test]
    fn forgetting_removes_what_the_phrase_asks_for_and_says_whether_it_did() {
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        assert!(!file.forget("open mail", 1));
        assert!(
            !file.forget("please", 1),
            "an empty request forgets nothing"
        );
        assert!(file.forget("notes open", 1));
        assert!(file.routines.is_empty());
    }

    #[test]
    fn a_new_routine_past_the_cap_is_refused_and_a_replacement_is_not() {
        let mut file = Routines::default();
        for i in 0..MAX_ROUTINES {
            file.learn(&format!("task number{i}"), steps(), true, 0)
                .unwrap();
        }
        let why = file.learn("one more", steps(), true, 0).unwrap_err();
        assert!(why.contains("routine_forget"), "{why}");
        assert!(
            file.learn("task number7", steps(), true, 5)
                .unwrap()
                .replaced
        );
    }

    #[test]
    fn a_missing_file_is_empty_and_a_saved_one_comes_back_whole() {
        let dir = folder("roundtrip");
        let store = Store::at(dir.join("nested").join("routines.json"));
        assert_eq!(store.load().unwrap(), Routines::default());
        let mut file = Routines::default();
        file.learn("open notes", steps(), true, 1_790_107_969_427)
            .unwrap();
        store.save(&file).unwrap();
        assert_eq!(store.load().unwrap(), file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_replaces_the_file_whole_and_leaves_no_temporary_file() {
        let dir = folder("atomic");
        let store = Store::at(dir.join("routines.json"));
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        store.save(&file).unwrap();
        file.learn("open mail", steps(), false, 0).unwrap();
        store.save(&file).unwrap();
        assert_eq!(store.load().unwrap().routines.len(), 2);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, ["routines.json"], "no temporary file is left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_leaves_the_old_file_exactly_as_it_was() {
        let dir = folder("failed");
        let path = dir.join("routines.json");
        let store = Store::at(path.clone());
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        store.save(&file).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        // A directory where the temporary file must go makes the write fail.
        let temp = dir.join(format!(".routines.json.tmp-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        file.learn("open mail", steps(), false, 0).unwrap();
        let why = store.save(&file).unwrap_err();
        assert!(why.contains("cannot save routines"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_temporary_file_from_a_crash_is_overwritten_not_loaded() {
        let dir = folder("stale");
        let store = Store::at(dir.join("routines.json"));
        let temp = dir.join(format!(".routines.json.tmp-{}", std::process::id()));
        std::fs::write(&temp, "{half a document").unwrap();
        assert_eq!(
            store.load().unwrap(),
            Routines::default(),
            "a temp file is never read"
        );
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        store.save(&file).unwrap();
        assert_eq!(store.load().unwrap(), file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_routines_is_refused_and_never_overwritten_by_a_load() {
        let dir = folder("bad");
        let path = dir.join("routines.json");
        std::fs::write(&path, "{not json").unwrap();
        let store = Store::at(path.clone());
        let why = store.load().unwrap_err();
        assert!(why.contains("not a routines file"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
        std::fs::write(&path, r#"{"version": 2, "routines": []}"#).unwrap();
        assert!(store.load().unwrap_err().contains("version 2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_byte_order_mark_from_a_windows_editor_is_tolerated() {
        let dir = folder("bom");
        let path = dir.join("routines.json");
        std::fs::write(&path, "\u{feff}{\"version\": 1, \"routines\": []}").unwrap();
        assert!(Store::at(path).load().unwrap().routines.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_over_the_size_a_load_accepts_is_refused_before_anything_is_written() {
        let dir = folder("toobig");
        let path = dir.join("routines.json");
        let store = Store::at(path.clone());
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        store.save(&file).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        // Stuff the document past the bound without going through `learn`.
        file.routines[0].steps = vec![Step::new(
            "type_text",
            json!({"text": "x".repeat(MAX_FILE_BYTES as usize)}),
        )];
        let why = store.save(&file).unwrap_err();
        assert!(why.contains("could not be read again"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(store.load().is_ok(), "the file is still readable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The arithmetic behind "never locked out": the most the cap on routines
    /// and the cap on one routine allow, written out, fits what `load` reads.
    #[test]
    fn the_most_the_caps_allow_is_a_file_that_can_be_read_again() {
        let dir = folder("arithmetic");
        let store = Store::at(dir.join("routines.json"));
        // Two shapes of a full routine: a few steps with the most text each can
        // hold, and the most steps with the least in each (the most structure).
        for shape in 0..2 {
            let mut file = Routines::default();
            for i in 0..MAX_ROUTINES {
                let steps = if shape == 0 {
                    // Typed text up to the routine's whole allowance.
                    let each = MAX_ROUTINE_BYTES / 4;
                    (0..3)
                        .map(|_| Step::new("type_text", json!({"text": "t".repeat(each - 100)})))
                        .collect()
                } else {
                    (0..crate::routine::MAX_STEPS)
                        .map(|_| Step::new("key", json!({"keys": ["a"]})))
                        .collect::<Vec<_>>()
                };
                let mut routine = Routine {
                    phrase: format!("{}{i}", "p".repeat(190)),
                    steps,
                    date: "2026-01-01".to_string(),
                    last_used_ms: 0,
                    taught: true,
                };
                // Grow it to the cap exactly, with filler in its last step.
                let room = MAX_ROUTINE_BYTES.saturating_sub(routine.size_bytes());
                if let Some(last) = routine.steps.last_mut() {
                    last.args = json!({"text": "f".repeat(room.saturating_sub(10))});
                    last.tool = "type_text".to_string();
                }
                assert!(routine.size_bytes() <= MAX_ROUTINE_BYTES);
                file.routines.push(routine);
            }
            store
                .save(&file)
                .unwrap_or_else(|why| panic!("shape {shape}: {why}"));
            assert_eq!(store.load().unwrap().routines.len(), MAX_ROUTINES);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_changes_at_once_each_keep_the_others_routine() {
        let dir = folder("race");
        let path = dir.join("routines.json");
        let workers: Vec<_> = (0..8)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // A store of its own, as a second server has.
                    let store = Store::at(path);
                    for i in 0..5 {
                        store
                            .update(|file| {
                                file.learn(&format!("task w{w} n{i}"), steps(), false, 0)
                                    .map(|_| ())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let kept = Store::at(path).load().unwrap().routines.len();
        assert_eq!(kept, 40, "a save overwrote another's routine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two writers can never be in the change at the same time, however slow
    /// the first is: a holder is never taken to be dead because it is slow.
    #[test]
    fn a_slow_holder_is_waited_for_and_never_robbed() {
        let dir = folder("slow-holder");
        let path = dir.join("routines.json");
        let inside = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlapped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers: Vec<_> = (0..4)
            .map(|w| {
                let (path, inside, overlapped) = (path.clone(), inside.clone(), overlapped.clone());
                std::thread::spawn(move || {
                    Store::at(path)
                        .update(|file| {
                            use std::sync::atomic::Ordering::SeqCst;
                            if inside.fetch_add(1, SeqCst) != 0 {
                                overlapped.store(true, SeqCst);
                            }
                            // Slow, and in the middle of the change.
                            std::thread::sleep(Duration::from_millis(150));
                            let kept = file.learn(&format!("task w{w}"), steps(), false, 0);
                            inside.fetch_sub(1, SeqCst);
                            kept.map(|_| ())
                        })
                        .unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(
            !overlapped.load(std::sync::atomic::Ordering::SeqCst),
            "two writers were inside a change at once"
        );
        assert_eq!(Store::at(path).load().unwrap().routines.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_change_that_refuses_saves_nothing_and_frees_the_lock() {
        let dir = folder("refuse");
        let store = Store::at(dir.join("routines.json"));
        let why = store
            .update(|_| Err::<(), _>("no such routine".to_string()))
            .unwrap_err();
        assert_eq!(why, "no such routine");
        assert!(!dir.join("routines.json").exists());
        // The lock is free again: another change goes straight in.
        store
            .update(|file| file.learn("open notes", steps(), false, 0).map(|_| ()))
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lock a holder left when it crashed is released by the OS with its
    /// handle: no file is "stale", nothing is taken over, nothing is waited for.
    #[test]
    fn a_holder_that_crashed_without_unlocking_frees_the_lock_at_once() {
        let dir = folder("crashed-holder");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::at(dir.join("routines.json"));
        let lock = dir.join("routines.json.lock");
        let mut holder = OsLock::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock)
                .unwrap(),
        );
        assert!(holder.try_acquire().unwrap());
        // The holder "crashes": its handle goes, with no unlock. Nothing about
        // the file says it is dead; the OS drops the lock with the handle.
        drop(holder);
        let started = Instant::now();
        store
            .update(|file| file.learn("open notes", steps(), false, 0).map(|_| ()))
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} for a lock nobody holds",
            started.elapsed()
        );
        assert_eq!(store.load().unwrap().routines.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lock_held_by_another_handle_is_waited_for_until_it_is_let_go() {
        let dir = folder("held-lock");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::at(dir.join("routines.json"));
        let mut holder = OsLock::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join("routines.json.lock"))
                .unwrap(),
        );
        assert!(holder.try_acquire().unwrap());
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            drop(holder);
        });
        let started = Instant::now();
        store
            .update(|file| file.learn("open mail", steps(), false, 0).map(|_| ()))
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "went in while it was held: {:?}",
            started.elapsed()
        );
        releaser.join().unwrap();
        assert_eq!(store.load().unwrap().routines.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_person_can_keep_a_routine_and_unkeeping_restarts_its_thirty_days() {
        let mut file = Routines::default();
        file.learn("open notes", steps(), false, 0).unwrap();
        assert!(file.set_taught("notes open", true, 1_000));
        assert!(file.matching("open notes", 400 * DAY_MS).is_some());
        assert!(file.set_taught("open notes", false, 100 * DAY_MS));
        assert!(file.matching("open notes", 120 * DAY_MS).is_some());
        assert!(file.matching("open notes", 131 * DAY_MS).is_none());
        assert!(!file.set_taught("open mail", true, 0));
        assert!(!file.set_taught("please", true, 0), "no words, no routine");
    }

    #[test]
    fn a_routine_over_its_own_size_is_refused_with_the_number() {
        let mut file = Routines::default();
        let big = vec![Step::new(
            "type_text",
            json!({"text": "x".repeat(MAX_ROUTINE_BYTES)}),
        )];
        let why = file.learn("type a lot", big, false, 0).unwrap_err();
        assert!(why.contains("one routine may take"), "{why}");
        assert!(file.routines.is_empty());
    }

    #[test]
    fn the_default_path_is_the_environment_variable_first() {
        // Reads the process environment, so it is the one test that sets it.
        let dir = folder("env");
        let wanted = dir.join("mine.json");
        std::env::set_var(ROUTINES_ENV, &wanted);
        assert_eq!(Store::default_path(), Some(wanted));
        std::env::remove_var(ROUTINES_ENV);
        if let Some(path) = Store::default_path() {
            assert!(
                path.ends_with("incord-desktop/routines.json")
                    || path.ends_with("incord-desktop\\routines.json")
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
