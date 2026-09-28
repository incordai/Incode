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
//! # One turn, from open to sealed
//!
//! A turn opens a checkpoint over the whole tree ([`CheckpointStore::begin_tree`])
//! — the agent's writes cannot be intercepted as they happen, so everything is
//! captured up front. When the turn ends it is SEALED
//! ([`CheckpointStore::finish_turn`]): the tree is looked at again, every file
//! that did not change is dropped, and every file and directory the turn
//! CREATED is added — a created file has no "before" in the snapshot, so it
//! used to survive an undo. Each kept file also records what it held when the
//! turn ended, so an undo later can tell a file the turn left from one a
//! person has edited since, and leaves the second alone.
//!
//! # Kept on disk, bounded
//!
//! A store made with [`CheckpointStore::persisted`] writes each sealed turn to
//! `.incode/checkpoints/<session>/<turn>/` — a manifest and the "before" of
//! each changed file — so `/undo` and `/rewind` work after incode restarts.
//! Every session in a workspace shares one stack, ordered by when each turn
//! ended: a new session's first `/undo` undoes the last turn of the session
//! before it. The stack keeps the newest [`MAX_CHECKPOINTS`] turns and at most
//! [`MAX_STORED_BYTES`] of file contents; the oldest go first. A turn whose
//! contents alone are over that cap is kept in memory only, and said.
//!
//! A manifest is data from the workspace, so every path in it is checked to
//! lie inside the session's roots before anything is written or removed.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Largest file that will be snapshotted, per file.
///
/// A cap is needed because snapshots are held in memory, but a file over it is
/// never silently skipped — it is recorded as [`FileSnapshot::Unrecoverable`]
/// and reported by the undo when it changed.
const MAX_SNAPSHOT_BYTES: usize = 5_000_000;

/// How many checkpoints are kept, in memory and on disk. Older ones are
/// dropped as new ones arrive.
pub const MAX_CHECKPOINTS: usize = 20;

/// How many files a whole-tree snapshot will hold.
///
/// Bounds the ACTION — the snapshot — and never the report: a tree over this
/// is told that undo is off for the turn, not quietly given half of one.
pub const MAX_SNAPSHOT_FILES: usize = 5_000;

/// How many files a turn may be found to have CREATED before the list is
/// cut. An undo past the cut says how many it could not name.
pub const MAX_CREATED_FILES: usize = 5_000;

/// The most file contents the stack keeps on disk, across every session in
/// the workspace.
pub const MAX_STORED_BYTES: u64 = 256 * 1024 * 1024;

/// Directories that never hold hand-written source and are enormous.
///
/// `.incode` is incode's own state (the checkpoints themselves live there);
/// `.incord` is the Incord Memory engine's, which is not the project's source
/// either.
pub const SKIP_DIRS: [&str; 6] = [
    "target",
    "node_modules",
    "dist",
    ".git",
    ".incode",
    ".incord",
];

/// Where the stack is kept, under the workspace.
pub const CHECKPOINTS_DIR: &str = ".incode/checkpoints";

/// The file naming one stored turn; written last, so a turn without one is
/// an interrupted write and is ignored.
const MANIFEST: &str = "manifest.json";

/// The largest manifest read back.
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

/// The manifest format this build writes and reads.
const FORMAT: u32 = 1;

/// What a file holds, in two numbers: its length and a hash of its bytes.
/// Enough to tell whether it changed; not a security check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mark {
    len: u64,
    hash: u64,
}

impl Mark {
    fn of_bytes(bytes: &[u8]) -> Self {
        let mut hash = Fnv::new();
        hash.write(bytes);
        Self {
            len: bytes.len() as u64,
            hash: hash.finish(),
        }
    }

    /// The mark of the file at `path`, read in pieces; `None` when there is
    /// no file there to read.
    fn of_file(path: &Path) -> Option<Self> {
        let mut file = std::fs::File::open(path).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut hash = Fnv::new();
        let mut len = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            hash.write(&buf[..n]);
            len += n as u64;
        }
        Some(Self {
            len,
            hash: hash.finish(),
        })
    }
}

/// FNV-1a, 64 bits: small, stable across builds and platforms.
struct Fnv(u64);

