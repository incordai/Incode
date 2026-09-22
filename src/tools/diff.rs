//! Code diffing utilities for agents and operators.
//!
//! Provides unified diffing across revisions, files, and snapshots so that an
//! agent or operator can inspect how code changed from older versions.

use similar::{ChangeTag, TextDiff};
use std::path::Path;

/// Compute a unified diff between old text and new text.
///
/// Returns empty string if there are no changes.
#[must_use]
pub fn unified_diff(old: &str, new: &str, file_label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut has_changes = false;
    let mut body = String::new();

    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => {
                has_changes = true;
                "-"
            }
            ChangeTag::Insert => {
                has_changes = true;
                "+"
            }
            ChangeTag::Equal => " ",
        };
        body.push_str(&format!("{prefix}{change}"));
    }

    if !has_changes {
        return String::new();
    }

    format!("--- a/{file_label}\n+++ b/{file_label}\n{body}")
}

/// Compute a unified diff between two files on disk.
pub fn diff_files(old_path: &Path, new_path: &Path) -> Result<String, String> {
    let old_content = std::fs::read_to_string(old_path)
        .map_err(|e| format!("Failed to read {}: {e}", old_path.display()))?;
    let new_content = std::fs::read_to_string(new_path)
        .map_err(|e| format!("Failed to read {}: {e}", new_path.display()))?;
    let label = new_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    Ok(unified_diff(&old_content, &new_content, &label))
}

/// Run a git diff for a specific file against a revision (or HEAD by default).
pub fn git_diff_file(
    workspace: &Path,
    file_path: &str,
    rev: Option<&str>,
) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(workspace).arg("diff");
    if let Some(r) = rev {
        cmd.arg(r);
    }
    cmd.arg("--").arg(file_path);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to execute git diff: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git diff failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run a git diff for the entire workspace against a revision (or HEAD by default).
pub fn git_diff_workspace(workspace: &Path, rev: Option<&str>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(workspace).arg("diff");
    if let Some(r) = rev {
        cmd.arg(r);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to execute git diff: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git diff failed: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Tally the added and removed lines from a unified diff string.
#[must_use]
pub fn diff_stats(diff_text: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff_text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_yields_empty_diff() {
        let text = "fn main() {\n    println!(\"hello\");\n}\n";
        let diff = unified_diff(text, text, "src/main.rs");
        assert!(diff.is_empty(), "identical content should yield no diff");
        assert_eq!(diff_stats(&diff), (0, 0));
    }

    #[test]
    fn line_modifications_are_detected() {
        let old = "fn main() {\n    println!(\"hello\");\n}\n";
        let new = "fn main() {\n    println!(\"world\");\n    return;\n}\n";
        let diff = unified_diff(old, new, "src/main.rs");

        assert!(diff.contains("--- a/src/main.rs"), "must have diff header");
        assert!(diff.contains("+++ b/src/main.rs"), "must have diff header");
        assert!(diff.contains("-    println!(\"hello\");"));
        assert!(diff.contains("+    println!(\"world\");"));
        assert!(diff.contains("+    return;"));

        let (added, removed) = diff_stats(&diff);
        assert_eq!(added, 2);
        assert_eq!(removed, 1);
    }

    #[test]
    fn diffing_files_on_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file_a = dir.path().join("a.rs");
        let file_b = dir.path().join("b.rs");

        std::fs::write(&file_a, "line 1\nline 2\n").expect("write a");
        std::fs::write(&file_b, "line 1\nline 2 modified\nline 3\n").expect("write b");

        let diff = diff_files(&file_a, &file_b).expect("diff succeeded");
        assert!(diff.contains("-line 2\n"));
        assert!(diff.contains("+line 2 modified\n"));
        assert!(diff.contains("+line 3\n"));

        let (added, removed) = diff_stats(&diff);
        assert_eq!(added, 2);
        assert_eq!(removed, 1);
    }

    #[test]
    fn diff_nonexistent_file_returns_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file_a = dir.path().join("does_not_exist.rs");
        let file_b = dir.path().join("b.rs");
        std::fs::write(&file_b, "hello").expect("write b");

        let err = diff_files(&file_a, &file_b).expect_err("should fail");
        assert!(err.contains("Failed to read"));
    }
}
