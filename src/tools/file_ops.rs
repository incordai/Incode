//! File operation tools — read, write, edit, glob, grep.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Read a file, optionally with offset and limit.
pub fn read_file(workspace: &Path, input: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({"path": input}));

    let path_str = parsed["path"].as_str().context("missing 'path' field")?;
    let path = resolve_path(workspace, path_str)?;

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let offset = parsed["offset"].as_u64().unwrap_or(0) as usize;
    let limit = parsed["limit"].as_u64().map(|l| l as usize);

    let lines: Vec<&str> = content.lines().collect();
    let end = limit.map_or(lines.len(), |l| (offset + l).min(lines.len()));

    let mut result = String::new();
    for (i, line) in lines[offset..end].iter().enumerate() {
        result.push_str(&format!("{:>6}\t{}\n", offset + i + 1, line));
    }
    Ok(result)
}

/// The file a `write_file` or `edit_file` call is going to touch, resolved
/// exactly the way the tool itself resolves it.
///
/// Exposed so a caller can snapshot that file *before* the change. Working the
/// path out any other way risks snapshotting a different file than the one
/// written — and it would skip the workspace boundary check that
/// [`resolve_path`] applies.
pub fn target_path(workspace: &Path, input: &str) -> Result<PathBuf> {
    let parsed: serde_json::Value = serde_json::from_str(input)?;
    let path_str = parsed["path"].as_str().context("missing 'path'")?;
    resolve_path(workspace, path_str)
}

/// Write (create or overwrite) a file. Shows a diff if the file already exists.
pub fn write_file(workspace: &Path, input: &str) -> Result<String> {
    let parsed: serde_json::Value = serde_json::from_str(input)?;

    let path_str = parsed["path"].as_str().context("missing 'path'")?;
    let content = parsed["content"].as_str().context("missing 'content'")?;
    let path = resolve_path(workspace, path_str)?;

    // Compute diff if file exists
    let diff = if path.exists() {
        let old_content = std::fs::read_to_string(&path).unwrap_or_default();
        if old_content != content {
            Some(compute_diff(
                &old_content,
                content,
                &path.display().to_string(),
            ))
        } else {
            None
        }
    } else {
        None
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;

    let mut result = format!("Wrote {} bytes to {}", content.len(), path.display());
    if let Some(diff_text) = diff {
        result.push_str("\n\n");
        result.push_str(&diff_text);
    }
    Ok(result)
}

/// Edit a file — find unique old_string and replace with new_string.
pub fn edit_file(workspace: &Path, input: &str) -> Result<String> {
    let parsed: serde_json::Value = serde_json::from_str(input)?;

    let path_str = parsed["path"].as_str().context("missing 'path'")?;
    let old_string = parsed["old_string"]
        .as_str()
        .context("missing 'old_string'")?;
    let new_string = parsed["new_string"]
        .as_str()
        .context("missing 'new_string'")?;
    let path = resolve_path(workspace, path_str)?;

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let count = content.matches(old_string).count();
    if count == 0 {
        anyhow::bail!("old_string not found in {}", path.display());
    }
    if count > 1 {
        anyhow::bail!(
            "old_string matches {} times in {} — must be unique",
            count,
            path.display()
        );
    }

    let new_content = content.replacen(old_string, new_string, 1);

    // Compute diff before writing
    let diff = compute_diff(&content, &new_content, &path.display().to_string());

    std::fs::write(&path, &new_content)?;

    let mut result = format!("Edited {}", path.display());
    if !diff.is_empty() {
        result.push_str("\n\n");
        result.push_str(&diff);
    }
    Ok(result)
}

/// Search for files matching a glob pattern.
pub fn glob_search(workspace: &Path, input: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({"pattern": input}));

    let pattern = parsed["pattern"].as_str().context("missing 'pattern'")?;
    let base = parsed["path"]
        .as_str()
        .map(|p| resolve_path(workspace, p))
        .transpose()?
        .unwrap_or_else(|| workspace.to_path_buf());

    let base_str = base.to_string_lossy().replace('\\', "/");
    let base_str = base_str.trim_end_matches('/');
    let full_pattern = format!("{base_str}/{pattern}");
    let mut results = Vec::new();

    // A path the glob could not read is skipped rather than failing the search:
    // one unreadable entry must not hide every other match.
    for path in glob::glob(&full_pattern)
        .context("invalid glob pattern")?
        .flatten()
    {
        results.push(path.display().to_string());
    }

    results.sort();
    if results.is_empty() {
        Ok("No files found.".to_string())
    } else {
        Ok(results.join("\n"))
    }
}

