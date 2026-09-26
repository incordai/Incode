//! Tests for [`super`]: undo within a session, and the stack kept on disk.

use super::*;

/// **An untouched file is not rewritten.** The snapshot covers the whole
/// tree, so most captured files never change. Writing them back would change
/// every mtime (rebuilding everything), could clobber an edit made in another
/// editor while the turn ran, and would report files as "restored" that
/// nothing had undone.
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

fn write(root: &Path, name: &str, contents: &str) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent must be creatable");
    }
    std::fs::write(&path, contents).expect("fixture must be writable");
    path
}

fn read(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).expect("file must be readable")
}

/// The whole point: a bad edit is reversible.
#[test]
fn undo_restores_a_modified_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "a.txt", "ORIGINAL");

    let mut store = CheckpointStore::new();
    store.begin("turn 1");
    store.snapshot(&path);
    std::fs::write(&path, "RUINED").unwrap();

    let outcome = store.undo_last(dir.path()).expect("undo must succeed");

    assert_eq!(read(dir.path(), "a.txt"), "ORIGINAL");
    assert!(outcome.is_complete(), "failed: {:?}", outcome.failed);
    assert_eq!(outcome.restored, vec!["a.txt".to_string()]);
}

/// A file the agent CREATED has no previous contents, so undoing it means
/// deleting it. Without the `Absent` case it would survive the undo and leave
/// the workspace dirty in a way the user did not ask for.
#[test]
fn undo_deletes_a_file_that_did_not_exist_before() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("new.txt");

    let mut store = CheckpointStore::new();
    store.begin("turn 1");
    store.snapshot(&path); // taken BEFORE creation, so: Absent
    std::fs::write(&path, "brand new").unwrap();
    assert!(path.exists(), "the fixture must actually create the file");

    let outcome = store.undo_last(dir.path()).expect("undo must succeed");

    assert!(!path.exists(), "a file created in the turn must be removed");
    assert!(outcome.is_complete(), "failed: {:?}", outcome.failed);
}

/// Undo must return to how things were when the checkpoint OPENED, not to the
/// state between two edits. Keeping the later snapshot would restore the file
/// to a version the user never had.
#[test]
fn only_the_first_state_of_a_file_in_a_checkpoint_is_kept() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "a.txt", "V1");

    let mut store = CheckpointStore::new();
    store.begin("turn 1");
    store.snapshot(&path);
    std::fs::write(&path, "V2").unwrap();
    store.snapshot(&path); // second edit in the same turn
    std::fs::write(&path, "V3").unwrap();

    store.undo_last(dir.path()).expect("undo must succeed");

    assert_eq!(
        read(dir.path(), "a.txt"),
        "V1",
        "undo must reach the state at the start of the checkpoint, not the middle"
    );
}

/// Each checkpoint is its own step, and undoing one must not disturb what an
/// earlier one covered.
#[test]
fn undo_steps_back_one_checkpoint_at_a_time() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "a.txt", "V1");

    let mut store = CheckpointStore::new();
    store.begin("turn 1");
    store.snapshot(&path);
    std::fs::write(&path, "V2").unwrap();

    store.begin("turn 2");
    store.snapshot(&path);
    std::fs::write(&path, "V3").unwrap();

    let second = store.undo_last(dir.path()).expect("first undo");
    assert_eq!(second.label, "turn 2");
    assert_eq!(
        read(dir.path(), "a.txt"),
        "V2",
        "must step back one turn only"
    );

    let first = store.undo_last(dir.path()).expect("second undo");
    assert_eq!(first.label, "turn 1");
    assert_eq!(read(dir.path(), "a.txt"), "V1");
}

/// Undo covers every file the turn touched, not just the last one.
#[test]
fn undo_restores_every_file_in_the_checkpoint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let a = write(dir.path(), "a.txt", "A1");
    let b = write(dir.path(), "nested/b.txt", "B1");

    let mut store = CheckpointStore::new();
    store.begin("turn 1");
    store.snapshot(&a);
    store.snapshot(&b);
    std::fs::write(&a, "A2").unwrap();
    std::fs::write(&b, "B2").unwrap();

    let outcome = store.undo_last(dir.path()).expect("undo must succeed");

    assert_eq!(read(dir.path(), "a.txt"), "A1");
    assert_eq!(read(dir.path(), "nested/b.txt"), "B1");
    assert_eq!(outcome.restored.len(), 2, "got: {:?}", outcome.restored);
}

/// A file too large to hold must be REPORTED, never quietly dropped. An undo
/// that claims success while leaving a file modified is worse than no undo at
/// all, because the user stops looking.
#[test]
fn a_file_too_large_to_snapshot_is_reported_not_skipped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "big.txt", &"x".repeat(MAX_SNAPSHOT_BYTES + 1));

    let mut store = CheckpointStore::new();
    store.begin("turn 1");
    store.snapshot(&path);
    std::fs::write(&path, "clobbered").unwrap();

    let outcome = store.undo_last(dir.path()).expect("undo must return");

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