impl Fnv {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    const fn finish(&self) -> u64 {
        self.0
    }
}

/// What a single file looked like before it was touched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileSnapshot {
    /// The file existed, with these contents.
    Existed(Vec<u8>),
    /// The file existed; its contents are kept in this file on disk.
    Stored(PathBuf),
    /// The file did not exist. Undoing means deleting it again — without this
    /// case, undoing a newly created file would leave it behind.
    Absent,
    /// The file was too large to hold, or could not be read. Recorded rather
    /// than skipped: an undo that silently leaves a file modified is worse
    /// than one that says which files it could not restore. `seen` is its
    /// length and time when it was looked at, so an untouched one is dropped
    /// at the seal rather than reported.
    Unrecoverable {
        reason: String,
        seen: Option<(u64, Option<std::time::SystemTime>)>,
    },
}

/// What a file held when its turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Present(Mark),
    Gone,
}

impl After {
    fn now(path: &Path) -> Self {
        Mark::of_file(path).map_or(Self::Gone, Self::Present)
    }
}

/// One file a checkpoint covers.
#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    before: FileSnapshot,
    /// Set when the turn is sealed.
    after: Option<After>,
}

/// The tree a checkpoint was opened over, kept until the turn is sealed.
#[derive(Debug, Clone)]
struct Walk {
    roots: Vec<PathBuf>,
    /// Every directory that existed when the turn began.
    dirs: HashSet<PathBuf>,
}

/// One restorable point, covering every file changed since it was opened.
#[derive(Debug, Clone)]
struct Checkpoint {
    label: String,
    /// First-seen state per file. Only the first snapshot of a file counts:
    /// undoing must return to how things were when the checkpoint opened, not
    /// to the state between two edits of the same file.
    files: Vec<Entry>,
    walk: Option<Walk>,
    /// False when the whole-tree walk hit its cap: undo is refused then.
    complete: bool,
    /// More files were created than [`MAX_CREATED_FILES`].
    created_cut: bool,
    sealed: bool,
    /// Directories the turn created, removed on undo when empty.
    created_dirs: Vec<PathBuf>,
    /// Where it is kept on disk, once it is.
    stored_at: Option<PathBuf>,
    /// The file contents it keeps on disk, in bytes.
    stored_bytes: u64,
}

impl Checkpoint {
    fn opened(label: String) -> Self {
        Self {
            label,
            files: Vec::new(),
            walk: None,
            complete: true,
            created_cut: false,
            sealed: false,
            created_dirs: Vec::new(),
            stored_at: None,
            stored_bytes: 0,
        }
    }

    /// Whether undoing it would do — or refuse — anything.
    fn undoable(&self) -> bool {
        !self.files.is_empty() || !self.created_dirs.is_empty() || !self.complete
    }
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

/// What the newest checkpoint holds for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded<'a> {
    /// Its contents when the checkpoint first saw it.
    Contents(&'a [u8]),
    /// It did not exist then.
    Absent,
    /// It was seen and could not be held: too large, or unreadable.
    Unheld,
    /// The checkpoint never saw it — which is NOT the same as absent: a
    /// snapshot that stopped early, or skipped the directory, never looked.
    NotSeen,
}

/// Where a persisted store writes, and what it has written.
#[derive(Debug)]
struct Disk {
    /// `<workspace>/.incode/checkpoints`.
    root: PathBuf,
    /// This session's directory name under `root`.
    session: String,
    /// Paths in a manifest must lie under one of these.
    roots: Vec<PathBuf>,
    /// The next turn's number in this session.
    next: u64,
    /// The most file contents kept on disk: [`MAX_STORED_BYTES`], less in a
    /// test.
    cap: u64,
}

/// The stack of checkpoints for one session.
#[derive(Debug, Default)]
pub struct CheckpointStore {
    checkpoints: Vec<Checkpoint>,
    disk: Option<Disk>,
}

impl CheckpointStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store kept under `<workspace>/.incode/checkpoints/<session>/`, that
    /// starts with the turns every earlier session in the workspace kept.
    /// `roots` are the directories the session works in; a stored path
    /// outside them is not trusted, and its turn is left out.
    #[must_use]
    pub fn persisted(workspace: &Path, roots: &[PathBuf], session: &str) -> Self {
        let disk = Disk {
            root: workspace.join(CHECKPOINTS_DIR),
            session: safe_name(session),
            roots: roots.to_vec(),
            next: 1,
            cap: MAX_STORED_BYTES,
        };
        let checkpoints = load_stored(&disk);
        crate::trace!(
            "checkpoint",
            "{} stored turn(s) found under {}",
            checkpoints.len(),
            disk.root.display()
        );
        Self {
            checkpoints,
            disk: Some(disk),
        }
    }

