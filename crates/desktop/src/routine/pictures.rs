//! The pictures of the last replay: one before and one after each step.
//!
//! # What and why
//!
//! A replay that fails is only useful if the agent that redoes the task, and the
//! person reading over its shoulder, can see what the screen looked like. So
//! each step of `routine_run` gets one picture before it and one after it, taken
//! through the same gate as `screenshot` (the window the step is for, else the
//! screen), and a failure names the failed step's two pictures by path. There is
//! no video and no ffmpeg: a step is a still.
//!
//! # One folder for each run
//!
//! Two replays can be going at once (two servers share the routines file). Each
//! run keeps its pictures in a folder of its own beside the routines file,
//! `last-replay/run-<time>-<pid>-<n>/`, so that one run can never clear, or be
//! mistaken for, another's, and a failure points only into its own run's folder.
//!
//! # Kept small, last run only
//!
//! A new run deletes the folders of runs that have FINISHED, so only the last run
//! is kept, and never the folder of a run that is still going: each run holds an
//! exclusive OS lock on a `.active` file in its folder for as long as it lives
//! (the OS drops it if the run dies), and a folder whose mark cannot be locked
//! is left alone. Within a run the folder is held under [`MAX_TOTAL_BYTES`]: when
//! a new picture would pass it the OLDEST picture is deleted, so the pictures of
//! the step that failed (the newest) are always the ones that survive. One
//! picture over [`MAX_PICTURE_BYTES`] is not kept. (Runs going at once each have
//! their own total.)
//!
//! # Who can read them
//!
//! A picture is of a screen the person is signed into. On Unix the folder is made
//! `0700` and each file `0600`, readable by their owner only. On Windows they sit
//! in the user's own profile folder (`%APPDATA%`), which other accounts cannot
//! read by default, and no permission is changed.
//!
//! # Never sent anywhere
//!
//! A picture is written to disk and its path returned. It is not sent to the
//! memory engine's OCR (or to anything), now or later: reading a screen with a
//! model is the job of the agent that is told the path, not of the replay.
//!
//! Called by `mcp::routine_tool`'s live runner, through
//! [`super::replay::Runner::look`].

use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::oslock::OsLock;

/// The most one picture may be, in bytes.
pub const MAX_PICTURE_BYTES: u64 = 4 * 1024 * 1024;

/// The most the pictures of one run may take, in bytes.
pub const MAX_TOTAL_BYTES: u64 = 48 * 1024 * 1024;

/// The mark in a run's folder that its OS lock is held on while the run lives.
const ACTIVE: &str = ".active";

/// Which of a step's two pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Before the step runs.
    Before,
    /// After it has run and the screen has had its moment to settle.
    After,
}

impl When {
    fn word(self) -> &'static str {
        match self {
            When::Before => "before",
            When::After => "after",
        }
    }
}

/// The folder of one run's pictures, as it fills.
#[derive(Debug)]
pub struct Pictures {
    dir: PathBuf,
    kept: VecDeque<(PathBuf, u64)>,
    total: u64,
    max_total: u64,
    /// Holds the OS lock that says this run is still going; the lock goes with
    /// the handle, when the run ends or dies.
    _active: OsLock,
}

impl Pictures {
    /// Start a run's pictures under `root`: a folder of its own for this run, and
    /// the folders of finished runs cleared (see the module header).
    ///
    /// # Errors
    ///
    /// The folder cannot be made or cleared. The replay goes on without pictures.
    pub fn begin(root: PathBuf) -> Result<Self, String> {
        Self::begin_within(root, MAX_TOTAL_BYTES, &run_id())
    }

