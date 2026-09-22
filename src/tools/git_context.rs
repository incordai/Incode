//! Git context detection — branch, dirty status, short summary.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Git repository context detected from the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitContext {
    pub branch: String,
    pub is_dirty: bool,
    pub has_staged: bool,
    pub short_status: String,
}

impl GitContext {
    /// Format a one-line summary for display (e.g., "main (dirty)").
    pub fn summary(&self) -> String {
        let mut s = self.branch.clone();
        if self.is_dirty {
            s.push_str(" (dirty)");
        }
        if !self.short_status.is_empty() {
            s.push_str(&format!(" — {}", self.short_status));
        }
        s
    }
}

/// Detect git context by walking up from `workspace` looking for `.git/`.
///
/// Returns `None` if not inside a git repo.
pub fn detect_git_context(workspace: &Path) -> Option<GitContext> {
    let git_dir = find_git_dir(workspace)?;

    let branch = read_branch(&git_dir).unwrap_or_else(|| "HEAD".to_string());

    // Run `git status --porcelain` to detect dirty/staged state
    let (is_dirty, has_staged, short_status) = parse_git_status(workspace);

    Some(GitContext {
        branch,
        is_dirty,
        has_staged,
        short_status,
    })
}

/// Walk up from `start` looking for a `.git` directory.
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let git_path = current.join(".git");
        if git_path.is_dir() {
            return Some(git_path);
        }
        // .git can also be a file (worktrees)
        if git_path.is_file() {
            return Some(git_path);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Read the current branch name from `.git/HEAD`.
fn read_branch(git_dir: &Path) -> Option<String> {
    let head_path = git_dir.join("HEAD");
    let content = std::fs::read_to_string(head_path).ok()?;
    let trimmed = content.trim();

    // Format: "ref: refs/heads/branch-name"
    if let Some(ref_path) = trimmed.strip_prefix("ref: refs/heads/") {
        return Some(ref_path.to_string());
    }

    // Detached HEAD — return short hash
    if trimmed.len() >= 8 {
        return Some(trimmed[..8].to_string());
    }

    None
}

/// Run `git status --porcelain` and parse the output.
fn parse_git_status(workspace: &Path) -> (bool, bool, String) {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    let Ok(output) = output else {
        return (false, false, String::new());
    };

    let text = String::from_utf8_lossy(&output.stdout);
    if text.is_empty() {
        return (false, false, String::new());
    }

    let mut modified = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;
    let mut untracked = 0u32;
    let mut staged = false;

    for line in text.lines() {
        if line.len() < 2 {
            continue;
        }
        let index_char = line.as_bytes()[0];
        let worktree_char = line.as_bytes()[1];

        // Index column (first char): non-space and non-? means staged
        if index_char != b' ' && index_char != b'?' {
            staged = true;
        }

        match (index_char, worktree_char) {
            (b'?', b'?') => untracked += 1,
            (_, b'M') | (b'M', _) => modified += 1,
            (b'A', _) | (_, b'A') => added += 1,
            (b'D', _) | (_, b'D') => deleted += 1,
            _ => modified += 1,
        }
    }

    let is_dirty = modified > 0 || added > 0 || deleted > 0 || untracked > 0;

    let mut parts = Vec::new();
    if modified > 0 {
        parts.push(format!("{modified} modified"));
    }
    if added > 0 {
        parts.push(format!("{added} added"));
    }
    if deleted > 0 {
        parts.push(format!("{deleted} deleted"));
    }
    if untracked > 0 {
        parts.push(format!("{untracked} untracked"));
    }

    (is_dirty, staged, parts.join(", "))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_branch_from_ref() {
        let dir = tempfile::tempdir().unwrap();
        let head = dir.path().join("HEAD");
        std::fs::write(&head, "ref: refs/heads/feature/cool-stuff\n").unwrap();
        assert_eq!(
            read_branch(dir.path()),
            Some("feature/cool-stuff".to_string())
        );
    }

    #[test]
    fn parse_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        let head = dir.path().join("HEAD");
        std::fs::write(&head, "abc1234567890def\n").unwrap();
        assert_eq!(read_branch(dir.path()), Some("abc12345".to_string()));
    }

    #[test]
    fn git_context_summary_clean() {
        let ctx = GitContext {
            branch: "main".to_string(),
            is_dirty: false,
            has_staged: false,
            short_status: String::new(),
        };
        assert_eq!(ctx.summary(), "main");
    }

    #[test]
    fn git_context_summary_dirty() {
        let ctx = GitContext {
            branch: "dev".to_string(),
            is_dirty: true,
            has_staged: false,
            short_status: "2 modified".to_string(),
        };
        assert_eq!(ctx.summary(), "dev (dirty) — 2 modified");
    }
}
