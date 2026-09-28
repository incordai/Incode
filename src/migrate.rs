//! Moving incode's state out of the names it used to share with Incord Memory.
//!
//! incode kept its per-project state in `<project>/.incord/` and its settings in
//! `~/.incord-code/`. Incord Memory, a separate product, ALSO writes to the
//! project's `.incord/` (`RULE_ACK`, `MEMORY_FIRST_OFF`, `PROJECT.md`, its
//! backups), so the two shared a folder and a prefix. incode now owns
//! `.incode/` and `~/.incode/` alone.
//!
//! # What happens, and when
//!
//! - **Machine-wide**, once, at the start of a mode that reads the data
//!   directory ([`global_report`]): when `~/.incode/` does
//!   not exist and `~/.incord-code/` does, the old folder is COPIED — into a
//!   staging folder first, renamed into place only when complete, so a copy
//!   cut short is never taken for the real one — and a note saying where it
//!   went is left in the old folder. The old folder is never deleted. Skipped
//!   entirely when [`crate::admin::DATA_DIR_ENV`] names the data directory:
//!   the operator chose it, and it is not under the home directory.
//! - **Per project**, at start and at every session start ([`project`]): each
//!   of incode's own items ([`PROJECT_ITEMS`]) still in `.incord/` is MOVED to
//!   `.incode/`, one item at a time, each move atomic. An item whose target
//!   already exists is skipped and left where it is — unless the target is an
//!   empty folder, which holds nothing and gives way. Everything else in
//!   `.incord/` is the memory engine's and is not touched.
//!
//! Both are idempotent: a second run finds nothing to do, and a run cut short
//! is finished by the next one.

use std::path::{Path, PathBuf};

/// incode's per-project folder, under the workspace.
pub const PROJECT_DIR: &str = ".incode";

/// Where incode kept per-project state before. Incord Memory writes here too.
pub const OLD_PROJECT_DIR: &str = ".incord";

/// incode's machine-wide folder, under the home directory.
pub const GLOBAL_DIR: &str = ".incode";

/// Where incode kept its machine-wide state before.
pub const OLD_GLOBAL_DIR: &str = ".incord-code";

/// The note left in the old machine-wide folder once it has been copied.
pub const MOVED_NOTE: &str = "MOVED_TO_INCODE.txt";

/// Where the machine-wide copy is assembled before it is renamed into place.
const STAGING_DIR: &str = ".incode.migrating";

/// incode's own items in a project's old folder. Anything else there belongs
/// to Incord Memory and stays.
///
/// A test holds this list to every path constant incode writes under
/// [`PROJECT_DIR`], so a new file cannot be added there and forgotten here.
pub const PROJECT_ITEMS: &[&str] = &[
    "ledger.json",
    "brief.cache.json",
    "contracts.json",
    "guards.json",
    "agents",
    "transcripts",
    "checkpoints",
    "context",
    "findings",
    "serve.json",
    "serve.token",
    "run.stop",
    "team.jsonl",
    "desktop.jsonl",
    "desktop.stop",
];

/// What a project migration did, item by item.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectMove {
    /// Moved from `.incord/` to `.incode/`.
    pub moved: Vec<String>,
    /// Left in `.incord/` because `.incode/` already has one.
    pub skipped: Vec<String>,
    /// Could not be moved, with the reason. Left where it was.
    pub failed: Vec<(String, String)>,
}

/// What the machine-wide migration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalCopy {
    /// Nothing to copy: the new folder exists, or there is no old one.
    NotNeeded,
    /// The old folder was copied. `links` counts symbolic links left out.
    Copied { files: usize, links: usize },
    /// The copy failed; the old folder is untouched and the new one absent.
    Failed(String),
}

/// Run the machine-wide migration under the home directory, and return a line
/// for the operator when it did something.
///
/// Only a mode that reads the data directory calls this. A mode that never
/// reads it — `--check-findings`, run as a subprocess by the check machinery
/// and by this crate's own tests — has no business copying it.
#[must_use]
pub fn global_report() -> Vec<String> {
    let mut said = Vec::new();
    if std::env::var_os(crate::admin::DATA_DIR_ENV).is_none() {
        if let Some(home) = crate::admin::home_dir() {
            match global(&home) {
                GlobalCopy::NotNeeded => {}
                GlobalCopy::Copied { files, links } => said.push(format!(
                    "incode: copied {} to {} ({files} file(s){}); the old folder is kept, \
                     and incode no longer reads it.",
                    home.join(OLD_GLOBAL_DIR).display(),
                    home.join(GLOBAL_DIR).display(),
                    if links == 0 {
                        String::new()
                    } else {
                        format!(", {links} symbolic link(s) left out")
                    },
                )),
                GlobalCopy::Failed(why) => said.push(format!(
                    "incode: could not copy {} to {}: {why}. Nothing was changed; \
                     settings start empty until it is copied.",
                    home.join(OLD_GLOBAL_DIR).display(),
                    home.join(GLOBAL_DIR).display(),
                )),
            }
        }
    }
    said
}