    fn begin_within(root: PathBuf, max_total: u64, id: &str) -> Result<Self, String> {
        make_private_dir(&root)?;
        let dir = root.join(id);
        make_private_dir(&dir)?;
        let mut active = OsLock::new(
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(ACTIVE))
                .map_err(|e| format!("cannot mark {} as in use: {e}", dir.display()))?,
        );
        active
            .acquire_within(Duration::from_secs(1))
            .map_err(|_| format!("cannot mark {} as in use", dir.display()))?;
        clear_finished(&root, id)?;
        Ok(Self {
            dir,
            kept: VecDeque::new(),
            total: 0,
            max_total,
            _active: active,
        })
    }

    /// This run's folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many pictures are kept now.
    #[must_use]
    pub fn count(&self) -> usize {
        self.kept.len()
    }

    /// Keep `bytes` (an image of type `extension`) as the `when` picture of step
    /// `n` (1-based), deleting the oldest pictures if the run would pass its
    /// total. The path it is kept at.
    ///
    /// # Errors
    ///
    /// The picture is over [`MAX_PICTURE_BYTES`] or cannot be written.
    pub fn keep(
        &mut self,
        n: usize,
        when: When,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        let size = bytes.len() as u64;
        if size > MAX_PICTURE_BYTES {
            return Err(format!(
                "the picture is {size} bytes, over the {MAX_PICTURE_BYTES} one may be"
            ));
        }
        let path = self
            .dir
            .join(format!("step-{n:03}-{}.{extension}", when.word()));
        write_private(&path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        self.kept.push_back((path.clone(), size));
        self.total += size;
        while self.total > self.max_total && self.kept.len() > 1 {
            if let Some((old, old_size)) = self.kept.pop_front() {
                let _ = std::fs::remove_file(old);
                self.total -= old_size;
            }
        }
        Ok(path)
    }
}

/// A name for a run: the time it began (so the names sort oldest first), the
/// process, and a count so that two runs begun in the same millisecond differ.
fn run_id() -> String {
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!(
        "run-{ms:015}-{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::SeqCst)
    )
}

/// Delete what an earlier run left under `root`, except the folder `keep` and
/// the folder of any run still going. Only what this module makes is touched: a
/// folder named `run-*`, and (from an earlier layout) loose `step-*` pictures.
fn clear_finished(root: &Path, keep: &str) -> Result<(), String> {
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if name == keep {
            continue;
        }
        if is_loose_picture(&name) && path.is_file() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("cannot clear {}: {e}", path.display()))?;
        } else if name.starts_with("run-") && path.is_dir() && !is_going(&path) {
            // Best effort: a folder that will not go is left for the next run.
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

/// Whether the run whose folder is `dir` is still going: its mark is locked by
/// a handle that is still open. A folder with no mark is a finished (or never
/// started) run.
fn is_going(dir: &Path) -> bool {
    let Ok(mark) = File::options()
        .read(true)
        .write(true)
        .open(dir.join(ACTIVE))
    else {
        return false;
    };
    // Taking the lock for a moment (it goes when the file closes, at the end of
    // this) says nobody holds it. Cannot tell: do not delete what might be in use.
    OsLock::new(mark)
        .try_acquire()
        .map_or(true, |acquired| !acquired)
}

/// Whether `name` is a picture file of the earlier, flat layout.
fn is_loose_picture(name: &str) -> bool {
    name.starts_with("step-") && (name.ends_with(".jpg") || name.ends_with(".png"))
}

/// Make `dir` (and its parents), readable by its owner only on Unix.
fn make_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| format!("cannot make {}: {e}", dir.display()))
}

