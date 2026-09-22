//! Checkpoints — putting files back the way they were.
//!
//! An agent that edits files will sometimes edit them wrongly, and until now
//! the only recovery in this harness was whatever backup the user had thought
//! to make by hand. (The stray `.backups/` directories sitting inside `src/`
//! are what that looks like in practice.)
//!
//! A checkpoint records what a file looked like *before* a tool changed it.
//! Undoing one puts every file it covers back.
//!
//! # What this deliberately is not
//!
//! Snapshots are held in memory for the life of the session, not written to
//! disk. The problem being solved is "the agent just made a bad edit run and I
//! want it back", which is answerable in-session. Persisting them would mean
//! deciding where they live, when they are deleted and how much disk they may
//! use — and getting that wrong leaves junk scattered through the user's repo,
//! which is exactly the mess this module exists to replace.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Largest file that will be snapshotted, per file.
///
/// A cap is needed because snapshots are held in memory, but a file over it is
/// never silently skipped — it is recorded as [`FileSnapshot::Unrecoverable`]
/// and reported by the undo.
const MAX_SNAPSHOT_BYTES: usize = 5_000_000;

/// How many checkpoints are kept. Older ones are dropped as new ones arrive.
const MAX_CHECKPOINTS: usize = 20;

/// What a single file looked like before it was touched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileSnapshot {
    /// The file existed, with these contents.
    Existed(Vec<u8>),
    /// The file did not exist. Undoing means deleting it again — without this
    /// case, undoing a newly created file would leave it behind.
    Absent,
    /// The file was too large to hold, or could not be read. Recorded rather
    /// than skipped: an undo that silently leaves a file modified is worse
    /// than one that says which files it could not restore.
    Unrecoverable(String),
}

/// One restorable point, covering every file changed since it was opened.
#[derive(Debug, Clone)]
struct Checkpoint {
    label: String,
    /// First-seen state per file. Only the first snapshot of a file counts:
    /// undoing must return to how things were when the checkpoint opened, not
    /// to the state between two edits of the same file.
    files: Vec<(PathBuf, FileSnapshot)>,
}

/// What an undo actually managed to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
    pub label: String,
    /// Files put back, as workspace-relative display paths.
    pub restored: Vec<String>,
    /// Files that could not be put back, each with the reason. Never empty
    /// silently — the caller is expected to show these.
    pub failed: Vec<String>,
}

impl UndoOutcome {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// The stack of checkpoints for one session.
#[derive(Debug, Default)]
pub struct CheckpointStore {
    checkpoints: Vec<Checkpoint>,
}

impl CheckpointStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new checkpoint. Everything changed from now on belongs to it.
    pub fn begin(&mut self, label: impl Into<String>) {
        self.checkpoints.push(Checkpoint {
            label: label.into(),
            files: Vec::new(),
        });
        if self.checkpoints.len() > MAX_CHECKPOINTS {
            self.checkpoints.remove(0);
        }
    }

    /// Whether anything is currently undoable.
    #[must_use]
    pub fn has_undo(&self) -> bool {
        self.checkpoints.iter().any(|c| !c.files.is_empty())
    }

    /// Label of the most recent checkpoint that changed anything.
    #[must_use]
    pub fn pending_label(&self) -> Option<&str> {
        self.checkpoints
            .iter()
            .rev()
            .find(|c| !c.files.is_empty())
            .map(|c| c.label.as_str())
    }

    /// Record what `path` looks like now, before something changes it.
    ///
    /// Called for its side effect only; a failure to snapshot must not stop the
    /// edit, so the reason is stored in the checkpoint instead of returned.
    pub fn snapshot(&mut self, path: &Path) {
        // No open checkpoint means nothing asked for undo — recording anyway
        // would grow forever for no one.
        let Some(checkpoint) = self.checkpoints.last_mut() else {
            return;
        };

        // Only the FIRST state of a file in this checkpoint is kept.
        if checkpoint.files.iter().any(|(known, _)| known == path) {
            return;
        }

        let snapshot = match std::fs::metadata(path) {
            Err(_) => FileSnapshot::Absent,
            Ok(meta) if meta.len() as usize > MAX_SNAPSHOT_BYTES => FileSnapshot::Unrecoverable(
                format!("larger than the {MAX_SNAPSHOT_BYTES}-byte snapshot limit"),
            ),
            Ok(_) => match std::fs::read(path) {
                Ok(contents) => FileSnapshot::Existed(contents),
                Err(e) => FileSnapshot::Unrecoverable(format!("could not be read: {e}")),
            },
        };

        checkpoint.files.push((path.to_path_buf(), snapshot));
    }

