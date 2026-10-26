//! Which checkout a folder is: a linked git worktree belongs to its main one.
//!
//! # Wired to
//!
//! - **Called by** [`crate::brief::brief_for`], so a worktree is covered by
//!   its main checkout's brief (read by `brief::check`, written through
//!   `brief::canonical_project_for`).
//! - **Calls** nothing but the filesystem: it reads the files git writes
//!   rather than running git.
//! - **Sibling** to the engine's own resolution (`layout/resolve.rs`
//!   `checkout_project` in the memory service), which joins a worktree to its
//!   main checkout's project the same way, back-pointer check included. The
//!   two must agree, or a worktree's brief and its memory would be two projects.

use std::path::{Component, Path, PathBuf};

/// The main checkout a linked git worktree belongs to, or `None` for a folder
/// that is not one — a main checkout, a bare repository, or no repository.
///
/// A linked worktree's `.git` is a FILE naming its private git dir
/// (`gitdir: <main>/.git/worktrees/<name>`); that dir's `commondir` names the
/// shared one (`../..`), whose parent is the main checkout. The private dir's
/// own `gitdir` file must point back at THIS worktree, so a stray `.git` file
/// cannot claim another project as its own.
#[must_use]
pub fn main_checkout(workspace: &Path) -> Option<PathBuf> {
    let dot_git = workspace.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let private = pointer
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|p| !p.is_empty())?;
    let private = workspace.join(private);
    let back = std::fs::read_to_string(private.join("gitdir")).ok()?;
    if !same_file(&private.join(back.trim()), &dot_git) {
        return None;
    }
    let common = std::fs::read_to_string(private.join("commondir")).ok()?;
    // Resolved by its own `..`s, NOT canonicalised: a brief's `project` is the
    // folder as a person typed it, which is also how git wrote it in `gitdir:`,
    // and canonicalising would rewrite it (`\\?\`, short names, links) into a
    // spelling nothing is filed under.
    let common = lexical(&private.join(common.trim()));
    // A bare repository's common dir is the repository itself, with no
    // checkout around it.
    if common.file_name()? != ".git" || !common.is_dir() {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

/// Whether two paths name one file or folder, compared after resolving each
/// — so `/var/x` and `/private/var/x`, a short and a long Windows name, or a
/// path through `..` all count as the same place. Both must exist.
///
/// Called by [`crate::brief::brief_for`] when a brief's spelling of the main
/// checkout differs from git's, and by [`main_checkout`]'s back-pointer check.
#[must_use]
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `path` with its `.` and `..` components applied, without touching the
/// filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A main checkout and a linked worktree of it, made by git itself, so the
    /// files read are the ones git really writes.
    fn repo_with_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let main = root.join("game");
        std::fs::create_dir_all(&main).expect("main dir");
        git(&main, &["init", "-q"]);
        git(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first",
            ],
        );
        let worktree = root.join("game-worktrees").join("mc-1");
        git(
            &main,
            &["worktree", "add", "-q", &worktree.to_string_lossy()],
        );
        (main, worktree)
    }

    #[test]
    fn a_worktree_resolves_to_its_main_checkout() {
        let root = tempfile::tempdir().expect("tmp");
        let (main, worktree) = repo_with_worktree(root.path());
        let found = main_checkout(&worktree).expect("a linked worktree has a main checkout");
        assert!(
            same_file(&found, &main),
            "{} is not {}",
            found.display(),
            main.display()
        );
        assert!(
            main_checkout(&main).is_none(),
            "a main checkout is not a worktree"
        );
        assert!(
            main_checkout(root.path()).is_none(),
            "nor is a folder with no repository"
        );
    }

    /// The brief of the main checkout covers its worktree — the folder name
    /// `mc-1` matches no brief, so without this it read as having none.
    #[test]
    fn a_worktree_is_covered_by_its_main_checkouts_brief() {
        let root = tempfile::tempdir().expect("tmp");
        let (main, worktree) = repo_with_worktree(root.path());
        let briefs = vec![crate::brief::ProjectBrief {
            project: main.display().to_string(),
            about: "a ball game".to_string(),
            ..Default::default()
        }];
        let found = crate::brief::brief_for(&briefs, &worktree).map(|b| b.about.as_str());
        assert_eq!(found, Some("a ball game"));
        assert_eq!(
            crate::brief::canonical_project_for(&briefs, &worktree),
            main.display().to_string(),
            "a brief saved from the worktree completes the main checkout's"
        );
    }

    /// A brief that names the main checkout by ANOTHER SPELLING of the same
    /// folder still covers its worktree. CI found it: git records the folder
    /// as it resolves it — `/private/var/…` behind macOS's `/var` link, the
    /// long name behind a Windows `RUNNER~1` — while the brief keeps the
    /// spelling it was written with. `..` reproduces that on every system.
    #[test]
    fn a_brief_spelled_differently_still_covers_the_worktree() {
        let root = tempfile::tempdir().expect("tmp");
        let (_main, worktree) = repo_with_worktree(root.path());
        let other_spelling = root.path().join("game-worktrees").join("..").join("game");
        let briefs = vec![crate::brief::ProjectBrief {
            project: other_spelling.display().to_string(),
            about: "a ball game".to_string(),
            ..Default::default()
        }];
        let found = crate::brief::brief_for(&briefs, &worktree).map(|b| b.about.as_str());
        assert_eq!(found, Some("a ball game"));
    }

    /// A `.git` file pointing at another worktree's private dir is refused: that
    /// dir's back-pointer names the real worktree, not this folder.
    #[test]
    fn a_forged_pointer_does_not_claim_another_checkout() {
        let root = tempfile::tempdir().expect("tmp");
        let (main, _worktree) = repo_with_worktree(root.path());
        let forged = root.path().join("elsewhere");
        std::fs::create_dir_all(&forged).expect("dir");
        let private = main.join(".git").join("worktrees").join("mc-1");
        std::fs::write(
            forged.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .expect("write");
        assert!(main_checkout(&forged).is_none());
    }

    #[test]
    fn dot_dots_are_applied_without_the_filesystem() {
        assert_eq!(
            lexical(Path::new("D:/ball-game/.git/worktrees/mc-1/../..")),
            PathBuf::from("D:/ball-game/.git")
        );
    }
}