/// Undoing with nothing to undo is an error the caller can show, not a silent
/// success that looks like it did something.
#[test]
fn undoing_nothing_is_an_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = CheckpointStore::new();

    assert!(!store.has_undo());
    assert!(store.undo_last(dir.path()).is_err());

    // A checkpoint that changed nothing is still nothing to undo.
    store.begin("turn 1");
    assert!(!store.has_undo(), "an untouched checkpoint is not undoable");
    assert!(store.undo_last(dir.path()).is_err());
}

/// Snapshots taken with no checkpoint open would accumulate for a caller that
/// never intends to undo.
#[test]
fn nothing_is_recorded_before_a_checkpoint_is_opened() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "a.txt", "V1");

    let mut store = CheckpointStore::new();
    store.snapshot(&path);

    assert!(!store.has_undo(), "no checkpoint means nothing is recorded");
}

/// The stack is bounded, or a long session holds every version of every file
/// it ever touched.
#[test]
fn the_checkpoint_stack_is_bounded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = write(dir.path(), "a.txt", "V0");
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
    let dir = tempfile::tempdir().expect("temp dir");
    let original_bytes: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, 0x80, 0x90, 0xAA];
    let file_path = dir.path().join("data.bin");
    std::fs::write(&file_path, &original_bytes).unwrap();

    let mut store = CheckpointStore::new();
    store.begin("binary edit");
    store.snapshot(&file_path);
    std::fs::write(&file_path, [0x00, 0x11, 0x22, 0x33]).unwrap();

    let outcome = store.undo_last(dir.path()).expect("undo succeeds");
    assert!(outcome.is_complete());
    assert_eq!(outcome.restored.len(), 1);
    assert_eq!(std::fs::read(&file_path).unwrap(), original_bytes);
}

// ---------------------------------------------------------------------------
// Whole-tree turns: what a turn created, and what changed after it
// ---------------------------------------------------------------------------

/// **A FILE THE TURN CREATED IS DELETED ON UNDO, AND SO IS A DIRECTORY IT
/// MADE.** The pre-turn snapshot lists only the files that existed, so a file
/// written by the agent's shell was never in it and used to survive `/undo`.
#[test]
fn a_file_and_a_directory_created_in_a_turn_are_removed_on_undo() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    write(&root, "kept.txt", "kept");

    let mut store = CheckpointStore::new();
    assert!(store.begin_tree("make things", std::slice::from_ref(&root)));
    // Written out of the store's sight, as a shell command would.
    write(&root, "made.txt", "new");
    write(&root, "fresh/deep/also.txt", "new too");
    store.finish_turn();

    let outcome = store.undo_last(&root).expect("undo");
    assert!(!root.join("made.txt").exists(), "{outcome:?}");
    assert!(
        !root.join("fresh").exists(),
        "the made directory stays: {outcome:?}"
    );
    assert_eq!(read(&root, "kept.txt"), "kept");
    assert!(outcome.is_complete(), "{outcome:?}");
}

/// **A FILE CHANGED AFTER THE TURN ENDED IS LEFT ALONE, AND SAID.** An undo
/// later in the session — or after a restart — must not throw away what a
/// person wrote since.
#[test]
fn a_file_changed_since_the_turn_is_left_as_it_is() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    write(&root, "a.txt", "before");

    let mut store = CheckpointStore::new();
    store.begin_tree("edit", std::slice::from_ref(&root));
    write(&root, "a.txt", "by the turn");
    store.finish_turn();
    write(&root, "a.txt", "by a person, later");

    let outcome = store.undo_last(&root).expect("undo");
    assert_eq!(read(&root, "a.txt"), "by a person, later");
    assert!(
        outcome
            .failed
            .iter()
            .any(|f| f.contains("changed since that turn")),
        "{outcome:?}"
    );
}

/// A turn that changed nothing leaves nothing to undo behind it.
#[test]
fn a_turn_that_changed_nothing_is_not_kept() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    write(&root, "a.txt", "same");
    let mut store = CheckpointStore::new();
    store.begin_tree("look only", std::slice::from_ref(&root));
    store.finish_turn();
    assert!(!store.has_undo());
}

/// `rewind` goes back several turns, newest first.
#[test]
fn rewind_goes_back_several_turns() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    write(&root, "a.txt", "V1");
    let mut store = CheckpointStore::new();
    for next in ["V2", "V3", "V4"] {
        store.begin_tree(format!("to {next}"), std::slice::from_ref(&root));
        write(&root, "a.txt", next);
        store.finish_turn();
    }
    let (undone, stopped) = store.rewind(&root, 2);
    assert_eq!(stopped, None);
    assert_eq!(
        undone.iter().map(|u| u.label.as_str()).collect::<Vec<_>>(),
        ["to V4", "to V3"]
    );
    assert_eq!(read(&root, "a.txt"), "V2");

    let (more, stopped) = store.rewind(&root, 5);
    assert_eq!(more.len(), 1, "only one turn was left");
    assert!(stopped.is_some_and(|why| why.contains("nothing to undo")));
    assert_eq!(read(&root, "a.txt"), "V1");
}

// ---------------------------------------------------------------------------
// Kept on disk
// ---------------------------------------------------------------------------