/// Search file contents using regex.
pub fn grep_search(workspace: &Path, input: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({"pattern": input}));

    let pattern_str = parsed["pattern"].as_str().context("missing 'pattern'")?;
    let base = parsed["path"]
        .as_str()
        .map(|p| resolve_path(workspace, p))
        .transpose()?
        .unwrap_or_else(|| workspace.to_path_buf());
    let file_glob = parsed["glob"].as_str().unwrap_or("**/*");

    let re = regex::Regex::new(pattern_str).context("invalid regex")?;
    let base_str = base.to_string_lossy().replace('\\', "/");
    let base_str = base_str.trim_end_matches('/');
    let full_pattern = format!("{base_str}/{file_glob}");

    let mut results = Vec::new();
    let max_results = 50;

    // A BAD PATTERN IS AN ERROR, NOT AN EMPTY RESULT. This used to fall back to
    // `glob("")` — which matches nothing — so a mistyped `glob` argument came
    // back as "no matches found", indistinguishable from a search that genuinely
    // found nothing. The caller then concludes the code it was looking for does
    // not exist. Saying which pattern failed costs one line and ends the whole
    // class of wrong answer. (It also removed this file's only `unwrap`.)
    let entries =
        glob::glob(&full_pattern).with_context(|| format!("invalid glob pattern `{file_glob}`"))?;
    for entry in entries {
        let Ok(path) = entry else { continue };
        if !path.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };

        for (line_num, line) in content.lines().enumerate() {
            if re.is_match(line) {
                results.push(format!("{}:{}: {}", path.display(), line_num + 1, line));
                if results.len() >= max_results {
                    results.push(format!("... (truncated at {max_results} matches)"));
                    return Ok(results.join("\n"));
                }
            }
        }
    }

    if results.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(results.join("\n"))
    }
}

/// Compute a unified diff between old and new content.
fn compute_diff(old: &str, new: &str, path: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- {path}\n+++ {path}\n");
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        // `change` already includes a trailing newline from the line
        out.push_str(&format!("{prefix}{change}"));
    }
    out
}

/// Resolve a tool-supplied path and confine it to the workspace.
///
/// Every path here is untrusted: it arrives in a tool call the model produced,
/// which may in turn have been influenced by file contents. This previously
/// returned absolute paths unchanged and joined relative ones without resolving
/// `..`, so `../../../.ssh/id_rsa` escaped the workspace and `workspace-write`
/// promised a containment it never enforced.
///
/// Containment is checked against the *canonical* location — so a symlink
/// inside the workspace pointing outside it is caught too — while the returned
/// path stays in normal (non-verbatim) form so it remains usable and readable
/// in messages.
fn resolve_path(workspace: &Path, path_str: &str) -> Result<PathBuf> {
    let requested = Path::new(path_str);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };

    let normalized = lexically_normalize(&joined);

    // The workspace root must exist; if it cannot be canonicalized there is no
    // boundary to enforce, and guessing one would be worse than refusing.
    let root = workspace
        .canonicalize()
        .with_context(|| format!("workspace root is not accessible: {}", workspace.display()))?;

    if !real_location_of(&normalized).starts_with(&root) {
        anyhow::bail!(
            "path escapes the workspace and was refused: {path_str} (workspace: {})",
            workspace.display()
        );
    }

    Ok(normalized)
}

