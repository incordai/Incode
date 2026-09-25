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

// ---------------------------------------------------------------------------
// Reading and applying a unified diff
// ---------------------------------------------------------------------------

/// One file's part of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// The path before, or `None` for a file the patch creates.
    pub old_path: Option<String>,
    /// The path after, or `None` for a file the patch deletes.
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

impl FilePatch {
    /// The file this patch acts on: its new path, or the old one for a
    /// deletion.
    #[must_use]
    pub fn target(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

/// One `@@` hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Where the old side starts, 1-based; 0 for a hunk into an empty file.
    pub old_start: usize,
    pub old_len: usize,
    pub new_len: usize,
    pub lines: Vec<HunkLine>,
    /// `\ No newline at end of file` followed the old side's last line.
    pub old_no_newline: bool,
    /// `\ No newline at end of file` followed the new side's last line.
    pub new_no_newline: bool,
}

/// One line of a hunk, without its prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    Context(String),
    Removed(String),
    Added(String),
}

/// A unified diff (`--- a/x`, `+++ b/x`, `@@ -l,c +l,c @@`) read into
/// per-file patches.
///
/// `a/` and `b/` prefixes are dropped, `/dev/null` names a created or deleted
/// file, and git's header lines (`diff --git`, `index`, file modes) are
/// skipped. A rename, a copy or a binary patch is refused by name: applying
/// one partly would be worse than not at all.
///
/// # Errors
///
/// The text holds no file patch, a hunk's line counts do not match its
/// header, or a file is named twice.
pub fn parse_unified(text: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut patches: Vec<FilePatch> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        for refused in ["rename from", "rename to", "copy from", "copy to"] {
            if line.starts_with(refused) {
                return Err(format!(
                    "`{line}`: renames and copies are not applied; delete the old file and \
                     add the new one instead"
                ));
            }
        }
        if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
            return Err(format!("`{line}`: binary patches are not applied"));
        }
        let Some(old) = line.strip_prefix("--- ") else {
            i += 1;
            continue;
        };
        let Some(new) = lines.get(i + 1).and_then(|l| l.strip_prefix("+++ ")) else {
            return Err(format!(
                "line {}: `--- {old}` is not followed by a `+++` line",
                i + 1
            ));
        };
        let mut patch = FilePatch {
            old_path: patch_path(old, "a/"),
            new_path: patch_path(new, "b/"),
            hunks: Vec::new(),
        };
        if patch.old_path.is_none() && patch.new_path.is_none() {
            return Err(format!("line {}: both sides are /dev/null", i + 1));
        }
        i += 2;
        while let Some(header) = lines.get(i).filter(|l| l.starts_with("@@")) {
            let (hunk, next) = read_hunk(&lines, i, header, patch.target())?;
            patch.hunks.push(hunk);
            i = next;
        }
        if patch.hunks.is_empty() {
            return Err(format!("`{}` has no `@@` hunk", patch.target()));
        }
        if patches.iter().any(|p| p.target() == patch.target()) {
            return Err(format!(
                "`{}` is patched twice; put all its hunks under one header",
                patch.target()
            ));
        }
        patches.push(patch);
    }
    if patches.is_empty() {
        return Err(
            "no file patch was found: a unified diff needs `---`, `+++` and `@@` lines".to_string(),
        );
    }
    Ok(patches)
}

/// The path a `---`/`+++` line names: timestamps cut, `prefix` dropped,
/// `None` for `/dev/null`.
fn patch_path(raw: &str, prefix: &str) -> Option<String> {
    let path = raw.split('\t').next().unwrap_or_default().trim();
    if path == "/dev/null" || path.is_empty() {
        return None;
    }
    Some(path.strip_prefix(prefix).unwrap_or(path).to_string())
}

/// The numbers in `-l,c` or `+l,c`; a missing count is 1.
fn range(text: &str) -> Option<(usize, usize)> {
    let (start, len) = match text.split_once(',') {
        Some((s, l)) => (s.parse().ok()?, l.parse().ok()?),
        None => (text.parse().ok()?, 1),
    };
    Some((start, len))
}