    /// Put back everything covered by the most recent checkpoint that changed
    /// something, and drop it.
    pub fn undo_last(&mut self, workspace: &Path) -> Result<UndoOutcome> {
        let index = self
            .checkpoints
            .iter()
            .rposition(|c| !c.files.is_empty())
            .context("there is nothing to undo")?;
        let checkpoint = self.checkpoints.remove(index);

        let mut restored = Vec::new();
        let mut failed = Vec::new();

        for (path, snapshot) in &checkpoint.files {
            let shown = display_path(workspace, path);
            match snapshot {
                // A file that already matches its snapshot is LEFT ALONE.
                //
                // Writing it back would be a no-op in content and not a no-op
                // in fact: it changes the mtime, so every build downstream
                // rebuilds, and it would clobber an edit made in another editor
                // while the turn ran. It would also report the file as
                // "restored" when nothing about it was undone.
                //
                // This matters because the snapshot now covers the WHOLE tree
                // rather than the handful of files a tool was about to write —
                // the delegated agent's writes cannot be intercepted, so
                // everything is captured up front and most of it never changes.
                FileSnapshot::Existed(contents) => {
                    if std::fs::read(path).is_ok_and(|now| &now == contents) {
                        continue;
                    }
                    match restore_file(path, contents) {
                        Ok(()) => restored.push(shown),
                        Err(e) => failed.push(format!("{shown}: {e}")),
                    }
                }
                FileSnapshot::Absent => {
                    if !path.exists() {
                        continue;
                    }
                    match remove_file(path) {
                        Ok(()) => restored.push(format!("{shown} (removed)")),
                        Err(e) => failed.push(format!("{shown}: {e}")),
                    }
                }
                FileSnapshot::Unrecoverable(reason) => {
                    failed.push(format!("{shown}: {reason}"));
                }
            }
        }

        Ok(UndoOutcome {
            label: checkpoint.label,
            restored,
            failed,
        })
    }
}

/// Write a file back, recreating the directory if the edit removed it.
fn restore_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not recreate {}", parent.display()))?;
        }
    }
    std::fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))
}

/// Remove a file that did not exist when the checkpoint opened.
fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        // Already gone is the state we wanted, so it is not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!("could not remove {}: {e}", path.display())),
    }
}