    /// The same store, keeping at most `cap` bytes of contents on disk.
    #[cfg(test)]
    fn capped(mut self, cap: u64) -> Self {
        if let Some(disk) = self.disk.as_mut() {
            disk.cap = cap;
        }
        self
    }

    /// Open a new checkpoint. Everything changed from now on belongs to it;
    /// one still open is sealed first, so it keeps what its turn left.
    pub fn begin(&mut self, label: impl Into<String>) {
        if let Some(open) = self.checkpoints.last_mut() {
            seal(open);
        }
        self.checkpoints.push(Checkpoint::opened(label.into()));
        if self.checkpoints.len() > MAX_CHECKPOINTS {
            let dropped = self.checkpoints.remove(0);
            forget_stored(&dropped);
        }
    }

    /// Open a checkpoint over the whole of `roots`, snapshotting every file
    /// outside [`SKIP_DIRS`]. Returns whether the walk covered everything; a
    /// walk that hit [`MAX_SNAPSHOT_FILES`] makes the turn not undoable.
    pub fn begin_tree(&mut self, label: impl Into<String>, roots: &[PathBuf]) -> bool {
        self.begin(label);
        let (files, dirs, complete) = walk_tree(roots, MAX_SNAPSHOT_FILES);
        for file in &files {
            self.snapshot(file);
        }
        if let Some(open) = self.checkpoints.last_mut() {
            open.complete = complete;
            open.walk = Some(Walk {
                roots: roots.to_vec(),
                dirs,
            });
        }
        complete
    }

    /// Whether anything is currently undoable.
    #[must_use]
    pub fn has_undo(&self) -> bool {
        self.checkpoints.iter().any(Checkpoint::undoable)
    }

    /// Label of the most recent checkpoint that changed anything.
    #[must_use]
    pub fn pending_label(&self) -> Option<&str> {
        self.checkpoints
            .iter()
            .rev()
            .find(|c| c.undoable())
            .map(|c| c.label.as_str())
    }

