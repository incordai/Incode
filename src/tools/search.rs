//! Grep, Glob and LS over ripgrep's own walker and searcher.
//!
//! # One walker, three tools
//!
//! [`walker`] is the single place that decides what a search skips, and all
//! three tools start from it — so a file LS leaves out is a file Grep never
//! searches and Glob never lists. It skips:
//!
//! - what `.gitignore`, `.ignore` and `.git/info/exclude` exclude, INSIDE OR
//!   OUTSIDE a git repository (`require_git(false)`): a workspace that is not a
//!   repo still has a `.gitignore` worth honouring;
//! - hidden files and directories, `.git` among them;
//! - `node_modules`, and any directory holding a `CACHEDIR.TAG` (the cache
//!   directory tag cargo writes into `target/`), so build output is skipped
//!   even where nothing ignores it.
//!
//! The directory the call names is never skipped itself: asking to search
//! `node_modules/pkg` searches it.
//!
//! # A model's glob narrows, it never widens
//!
//! The glob a model passes is matched with `globset` AFTER the walk. Handing
//! it to the walker as an override or a file type instead was measured to
//! re-admit hidden and ignored files — an override takes precedence over every
//! ignore rule — which would make `glob: "*.rs"` search `target/`.
//!
//! # Nothing is dropped in silence
//!
//! A binary file is reported as binary, with whether it matched; a file that
//! could not be read is counted; a result past its bound is counted, or, for a
//! Grep that stopped early, said to have stopped. A UTF-16 or UTF-8 file with a
//! byte-order mark is transcoded before it is searched.

use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde_json::{json, Value};

/// The most matching lines one Grep shows.
pub const MAX_GREP_MATCHES: usize = 100;
/// The most files one Glob lists. The rest are counted.
pub const MAX_GLOB_RESULTS: usize = 500;
/// The most context lines asked for on each side of a match.
pub const MAX_CONTEXT_LINES: u64 = 10;
/// A line longer than this is cut, so one minified file cannot fill the reply.
const MAX_LINE_CHARS: usize = 300;
/// A file larger than this is not searched; it is counted instead.
const MAX_SEARCH_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// The most characters one Grep reply carries, whatever the line count.
const MAX_GREP_CHARS: usize = 30_000;

/// The directory walk every search tool starts from. See the module note for
/// what it skips.
pub fn walker(base: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(base);
    builder
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(std::cmp::Ord::cmp)
        .filter_entry(|entry| !is_build_output(entry.path()));
    builder
}

/// A directory of generated output nothing should search: `node_modules`, or
/// one carrying the cache directory tag.
fn is_build_output(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    path.file_name().is_some_and(|n| n == "node_modules") || path.join("CACHEDIR.TAG").is_file()
}

/// Where a search starts: the `path` the call names, confined to `root`
/// exactly as every file tool confines it, or `root` itself.
fn base_of(root: &Path, input: &Value) -> Result<PathBuf, String> {
    match input.get("path").and_then(Value::as_str) {
        Some(path) if !path.trim().is_empty() => {
            let probe = json!({ "path": path }).to_string();
            let base =
                crate::tools::file_ops::target_path(root, &probe).map_err(|e| e.to_string())?;
            if !base.exists() {
                return Err(format!("{} does not exist", base.display()));
            }
            Ok(base)
        }
        _ => Ok(root.to_path_buf()),
    }
}

/// A model's glob, compiled the way `.gitignore` reads one: a pattern with no
/// `/` matches a file's NAME at any depth (`*.rs`); one with a `/` matches the
/// path from the search's base, where `*` stays within one directory and `**`
/// crosses them (`src/*.rs`, `src/**/*.rs`).
pub struct NameGlob {
    matcher: globset::GlobMatcher,
    whole_path: bool,
}

impl NameGlob {
    /// Compile `pattern`, or say why it cannot be.
    pub fn new(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim().trim_start_matches("./");
        if pattern.is_empty() {
            return Err("the glob pattern is empty".to_string());
        }
        if Path::new(pattern).is_absolute() {
            return Err(format!(
                "`{pattern}` is an absolute path; give a pattern relative to `path` and put the \
                 directory in `path`"
            ));
        }
        let whole_path = pattern.contains('/');
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(whole_path)
            .backslash_escape(false)
            .build()
            .map_err(|e| format!("invalid glob pattern `{pattern}`: {e}"))?;
        Ok(Self {
            matcher: glob.compile_matcher(),
            whole_path,
        })
    }