/// Resolve `.` and `..` textually, without touching the filesystem.
///
/// Done lexically because the target may not exist yet — `write_file` creates
/// new files, and `canonicalize` fails on a path that isn't there.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonical location a path *would* occupy, even if it does not exist yet.
///
/// Canonicalizes the deepest ancestor that does exist and re-appends the rest,
/// so a not-yet-created file is still checked against real, symlink-resolved
/// directories rather than its textual form.
fn real_location_of(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();

    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            // Reached a root that does not exist; nothing further to strip.
            _ => break,
        }
    }

    let mut base = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    for name in tail.iter().rev() {
        base.push(name);
    }
    base
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace with a real secret placed OUTSIDE it, so an escape is
    /// observable rather than hypothetical.
    fn workspace_with_outside_secret() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(root.path().join("outside.txt"), "SECRET-OUTSIDE-WORKSPACE").unwrap();
        (root, workspace)
    }

    /// **A bad glob is an error, not "no matches".** It used to fall back to
    /// `glob("")`, which matches nothing — so a mistyped pattern came back
    /// looking exactly like a search that found nothing, and the caller
    /// concluded the code it was hunting for did not exist.
    ///
    /// `a[` is unterminated, which is what `glob` rejects.
    #[test]
    fn a_malformed_glob_is_refused_rather_than_reported_as_no_matches() {
        let (_root, workspace) = workspace_with_outside_secret();
        std::fs::write(workspace.join("found.txt"), "NEEDLE").unwrap();

        let input = serde_json::json!({ "pattern": "NEEDLE", "glob": "a[" }).to_string();
        let error = grep_search(&workspace, &input)
            .expect_err("an unparseable pattern must not read as an empty result");
        let message = format!("{error:#}");
        assert!(
            message.contains("invalid glob pattern"),
            "the error must say the pattern was the problem; got: {message}"
        );
    }

    /// The same call with a valid pattern still finds things — so the test above
    /// is failing on the pattern and not on a search that never worked.
    #[test]
    fn a_valid_glob_still_finds_matches() {
        let (_root, workspace) = workspace_with_outside_secret();
        std::fs::write(workspace.join("found.txt"), "NEEDLE").unwrap();

        let input = serde_json::json!({ "pattern": "NEEDLE", "glob": "**/*" }).to_string();
        let out = grep_search(&workspace, &input).expect("a valid pattern must work");
        assert!(out.contains("found.txt"), "got: {out}");
    }

    /// Traversal out of the workspace must be refused. Reading proves it: if
    /// containment fails, the call returns the secret's contents.
    #[test]
    fn read_cannot_escape_the_workspace_with_dotdot() {
        let (_root, workspace) = workspace_with_outside_secret();

        let input = serde_json::json!({ "path": "../outside.txt" }).to_string();
        let result = read_file(&workspace, &input);

        let err = match result {
            Ok(contents) => panic!("traversal must be refused, but the file was read: {contents}"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("escapes the workspace"),
            "refusal must say why; got: {err}"
        );
    }

    /// The search tools take a `path` too, and it goes through the same
    /// resolution. Containment was fixed for all call sites at once but only
    /// proven on read and write; these two were the untested half.
    ///
    /// A search that escapes leaks more than a read: it enumerates and greps
    /// files anywhere on disk.
    #[test]
    fn searching_cannot_escape_the_workspace() {
        let (root, workspace) = workspace_with_outside_secret();
        let outside_dir = root.path().to_str().unwrap().to_string();

        for base in ["..", outside_dir.as_str()] {
            let glob_input = serde_json::json!({ "pattern": "*.txt", "path": base }).to_string();
            match glob_search(&workspace, &glob_input) {
                Ok(found) => panic!("glob escaped to `{base}` and listed: {found}"),
                Err(e) => assert!(
                    e.to_string().contains("escapes the workspace"),
                    "refusal must say why; got: {e}"
                ),
            }

            let grep_input = serde_json::json!({ "pattern": "SECRET", "path": base }).to_string();
            match grep_search(&workspace, &grep_input) {
                Ok(found) => panic!("grep escaped to `{base}` and matched: {found}"),
                Err(e) => assert!(
                    e.to_string().contains("escapes the workspace"),
                    "refusal must say why; got: {e}"
                ),
            }
        }
    }

    /// Searching inside the workspace must keep working, or containment has
    /// simply broken the tools instead of securing them.
    #[test]
    fn searching_inside_the_workspace_still_works() {
        let (_root, workspace) = workspace_with_outside_secret();
        std::fs::create_dir_all(workspace.join("sub")).unwrap();
        std::fs::write(workspace.join("sub/found.txt"), "INSIDE-MARKER").unwrap();

        let glob_input = serde_json::json!({ "pattern": "*.txt", "path": "sub" }).to_string();
        let listed = glob_search(&workspace, &glob_input).expect("glob inside must work");
        assert!(listed.contains("found.txt"), "got: {listed}");

        let grep_input =
            serde_json::json!({ "pattern": "INSIDE-MARKER", "path": "sub" }).to_string();
        let matched = grep_search(&workspace, &grep_input).expect("grep inside must work");
        assert!(matched.contains("found.txt"), "got: {matched}");
    }

    /// An absolute path outside the workspace must be refused too — it was
    /// previously passed straight through with no check at all.
    #[test]
    fn read_cannot_escape_via_an_absolute_path() {
        let (root, workspace) = workspace_with_outside_secret();
        let outside = root.path().join("outside.txt");

        let input = serde_json::json!({ "path": outside.to_str().unwrap() }).to_string();
        match read_file(&workspace, &input) {
            Ok(contents) => panic!("absolute escape must be refused, got: {contents}"),
            Err(e) => assert!(e.to_string().contains("escapes the workspace"), "got: {e}"),
        }
    }

    /// Writing outside the workspace must be refused, and must leave no file
    /// behind — a refusal that still created the file would be no refusal.
    #[test]
    fn write_cannot_escape_the_workspace() {
        let (root, workspace) = workspace_with_outside_secret();
        let target = root.path().join("planted.txt");

        let input = serde_json::json!({
            "path": "../planted.txt",
            "content": "should never be written"
        })
        .to_string();

        assert!(write_file(&workspace, &input).is_err(), "must be refused");
        assert!(
            !target.exists(),
            "a refused write must not create the file at {}",
            target.display()
        );
    }

    /// Containment must not break ordinary work: paths inside the workspace,
    /// including nested ones and ones that dip through `..` but stay inside,
    /// still resolve.
    #[test]
    fn legitimate_paths_inside_the_workspace_still_work() {
        let (_root, workspace) = workspace_with_outside_secret();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "fn main() {}").unwrap();

        let input = serde_json::json!({ "path": "src/lib.rs" }).to_string();
        assert!(
            read_file(&workspace, &input).is_ok(),
            "a normal relative path must still resolve"
        );

        // Dips out of `src` but lands back inside the workspace.
        let input = serde_json::json!({ "path": "src/../src/lib.rs" }).to_string();
        assert!(
            read_file(&workspace, &input).is_ok(),
            "`..` that stays inside the workspace must be allowed"
        );
    }

    /// Creating a NEW file inside the workspace must work — the check has to
    /// handle paths that do not exist yet, which is why containment is computed
    /// from the deepest existing ancestor rather than `canonicalize` alone.
    #[test]
    fn writing_a_new_nested_file_inside_the_workspace_works() {
        let (_root, workspace) = workspace_with_outside_secret();

        let input = serde_json::json!({
            "path": "newdir/newfile.txt",
            "content": "hello"
        })
        .to_string();

        assert!(
            write_file(&workspace, &input).is_ok(),
            "new files must work"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("newdir/newfile.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn compute_diff_shows_changes() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nmodified\nline3\n";
        let diff = compute_diff(old, new, "test.txt");
        assert!(diff.contains("--- test.txt"));
        assert!(diff.contains("+++ test.txt"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+modified"));
        assert!(diff.contains(" line1"));
    }

    #[test]
    fn compute_diff_no_changes() {
        let content = "hello\nworld\n";
        let diff = compute_diff(content, content, "same.txt");
        assert!(diff.contains("--- same.txt"));
        assert!(diff.contains("+++ same.txt"));
        // No actual change lines (skip the +++ header — look for lines starting
        // with exactly one + or - followed by non-header content)
        for line in diff.lines().skip(2) {
            assert!(
                line.starts_with(' '),
                "expected context line but got: {line}"
            );
        }
    }

    #[test]
    fn edit_file_includes_diff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let input = serde_json::json!({
            "path": path.to_str().unwrap(),
            "old_string": "println!(\"hello\")",
            "new_string": "println!(\"world\")"
        })
        .to_string();

        let result = edit_file(dir.path(), &input).unwrap();
        assert!(result.contains("Edited"));
        assert!(result.contains("-    println!(\"hello\")"));
        assert!(result.contains("+    println!(\"world\")"));
    }

    #[test]
    fn write_file_shows_diff_when_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content\n").unwrap();

        let input = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "new content\n"
        })
        .to_string();

        let result = write_file(dir.path(), &input).unwrap();
        assert!(result.contains("Wrote"));
        assert!(result.contains("-old content"));
        assert!(result.contains("+new content"));
    }

    #[test]
    fn glob_and_grep_work_with_nested_subdirectories_and_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("findme.txt");
        std::fs::write(&file, "magic_target_token\n").unwrap();

        // Test glob search
        let glob_input = serde_json::json!({
            "pattern": "**/*.txt"
        })
        .to_string();
        let glob_res = glob_search(dir.path(), &glob_input).unwrap();
        assert!(glob_res.contains("findme.txt"));

        // Test grep search
        let grep_input = serde_json::json!({
            "pattern": "magic_target_token"
        })
        .to_string();
        let grep_res = grep_search(dir.path(), &grep_input).unwrap();
        assert!(grep_res.contains("magic_target_token"));
    }
}