/// Write a file readable by its owner only (on Unix).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("incu-pictures-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    fn runs(root: &Path) -> Vec<String> {
        files(root)
            .into_iter()
            .filter(|n| n.starts_with("run-"))
            .collect()
    }

    #[test]
    fn a_step_has_a_picture_before_and_one_after_named_for_it() {
        let root = folder("names");
        let mut pictures = Pictures::begin(root.clone()).unwrap();
        let before = pictures.keep(2, When::Before, "jpg", b"b").unwrap();
        let after = pictures.keep(2, When::After, "jpg", b"a").unwrap();
        assert!(before.ends_with("step-002-before.jpg"), "{before:?}");
        assert!(after.ends_with("step-002-after.jpg"), "{after:?}");
        assert_eq!(std::fs::read(&before).unwrap(), b"b");
        assert_eq!(pictures.count(), 2);
        // In the run's own folder, under the root.
        assert_eq!(before.parent().unwrap(), pictures.dir());
        assert_eq!(pictures.dir().parent().unwrap(), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_last_run_is_kept_and_only_our_own_files_are_cleared() {
        let root = folder("last-run");
        let mut first = Pictures::begin(root.clone()).unwrap();
        first.keep(1, When::Before, "jpg", b"old").unwrap();
        let first_dir = first.dir().to_path_buf();
        drop(first);
        // An earlier layout's loose picture, and a file that is not ours.
        std::fs::write(root.join("step-099-after.jpg"), "older").unwrap();
        std::fs::write(root.join("notes.txt"), "mine").unwrap();
        std::fs::create_dir_all(root.join("my-folder")).unwrap();
        let second = Pictures::begin(root.clone()).unwrap();
        assert!(!first_dir.exists(), "the finished run's folder remains");
        assert_eq!(runs(&root).len(), 1);
        // This run's folder, and the two things that are not ours.
        assert_eq!(files(&root).len(), 3, "{:?}", files(&root));
        assert!(root.join("notes.txt").exists() && root.join("my-folder").exists());
        assert!(!root.join("step-099-after.jpg").exists());
        assert_eq!(second.count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two runs at once each have their own folder, and neither clears the other.
    #[test]
    fn a_run_that_is_still_going_is_never_deleted_by_another() {
        let root = folder("concurrent");
        let mut a = Pictures::begin(root.clone()).unwrap();
        let in_a = a.keep(1, When::Before, "jpg", b"a's").unwrap();
        let mut b = Pictures::begin(root.clone()).unwrap();
        assert_ne!(a.dir(), b.dir(), "two runs share a folder");
        assert!(in_a.exists(), "b's start cleared a run that is going");
        let in_b = b.keep(1, When::Before, "jpg", b"b's").unwrap();
        // Same step number, different runs, different files.
        assert_ne!(in_a, in_b);
        assert_eq!(std::fs::read(&in_a).unwrap(), b"a's");
        assert_eq!(std::fs::read(&in_b).unwrap(), b"b's");
        assert_eq!(runs(&root).len(), 2);
        // a finishes; b is still going: a third run clears a's and keeps b's.
        drop(a);
        let c = Pictures::begin(root.clone()).unwrap();
        assert!(!in_a.exists(), "a finished run was kept");
        assert!(in_b.exists(), "a run still going was deleted");
        assert_eq!(runs(&root).len(), 2, "b's and c's");
        drop(b);
        drop(c);
        let _d = Pictures::begin(root.clone()).unwrap();
        assert_eq!(runs(&root).len(), 1, "only the newest run is kept");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A run that died holds nothing: the OS dropped its mark.
    #[test]
    fn the_folder_of_a_run_that_died_is_cleared_by_the_next() {
        let root = folder("died");
        let dead = root.join("run-000000000000001-99999-0");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("step-001-after.jpg"), "x").unwrap();
        std::fs::write(dead.join(ACTIVE), "").unwrap();
        let _live = Pictures::begin(root.clone()).unwrap();
        assert!(!dead.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_runs_begun_at_once_get_two_names() {
        let ids: std::collections::BTreeSet<String> = (0..50).map(|_| run_id()).collect();
        assert_eq!(ids.len(), 50);
        assert!(ids.iter().all(|id| id.starts_with("run-")));
    }

    #[test]
    fn the_run_is_held_under_its_total_by_dropping_the_oldest_pictures() {
        let root = folder("bounded");
        let mut pictures = Pictures::begin_within(root.clone(), 10, "run-x").unwrap();
        for n in 1..=5 {
            pictures.keep(n, When::Before, "jpg", b"1234").unwrap();
            pictures.keep(n, When::After, "jpg", b"5678").unwrap();
        }
        let left: Vec<String> = files(pictures.dir())
            .into_iter()
            .filter(|n| n.starts_with("step-"))
            .collect();
        assert!(left.len() <= 2, "{left:?}");
        // What is left is the newest: the step that failed.
        assert_eq!(left, ["step-005-after.jpg", "step-005-before.jpg"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_picture_over_its_own_cap_is_refused_and_leaves_nothing() {
        let root = folder("huge");
        let mut pictures = Pictures::begin(root.clone()).unwrap();
        let huge = vec![0u8; MAX_PICTURE_BYTES as usize + 1];
        assert!(pictures.keep(1, When::After, "jpg", &huge).is_err());
        assert!(!files(pictures.dir()).iter().any(|n| n.starts_with("step-")));
        assert_eq!(pictures.count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_that_cannot_be_made_is_an_error_not_a_panic() {
        let root = folder("blocked");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("a-file");
        std::fs::write(&file, "x").unwrap();
        // A folder under a file cannot exist.
        assert!(Pictures::begin(file.join("pictures")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// On Unix a picture of a signed-in screen is for its owner only.
    #[cfg(unix)]
    #[test]
    fn the_folder_and_its_pictures_are_private_to_their_owner() {
        use std::os::unix::fs::PermissionsExt;
        let root = folder("private");
        let mut pictures = Pictures::begin(root.clone()).unwrap();
        let shot = pictures.keep(1, When::After, "jpg", b"x").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&shot), 0o600);
        assert_eq!(mode(pictures.dir()), 0o700);
        assert_eq!(mode(&root), 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }
}