    /// What the newest checkpoint recorded for `path`, matched exactly as
    /// [`Self::snapshot`] stored it.
    ///
    /// The newest, because it is the one the running turn opened: a turn's
    /// "before" is the state that turn began from.
    #[must_use]
    pub fn recorded(&self, path: &Path) -> Recorded<'_> {
        let Some(checkpoint) = self.checkpoints.last() else {
            return Recorded::NotSeen;
        };
        match checkpoint.files.iter().find(|entry| entry.path == path) {
            Some(Entry {
                before: FileSnapshot::Existed(contents),
                ..
            }) => Recorded::Contents(contents),
            Some(Entry {
                before: FileSnapshot::Absent,
                ..
            }) => Recorded::Absent,
            Some(_) => Recorded::Unheld,
            None => Recorded::NotSeen,
        }
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
        if checkpoint.sealed {
            return;
        }
        // Only the FIRST state of a file in this checkpoint is kept.
        if checkpoint.files.iter().any(|entry| entry.path == path) {
            return;
        }
        let before = match std::fs::metadata(path) {
            Err(_) => FileSnapshot::Absent,
            Ok(meta) if meta.len() as usize > MAX_SNAPSHOT_BYTES => FileSnapshot::Unrecoverable {
                reason: format!("larger than the {MAX_SNAPSHOT_BYTES}-byte snapshot limit"),
                seen: Some((meta.len(), meta.modified().ok())),
            },
            Ok(_) => match std::fs::read(path) {
                Ok(contents) => FileSnapshot::Existed(contents),
                Err(e) => FileSnapshot::Unrecoverable {
                    reason: format!("could not be read: {e}"),
                    seen: None,
                },
            },
        };
        checkpoint.files.push(Entry {
            path: path.to_path_buf(),
            before,
            after: None,
        });
    }

    /// The turn is over: seal its checkpoint — keep only what changed, add
    /// what it created — and, for a persisted store, write it to disk and
    /// prune the stack to its bounds.
    pub fn finish_turn(&mut self) {
        let Some(open) = self.checkpoints.last_mut() else {
            return;
        };
        if open.sealed {
            return;
        }
        seal(open);
        if !open.undoable() {
            self.checkpoints.pop();
            return;
        }
        self.persist_newest();
    }

    /// Write the newest (sealed) checkpoint to disk, then prune.
    fn persist_newest(&mut self) {
        let Some(disk) = self.disk.as_mut() else {
            return;
        };
        let Some(newest) = self.checkpoints.last_mut() else {
            return;
        };
        if newest.stored_at.is_some() {
            return;
        }
        let seq = disk.next;
        disk.next += 1;
        match store(disk, seq, newest) {
            Ok(()) => {}
            Err(why) => crate::trace!(
                "checkpoint",
                "turn `{}` kept in memory only: {why}",
                newest.label
            ),
        }
        self.prune();
    }

    /// Keep the newest [`MAX_CHECKPOINTS`] stored turns within
    /// [`MAX_STORED_BYTES`]; the rest are removed from disk and from the stack.
    fn prune(&mut self) {
        let cap = self.disk.as_ref().map_or(MAX_STORED_BYTES, |d| d.cap);
        let mut total = 0u64;
        let mut count = 0usize;
        let mut keep = vec![true; self.checkpoints.len()];
        for (i, cp) in self.checkpoints.iter().enumerate().rev() {
            if cp.stored_at.is_none() {
                continue;
            }
            count += 1;
            total = total.saturating_add(cp.stored_bytes);
            if count > MAX_CHECKPOINTS || total > cap {
                keep[i] = false;
            }
        }
        let mut index = 0;
        self.checkpoints.retain(|cp| {
            let kept = keep[index];
            index += 1;
            if !kept {
                crate::trace!(
                    "checkpoint",
                    "dropped the stored turn `{}` to stay in bounds",
                    cp.label
                );
                forget_stored(cp);
            }
            kept
        });
    }

    /// Put back everything covered by the most recent checkpoint that changed
    /// something, and drop it.
    pub fn undo_last(&mut self, workspace: &Path) -> Result<UndoOutcome> {
        // A turn still open — undone mid-session, or by a caller that never
        // sealed it — is sealed first, so only what changed is touched.
        if let Some(open) = self.checkpoints.last_mut() {
            seal(open);
        }
        let index = self
            .checkpoints
            .iter()
            .rposition(Checkpoint::undoable)
            .context("there is nothing to undo")?;
        if !self.checkpoints[index].complete {
            anyhow::bail!(
                "undo is not available for that turn: the workspace has more than \
                 {MAX_SNAPSHOT_FILES} files, so no complete snapshot was taken. A partial undo \
                 would restore some files and not others."
            );
        }
        let checkpoint = self.checkpoints.remove(index);
        let outcome = restore(&checkpoint, workspace);
        forget_stored(&checkpoint);
        Ok(outcome)
    }

    /// Undo up to `turns` checkpoints, newest first. Stops before the first
    /// one that cannot be undone, and after the first that could only be
    /// partly undone — going further back from a tree that matches no turn
    /// would compound it. Returns what was undone and, when it stopped
    /// early, why.
    pub fn rewind(&mut self, workspace: &Path, turns: usize) -> (Vec<UndoOutcome>, Option<String>) {
        let mut undone = Vec::new();
        for _ in 0..turns {
            match self.undo_last(workspace) {
                Ok(outcome) => {
                    let partial = !outcome.is_complete();
                    let label = outcome.label.clone();
                    undone.push(outcome);
                    if partial && undone.len() < turns {
                        return (
                            undone,
                            Some(format!(
                                "`{label}` could only be partly undone, so nothing older was \
                                 touched"
                            )),
                        );
                    }
                }
                Err(e) => return (undone, Some(e.to_string())),
            }
        }
        (undone, None)
    }
}

