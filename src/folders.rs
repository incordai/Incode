//! What in a project's `.incode/` is shared, and what is this machine's own.
//!
//! # Why a project folder needs its own `.gitignore`
//!
//! `.incode/` holds two kinds of file side by side. Some are the owner's
//! decisions — the frozen interfaces (`contracts.json`), the protected paths
//! (`guards.json`), the project's sub-agents (`agents/*.md`) — and a teammate
//! who clones the repository needs them exactly as much as the source. The
//! rest is what happened on THIS machine: the ledger of one checkout's run, a
//! cached brief, checkpoints, transcripts, a server's token. Committed, those
//! are noise at best and a leaked token at worst.
//!
//! A repository that ignores all of `.incode/` loses the first kind; one that
//! ignores none of it commits the second. So incode writes the split itself,
//! into `.incode/.gitignore`, the moment it creates the folder — before any
//! file lands there that should not be committed.
//!
//! # The owner's edits win
//!
//! The file is written once and then only ever ADDED to: a line missing from
//! [`MACHINE_LOCAL`] is appended, nothing is removed or reordered, and a line
//! the owner negated (`!findings/`) counts as a decision and is left alone.

use std::path::Path;

/// The file this module keeps, inside `.incode/`.
pub const GITIGNORE: &str = ".gitignore";

/// Everything incode writes under `.incode/` that belongs to one machine.
///
/// A test holds this, together with [`SHARED`], to every item incode writes
/// there ([`crate::migrate::PROJECT_ITEMS`]), so a new state file cannot be
/// added and left to be committed by accident.
pub const MACHINE_LOCAL: &[&str] = &[
    "ledger.json",
    "brief.cache.json",
    "checkpoints/",
    "transcripts/",
    "context/",
    "findings/",
    "serve.json",
    "serve.token",
    "run.stop",
    "desktop.*",
    "team.jsonl",
    "lifecycle.json",
    // Task transcripts an older incode wrote beside the definitions. They are
    // moved to `transcripts/` at start, but one whose target already exists is
    // left where it is — and it is still this machine's.
    "agents/*.jsonl",
];

/// What stays tracked: the files the owner approves. Named in the file's
/// header so a reader knows the omission is deliberate.
pub const SHARED: &[&str] = &[
    "contracts.json",
    "guards.json",
    "settings.json",
    "agents/*.md",
];

/// The file as first written.
fn fresh_text() -> String {
    let mut out = format!(
        "# Written by incode. Each line below is this machine's own state: it means\n\
         # nothing in another checkout, so it is not committed.\n\
         #\n\
         # Committed, and so not listed: {}, and this file.\n\
         #\n\
         # Edit freely. incode only ever adds a line that is missing, and a line you\n\
         # negate (`!findings/`) is your decision and is left alone.\n",
        SHARED.join(", ")
    );
    for entry in MACHINE_LOCAL {
        out.push_str(entry);
        out.push('\n');
    }
    out
}

/// The entries of [`MACHINE_LOCAL`] that `text` neither lists nor negates.
fn missing_from(text: &str) -> Vec<&'static str> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim_start_matches('!').trim_start_matches('/'))
        .collect();
    MACHINE_LOCAL
        .iter()
        .copied()
        .filter(|entry| !lines.contains(entry))
        .collect()
}

/// Make sure `incode_dir/.gitignore` ignores every machine-local item.
///
/// Absent, the whole file is written. Present, only the missing lines are
/// appended; the owner's lines are kept as they are. Returns whether the file
/// changed.
///
/// # Errors
///
/// The I/O error, for the caller to report. Nothing here should fail the work
/// that created the folder, so every caller in this crate traces it and goes
/// on.
pub fn ensure_gitignore(incode_dir: &Path) -> std::io::Result<bool> {
    use std::io::Write;
    let path = incode_dir.join(GITIGNORE);
    // Created, not overwritten: two writers making the folder at once must
    // not clobber each other, and the loser merges into what the winner wrote.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(fresh_text().as_bytes())?;
            return Ok(true);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let text = std::fs::read_to_string(&path)?;
    let missing = missing_from(&text);
    if missing.is_empty() {
        return Ok(false);
    }
    let mut out = text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# Added by incode:\n");
    for entry in missing {
        out.push_str(entry);
        out.push('\n');
    }
    std::fs::write(&path, out)?;
    Ok(true)
}

/// `std::fs::create_dir_all`, and when that creates a project's `.incode/`,
/// the `.gitignore` in it.
///
/// Every writer of incode's project state goes through this, so the file is
/// there before the first machine-local file is — whichever writer happens to
/// be first. Only a folder this call CREATED gets the file: an existing one is
/// the owner's, healed once per session by [`heal`], and a writer that appends
/// on every event must not read a file on every event.
///
/// # Errors
///
/// Only the error from creating the directories. The `.gitignore` is traced,
/// never returned: losing it costs a noisy `git status`, and failing the write
/// over it would cost the work.
pub fn create_dir_all(path: &Path) -> std::io::Result<()> {
    let incode = path.ancestors().find(|a| {
        a.file_name()
            .is_some_and(|n| n == crate::migrate::PROJECT_DIR)
    });
    let fresh = incode.is_some_and(|dir| !dir.exists());
    std::fs::create_dir_all(path)?;
    if let (true, Some(dir)) = (fresh, incode) {
        if let Err(e) = ensure_gitignore(dir) {
            crate::trace!("folders", "{}: {e}", dir.join(GITIGNORE).display());
        }
    }
    Ok(())
}