/// Show a path relative to the workspace when possible, so output is readable.
fn display_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .display()
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    /// **An untouched file is not rewritten.** The snapshot now covers the
    /// whole tree, so most captured files never change. Writing them back would
    /// change every mtime (rebuilding everything), could clobber an edit made
    /// in another editor while the turn ran, and would report files as
    /// "restored" that nothing had undone.
    #[test]
    fn undo_leaves_files_that_did_not_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let touched = dir.path().join("changed.txt");
        let untouched = dir.path().join("same.txt");
        std::fs::write(&touched, "before").expect("write");
        std::fs::write(&untouched, "constant").expect("write");

        let mut store = CheckpointStore::new();
        store.begin("a turn");
        store.snapshot(&touched);
        store.snapshot(&untouched);

        // Only one of them actually moves.
        std::fs::write(&touched, "after").expect("write");
        let before_mtime = std::fs::metadata(&untouched)
            .and_then(|m| m.modified())
            .expect("mtime");

        let outcome = store.undo_last(dir.path()).expect("undo");

        assert_eq!(std::fs::read_to_string(&touched).unwrap(), "before");
        assert_eq!(
            outcome.restored.len(),
            1,
            "only the changed file was undone, got: {:?}",
            outcome.restored
        );
        let after_mtime = std::fs::metadata(&untouched)
            .and_then(|m| m.modified())
            .expect("mtime");
        assert_eq!(
            before_mtime, after_mtime,
            "an untouched file must not be rewritten"
        );
    }

    /// A file that did not exist and still does not is not "removed" either.
    #[test]
    fn undo_does_not_report_removing_what_was_never_created() {
        let dir = tempfile::tempdir().expect("temp dir");
        let never = dir.path().join("never.txt");

        let mut store = CheckpointStore::new();
        store.begin("a turn");
        store.snapshot(&never);
        // Something else in the same turn changed, so there IS a checkpoint.
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "before").expect("write");
        store.snapshot(&real);
        std::fs::write(&real, "after").expect("write");

        let outcome = store.undo_last(dir.path()).expect("undo");
        assert!(
            !outcome.restored.iter().any(|r| r.contains("never")),
            "got: {:?}",
            outcome.restored
        );
    }
    use super::*;

    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "incord-code-checkpoint-{label}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).expect("workspace must be creatable");
            Self { root }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.path(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent must be creatable");
            }
            std::fs::write(&path, contents).expect("fixture must be writable");
            path
        }

        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.path(name)).expect("file must be readable")
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            for attempt in 0..5 {
                if std::fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1)));
            }
        }
    }

    /// The whole point: a bad edit is reversible.
    #[test]
    fn undo_restores_a_modified_file() {
        let workspace = Workspace::new("modify");
        let path = workspace.write("a.txt", "ORIGINAL");

        let mut store = CheckpointStore::new();
        store.begin("turn 1");
        store.snapshot(&path);
        std::fs::write(&path, "RUINED").unwrap();

        let outcome = store.undo_last(&workspace.root).expect("undo must succeed");

        assert_eq!(workspace.read("a.txt"), "ORIGINAL");
        assert!(outcome.is_complete(), "failed: {:?}", outcome.failed);
        assert_eq!(outcome.restored, vec!["a.txt".to_string()]);
    }

    /// A file the agent CREATED has no previous contents, so undoing it means
    /// deleting it. Without the `Absent` case it would survive the undo and
    /// leave the workspace dirty in a way the user did not ask for.
    #[test]
    fn undo_deletes_a_file_that_did_not_exist_before() {
        let workspace = Workspace::new("create");
        let path = workspace.path("new.txt");

        let mut store = CheckpointStore::new();
        store.begin("turn 1");
        store.snapshot(&path); // taken BEFORE creation, so: Absent
        std::fs::write(&path, "brand new").unwrap();
        assert!(path.exists(), "the fixture must actually create the file");

        let outcome = store.undo_last(&workspace.root).expect("undo must succeed");

        assert!(!path.exists(), "a file created in the turn must be removed");
        assert!(outcome.is_complete(), "failed: {:?}", outcome.failed);
    }

    /// Undo must return to how things were when the checkpoint OPENED, not to
    /// the state between two edits. Keeping the later snapshot would restore
    /// the file to a version the user never had.
    #[test]
    fn only_the_first_state_of_a_file_in_a_checkpoint_is_kept() {
        let workspace = Workspace::new("first");
        let path = workspace.write("a.txt", "V1");

        let mut store = CheckpointStore::new();
        store.begin("turn 1");
        store.snapshot(&path);
        std::fs::write(&path, "V2").unwrap();
        store.snapshot(&path); // second edit in the same turn
        std::fs::write(&path, "V3").unwrap();

        store.undo_last(&workspace.root).expect("undo must succeed");

        assert_eq!(
            workspace.read("a.txt"),
            "V1",
            "undo must reach the state at the start of the checkpoint, not the middle"
        );
    }

    /// Each checkpoint is its own step, and undoing one must not disturb what
    /// an earlier one covered.
    #[test]
    fn undo_steps_back_one_checkpoint_at_a_time() {
        let workspace = Workspace::new("stack");
        let path = workspace.write("a.txt", "V1");

        let mut store = CheckpointStore::new();
        store.begin("turn 1");
        store.snapshot(&path);
        std::fs::write(&path, "V2").unwrap();

        store.begin("turn 2");
        store.snapshot(&path);
        std::fs::write(&path, "V3").unwrap();

        let second = store.undo_last(&workspace.root).expect("first undo");
        assert_eq!(second.label, "turn 2");
        assert_eq!(
            workspace.read("a.txt"),
            "V2",
            "must step back one turn only"
        );

        let first = store.undo_last(&workspace.root).expect("second undo");
        assert_eq!(first.label, "turn 1");
        assert_eq!(workspace.read("a.txt"), "V1");
    }

    /// Undo covers every file the turn touched, not just the last one.
    #[test]
    fn undo_restores_every_file_in_the_checkpoint() {
        let workspace = Workspace::new("many");
        let a = workspace.write("a.txt", "A1");
        let b = workspace.write("nested/b.txt", "B1");

        let mut store = CheckpointStore::new();
        store.begin("turn 1");
        store.snapshot(&a);
        store.snapshot(&b);
        std::fs::write(&a, "A2").unwrap();
        std::fs::write(&b, "B2").unwrap();

        let outcome = store.undo_last(&workspace.root).expect("undo must succeed");

        assert_eq!(workspace.read("a.txt"), "A1");
        assert_eq!(workspace.read("nested/b.txt"), "B1");
        assert_eq!(outcome.restored.len(), 2, "got: {:?}", outcome.restored);
    }

    /// A file too large to hold must be REPORTED, never quietly dropped. An
    /// undo that claims success while leaving a file modified is worse than no
    /// undo at all, because the user stops looking.
    #[test]
    fn a_file_too_large_to_snapshot_is_reported_not_skipped() {
        let workspace = Workspace::new("toolarge");
        let path = workspace.write("big.txt", &"x".repeat(MAX_SNAPSHOT_BYTES + 1));

        let mut store = CheckpointStore::new();
        store.begin("turn 1");
        store.snapshot(&path);
        std::fs::write(&path, "clobbered").unwrap();

        let outcome = store.undo_last(&workspace.root).expect("undo must return");

        assert!(
            !outcome.is_complete(),
            "an unrestorable file must make the undo incomplete"
        );
        assert_eq!(outcome.failed.len(), 1, "got: {:?}", outcome.failed);
        assert!(
            outcome.failed[0].contains("big.txt") && outcome.failed[0].contains("limit"),
            "the report must name the file and the reason; got: {:?}",
            outcome.failed
        );
    }

    /// Undoing with nothing to undo is an error the caller can show, not a
    /// silent success that looks like it did something.
    #[test]
    fn undoing_nothing_is_an_error() {
        let workspace = Workspace::new("empty");
        let mut store = CheckpointStore::new();

        assert!(!store.has_undo());
        assert!(store.undo_last(&workspace.root).is_err());

        // A checkpoint that changed nothing is still nothing to undo.
        store.begin("turn 1");
        assert!(!store.has_undo(), "an untouched checkpoint is not undoable");
        assert!(store.undo_last(&workspace.root).is_err());
    }

    /// Snapshots taken with no checkpoint open would accumulate for a caller
    /// that never intends to undo.
    #[test]
    fn nothing_is_recorded_before_a_checkpoint_is_opened() {
        let workspace = Workspace::new("noopen");
        let path = workspace.write("a.txt", "V1");

        let mut store = CheckpointStore::new();
        store.snapshot(&path);

        assert!(!store.has_undo(), "no checkpoint means nothing is recorded");
    }

    /// The stack is bounded, or a long session holds every version of every
    /// file it ever touched.
    #[test]
    fn the_checkpoint_stack_is_bounded() {
        let workspace = Workspace::new("bounded");
        let path = workspace.write("a.txt", "V0");
        let mut store = CheckpointStore::new();

        for turn in 0..(MAX_CHECKPOINTS + 5) {
            store.begin(format!("turn {turn}"));
            store.snapshot(&path);
            std::fs::write(&path, format!("V{turn}")).unwrap();
        }

        assert_eq!(
            store.checkpoints.len(),
            MAX_CHECKPOINTS,
            "the stack must stop growing at {MAX_CHECKPOINTS}"
        );
        assert_eq!(
            store.pending_label(),
            Some(format!("turn {}", MAX_CHECKPOINTS + 4).as_str()),
            "the newest checkpoint must be the one kept"
        );
    }

    #[test]
    fn binary_files_can_be_snapshotted_and_restored() {
        let workspace = Workspace::new("binary");
        // Non-UTF8 invalid bytes: 0xFF, 0xFE, 0x00, 0x01
        let original_bytes: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, 0x80, 0x90, 0xAA];
        let file_path = workspace.root.join("data.bin");
        std::fs::write(&file_path, &original_bytes).unwrap();

        let mut store = CheckpointStore::new();
        store.begin("binary edit");
        store.snapshot(&file_path);

        // Overwrite with different binary bytes
        let modified_bytes: Vec<u8> = vec![0x00, 0x11, 0x22, 0x33];
        std::fs::write(&file_path, &modified_bytes).unwrap();

        // Undo
        let outcome = store.undo_last(&workspace.root).expect("undo succeeds");
        assert!(outcome.is_complete());
        assert_eq!(outcome.restored.len(), 1);

        // Verify restored content matches exact binary bytes
        let restored = std::fs::read(&file_path).unwrap();
        assert_eq!(restored, original_bytes);
    }
}