/// Seal `checkpoint`: record what each file holds now, drop the unchanged,
/// and add what the turn created. Once only.
fn seal(checkpoint: &mut Checkpoint) {
    if checkpoint.sealed {
        return;
    }
    checkpoint.sealed = true;
    let seen: HashSet<PathBuf> = checkpoint.files.iter().map(|e| e.path.clone()).collect();
    checkpoint.files.retain_mut(|entry| {
        let now = After::now(&entry.path);
        entry.after = Some(now);
        changed(&entry.path, &entry.before, now)
    });
    let Some(walk) = checkpoint.walk.take() else {
        return;
    };
    // An incomplete walk never saw part of the tree: what is there now may
    // have been there before, and deleting it would destroy it.
    if !checkpoint.complete {
        return;
    }
    let (files, dirs, complete) = walk_tree(&walk.roots, MAX_SNAPSHOT_FILES + MAX_CREATED_FILES);
    checkpoint.created_cut = !complete;
    for file in files {
        if seen.contains(&file) {
            continue;
        }
        if let Some(mark) = Mark::of_file(&file) {
            checkpoint.files.push(Entry {
                path: file,
                before: FileSnapshot::Absent,
                after: Some(After::Present(mark)),
            });
        }
    }
    let mut created: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|d| !walk.dirs.contains(d))
        .collect();
    created.sort();
    checkpoint.created_dirs = created;
}

/// Whether a file differs from what the checkpoint holds for it.
fn changed(path: &Path, before: &FileSnapshot, now: After) -> bool {
    match (before, now) {
        (FileSnapshot::Existed(bytes), After::Present(mark)) => Mark::of_bytes(bytes) != mark,
        (FileSnapshot::Absent, After::Gone) => false,
        (
            FileSnapshot::Unrecoverable {
                seen: Some(seen), ..
            },
            After::Present(_),
        ) => std::fs::metadata(path)
            .map(|meta| (meta.len(), meta.modified().ok()) != *seen)
            .unwrap_or(true),
        _ => true,
    }
}

/// Put one checkpoint's files back.
fn restore(checkpoint: &Checkpoint, workspace: &Path) -> UndoOutcome {
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for entry in &checkpoint.files {
        let shown = display_path(workspace, &entry.path);
        let now = After::now(&entry.path);
        // A file someone changed after the turn ended is theirs now.
        if entry.after.is_some_and(|after| after != now) {
            failed.push(format!(
                "{shown}: changed since that turn, so it was left as it is"
            ));
            continue;
        }
        match &entry.before {
            FileSnapshot::Existed(contents) => match restore_file(&entry.path, contents) {
                Ok(()) => restored.push(shown),
                Err(e) => failed.push(format!("{shown}: {e}")),
            },
            FileSnapshot::Stored(blob) => {
                match std::fs::read(blob)
                    .with_context(|| {
                        format!("its stored copy {} could not be read", blob.display())
                    })
                    .and_then(|contents| restore_file(&entry.path, &contents))
                {
                    Ok(()) => restored.push(shown),
                    Err(e) => failed.push(format!("{shown}: {e:#}")),
                }
            }
            FileSnapshot::Absent => {
                if now == After::Gone {
                    continue;
                }
                match remove_file(&entry.path) {
                    Ok(()) => restored.push(format!("{shown} (removed)")),
                    Err(e) => failed.push(format!("{shown}: {e}")),
                }
            }
            FileSnapshot::Unrecoverable { reason, .. } => {
                failed.push(format!("{shown}: {reason}"));
            }
        }
    }
    // Deepest first, and only when empty: a directory something else put a
    // file in is not the turn's to remove.
    for dir in checkpoint.created_dirs.iter().rev() {
        if dir.is_dir() && std::fs::remove_dir(dir).is_ok() {
            restored.push(format!("{}/ (removed)", display_path(workspace, dir)));
        }
    }
    if checkpoint.created_cut {
        failed.push(format!(
            "that turn created more than {MAX_CREATED_FILES} files; only those were recorded, \
             and any others are still there"
        ));
    }
    UndoOutcome {
        label: checkpoint.label.clone(),
        restored,
        failed,
    }
}