    /// Whether `path`, found under `base`, is one the pattern names.
    pub fn matches(&self, base: &Path, path: &Path) -> bool {
        if self.whole_path {
            let relative = path.strip_prefix(base).unwrap_or(path);
            let spelled = relative.to_string_lossy().replace('\\', "/");
            self.matcher.is_match(spelled.as_str())
        } else {
            path.file_name()
                .is_some_and(|name| self.matcher.is_match(Path::new(name)))
        }
    }
}

/// Files whose paths match a glob, under the shared walker.
pub fn glob_search(root: &Path, input: &Value) -> Result<String, String> {
    let pattern = input
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or("missing 'pattern'")?;
    let glob = NameGlob::new(pattern)?;
    let base = base_of(root, input)?;

    let mut found: Vec<String> = Vec::new();
    let mut unreadable = 0usize;
    for entry in walker(&base).build() {
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if glob.matches(&base, entry.path()) {
            found.push(entry.path().display().to_string());
        }
    }
    found.sort();

    let total = found.len();
    let mut out = if total == 0 {
        "No files found.".to_string()
    } else {
        found
            .iter()
            .take(MAX_GLOB_RESULTS)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    };
    if total > MAX_GLOB_RESULTS {
        out.push_str(&format!(
            "\n… {} more files not shown ({total} matched); narrow the pattern or the path",
            total - MAX_GLOB_RESULTS
        ));
    }
    if unreadable > 0 {
        out.push_str(&format!(
            "\n({unreadable} entries could not be read and were not searched)"
        ));
    }
    Ok(out)
}

/// What one file's search produced.
struct FileHits {
    /// Lines to show: `(line number, is a match, text)`, with `None` marking
    /// a break between non-adjacent groups.
    lines: Vec<Option<(u64, bool, String)>>,
    matches: usize,
    /// Where the first NUL byte was, when the file is binary.
    binary_at: Option<u64>,
    /// The match budget left for this file; the search stops at zero.
    budget: usize,
}

impl Sink for FileHits {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.matches += 1;
        let number = mat.line_number().unwrap_or(0);
        self.lines
            .push(Some((number, true, shown_line(mat.bytes()))));
        Ok(self.matches < self.budget)
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        let number = ctx.line_number().unwrap_or(0);
        self.lines
            .push(Some((number, false, shown_line(ctx.bytes()))));
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, Self::Error> {
        self.lines.push(None);
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, offset: u64) -> Result<bool, Self::Error> {
        self.binary_at.get_or_insert(offset);
        Ok(true)
    }
}

/// One line as it is shown: its terminator dropped, lossily decoded, cut.
fn shown_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches(['\n', '\r']);
    crate::text::truncate(text, MAX_LINE_CHARS)
}