/// Read the hunk whose header is at `lines[at]`; returns it and the index
/// of the line after it.
fn read_hunk(lines: &[&str], at: usize, header: &str, file: &str) -> Result<(Hunk, usize), String> {
    let bad = || format!("`{file}`: `{header}` is not a hunk header like `@@ -1,3 +1,4 @@`");
    let inner = header
        .strip_prefix("@@ ")
        .and_then(|rest| rest.split(" @@").next())
        .ok_or_else(bad)?;
    let mut parts = inner.split_whitespace();
    let old = parts
        .next()
        .and_then(|p| p.strip_prefix('-'))
        .ok_or_else(bad)?;
    let new = parts
        .next()
        .and_then(|p| p.strip_prefix('+'))
        .ok_or_else(bad)?;
    let (old_start, old_len) = range(old).ok_or_else(bad)?;
    let (_, new_len) = range(new).ok_or_else(bad)?;

    let mut hunk = Hunk {
        old_start,
        old_len,
        new_len,
        lines: Vec::new(),
        old_no_newline: false,
        new_no_newline: false,
    };
    let (mut old_seen, mut new_seen) = (0, 0);
    let mut i = at + 1;
    while old_seen < old_len || new_seen < new_len {
        let Some(line) = lines.get(i) else {
            return Err(format!(
                "`{file}`: `{header}` ends early — {} old and {} new line(s) missing",
                old_len - old_seen,
                new_len - new_seen
            ));
        };
        match line.chars().next() {
            Some(' ') | None => {
                hunk.lines
                    .push(HunkLine::Context(line.get(1..).unwrap_or("").to_string()));
                old_seen += 1;
                new_seen += 1;
            }
            Some('-') => {
                hunk.lines.push(HunkLine::Removed(line[1..].to_string()));
                old_seen += 1;
            }
            Some('+') => {
                hunk.lines.push(HunkLine::Added(line[1..].to_string()));
                new_seen += 1;
            }
            Some('\\') => mark_no_newline(&mut hunk),
            Some(_) => {
                return Err(format!(
                    "`{file}`: in `{header}`, line `{line}` is neither context, removal nor \
                     addition — {} old and {} new line(s) were still expected",
                    old_len - old_seen,
                    new_len - new_seen
                ))
            }
        }
        i += 1;
    }
    if old_seen != old_len || new_seen != new_len {
        return Err(format!(
            "`{file}`: `{header}` holds {old_seen} old and {new_seen} new line(s), not the \
             {old_len} and {new_len} it says"
        ));
    }
    if lines.get(i).is_some_and(|l| l.starts_with('\\')) {
        mark_no_newline(&mut hunk);
        i += 1;
    }
    Ok((hunk, i))
}

/// `\ No newline at end of file`: it applies to the line before it.
fn mark_no_newline(hunk: &mut Hunk) {
    match hunk.lines.last() {
        Some(HunkLine::Removed(_)) => hunk.old_no_newline = true,
        Some(HunkLine::Added(_)) => hunk.new_no_newline = true,
        Some(HunkLine::Context(_)) => {
            hunk.old_no_newline = true;
            hunk.new_no_newline = true;
        }
        None => {}
    }
}