/// [`project`], as the lines an operator is shown: one when something moved,
/// one per item that could not be. Skips are traced only — they recur on
/// every start while the old copy stays, and say nothing new.
#[must_use]
pub fn project_report(workspace: &Path) -> Vec<String> {
    let done = project(workspace);
    let mut said = Vec::new();
    if !done.moved.is_empty() {
        said.push(format!(
            "incode: moved {} from {OLD_PROJECT_DIR}/ to {PROJECT_DIR}/ in {}.",
            done.moved.join(", "),
            workspace.display()
        ));
    }
    for (item, why) in &done.failed {
        said.push(format!(
            "incode: could not move {OLD_PROJECT_DIR}/{item} to {PROJECT_DIR}/{item} in {}: {why}. \
             It was left where it was.",
            workspace.display()
        ));
    }
    said
}

/// Move incode's own items from `<workspace>/.incord/` to
/// `<workspace>/.incode/`.
///
/// Per item, never all-or-nothing: a project half moved by an interrupted run
/// is finished by the next. A target that already exists is never replaced.
#[must_use]
pub fn project(workspace: &Path) -> ProjectMove {
    let old = workspace.join(OLD_PROJECT_DIR);
    let new = workspace.join(PROJECT_DIR);
    let mut done = ProjectMove::default();
    if !old.is_dir() {
        return done;
    }
    for item in PROJECT_ITEMS {
        let from = old.join(item);
        let to = new.join(item);
        if std::fs::symlink_metadata(&from).is_err() {
            continue;
        }
        // An empty folder in the way holds nothing — something made it before
        // anything was written there — so it gives way. `remove_dir` refuses
        // a folder with contents, so one filled in the meantime stays.
        if is_empty_dir(&to) {
            let _ = std::fs::remove_dir(&to);
        }
        if std::fs::symlink_metadata(&to).is_ok() {
            crate::trace!(
                "migrate",
                "kept {} — {} already exists",
                from.display(),
                to.display()
            );
            done.skipped.push((*item).to_string());
            continue;
        }
        let moved = std::fs::create_dir_all(&new).and_then(|()| move_one(&from, &to));
        match moved {
            Ok(()) => {
                crate::trace!("migrate", "moved {} -> {}", from.display(), to.display());
                done.moved.push((*item).to_string());
            }
            // Gone between the look and the move: another incode starting in
            // the same workspace moved it first.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::trace!(
                    "migrate",
                    "kept {} — {} appeared",
                    from.display(),
                    to.display()
                );
                done.skipped.push((*item).to_string());
            }
            Err(e) => {
                crate::trace!("migrate", "could not move {}: {e}", from.display());
                done.failed.push(((*item).to_string(), e.to_string()));
            }
        }
    }
    done
}

/// A real directory (not a link to one) with nothing in it.
fn is_empty_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// Move one file or directory, atomically and without replacing anything.
///
/// A file is hard-linked to its new name and then unlinked from the old one:
/// the link is refused when the target exists, where a rename on Windows
/// would silently replace it. Interrupted between the two, both names hold
/// the same file and the next run skips it — nothing is lost. A directory is
/// renamed: no platform lets a rename replace a directory that has contents,
/// and the target was just seen to be absent.
fn move_one(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_dir() {
        return std::fs::rename(from, to);
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // A filesystem without hard links: rename, the target having been
        // checked a moment ago.
        Err(_) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(from, to)
        }
    }
}