/// Bring an existing `.incode/.gitignore` up to date, or write it when a
/// folder made before this existed has none. Returns a line for the operator
/// when it wrote. A workspace with no `.incode/` is left without one.
#[must_use]
pub fn heal(workspace: &Path) -> Option<String> {
    let dir = workspace.join(crate::migrate::PROJECT_DIR);
    if !dir.is_dir() {
        return None;
    }
    match ensure_gitignore(&dir) {
        Ok(true) => Some(format!(
            "incode: updated {} so this machine's state in {}/ is not committed.",
            dir.join(GITIGNORE).display(),
            crate::migrate::PROJECT_DIR
        )),
        Ok(false) => None,
        Err(e) => {
            crate::trace!("folders", "{}: {e}", dir.join(GITIGNORE).display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read")
    }

    /// The folder's first writer leaves the file behind, whatever it wrote.
    #[test]
    fn creating_the_folder_writes_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let deep = dir.path().join(".incode").join("transcripts").join("tasks");

        create_dir_all(&deep).expect("created");

        let text = read(&dir.path().join(".incode").join(GITIGNORE));
        for entry in MACHINE_LOCAL {
            assert!(
                text.lines().any(|l| l == *entry),
                "{entry} missing:\n{text}"
            );
        }
        // No line that would ignore the owner's files: the files themselves,
        // the whole agents folder, or every Markdown file in it.
        for shared in [
            "contracts.json",
            "guards.json",
            "agents",
            "agents/",
            "agents/*",
            "agents/*.md",
        ] {
            assert!(
                !text.lines().any(|l| l.trim() == shared),
                "{shared} is the owner's and must stay tracked:\n{text}"
            );
        }
    }

    /// A folder that is not incode's gets nothing.
    #[test]
    fn other_folders_get_no_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        create_dir_all(&dir.path().join("docs").join("x")).expect("created");
        assert!(!dir.path().join("docs").join(GITIGNORE).exists());
    }

    /// Idempotent, and the owner's edits survive: their own lines stay, a
    /// negated entry is their decision, and only what is missing is added.
    #[test]
    fn an_edited_file_is_added_to_never_rewritten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let incode = dir.path().join(".incode");
        std::fs::create_dir_all(&incode).expect("mkdir");
        let owner = "# mine\nledger.json\n!findings/\nscratch/";
        std::fs::write(incode.join(GITIGNORE), owner).expect("write");

        assert!(ensure_gitignore(&incode).expect("merged"));
        let text = read(&incode.join(GITIGNORE));
        assert!(text.starts_with(owner), "the owner's text is kept:\n{text}");
        assert!(!text[owner.len()..].contains("ledger.json"), "{text}");
        assert!(
            !text[owner.len()..].contains("findings/"),
            "a negated entry is not put back:\n{text}"
        );
        assert!(text.contains("\ncheckpoints/\n"), "{text}");

        assert!(!ensure_gitignore(&incode).expect("second run"));
        assert_eq!(
            read(&incode.join(GITIGNORE)),
            text,
            "a second run changes nothing"
        );
    }

    /// An existing folder is healed at session start; none is made for a
    /// workspace that has no `.incode/`.
    #[test]
    fn heal_fills_an_existing_folder_only() {
        let bare = tempfile::tempdir().expect("temp dir");
        assert_eq!(heal(bare.path()), None);
        assert!(!bare.path().join(".incode").exists());

        let old = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(old.path().join(".incode").join("agents")).expect("mkdir");
        assert!(heal(old.path()).is_some());
        assert!(old.path().join(".incode").join(GITIGNORE).is_file());
        assert_eq!(heal(old.path()), None, "nothing to say the second time");
    }

    /// Every item incode writes under `.incode/` is either ignored here or one
    /// of the owner's shared files — so a new state file cannot be committed by
    /// accident. [`crate::migrate::PROJECT_ITEMS`] is itself held to every path
    /// constant by the migration's own test.
    #[test]
    fn every_item_incode_writes_is_classified() {
        for item in crate::migrate::PROJECT_ITEMS {
            let ignored = MACHINE_LOCAL.iter().any(|entry| {
                let entry = entry.trim_end_matches('/');
                match entry.strip_suffix('*') {
                    Some(prefix) => item.starts_with(prefix),
                    None => *item == entry,
                }
            });
            let shared = SHARED.iter().any(|s| s.split('/').next() == Some(*item));
            assert!(
                ignored ^ shared,
                "`{item}` is written under .incode/ and must be exactly one of \
                 ignored or shared (ignored: {ignored}, shared: {shared})"
            );
        }
    }
}