/// Search file contents for a regular expression, under the shared walker.
pub fn grep_search(root: &Path, input: &Value) -> Result<String, String> {
    let pattern = input
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or("missing 'pattern'")?;
    let base = base_of(root, input)?;
    let glob = match input.get("glob").and_then(Value::as_str) {
        Some(g) if !g.trim().is_empty() => Some(NameGlob::new(g)?),
        _ => None,
    };
    let context = input
        .get("context")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_CONTEXT_LINES);
    let context = usize::try_from(context).unwrap_or(0);
    let insensitive = input
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(insensitive)
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|e| format!("invalid regex `{pattern}`: {e}"))?;
    // BOM sniffing is on by default: a UTF-16 file is transcoded to UTF-8
    // before it is searched. A NUL byte marks a file binary; it is converted
    // rather than quit on, so whether the file matched can still be said.
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::convert(b'\0'))
        .before_context(context)
        .after_context(context)
        .build();

    let mut out = String::new();
    let mut shown = 0usize;
    let mut files_matched = 0usize;
    let mut binary_matched: Vec<String> = Vec::new();
    let mut binary_searched = 0usize;
    let mut unreadable = 0usize;
    let mut too_large = 0usize;
    let mut stopped = false;

    for entry in walker(&base).build() {
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(glob) = &glob {
            if !glob.matches(&base, path) {
                continue;
            }
        }
        if entry
            .metadata()
            .is_ok_and(|m| m.len() > MAX_SEARCH_FILE_BYTES)
        {
            too_large += 1;
            continue;
        }
        let mut hits = FileHits {
            lines: Vec::new(),
            matches: 0,
            binary_at: None,
            budget: MAX_GREP_MATCHES - shown,
        };
        if searcher.search_path(&matcher, path, &mut hits).is_err() {
            unreadable += 1;
            continue;
        }
        if hits.binary_at.is_some() {
            binary_searched += 1;
        }
        if hits.matches == 0 {
            continue;
        }
        files_matched += 1;
        let name = path.display().to_string();
        if let Some(offset) = hits.binary_at {
            // Its lines are not shown: they are bytes, not text.
            binary_matched.push(format!(
                "{name}: binary file matches ({} lines; NUL byte at offset {offset}), \
                 contents not shown",
                hits.matches
            ));
            shown += hits.matches;
        } else {
            if !out.is_empty() && context > 0 {
                out.push_str("--\n");
            }
            for line in hits.lines {
                match line {
                    Some((number, true, text)) => {
                        out.push_str(&format!("{name}:{number}: {text}\n"));
                    }
                    Some((number, false, text)) => {
                        out.push_str(&format!("{name}-{number}- {text}\n"));
                    }
                    None => out.push_str("--\n"),
                }
            }
            shown += hits.matches;
        }
        if shown >= MAX_GREP_MATCHES || out.len() >= MAX_GREP_CHARS {
            stopped = true;
            break;
        }
    }

    for line in &binary_matched {
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("No matches found.\n");
    }
    crate::text::truncate_in_place(&mut out, MAX_GREP_CHARS, "\n… (output cut)\n");
    if stopped {
        out.push_str(&format!(
            "[stopped at {shown} matching lines in {files_matched} files — the cap is \
             {MAX_GREP_MATCHES}; more may exist. Narrow the pattern, `path` or `glob`.]\n"
        ));
    }
    let binary_other = binary_searched.saturating_sub(binary_matched.len());
    if binary_other > 0 {
        out.push_str(&format!(
            "({binary_other} binary files were searched and did not match)\n"
        ));
    }
    if too_large > 0 {
        out.push_str(&format!(
            "({too_large} files over {} MiB were not searched)\n",
            MAX_SEARCH_FILE_BYTES / (1024 * 1024)
        ));
    }
    if unreadable > 0 {
        out.push_str(&format!(
            "({unreadable} files or directories could not be read)\n"
        ));
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree with everything the walker must skip, and two files it must
    /// find. Not a git repository: `.gitignore` must apply regardless.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path();
        for d in [
            "src",
            "target/debug",
            "node_modules/pkg",
            "logs",
            "dist",
            ".cache",
        ] {
            std::fs::create_dir_all(root.join(d)).expect("dir");
        }
        std::fs::write(root.join(".gitignore"), "*.log\ndist/\n").expect("seed");
        std::fs::write(
            root.join("target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .expect("seed");
        std::fs::write(
            root.join("src/lib.rs"),
            "fn needle_one() {}\nfn other() {}\n",
        )
        .expect("seed");
        std::fs::write(root.join("src/app.ts"), "export const needle_two = 2;\n").expect("seed");
        std::fs::write(
            root.join("target/debug/gen.rs"),
            "fn needle_generated() {}\n",
        )
        .expect("seed");
        std::fs::write(
            root.join("node_modules/pkg/index.js"),
            "var needle_vendor;\n",
        )
        .expect("seed");
        std::fs::write(root.join("logs/run.log"), "needle_logged\n").expect("seed");
        std::fs::write(root.join("dist/out.js"), "needle_dist\n").expect("seed");
        std::fs::write(root.join(".hidden.rs"), "fn needle_hidden() {}\n").expect("seed");
        std::fs::write(root.join(".cache/c.rs"), "fn needle_cached() {}\n").expect("seed");
        dir
    }

    fn grep(root: &Path, input: Value) -> String {
        grep_search(root, &input).expect("grep runs")
    }

    /// **GREP SKIPS WHAT THE WALKER SKIPS**: gitignored files, build output,
    /// vendored packages and hidden files — outside a git repository.
    #[test]
    fn grep_skips_ignored_hidden_and_build_output() {
        let dir = fixture();
        let out = grep(dir.path(), json!({"pattern": r"needle_\w+"}));
        assert!(out.contains("needle_one"), "{out}");
        assert!(out.contains("needle_two"), "{out}");
        for leaked in [
            "needle_generated",
            "needle_vendor",
            "needle_logged",
            "needle_dist",
            "needle_hidden",
            "needle_cached",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
    }

    /// **A GLOB NARROWS, IT NEVER WIDENS.** `*.rs` must not bring back the
    /// hidden `.rs` file or the one under `target/`, which an override or a
    /// type filter was measured to do.
    #[test]
    fn a_glob_filters_after_the_walk_and_admits_nothing_ignored() {
        let dir = fixture();
        let out = grep(
            dir.path(),
            json!({"pattern": r"needle_\w+", "glob": "*.rs"}),
        );
        assert!(out.contains("needle_one"), "{out}");
        assert!(!out.contains("needle_two"), "the .ts file: {out}");
        assert!(!out.contains("needle_hidden"), "{out}");
        assert!(!out.contains("needle_generated"), "{out}");
    }

    /// `.ignore` and `.git/info/exclude` are honoured too.
    #[test]
    fn dot_ignore_and_git_exclude_are_honoured() {
        let dir = fixture();
        let root = dir.path();
        std::fs::write(root.join(".ignore"), "private.txt\n").expect("seed");
        std::fs::create_dir_all(root.join(".git/info")).expect("git");
        std::fs::write(root.join(".git/info/exclude"), "scratch.txt\n").expect("seed");
        std::fs::write(root.join("private.txt"), "needle_private\n").expect("seed");
        std::fs::write(root.join("scratch.txt"), "needle_scratch\n").expect("seed");
        std::fs::write(root.join("kept.txt"), "needle_kept\n").expect("seed");
        let out = grep(root, json!({"pattern": r"needle_\w+"}));
        assert!(out.contains("needle_kept"), "{out}");
        assert!(!out.contains("needle_private"), "{out}");
        assert!(!out.contains("needle_scratch"), "{out}");
    }

    /// **A BINARY FILE IS REPORTED, NEVER SILENTLY SKIPPED OR PRINTED.**
    #[test]
    fn a_binary_file_is_reported_as_binary() {
        let dir = fixture();
        std::fs::write(
            dir.path().join("src/blob.bin"),
            b"\x00\x01needle_binary\x00",
        )
        .expect("seed");
        std::fs::write(dir.path().join("src/other.bin"), b"\x00\x01nothing\x00").expect("seed");
        let out = grep(dir.path(), json!({"pattern": "needle_binary"}));
        assert!(out.contains("blob.bin: binary file matches"), "{out}");
        assert!(!out.contains("\u{0}"), "raw bytes printed: {out:?}");
        let other = grep(dir.path(), json!({"pattern": "nothing"}));
        assert!(other.contains("other.bin: binary file matches"), "{other}");
        let none = grep(dir.path(), json!({"pattern": "zzz_absent"}));
        assert!(
            none.contains("binary files were searched and did not match"),
            "{none}"
        );
    }

    /// **A UTF-16 FILE IS TRANSCODED AND FOUND.**
    #[test]
    fn a_utf16_file_is_found() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "first line\r\nneedle_wide here\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(dir.path().join("wide.txt"), bytes).expect("seed");
        let out = grep(dir.path(), json!({"pattern": "needle_wide"}));
        assert!(out.contains("wide.txt:2: needle_wide here"), "{out}");
        assert!(!out.contains("binary"), "UTF-16 read as binary: {out}");
    }

    /// Context lines come on both sides, marked apart from the match.
    #[test]
    fn grep_offers_context_lines() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("c.txt"), "a\nb\nTARGET\nd\ne\n").expect("seed");
        let out = grep(dir.path(), json!({"pattern": "TARGET", "context": 1}));
        assert!(out.contains("c.txt-2- b"), "{out}");
        assert!(out.contains("c.txt:3: TARGET"), "{out}");
        assert!(out.contains("c.txt-4- d"), "{out}");
        assert!(!out.contains("c.txt-1- a"), "{out}");
    }

    /// **THE CAP IS REPORTED.** A search that stops early says so.
    #[test]
    fn grep_is_bounded_and_says_it_stopped() {
        let dir = tempfile::tempdir().expect("tmp");
        let body: String = (0..MAX_GREP_MATCHES + 20)
            .map(|n| format!("hit {n}\n"))
            .collect();
        std::fs::write(dir.path().join("many.txt"), body).expect("seed");
        let out = grep(dir.path(), json!({"pattern": "hit"}));
        let lines = out.lines().filter(|l| l.contains("many.txt:")).count();
        assert_eq!(lines, MAX_GREP_MATCHES, "{out}");
        assert!(out.contains("stopped at 100 matching lines"), "{out}");
    }

    #[test]
    fn a_bad_regex_or_glob_is_an_error_not_no_matches() {
        let dir = fixture();
        let bad_glob = grep_search(dir.path(), &json!({"pattern": "x", "glob": "a["}))
            .expect_err("unparseable glob");
        assert!(bad_glob.contains("invalid glob pattern"), "{bad_glob}");
        let bad_regex =
            grep_search(dir.path(), &json!({"pattern": "("})).expect_err("unparseable regex");
        assert!(bad_regex.contains("invalid regex"), "{bad_regex}");
    }

    #[test]
    fn case_insensitive_search_is_offered() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("x.txt"), "Hello World\n").expect("seed");
        assert!(grep(dir.path(), json!({"pattern": "hello"})).contains("No matches"));
        let out = grep(
            dir.path(),
            json!({"pattern": "hello", "case_insensitive": true}),
        );
        assert!(out.contains("Hello World"), "{out}");
    }

    /// **GLOB AGREES WITH GREP** on what is ignored, and matches a bare name
    /// at any depth while a pattern with `/` is anchored to the base.
    #[test]
    fn glob_uses_the_same_walker_and_gitignore_semantics() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("src/deep")).expect("dir");
        std::fs::write(dir.path().join("src/deep/inner.rs"), "").expect("seed");
        let any_depth = glob_search(dir.path(), &json!({"pattern": "*.rs"})).expect("glob");
        assert!(any_depth.contains("lib.rs"), "{any_depth}");
        assert!(any_depth.contains("inner.rs"), "{any_depth}");
        assert!(!any_depth.contains(".hidden.rs"), "{any_depth}");
        assert!(!any_depth.contains("gen.rs"), "{any_depth}");
        assert!(!any_depth.contains("c.rs"), "{any_depth}");

        let anchored = glob_search(dir.path(), &json!({"pattern": "src/*.rs"})).expect("glob");
        assert!(anchored.contains("lib.rs"), "{anchored}");
        assert!(!anchored.contains("inner.rs"), "{anchored}");
        let deep = glob_search(dir.path(), &json!({"pattern": "src/**/*.rs"})).expect("glob");
        assert!(deep.contains("inner.rs"), "{deep}");
    }

    #[test]
    fn glob_is_bounded_and_counts_the_rest() {
        let dir = tempfile::tempdir().expect("tmp");
        for n in 0..MAX_GLOB_RESULTS + 7 {
            std::fs::write(dir.path().join(format!("f{n:04}.txt")), "").expect("seed");
        }
        let out = glob_search(dir.path(), &json!({"pattern": "*.txt"})).expect("glob");
        assert!(
            out.contains("7 more files not shown (507 matched)"),
            "{out}"
        );
    }

    /// Searching cannot leave the workspace, by `..` or by an absolute path.
    #[test]
    fn searching_cannot_escape_the_workspace() {
        let outer = tempfile::tempdir().expect("tmp");
        let ws = outer.path().join("ws");
        std::fs::create_dir_all(&ws).expect("ws");
        std::fs::write(outer.path().join("outside.txt"), "SECRET").expect("seed");
        let outside = outer.path().to_string_lossy().to_string();
        for base in ["..", outside.as_str()] {
            let g = glob_search(&ws, &json!({"pattern": "*.txt", "path": base}));
            assert!(
                g.is_err_and(|e| e.contains("escapes the workspace")),
                "glob via {base}"
            );
            let r = grep_search(&ws, &json!({"pattern": "SECRET", "path": base}));
            assert!(
                r.is_err_and(|e| e.contains("escapes the workspace")),
                "grep via {base}"
            );
        }
    }

    /// Containment must not break ordinary work: a subdirectory named as
    /// `path` is searched, at any depth.
    #[test]
    fn searching_inside_a_named_subdirectory_works() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join("sub/nested")).expect("dir");
        std::fs::write(dir.path().join("sub/nested/found.txt"), "INSIDE-MARKER").expect("seed");
        let listed = glob_search(dir.path(), &json!({"pattern": "*.txt", "path": "sub"}))
            .expect("glob inside");
        assert!(listed.contains("found.txt"), "{listed}");
        let matched = grep(
            dir.path(),
            json!({"pattern": "INSIDE-MARKER", "path": "sub"}),
        );
        assert!(matched.contains("found.txt:1: INSIDE-MARKER"), "{matched}");
    }

    /// A named directory is searched even when the walker would skip it.
    #[test]
    fn a_directory_named_explicitly_is_searched() {
        let dir = fixture();
        let out = grep(
            dir.path(),
            json!({"pattern": "needle_vendor", "path": "node_modules/pkg"}),
        );
        assert!(out.contains("needle_vendor"), "{out}");
    }
}