/// `old` with every hunk applied, in order, or why one does not fit.
///
/// Each hunk is placed where its old side — context and removals — matches
/// the file, nearest to the line its header names; a file edited since the
/// diff was made still takes it when the lines around the change are
/// unchanged. The file's line ending (`\r\n` or `\n`) is kept.
///
/// # Errors
///
/// A hunk's old side is found nowhere after the previous hunk.
pub fn apply_hunks(old: &str, hunks: &[Hunk]) -> Result<String, String> {
    let eol = if old.contains("\r\n") { "\r\n" } else { "\n" };
    let old_lines: Vec<&str> = old.lines().collect();
    let old_ends_in_newline = old.is_empty() || old.ends_with('\n');
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let mut shift: isize = 0;
    let mut reached_end = false;
    let mut last_new_no_newline = false;
    let mut last_old_no_newline = false;

    for (n, hunk) in hunks.iter().enumerate() {
        let before: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(t) | HunkLine::Removed(t) => Some(t.as_str()),
                HunkLine::Added(_) => None,
            })
            .collect();
        let after = hunk.lines.iter().filter_map(|l| match l {
            HunkLine::Context(t) | HunkLine::Added(t) => Some(t.clone()),
            HunkLine::Removed(_) => None,
        });
        let stated = if hunk.old_len == 0 {
            hunk.old_start
        } else {
            hunk.old_start.saturating_sub(1)
        };
        let expected = stated.saturating_add_signed(shift);
        let at = if before.is_empty() {
            expected.clamp(cursor, old_lines.len())
        } else {
            (cursor..=old_lines.len())
                .filter(|&p| {
                    old_lines
                        .get(p..p + before.len())
                        .is_some_and(|window| window == &before[..])
                })
                .min_by_key(|&p| p.abs_diff(expected))
                .ok_or_else(|| {
                    format!(
                        "hunk {} (`@@ -{},{}`) does not match the file: its lines starting \
                         `{}` were not found{}",
                        n + 1,
                        hunk.old_start,
                        hunk.old_len,
                        before[0],
                        if n > 0 {
                            " after the previous hunk"
                        } else {
                            ""
                        }
                    )
                })?
        };
        out.extend(old_lines[cursor..at].iter().map(|l| (*l).to_string()));
        out.extend(after);
        cursor = at + before.len();
        shift = at as isize - stated as isize;
        reached_end = cursor == old_lines.len();
        last_new_no_newline = hunk.new_no_newline;
        last_old_no_newline = hunk.old_no_newline;
    }
    out.extend(old_lines[cursor..].iter().map(|l| (*l).to_string()));

    let ends_in_newline = if reached_end {
        if last_new_no_newline {
            false
        } else {
            last_old_no_newline || old_ends_in_newline
        }
    } else {
        old_ends_in_newline
    };
    if out.is_empty() {
        return Ok(String::new());
    }
    let mut text = out.join(eol);
    if ends_in_newline {
        text.push_str(eol);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FILES: &str = "\
diff --git a/src/one.txt b/src/one.txt
index 111..222 100644
--- a/src/one.txt
+++ b/src/one.txt
@@ -1,3 +1,3 @@
 alpha
-beta
+BETA
 gamma
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+fresh
+file
";

    #[test]
    fn a_two_file_patch_is_read_whole() {
        let patches = parse_unified(TWO_FILES).expect("parses");
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].target(), "src/one.txt");
        assert_eq!(patches[1].old_path, None, "a created file");
        assert_eq!(patches[1].target(), "new.txt");
        assert_eq!(
            apply_hunks("alpha\nbeta\ngamma\n", &patches[0].hunks).expect("fits"),
            "alpha\nBETA\ngamma\n"
        );
        assert_eq!(
            apply_hunks("", &patches[1].hunks).expect("fits"),
            "fresh\nfile\n"
        );
    }

    #[test]
    fn a_hunk_is_found_where_the_file_moved_it() {
        let patch =
            parse_unified("--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n x\n-y\n+Y\n").expect("parses");
        let moved = "new first line\nx\ny\nz\n";
        assert_eq!(
            apply_hunks(moved, &patch[0].hunks).expect("found lower down"),
            "new first line\nx\nY\nz\n"
        );
    }

    #[test]
    fn a_hunk_that_does_not_fit_says_which() {
        let patch =
            parse_unified("--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-absent\n+x\n").expect("parses");
        let refused = apply_hunks("present\n", &patch[0].hunks).expect_err("no match");
        assert!(refused.contains("hunk 1"), "{refused}");
        assert!(refused.contains("absent"), "{refused}");
    }

    #[test]
    fn counts_that_disagree_with_the_header_are_refused() {
        let refused =
            parse_unified("--- a/f\n+++ b/f\n@@ -1,3 +1,1 @@\n-a\n+b\n").expect_err("short");
        assert!(refused.contains("ends early"), "{refused}");
    }

    #[test]
    fn crlf_and_a_missing_final_newline_are_kept() {
        let patch = parse_unified(
            "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n one\n-two\n\\ No newline at end of file\n+TWO\n\\ No newline at end of file\n",
        )
        .expect("parses");
        assert_eq!(
            apply_hunks("one\r\ntwo", &patch[0].hunks).expect("fits"),
            "one\r\nTWO"
        );
    }

    #[test]
    fn renames_and_binary_patches_are_refused_by_name() {
        assert!(parse_unified("rename from a\nrename to b\n").is_err());
        assert!(parse_unified("Binary files a/x and b/x differ\n").is_err());
        assert!(parse_unified("just some text\n").is_err());
    }

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