/// Every file and directory under `roots`, outside [`SKIP_DIRS`], up to
/// `limit` files. The third value is whether the walk finished.
fn walk_tree(roots: &[PathBuf], limit: usize) -> (Vec<PathBuf>, HashSet<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut dirs = HashSet::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                dirs.insert(path.clone());
                stack.push(path);
                continue;
            }
            if files.len() >= limit {
                return (files, dirs, false);
            }
            files.push(path);
        }
    }
    (files, dirs, true)
}

// ---------------------------------------------------------------------------
// On disk
// ---------------------------------------------------------------------------

/// One stored turn, as its manifest spells it.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    label: String,
    session: String,
    /// When the turn was sealed, in milliseconds since the epoch: the order
    /// of the shared stack.
    sealed_ms: u64,
    seq: u64,
    complete: bool,
    #[serde(default)]
    created_cut: bool,
    bytes: u64,
    files: Vec<StoredEntry>,
    #[serde(default)]
    created_dirs: Vec<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredEntry {
    path: PathBuf,
    before: StoredBefore,
    after: Option<StoredAfter>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredBefore {
    Contents { blob: String },
    Absent,
    Unheld { reason: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredAfter {
    Present { len: u64, hash: u64 },
    Gone,
}

/// Write `checkpoint` under the session's directory: each "before" as a
/// file of its own, then the manifest. On success its contents are dropped
/// from memory and read back from disk when undone.
fn store(disk: &Disk, seq: u64, checkpoint: &mut Checkpoint) -> Result<(), String> {
    let bytes: u64 = checkpoint
        .files
        .iter()
        .map(|entry| match &entry.before {
            FileSnapshot::Existed(contents) => contents.len() as u64,
            _ => 0,
        })
        .sum();
    if bytes > disk.cap {
        return Err(format!(
            "its files hold {bytes} bytes, over the {}-byte cap on disk",
            disk.cap
        ));
    }
    let sealed_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
    )
    .unwrap_or(u64::MAX);
    let dir = disk
        .root
        .join(&disk.session)
        .join(format!("{sealed_ms:013}-{seq:04}"));
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs).map_err(|e| format!("{}: {e}", blobs.display()))?;
    let mut entries = Vec::with_capacity(checkpoint.files.len());
    for (i, entry) in checkpoint.files.iter().enumerate() {
        let before = match &entry.before {
            FileSnapshot::Existed(contents) => {
                let name = format!("{i}");
                std::fs::write(blobs.join(&name), contents)
                    .map_err(|e| format!("{}: {e}", blobs.join(&name).display()))?;
                StoredBefore::Contents { blob: name }
            }
            FileSnapshot::Absent => StoredBefore::Absent,
            FileSnapshot::Unrecoverable { reason, .. } => StoredBefore::Unheld {
                reason: reason.clone(),
            },
            FileSnapshot::Stored(_) => return Err("already stored".to_string()),
        };
        entries.push(StoredEntry {
            path: entry.path.clone(),
            before,
            after: entry.after.map(|after| match after {
                After::Present(mark) => StoredAfter::Present {
                    len: mark.len,
                    hash: mark.hash,
                },
                After::Gone => StoredAfter::Gone,
            }),
        });
    }
    let manifest = Manifest {
        format: FORMAT,
        label: checkpoint.label.clone(),
        session: disk.session.clone(),
        sealed_ms,
        seq,
        complete: checkpoint.complete,
        created_cut: checkpoint.created_cut,
        bytes,
        files: entries,
        created_dirs: checkpoint.created_dirs.clone(),
    };
    let text = serde_json::to_string(&manifest).map_err(|e| e.to_string())?;
    let temp = dir.join(format!("{MANIFEST}.tmp"));
    std::fs::write(&temp, text).map_err(|e| format!("{}: {e}", temp.display()))?;
    std::fs::rename(&temp, dir.join(MANIFEST)).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Stored: the contents now live on disk.
    for (i, entry) in checkpoint.files.iter_mut().enumerate() {
        if matches!(entry.before, FileSnapshot::Existed(_)) {
            entry.before = FileSnapshot::Stored(blobs.join(format!("{i}")));
        }
    }
    checkpoint.stored_at = Some(dir);
    checkpoint.stored_bytes = bytes;
    Ok(())
}

/// Remove a stored turn from disk, and its session directory once empty.
fn forget_stored(checkpoint: &Checkpoint) {
    let Some(dir) = &checkpoint.stored_at else {
        return;
    };
    if let Err(e) = std::fs::remove_dir_all(dir) {
        crate::trace!("checkpoint", "{} not removed: {e}", dir.display());
    }
    if let Some(session) = dir.parent() {
        let _ = std::fs::remove_dir(session);
    }
}

/// Every stored turn under `disk.root`, oldest first, newest
/// [`MAX_CHECKPOINTS`] kept. A turn with an unreadable manifest, or one
/// naming a path outside the roots, is left out.
fn load_stored(disk: &Disk) -> Vec<Checkpoint> {
    let mut found: Vec<(u64, u64, Checkpoint)> = Vec::new();
    let Ok(sessions) = std::fs::read_dir(&disk.root) else {
        return Vec::new();
    };
    for session in sessions.flatten() {
        let Ok(turns) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for turn in turns.flatten() {
            let dir = turn.path();
            match read_manifest(&dir, &disk.roots) {
                Ok((sealed_ms, seq, checkpoint)) => found.push((sealed_ms, seq, checkpoint)),
                Err(why) => crate::trace!("checkpoint", "{} left out: {why}", dir.display()),
            }
        }
    }
    found.sort_by_key(|(ms, seq, _)| (*ms, *seq));
    let drop = found.len().saturating_sub(MAX_CHECKPOINTS);
    found
        .into_iter()
        .enumerate()
        .filter_map(|(i, (_, _, checkpoint))| {
            if i < drop {
                forget_stored(&checkpoint);
                None
            } else {
                Some(checkpoint)
            }
        })
        .collect()
}

/// One stored turn, checked: every path inside `roots`, every blob a plain
/// name in its own directory.
fn read_manifest(dir: &Path, roots: &[PathBuf]) -> Result<(u64, u64, Checkpoint), String> {
    let path = dir.join(MANIFEST);
    let meta = std::fs::metadata(&path).map_err(|_| "no manifest".to_string())?;
    if meta.len() > MAX_MANIFEST_BYTES {
        return Err(format!("manifest over {MAX_MANIFEST_BYTES} bytes"));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if manifest.format != FORMAT {
        return Err(format!("format {} is not {FORMAT}", manifest.format));
    }
    let inside = |p: &Path| {
        p.is_absolute()
            && !p.components().any(|c| matches!(c, Component::ParentDir))
            && roots.iter().any(|root| p.starts_with(root) && p != root)
    };
    let mut files = Vec::with_capacity(manifest.files.len());
    for stored in manifest.files {
        if !inside(&stored.path) {
            return Err(format!(
                "{} is outside the session's roots",
                stored.path.display()
            ));
        }
        let before = match stored.before {
            StoredBefore::Contents { blob } => {
                if blob.is_empty() || blob.contains(['/', '\\']) || blob.contains("..") {
                    return Err(format!("`{blob}` is not a plain file name"));
                }
                FileSnapshot::Stored(dir.join("blobs").join(blob))
            }
            StoredBefore::Absent => FileSnapshot::Absent,
            StoredBefore::Unheld { reason } => FileSnapshot::Unrecoverable { reason, seen: None },
        };
        files.push(Entry {
            path: stored.path,
            before,
            after: stored.after.map(|after| match after {
                StoredAfter::Present { len, hash } => After::Present(Mark { len, hash }),
                StoredAfter::Gone => After::Gone,
            }),
        });
    }
    if let Some(outside) = manifest.created_dirs.iter().find(|d| !inside(d)) {
        return Err(format!(
            "{} is outside the session's roots",
            outside.display()
        ));
    }
    let checkpoint = Checkpoint {
        label: manifest.label,
        files,
        walk: None,
        complete: manifest.complete,
        created_cut: manifest.created_cut,
        sealed: true,
        created_dirs: manifest.created_dirs,
        stored_at: Some(dir.to_path_buf()),
        stored_bytes: manifest.bytes,
    };
    Ok((manifest.sealed_ms, manifest.seq, checkpoint))
}

/// A session id as a directory name that cannot leave its parent.
fn safe_name(id: &str) -> String {
    let out: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
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

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;