/// Copy `<home>/.incord-code/` to `<home>/.incode/` when only the old one
/// exists, and leave a note in the old one saying where it went.
#[must_use]
pub fn global(home: &Path) -> GlobalCopy {
    let old = home.join(OLD_GLOBAL_DIR);
    let new = home.join(GLOBAL_DIR);
    if std::fs::symlink_metadata(&new).is_ok() || !old.is_dir() {
        return GlobalCopy::NotNeeded;
    }
    let staging = home.join(STAGING_DIR);
    // A staging folder here is a copy an earlier run did not finish. It is
    // this code's own, never the operator's, and the old folder is intact.
    if std::fs::symlink_metadata(&staging).is_ok() {
        if let Err(e) = std::fs::remove_dir_all(&staging) {
            return GlobalCopy::Failed(format!(
                "an unfinished copy at {} could not be cleared: {e}",
                staging.display()
            ));
        }
    }
    let (files, links) = match copy_tree(&old, &staging) {
        Ok(counts) => counts,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return GlobalCopy::Failed(e.to_string());
        }
    };
    if let Err(e) = std::fs::rename(&staging, &new) {
        let _ = std::fs::remove_dir_all(&staging);
        // Another incode starting at the same moment finished first.
        if std::fs::symlink_metadata(&new).is_ok() {
            return GlobalCopy::NotNeeded;
        }
        return GlobalCopy::Failed(format!("the copy could not be put in place: {e}"));
    }
    crate::trace!(
        "migrate",
        "copied {} -> {} ({files} files, {links} links left out)",
        old.display(),
        new.display()
    );
    let note = format!(
        "incode now keeps its settings and records in:\n\n    {}\n\n\
         This folder was copied there on {} and is kept, unchanged, as a backup.\n\
         incode no longer reads or writes it; delete it once you no longer need it.\n",
        new.display(),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z"),
    );
    if let Err(e) = std::fs::write(old.join(MOVED_NOTE), note) {
        crate::trace!("migrate", "note not written in {}: {e}", old.display());
    }
    GlobalCopy::Copied { files, links }
}

