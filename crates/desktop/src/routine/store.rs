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
//! The file holds typed text literally (a routine that types a phrase must
//! store it), so on Unix it is created readable by its owner only.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::words::{key, word_set};
use super::{day_of, Routine, Step, MAX_ROUTINES};

/// The environment variable that names the routines file when the config does
/// not.
pub const ROUTINES_ENV: &str = "INCU_ROUTINES";

/// The most of a routines file that is read, in bytes. A file over this was not
/// written by this crate.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

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
    /// The replaced routine had been taught by name, and the new one keeps that.
    pub taught: bool,
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
        let want = word_set(phrase);
        if want.is_empty() {
            return None;
        }
        self.live(now_ms).find(|r| word_set(&r.phrase) == want)
    }

    /// Learn `steps` under `phrase`, replacing the routine that asks the same.
    ///
    /// A redo after a failed replay replaces the old routine and keeps it
    /// taught if it was (Atticus keeps what the user taught it as).
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
            taught: taught || was_taught,
        };
        let taught = routine.taught;
        match at {
            Some(i) => self.routines[i] = routine,
            None => self.routines.push(routine),
        }
        Ok(Saved {
            replaced: at.is_some(),
            taught,
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
    fn learning_a_phrase_twice_replaces_the_routine_and_keeps_it_taught() {
        let mut file = Routines::default();
        let first = file.learn("open notes", steps(), true, 1_000).unwrap();
        assert_eq!(
            first,
            Saved {
                replaced: false,
                taught: true
            }
        );
        let newer = vec![Step::new("key", json!({"keys": ["ctrl", "t"]}))];
        // The same words in another order are the same request.
        let second = file
            .learn("Notes, open", newer.clone(), false, 2_000)
            .unwrap();
        assert_eq!(
            second,
            Saved {
                replaced: true,
                taught: true
            }
        );
        assert_eq!(file.routines.len(), 1);
        assert_eq!(file.routines[0].steps, newer);
        assert_eq!(file.routines[0].last_used_ms, 2_000);
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
