//! File operation tools — read, write, edit, glob, grep.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Read a file, optionally with offset and limit.
pub fn read_file(workspace: &Path, input: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({"path": input}));

    let path_str = parsed["path"].as_str().context("missing 'path' field")?;
    let path = resolve_path(workspace, path_str)?;

    let offset = parsed["offset"].as_u64().unwrap_or(0) as usize;
    let limit = parsed["limit"].as_u64().map(|l| l as usize);

    // A document is read as markdown: always for a binary format, and for
    // HTML, CSV and TSV — which are also source someone edits — when asked.
    if let Some(kind) = super::documents::Kind::of(&path) {
        let render = parsed["render"].as_bool().unwrap_or(false);
        if kind.converts_by_default() || render {
            let markdown =
                super::documents::to_markdown(&path, kind).map_err(anyhow::Error::msg)?;
            return Ok(super::documents::page(
                &path, kind, &markdown, offset, limit,
            ));
        }
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let lines: Vec<&str> = content.lines().collect();
    // The offset comes from the model; slicing past the end would panic.
    if offset > lines.len() {
        anyhow::bail!(
            "offset {offset} is past the end of {}, which has {} line(s)",
            path.display(),
            lines.len()
        );
    }
    let end = limit.map_or(lines.len(), |l| offset.saturating_add(l).min(lines.len()));

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
/// A workspace as a session holds it: absolute, symlinks resolved, and in the
/// plain spelling (`C:\x`, not the verbatim `\\?\C:\x` that `canonicalize`
/// returns on Windows), made once where the session is prepared.
///
/// # Errors
///
/// When the directory does not exist or cannot be resolved.
pub fn workspace_root(workspace: &Path) -> Result<PathBuf> {
    let resolved = workspace
        .canonicalize()
        .with_context(|| format!("the workspace {} is not accessible", workspace.display()))?;
    if !resolved.is_dir() {
        anyhow::bail!("the workspace {} is not a directory", workspace.display());
    }
    Ok(plain(resolved))
}

/// `path` without a Windows verbatim disk prefix; anything else unchanged.
fn plain(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) => {
                let mut out = PathBuf::from(format!("{}:\\", char::from(letter)));
                out.extend(components.filter(|c| !matches!(c, Component::RootDir)));
                out
            }
            _ => path,
        },
        _ => path,
    }
}

fn resolve_path(workspace: &Path, path_str: &str) -> Result<PathBuf> {
    let requested = Path::new(path_str);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    // Anchored before it is normalised. A workspace named relative to this
    // process (`-w .`) makes `./.` normalise to an EMPTY path and `./new/x`
    // to one whose first component does not exist — and the walk to the
    // nearest existing ancestor then ended at nothing, which resolves to no
    // location at all, so the workspace itself and every new top-level
    // directory were refused as escapes.
    let joined = std::path::absolute(&joined).with_context(|| {
        format!("could not resolve {path_str} against this process's directory")
    })?;

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
    // A relative path whose every component is new: its nearest existing
    // ancestor is the directory it is relative to.
    if existing.as_os_str().is_empty() {
        existing = Path::new(".");
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

    /// **An offset past the end is refused, not a panic.** The offset comes
    /// from the model, and slicing the line list past its end panicked.
    #[test]
    fn an_offset_past_the_end_is_refused_not_a_panic() {
        let (_root, workspace) = workspace_with_outside_secret();
        std::fs::write(workspace.join("short.txt"), "one\ntwo\n").unwrap();

        let past = serde_json::json!({ "path": "short.txt", "offset": 9 }).to_string();
        match read_file(&workspace, &past) {
            Ok(contents) => panic!("an offset past the end must be refused, got: {contents}"),
            Err(e) => assert!(e.to_string().contains("past the end"), "got: {e}"),
        }

        let huge_limit =
            serde_json::json!({ "path": "short.txt", "offset": 1, "limit": u64::MAX }).to_string();
        let tail = read_file(&workspace, &huge_limit).unwrap();
        assert!(tail.contains("two") && !tail.contains("one"), "got: {tail}");
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

    /// A workspace named RELATIVE to this process's directory, as `-w .`
    /// names one: a scratch directory under `target/`, which `cargo test`
    /// runs from the crate root beside.
    fn relative_workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("relative-ws")
            .tempdir_in("target")
            .unwrap();
        let name = dir.path().file_name().unwrap().to_os_string();
        (dir, Path::new("target").join(name))
    }

    /// Removes a directory this test made in the process's directory, even
    /// when an assertion fails first.
    struct MadeHere(PathBuf);

    impl Drop for MadeHere {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// **A RELATIVE WORKSPACE IS STILL A WORKSPACE.** With the workspace
    /// named `.` — `incode -w .` — `LS "."` names the workspace itself, and a
    /// write into a directory that does not exist yet lands inside it. Both
    /// were refused as "path escapes the workspace": the path normalised to a
    /// relative one whose first component did not exist, so the walk to the
    /// nearest existing ancestor ended at an empty path, which resolves to
    /// nothing. A `..` out of it is still refused.
    #[test]
    fn a_relative_workspace_resolves_itself_and_new_directories() {
        let workspace = Path::new(".");

        let itself = target_path(workspace, r#"{"path":"."}"#);
        assert!(itself.is_ok(), "the workspace itself: {itself:?}");

        // A directory that does not exist yet, at the top of the workspace —
        // the case that walked up to nothing. Removed however the test ends.
        let fresh = format!("relative-ws-probe-{}", uuid::Uuid::new_v4().simple());
        let _made = MadeHere(PathBuf::from(&fresh));
        let input =
            serde_json::json!({"path": format!("{fresh}/new.txt"), "content": "made"}).to_string();
        let wrote = write_file(workspace, &input);
        assert!(wrote.is_ok(), "a write into a new directory: {wrote:?}");
        assert_eq!(
            std::fs::read_to_string(Path::new(&fresh).join("new.txt")).unwrap(),
            "made"
        );

        let escape = target_path(workspace, r#"{"path":"../outside.txt"}"#);
        assert!(escape.is_err(), "a `..` out of it: {escape:?}");
    }

    /// The workspace a session is prepared with is made absolute and
    /// canonical once, in the plain spelling the rest of the harness prints.
    #[test]
    fn a_workspace_root_is_absolute_and_plain() {
        let (_dir, workspace) = relative_workspace();
        let root = workspace_root(&workspace).unwrap();
        assert!(root.is_absolute(), "{}", root.display());
        assert!(
            !root.to_string_lossy().starts_with(r"\\?\"),
            "no verbatim prefix: {}",
            root.display()
        );
        assert_eq!(
            root.canonicalize().unwrap(),
            workspace.canonicalize().unwrap()
        );
        assert!(workspace_root(Path::new("target/no-such-dir-here")).is_err());
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
}
