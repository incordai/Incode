//! The pictures of the last replay: one before and one after each step.
//!
//! # What and why
//!
//! A replay that fails is only useful if the agent that redoes the task, and the
//! person reading over its shoulder, can see what the screen looked like. So
//! each step of `routine_run` gets one picture before it and one after it, taken
//! with incu's own capture (the window the step is for, else the screen), and a
//! failure names the failed step's two pictures by path. There is no video and no
//! ffmpeg: a step is a still.
//!
//! # Kept small, last run only
//!
//! The pictures are files in one folder beside the routines file
//! ([`super::store::Store::pictures_dir`]). The next run clears the folder before
//! it starts, so only the last run is kept. Within a run the folder is held under
//! [`MAX_TOTAL_BYTES`]: when a new picture would pass it the OLDEST picture is
//! deleted, so the pictures of the step that failed (the newest) are always the
//! ones that survive. One picture over [`MAX_PICTURE_BYTES`] is not kept.
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
use std::path::{Path, PathBuf};

/// The most one picture may be, in bytes.
pub const MAX_PICTURE_BYTES: u64 = 4 * 1024 * 1024;

/// The most the pictures of one run may take, in bytes.
pub const MAX_TOTAL_BYTES: u64 = 48 * 1024 * 1024;

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
}

impl Pictures {
    /// Start a run's pictures in `dir`: make it, and clear what the last run left
    /// there (only the files this module names: anything else in the folder is
    /// not touched).
    ///
    /// # Errors
    ///
    /// The folder cannot be made or cleared. The replay goes on without pictures.
    pub fn begin(dir: PathBuf) -> Result<Self, String> {
        Self::begin_within(dir, MAX_TOTAL_BYTES)
    }

    fn begin_within(dir: PathBuf, max_total: u64) -> Result<Self, String> {
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_ours(&name) {
                std::fs::remove_file(entry.path())
                    .map_err(|e| format!("cannot clear {}: {e}", entry.path().display()))?;
            }
        }
        Ok(Self {
            dir,
            kept: VecDeque::new(),
            total: 0,
            max_total,
        })
    }

    /// The folder.
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
        std::fs::write(&path, bytes)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
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

/// Whether `name` is a picture file this module made.
fn is_ours(name: &str) -> bool {
    name.starts_with("step-") && (name.ends_with(".jpg") || name.ends_with(".png"))
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

    #[test]
    fn a_step_has_a_picture_before_and_one_after_named_for_it() {
        let dir = folder("names");
        let mut pictures = Pictures::begin(dir.clone()).unwrap();
        let before = pictures.keep(2, When::Before, "jpg", b"b").unwrap();
        let after = pictures.keep(2, When::After, "jpg", b"a").unwrap();
        assert!(before.ends_with("step-002-before.jpg"), "{before:?}");
        assert!(after.ends_with("step-002-after.jpg"), "{after:?}");
        assert_eq!(std::fs::read(&before).unwrap(), b"b");
        assert_eq!(pictures.count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_last_run_is_kept_and_only_our_own_files_are_cleared() {
        let dir = folder("last-run");
        let mut first = Pictures::begin(dir.clone()).unwrap();
        first.keep(1, When::Before, "jpg", b"old").unwrap();
        first.keep(7, When::After, "png", b"old").unwrap();
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        let second = Pictures::begin(dir.clone()).unwrap();
        assert_eq!(
            files(&dir),
            ["notes.txt"],
            "the old run is gone, the note is not"
        );
        assert_eq!(second.count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_run_is_held_under_its_total_by_dropping_the_oldest_pictures() {
        let dir = folder("bounded");
        let mut pictures = Pictures::begin_within(dir.clone(), 10).unwrap();
        for n in 1..=5 {
            pictures.keep(n, When::Before, "jpg", b"1234").unwrap();
            pictures.keep(n, When::After, "jpg", b"5678").unwrap();
        }
        let left = files(&dir);
        assert!(left.len() <= 2, "{left:?}");
        // What is left is the newest: the step that failed.
        assert_eq!(left, ["step-005-after.jpg", "step-005-before.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_picture_over_its_own_cap_is_refused_and_leaves_nothing() {
        let dir = folder("huge");
        let mut pictures = Pictures::begin(dir.clone()).unwrap();
        let huge = vec![0u8; MAX_PICTURE_BYTES as usize + 1];
        assert!(pictures.keep(1, When::After, "jpg", &huge).is_err());
        assert!(files(&dir).is_empty());
        assert_eq!(pictures.count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_cannot_be_made_is_an_error_not_a_panic() {
        let dir = folder("blocked");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        // A folder under a file cannot exist.
        assert!(Pictures::begin(file.join("pictures")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