/// Copy the tree at `from` to a new directory `to`: directories and regular
/// files, leaving symbolic links out (one pointing up the tree would never
/// end). Returns the files copied and the links left out.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<(usize, usize)> {
    let mut files = 0;
    let mut links = 0;
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        std::fs::create_dir(&dst)?;
        for entry in std::fs::read_dir(&src)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let target = dst.join(entry.file_name());
            if kind.is_symlink() {
                links += 1;
            } else if kind.is_dir() {
                stack.push((entry.path(), target));
            } else {
                std::fs::copy(entry.path(), &target)?;
                files += 1;
            }
        }
    }
    Ok((files, links))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read")
    }

    /// A project as incode and Incord Memory both left it: incode's items and
    /// the memory engine's files side by side in `.incord/`.
    fn mixed_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let old = dir.path().join(OLD_PROJECT_DIR);
        write(&old.join("ledger.json"), "{\"tasks\":[]}");
        write(&old.join("contracts.json"), "{\"schemas\":{}}");
        write(&old.join("agents").join("reviewer.md"), "# reviewer");
        write(&old.join("agents").join("t-1.jsonl"), "{}\n");
        write(
            &old.join("checkpoints").join("s1").join("manifest.json"),
            "{}",
        );
        write(&old.join("serve.token"), "secret");
        // The memory engine's.
        write(&old.join("RULE_ACK"), "ack");
        write(&old.join("MEMORY_FIRST_OFF"), "");
        write(&old.join("PROJECT.md"), "# project");
        write(&old.join("backups").join("Cargo.toml.1.bak"), "[package]");
        dir
    }

    #[test]
    fn incodes_items_move_and_the_memory_engines_files_stay() {
        let dir = mixed_project();
        let old = dir.path().join(OLD_PROJECT_DIR);
        let new = dir.path().join(PROJECT_DIR);

        let done = project(dir.path());

        assert_eq!(
            done.moved,
            [
                "ledger.json",
                "contracts.json",
                "agents",
                "checkpoints",
                "serve.token"
            ]
        );
        assert!(
            done.skipped.is_empty() && done.failed.is_empty(),
            "{done:?}"
        );
        assert_eq!(read(&new.join("ledger.json")), "{\"tasks\":[]}");
        assert_eq!(read(&new.join("agents").join("reviewer.md")), "# reviewer");
        assert_eq!(read(&new.join("agents").join("t-1.jsonl")), "{}\n");
        assert!(new
            .join("checkpoints")
            .join("s1")
            .join("manifest.json")
            .is_file());
        assert_eq!(read(&new.join("serve.token")), "secret");
        for gone in [
            "ledger.json",
            "contracts.json",
            "agents",
            "checkpoints",
            "serve.token",
        ] {
            assert!(!old.join(gone).exists(), "{gone} left behind in .incord");
        }
        // Every file of the memory engine's is where it was, as it was.
        assert_eq!(read(&old.join("RULE_ACK")), "ack");
        assert!(old.join("MEMORY_FIRST_OFF").is_file());
        assert_eq!(read(&old.join("PROJECT.md")), "# project");
        assert_eq!(
            read(&old.join("backups").join("Cargo.toml.1.bak")),
            "[package]"
        );
        assert!(!new.join("RULE_ACK").exists() && !new.join("backups").exists());
    }

    #[test]
    fn a_second_run_does_nothing() {
        let dir = mixed_project();
        let first = project(dir.path());
        assert!(!first.moved.is_empty());

        let second = project(dir.path());

        assert_eq!(second, ProjectMove::default());
        assert!(project_report(dir.path()).is_empty());
    }

    /// Half moved — an interrupted run, or an item incode has already written
    /// fresh under the new name. What is missing moves; what exists stays, on
    /// both sides, byte for byte.
    #[test]
    fn a_partly_migrated_project_is_finished_without_replacing_anything() {
        let dir = mixed_project();
        let old = dir.path().join(OLD_PROJECT_DIR);
        let new = dir.path().join(PROJECT_DIR);
        write(&new.join("ledger.json"), "{\"tasks\":[\"newer\"]}");
        write(&new.join("agents").join("other.md"), "# other");

        let done = project(dir.path());

        assert_eq!(done.moved, ["contracts.json", "checkpoints", "serve.token"]);
        assert_eq!(done.skipped, ["ledger.json", "agents"]);
        assert!(done.failed.is_empty(), "{done:?}");
        assert_eq!(read(&new.join("ledger.json")), "{\"tasks\":[\"newer\"]}");
        assert_eq!(read(&old.join("ledger.json")), "{\"tasks\":[]}");
        assert!(!new.join("agents").join("reviewer.md").exists());
        assert_eq!(read(&old.join("agents").join("reviewer.md")), "# reviewer");
        assert_eq!(read(&new.join("contracts.json")), "{\"schemas\":{}}");
    }

    /// An empty folder under the new name — made by something that ran before
    /// anything was written there — holds nothing, so it does not keep the
    /// real one stranded under the old name.
    #[test]
    fn an_empty_folder_in_the_way_gives_way() {
        let dir = mixed_project();
        let new = dir.path().join(PROJECT_DIR);
        std::fs::create_dir_all(new.join("agents")).expect("mkdir");

        let done = project(dir.path());

        assert!(done.moved.contains(&"agents".to_string()), "{done:?}");
        assert_eq!(read(&new.join("agents").join("reviewer.md")), "# reviewer");
        assert!(!dir.path().join(OLD_PROJECT_DIR).join("agents").exists());
    }

    #[test]
    fn a_project_without_the_old_folder_is_left_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(project(dir.path()), ProjectMove::default());
        assert!(
            !dir.path().join(PROJECT_DIR).exists(),
            "created for nothing"
        );
    }

    /// A `.incord/` holding only the memory engine's files: nothing is
    /// incode's, so nothing moves and no `.incode/` is made.
    #[test]
    fn a_memory_only_folder_creates_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        write(&dir.path().join(OLD_PROJECT_DIR).join("RULE_ACK"), "ack");
        assert_eq!(project(dir.path()), ProjectMove::default());
        assert!(!dir.path().join(PROJECT_DIR).exists());
    }

    /// Every path incode writes under its project folder is on the list, so a
    /// file added there later is migrated too — or this fails and says which.
    #[test]
    fn every_project_path_incode_writes_is_on_the_list() {
        let desktop_stop = crate::desktop::safety::stop_path_in(Path::new(""));
        let paths: Vec<String> = vec![
            crate::brief::CACHE_FILE.to_string(),
            crate::contracts::CONTRACTS_FILE.to_string(),
            crate::guards::GUARDS_FILE.to_string(),
            crate::findings::DIR.to_string(),
            crate::desktop::journal::FILE.to_string(),
            desktop_stop.to_string_lossy().replace('\\', "/"),
            crate::team::FILE.to_string(),
            crate::transcript::DIR.to_string(),
            crate::subagents::PROJECT_DIR.to_string(),
            crate::hooks::memory::TRANSCRIPTS.to_string(),
            crate::tools::checkpoint::CHECKPOINTS_DIR.to_string(),
            format!(
                "{}/{}",
                crate::ledger::LEDGER_DIR,
                crate::ledger::LEDGER_FILE
            ),
            format!(
                "{}/{}",
                crate::ledger::LEDGER_DIR,
                crate::runtime::cancel::STOP_FILE
            ),
            format!(
                "{}/{}",
                crate::ledger::LEDGER_DIR,
                crate::serve::http::ADDRESS_FILE
            ),
            format!(
                "{}/{}",
                crate::ledger::LEDGER_DIR,
                crate::serve::http::TOKEN_FILE
            ),
        ];
        for path in paths {
            let (dir, rest) = path.split_once('/').expect("a path under a folder");
            assert_eq!(dir, PROJECT_DIR, "{path} is not under {PROJECT_DIR}/");
            let item = rest.split('/').next().expect("an item");
            assert!(
                PROJECT_ITEMS.contains(&item),
                "{path}: `{item}` is written under {PROJECT_DIR}/ but not in PROJECT_ITEMS"
            );
        }
    }

    /// A home with only the old folder: copied whole, the old one kept intact
    /// and told where its contents went.
    #[test]
    fn the_global_folder_is_copied_and_the_old_one_kept() {
        let home = tempfile::tempdir().expect("temp dir");
        let old = home.path().join(OLD_GLOBAL_DIR);
        write(&old.join("admin.json"), "{\"agent\":\"claude-code\"}");
        write(&old.join("history.txt"), "hello\n");
        write(&old.join("sessions").join("C--work.json"), "{}");
        write(
            &old.join("tools").join("browser").join("1.0").join("b.exe"),
            "MZ",
        );

        let done = global(home.path());

        assert_eq!(done, GlobalCopy::Copied { files: 4, links: 0 });
        let new = home.path().join(GLOBAL_DIR);
        assert_eq!(read(&new.join("admin.json")), "{\"agent\":\"claude-code\"}");
        assert_eq!(read(&new.join("sessions").join("C--work.json")), "{}");
        assert_eq!(
            read(&new.join("tools").join("browser").join("1.0").join("b.exe")),
            "MZ"
        );
        assert!(
            !new.join(MOVED_NOTE).exists(),
            "the note belongs in the old folder"
        );
        // The old folder: everything still there, plus the note.
        assert_eq!(read(&old.join("admin.json")), "{\"agent\":\"claude-code\"}");
        assert_eq!(read(&old.join("history.txt")), "hello\n");
        assert!(read(&old.join(MOVED_NOTE)).contains(&new.display().to_string()));
        assert!(
            !home.path().join(STAGING_DIR).exists(),
            "staging left behind"
        );
    }

    #[test]
    fn the_global_copy_runs_once() {
        let home = tempfile::tempdir().expect("temp dir");
        write(&home.path().join(OLD_GLOBAL_DIR).join("admin.json"), "{}");
        assert!(matches!(global(home.path()), GlobalCopy::Copied { .. }));
        // Changed after the copy: a second run must not copy over it.
        write(
            &home.path().join(GLOBAL_DIR).join("admin.json"),
            "{\"agent\":\"x\"}",
        );

        assert_eq!(global(home.path()), GlobalCopy::NotNeeded);
        assert_eq!(
            read(&home.path().join(GLOBAL_DIR).join("admin.json")),
            "{\"agent\":\"x\"}"
        );
    }

    #[test]
    fn an_existing_new_folder_is_never_touched() {
        let home = tempfile::tempdir().expect("temp dir");
        write(
            &home.path().join(OLD_GLOBAL_DIR).join("admin.json"),
            "{\"old\":1}",
        );
        write(
            &home.path().join(GLOBAL_DIR).join("admin.json"),
            "{\"new\":1}",
        );

        assert_eq!(global(home.path()), GlobalCopy::NotNeeded);
        assert_eq!(
            read(&home.path().join(GLOBAL_DIR).join("admin.json")),
            "{\"new\":1}"
        );
        assert!(!home.path().join(OLD_GLOBAL_DIR).join(MOVED_NOTE).exists());
    }

    #[test]
    fn no_old_folder_means_nothing_is_made() {
        let home = tempfile::tempdir().expect("temp dir");
        assert_eq!(global(home.path()), GlobalCopy::NotNeeded);
        assert!(!home.path().join(GLOBAL_DIR).exists());
    }

    /// A copy cut short leaves the staging folder behind and no new folder;
    /// the next start throws the half copy away and makes a whole one.
    #[test]
    fn an_unfinished_copy_is_redone_whole() {
        let home = tempfile::tempdir().expect("temp dir");
        write(&home.path().join(OLD_GLOBAL_DIR).join("admin.json"), "{}");
        write(&home.path().join(OLD_GLOBAL_DIR).join("history.txt"), "x");
        write(&home.path().join(STAGING_DIR).join("admin.json"), "half");

        assert_eq!(
            global(home.path()),
            GlobalCopy::Copied { files: 2, links: 0 }
        );
        assert_eq!(read(&home.path().join(GLOBAL_DIR).join("admin.json")), "{}");
        assert!(!home.path().join(STAGING_DIR).exists());
    }
}