/// **THE STACK SURVIVES A RESTART.** A turn sealed by one session is undone by
/// the next, from what was written to disk — including a file the turn
/// created — and the stored turn is gone once undone.
#[test]
fn a_turn_kept_on_disk_is_undone_by_the_next_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    write(&root, "a.txt", "before");
    {
        let mut first = CheckpointStore::persisted(&root, std::slice::from_ref(&root), "sess-1");
        first.begin_tree("the first session's turn", std::slice::from_ref(&root));
        write(&root, "a.txt", "after");
        write(&root, "created.txt", "new");
        first.finish_turn();
    }
    assert!(root.join(CHECKPOINTS_DIR).join("sess-1").is_dir());

    let mut second = CheckpointStore::persisted(&root, std::slice::from_ref(&root), "sess-2");
    assert_eq!(second.pending_label(), Some("the first session's turn"));
    let outcome = second.undo_last(&root).expect("undo after a restart");
    assert_eq!(read(&root, "a.txt"), "before");
    assert!(!root.join("created.txt").exists(), "{outcome:?}");
    assert!(
        !root.join(CHECKPOINTS_DIR).join("sess-1").exists(),
        "an undone turn is removed from disk"
    );
    let third = CheckpointStore::persisted(&root, std::slice::from_ref(&root), "sess-3");
    assert!(!third.has_undo(), "nothing is undone twice");
}

/// The stack on disk keeps the newest turns only.
#[test]
fn the_stack_on_disk_keeps_the_newest_turns() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    write(&root, "a.txt", "V0");
    let mut store = CheckpointStore::persisted(&root, std::slice::from_ref(&root), "s");
    for turn in 1..=(MAX_CHECKPOINTS + 3) {
        store.begin_tree(format!("turn {turn}"), std::slice::from_ref(&root));
        write(&root, "a.txt", &format!("V{turn}"));
        store.finish_turn();
    }
    let kept = std::fs::read_dir(root.join(CHECKPOINTS_DIR).join("s"))
        .expect("session dir")
        .count();
    assert_eq!(kept, MAX_CHECKPOINTS);
    let again = CheckpointStore::persisted(&root, std::slice::from_ref(&root), "t");
    assert_eq!(
        again.pending_label(),
        Some(format!("turn {}", MAX_CHECKPOINTS + 3).as_str())
    );
}

/// **THE SIZE CAP BOUNDS WHAT IS KEPT**: older turns go first, and a turn over
/// the cap on its own is kept in memory only.
#[test]
fn the_stack_on_disk_stays_under_its_size_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    write(&root, "a.txt", &"a".repeat(400));
    let mut store =
        CheckpointStore::persisted(&root, std::slice::from_ref(&root), "s").capped(1_000);
    for turn in 1..=4 {
        store.begin_tree(format!("turn {turn}"), std::slice::from_ref(&root));
        write(&root, "a.txt", &"b".repeat(400 + turn));
        store.finish_turn();
    }
    let kept = std::fs::read_dir(root.join(CHECKPOINTS_DIR).join("s"))
        .expect("session dir")
        .count();
    assert_eq!(kept, 2, "two turns of ~400 bytes fit under 1000");

    write(&root, "big.txt", &"x".repeat(2_000));
    store.begin_tree("too big", std::slice::from_ref(&root));
    write(&root, "big.txt", "small now");
    store.finish_turn();
    assert_eq!(store.pending_label(), Some("too big"), "kept in memory");
    store.undo_last(&root).expect("undone from memory");
    assert_eq!(read(&root, "big.txt").len(), 2_000);
}

/// **A STORED PATH OUTSIDE THE ROOTS IS NOT TRUSTED.** The manifest is a file
/// in the workspace; a crafted one must not make `/undo` write elsewhere.
#[test]
fn a_manifest_naming_a_path_outside_the_roots_is_left_out() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("ws");
    let outside = dir.path().join("outside.txt");
    std::fs::create_dir_all(&root).expect("ws");
    std::fs::write(&outside, "precious").expect("outside");
    let turn = root
        .join(CHECKPOINTS_DIR)
        .join("evil")
        .join("0000000000001-0001");
    std::fs::create_dir_all(turn.join("blobs")).expect("dirs");
    std::fs::write(turn.join("blobs").join("0"), "overwritten").expect("blob");
    let manifest = serde_json::json!({
        "format": FORMAT, "label": "crafted", "session": "evil", "sealed_ms": 1, "seq": 1,
        "complete": true, "bytes": 11,
        "files": [{"path": outside, "before": {"kind": "contents", "blob": "0"},
                   "after": {"kind": "gone"}}]
    });
    std::fs::write(turn.join(MANIFEST), manifest.to_string()).expect("manifest");

    let mut store = CheckpointStore::persisted(&root, std::slice::from_ref(&root), "me");
    assert!(!store.has_undo(), "the crafted turn was trusted");
    assert!(store.undo_last(&root).is_err());
    assert_eq!(
        std::fs::read_to_string(&outside).expect("still there"),
        "precious"
    );
}
