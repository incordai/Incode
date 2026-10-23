//! Capabilities that exist, are tested, and that nothing calls.
//!
//! # Why this check exists
//!
//! Six of them were found by hand in a single session:
//!
//! | Capability | How it looked | What it was |
//! |---|---|---|
//! | `PermissionPolicy` | a permission system | never consulted |
//! | `Safety::heartbeat` | a dead-man's switch | nothing beat it |
//! | `goal.rs` | its own doc said *"called by the runner"* | only a slash command called it |
//! | `Supervisor` | a supervised desktop | started only by its own tests |
//! | `Task::with_dependencies` | a dependency graph | zero production callers |
//! | `tools/worktree.rs` | isolated work branches | wired to a slash command |
//!
//! Every one compiled, every one had passing tests, and not one of them ran.
//! Six is not bad luck — it is what happens when a codebase is built faster
//! than it is wired, and each one was found only because somebody happened to
//! look. A test suite cannot catch them: the tests are exactly the callers
//! that make the item look used.
//!
//! # What it actually checks
//!
//! A `pub` item is UNWIRED when its name appears nowhere in the crate's
//! production code except the file that defines it. Tests are excluded on
//! purpose — a capability whose only callers are its own tests is the precise
//! shape this is looking for. "Tests" means each item a `#[cfg(test)]` guards
//! (and nothing past it), plus every file a parent declares as
//! `#[cfg(test)] mod name;` and the module tree beneath it.
//!
//! # Comments are stripped first, and that is the load-bearing part
//!
//! This codebase documents heavily and refers to items by name constantly
//! (`[`crate::verify::verify`]`). Counting those as callers would mean nothing
//! is ever flagged: the better-documented an unwired item is, the more wired
//! it would appear. So comment lines are removed before anything is counted.
//!
//! # It is deliberately blunt
//!
//! Name matching, not a parser. It will miss an item whose name is a common
//! word — `new`, `path`, `id` — because those appear everywhere for other
//! reasons. That is a false NEGATIVE, and a check that quietly misses some is
//! worth having. A false POSITIVE is what kills a check, because people turn
//! it off, so anything genuinely ambiguous belongs in [`DELIBERATELY_UNWIRED`]
//! with the reason written down.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// How many unwired capabilities this crate is known to carry.
///
/// A RATCHET, not a target. It may go down and never up: the check landed
/// with 68 already there, and a gate that cannot be green on the day it
/// arrives is a gate somebody deletes. What it enforces is that a seventh
/// `PermissionPolicy` cannot be added quietly — which is the thing that
/// actually happened, six times.
///
/// Lower it whenever the number falls. The test fails and tells you to.
///
/// It rose once, from 38 to 41, and not because anything new was added: the
/// scanner had been blind. It stopped reading a file at a `#[cfg(test)]` that
/// guarded one `use`, and it read split-out test files as production callers.
/// Seeing again found `toolbox::Toolbox::set_permission_rules` and
/// `ClaudeCodeBackend::{with_session, with_models}`, each reached only by tests.
pub const MAX_UNWIRED: usize = 41;

/// A `pub` item nothing outside its own file calls.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unwired {
    pub name: String,
    pub file: String,
    pub line: usize,
}

impl Unwired {
    /// One line, in the shape an editor can jump to.
    #[must_use]
    pub fn line_report(&self) -> String {
        format!("{}:{} `{}` has no caller", self.file, self.line, self.name)
    }
}

/// Items that are public and uncalled ON PURPOSE, and why.
///
/// # This list is the point, not an escape hatch
///
/// Every entry is a decision someone had to write down. An item added here
/// without a reason is the same defect the check exists to find, one step
/// further from view — so the reason is a required field, and a blank one
/// fails the test.
pub const DELIBERATELY_UNWIRED: &[(&str, &str)] = &[
    (
        "run",
        "The simplest entry point to the runner, kept as the documented API \
         even though `cli` uses `run_watched`. Removing it would make the \
         module's own example untestable.",
    ),
    (
        "run_with_owner",
        "A layer of the runner's entry ladder (`run` -> `run_with_owner` -> \
         `run_with_journal` -> `run_watched`). Each exists so a caller can \
         take exactly the capabilities it wants; the ladder is the API.",
    ),
    ("run_with_journal", "Same ladder as `run_with_owner`."),
    (
        "rules_for",
        "The unbounded spelling of a request the two real callers both make \
         with an explicit deadline (`session_context`, `context_for_turn`), \
         because both run in front of a person waiting. Kept as the plain \
         form of the module's one question. NOT a sign the rules are \
         unloaded — they are, through those two.",
    ),
    (
        "exhausted_message",
        "The method spelling of `Budget::exhausted_text`, which IS wired — \
         `StopReason::BudgetSpent` calls it after the `Budget` itself is out \
         of reach. Kept because asking a budget why it stopped should not \
         require unpacking it into two numbers first.",
    ),
    (
        "findings_for",
        "Same shape as `rules_for`: the unbounded spelling of what \
         `context_for_session_within` asks with a deadline. The auditor is \
         wired; this is the entry point without a clock on it.",
    ),
    (
        "run_on_compact",
        "The operator's `on_compact` command, kept as configuration with nothing \
         to fire it truthfully on the CLI routes: the model CLI compacts its \
         own, inside its own process. (The API route's own compaction fires \
         the operator's `pre_compact` hooks instead, through \
         `Toolbox::operator_pre_compact`; those supersede this.) The claude CLI's stream \
         does carry a `compact_boundary` system frame (found in the 2.1.283 \
         binary, NOT measured live: a compaction needs a full context window), \
         which is where this would be wired once it has been seen.",
    ),
    (
        "Supervisor",
        "The generic dead-man's switch. `run_stdio` supervises the desktop \
         directly instead — EOF on the pipe is a liveness signal no timer can \
         fake — so this is the shape for a future supervisor that has no pipe. \
         Deliberate, and recorded here rather than discovered again.",
    ),
];

/// Every `pub` item with no caller outside the file that defines it.
///
/// # Errors
///
/// Returns the I/O error when the tree cannot be read. A check that silently
/// reported "nothing unwired" because it could not open the directory would
/// be worse than no check.
pub fn unwired_in(src: &Path) -> std::io::Result<Vec<Unwired>> {
    let files = rust_files(src)?;
    let known: BTreeSet<PathBuf> = files.iter().cloned().collect();

    let mut sources: Vec<Source> = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let scan = scan_source(&text);
        sources.push(Source { path, text, scan });
    }

    // A file a parent declares as `#[cfg(test)] mod name;` carries no marker
    // of its own — every line of it is a test, and nothing inside says so.
    let tests = test_module_files(&sources, &known);

    // Every file's production code, keyed by path. Read once: this is quadratic
    // in the number of items otherwise, and the tree has thousands.
    let mut production: Vec<(PathBuf, String)> = Vec::new();
    let mut declared: Vec<(String, PathBuf, usize)> = Vec::new();

    for source in &sources {
        if tests.contains(&source.path) {
            continue;
        }
        let code = strip_comments(&without_regions(&source.text, &source.scan.test_regions));
        for (name, line) in declarations(&code) {
            declared.push((name, source.path.clone(), line));
        }
        production.push((source.path.clone(), code));
    }

    let allowed: BTreeSet<&str> = DELIBERATELY_UNWIRED.iter().map(|(name, _)| *name).collect();

    let mut out = Vec::new();
    for (name, path, line) in declared {
        if allowed.contains(name.as_str()) {
            continue;
        }
        // COUNTED ACROSS ALL PRODUCTION CODE, its own file included.
        //
        // "Used only in its own file" is not deadness — a module calling its
        // own helper is the ordinary case, and treating it as a finding
        // reported 225 items here. What every one of the six real capabilities
        // had in common is narrower and exact: the declaration is the ONLY
        // place the name appears outside a test.
        let mentions: usize = production
            .iter()
            .map(|(_, code)| count_mentions(code, &name))
            .sum();

        // One is the declaration itself. Anything more is a caller.
        if mentions <= 1 {
            out.push(Unwired {
                name,
                file: display_path(src, &path),
                line,
            });
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Every `.rs` file under `src`, skipping backups.
fn rust_files(src: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `.backups` holds old copies of live files: every item in
                // them would look defined twice and called never.
                if path.file_name().is_some_and(|n| n == ".backups") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".rs") && !name.contains(".bak") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// One source file, read once, with what [`scan_source`] found in it.
struct Source {
    path: PathBuf,
    text: String,
    scan: Scan,
}

/// What one pass over a file finds.
#[derive(Debug, Default)]
struct Scan {
    /// Byte ranges compiled only for tests, each from its `#[cfg(test)]` to
    /// the end of the ONE item that attribute guards.
    test_regions: Vec<(usize, usize)>,
    /// Every out-of-line `mod name;` the file declares.
    modules: Vec<ModDecl>,
}

/// A `mod name;` declaration: a module whose body is another file.
#[derive(Debug)]
struct ModDecl {
    name: String,
    /// The `#[path = "…"]` it carries, if any.
    path: Option<String>,
    /// Whether it exists only under `cfg(test)`.
    test: bool,
}

/// Find what in `text` is test-only, and which module files it declares.
///
/// # A `#[cfg(test)]` guards ONE item, and that is all it may hide
///
/// This used to cut each file at its first column-zero `#[cfg(test)]`, on the
/// theory that such a marker opens the trailing `mod tests`. It does not have
/// to. `agent/claude_code.rs` opened with `#[cfg(test)]\nuse std::io::BufRead;`
/// on line 8, so ~1,600 production lines went unread: none of its callers were
/// seen and none of its functions were declared — `run_once`, `spawn`,
/// `finish`, `consume` and `debug_stream_path` were invisible to the check in
/// both directions. Before that it was any `#[cfg(test)]` at all, and one
/// test-only method in `cli/agent_turn.rs` hid 1,028 lines and put four
/// working `/undo` capabilities on the dead list for months.
///
/// Both were the same mistake: treating an attribute as a line in a file
/// rather than as what it is, a guard on the next item. So the item is found —
/// brace-matched, with strings, chars and comments skipped so a `'{'` cannot
/// unbalance it — and exactly that span is test code. An inline `mod tests
/// { … }` is one item like any other; the code after its closing brace is
/// production again.
///
/// `#[cfg(all(test, …))]` is test-only too. `#[cfg(any(test, …))]` and
/// `#[cfg(not(test))]` are not: both compile into a shipped binary.
fn scan_source(text: &str) -> Scan {
    let b = text.as_bytes();
    let mut scan = Scan::default();
    let mut i = 0;
    while i < b.len() {
        if let Some(next) = skip_literal(b, i) {
            i = next;
            continue;
        }
        if b[i] == b'#' && b.get(i + 1) == Some(&b'[') {
            let attrs = attributes(text, i);
            let item = skip_visibility(b, skip_trivia(b, attrs.end));
            if let Some((name, end)) = module_declaration(text, item) {
                if attrs.test {
                    scan.test_regions.push((i, end));
                }
                scan.modules.push(ModDecl {
                    name,
                    path: attrs.path,
                    test: attrs.test,
                });
                i = end;
                continue;
            }
            if attrs.test {
                let end = item_end(text, item);
                scan.test_regions.push((i, end));
                i = end;
                continue;
            }
            i = attrs.end;
            continue;
        }
        if is_ident(b[i]) && (i == 0 || !is_ident(b[i - 1])) {
            let end = ident_end(b, i);
            if &text[i..end] == "mod" {
                if let Some((name, after)) = module_declaration(text, i) {
                    scan.modules.push(ModDecl {
                        name,
                        path: None,
                        test: false,
                    });
                    i = after;
                    continue;
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    scan
}

/// A run of `#[…]` attributes starting at `i`.
struct Attributes {
    /// Just past the last `]`.
    end: usize,
    /// Whether any of them confines the item to test builds.
    test: bool,
    /// The value of a `#[path = "…"]` among them.
    path: Option<String>,
}

fn attributes(text: &str, mut i: usize) -> Attributes {
    let b = text.as_bytes();
    let mut out = Attributes {
        end: i,
        test: false,
        path: None,
    };
    while b.get(i) == Some(&b'#') && b.get(i + 1) == Some(&b'[') {
        let close = closing(b, i + 1);
        let body = text[i + 2..close].trim();
        if let Some(cfg) = parenthesised(body, "cfg") {
            out.test |= cfg_is_test_only(cfg);
        } else if let Some(value) = body
            .strip_prefix("path")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('='))
        {
            out.path = Some(value.trim().trim_matches('"').to_string());
        }
        out.end = (close + 1).min(b.len());
        i = skip_trivia(b, out.end);
    }
    out
}

/// `inner` when `text` is `head(inner)`.
fn parenthesised<'a>(text: &'a str, head: &str) -> Option<&'a str> {
    text.strip_prefix(head)?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')
}

/// Whether a `cfg(…)` predicate can only be true in a test build.
fn cfg_is_test_only(predicate: &str) -> bool {
    let predicate = predicate.trim();
    if predicate == "test" {
        return true;
    }
    let Some(inner) = parenthesised(predicate, "all") else {
        return false;
    };
    let mut depth = 0usize;
    let mut from = 0;
    let mut args = Vec::new();
    for (at, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(&inner[from..at]);
                from = at + 1;
            }
            _ => {}
        }
    }
    args.push(&inner[from..]);
    args.into_iter().any(cfg_is_test_only)
}

/// `mod name;` at `i`, as the name and the offset just past the `;`.
///
/// `mod name { … }` is not a declaration of another file, so it is `None`.
fn module_declaration(text: &str, i: usize) -> Option<(String, usize)> {
    let b = text.as_bytes();
    if !b.get(i..)?.starts_with(b"mod") || !b.get(i + 3)?.is_ascii_whitespace() {
        return None;
    }
    let start = skip_trivia(b, i + 3);
    if !b.get(start).is_some_and(|&c| is_ident(c)) {
        return None;
    }
    let end = ident_end(b, start);
    let semi = skip_trivia(b, end);
    (b.get(semi) == Some(&b';')).then(|| (text[start..end].to_string(), semi + 1))
}

/// The first offset past `pub`, `pub(crate)`, `pub(in path)` at `i`.
fn skip_visibility(b: &[u8], i: usize) -> usize {
    let is_pub = b.get(i..).is_some_and(|rest| rest.starts_with(b"pub"))
        && !b.get(i + 3).is_some_and(|&c| is_ident(c));
    if !is_pub {
        return i;
    }
    let after = skip_trivia(b, i + 3);
    if b.get(after) == Some(&b'(') {
        return skip_trivia(b, (closing(b, after) + 1).min(b.len()));
    }
    after
}

/// Where the item starting at `start` ends.
///
/// At a `;`, or at the `}` that closes its body (a `fn`, an `impl`, a `mod
/// tests { … }`, a macro invocation). Something that is not a keyword-led item
/// — a struct field, an enum variant, a match arm — also ends at a `,`; a
/// keyword-led one does not, because `fn f<A, B>()` has commas outside any
/// bracket this counts. Either way it ends BEFORE a closer it did not open,
/// because that belongs to the enclosing scope.
fn item_end(text: &str, start: usize) -> usize {
    const KEYWORDS: [&str; 16] = [
        "fn",
        "impl",
        "mod",
        "trait",
        "struct",
        "enum",
        "union",
        "macro_rules",
        "use",
        "const",
        "static",
        "type",
        "extern",
        "let",
        "async",
        "unsafe",
    ];
    let b = text.as_bytes();
    let first = &text[start..ident_end(b, start)];
    let keyword_led = KEYWORDS.contains(&first);

    let mut depth = 0usize;
    let mut j = start;
    while j < b.len() {
        if let Some(next) = skip_literal(b, j) {
            j = next;
            continue;
        }
        match b[j] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return j;
                }
                depth -= 1;
                if depth == 0 && b[j] == b'}' {
                    return j + 1;
                }
            }
            b';' if depth == 0 => return j + 1,
            b',' if depth == 0 && !keyword_led => return j + 1,
            _ => {}
        }
        j += 1;
    }
    b.len()
}

/// The offset of the bracket that closes the one at `open`, or the end.
fn closing(b: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut j = open;
    while j < b.len() {
        if let Some(next) = skip_literal(b, j) {
            j = next;
            continue;
        }
        match b[j] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
        j += 1;
    }
    b.len()
}

/// Past any whitespace and comments at `i`.
fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i] == b'/' && matches!(b.get(i + 1), Some(b'/' | b'*')) {
            i = skip_literal(b, i).unwrap_or(i + 1);
        } else {
            break;
        }
    }
    i
}

/// Past the comment, string or char literal at `i`, if one starts there.
///
/// Brace matching is only as good as this: one `'{'` or `"}"` miscounted and a
/// test module's end lands in the wrong place.
fn skip_literal(b: &[u8], i: usize) -> Option<usize> {
    match b[i] {
        // Stops AT the newline, so line counting never loses one.
        b'/' if b.get(i + 1) == Some(&b'/') => Some(find_byte(b, i, b'\n')),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(block_comment_end(b, i)),
        b'"' => Some(string_end(b, i + 1)),
        b'\'' => char_literal_end(b, i),
        b'b' | b'c' | b'r' if i == 0 || !is_ident(b[i - 1]) => prefixed_literal_end(b, i),
        _ => None,
    }
}

fn find_byte(b: &[u8], from: usize, byte: u8) -> usize {
    b.get(from..)
        .and_then(|rest| rest.iter().position(|&c| c == byte))
        .map_or(b.len(), |at| from + at)
}

/// Past a `/* … */`, which nests.
fn block_comment_end(b: &[u8], i: usize) -> usize {
    let mut depth = 0usize;
    let mut j = i;
    while j + 1 < b.len() {
        if b[j] == b'/' && b[j + 1] == b'*' {
            depth += 1;
            j += 2;
        } else if b[j] == b'*' && b[j + 1] == b'/' {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return j;
            }
        } else {
            j += 1;
        }
    }
    b.len()
}

/// Past the closing `"` of a string whose body starts at `j`.
fn string_end(b: &[u8], mut j: usize) -> usize {
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

/// Past a char literal at `i`, or `None` when the `'` opens a lifetime.
fn char_literal_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i + 1) == Some(&b'\\') {
        // An escape: `'\n'`, `'\''`, `'\u{1F600}'`. The escaped character is
        // skipped first, so `'\''` does not end at its own second quote.
        return Some((find_byte(b, i + 3, b'\'') + 1).min(b.len()));
    }
    let width = match *b.get(i + 1)? {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    };
    // One character and a quote is a char; anything else is `'a` or `'static`.
    (b.get(i + 1 + width) == Some(&b'\'')).then_some(i + 2 + width)
}

/// Past `r"…"`, `r#"…"#`, `b"…"`, `br"…"`, `b'x'` or `c"…"` at `i`.
fn prefixed_literal_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    if b[j] != b'r' {
        j += 1;
        match b.get(j) {
            Some(b'"') => return Some(string_end(b, j + 1)),
            Some(b'\'') => return char_literal_end(b, j),
            Some(b'r') => {}
            _ => return None,
        }
    }
    j += 1;
    let hashes_from = j;
    while b.get(j) == Some(&b'#') {
        j += 1;
    }
    // `r#name` is a raw identifier, not a string.
    if b.get(j) != Some(&b'"') {
        return None;
    }
    let hashes = j - hashes_from;
    j += 1;
    while j < b.len() {
        if b[j] == b'"'
            && b.get(j + 1..j + 1 + hashes)
                .is_some_and(|tail| tail.iter().all(|&c| c == b'#'))
        {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(b.len())
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn ident_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && is_ident(b[i]) {
        i += 1;
    }
    i
}

/// `text` with every test region reduced to its newlines.
///
/// Newlines are kept so every declaration still reports the line it is on.
fn without_regions(text: &str, regions: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for &(start, end) in regions {
        out.push_str(&text[from..start]);
        out.extend(text[start..end].chars().filter(|&c| c == '\n'));
        from = end;
    }
    out.push_str(&text[from..]);
    out
}

/// Every file compiled only for tests because a parent declares it
/// `#[cfg(test)] mod name;` — and every module file declared beneath one.
///
/// # Why a file has to be told it is a test
///
/// A test module split into its own file (`runner/lane_tests.rs`,
/// `agent/claude_code/wire_tests.rs`, `permissions/rules/tests/*.rs`) has no
/// marker inside it: the `#[cfg(test)]` is on the declaration in the parent.
/// Read on its own, every call in it looked like a production caller and
/// every helper in it like a capability — the exact disguise the check exists
/// to see through. So the declaration is resolved to its file the way rustc
/// resolves it, and the file and its subtree are tests.
fn test_module_files(sources: &[Source], known: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    // A file loaded through `#[path]` keeps its children beside it, as a
    // `mod.rs` does.
    let path_loaded: BTreeSet<PathBuf> = sources
        .iter()
        .flat_map(|source| {
            source
                .scan
                .modules
                .iter()
                .filter(|m| m.path.is_some())
                .filter_map(|m| module_file(&source.path, false, m, known))
        })
        .map(|(file, _)| file)
        .collect();

    let mut pending: Vec<(PathBuf, bool)> = sources
        .iter()
        .flat_map(|source| {
            let owns_dir = owns_directory(&source.path) || path_loaded.contains(&source.path);
            source
                .scan
                .modules
                .iter()
                .filter(|m| m.test)
                .filter_map(move |m| module_file(&source.path, owns_dir, m, known))
        })
        .collect();

    let mut tests = BTreeSet::new();
    while let Some((file, owns_dir)) = pending.pop() {
        if !tests.insert(file.clone()) {
            continue;
        }
        // Everything a test file declares is test code too, `cfg` or not.
        if let Some(source) = sources.iter().find(|s| s.path == file) {
            pending.extend(
                source
                    .scan
                    .modules
                    .iter()
                    .filter_map(|m| module_file(&file, owns_dir, m, known)),
            );
        }
    }
    tests
}

/// Whether `file`'s child modules live beside it rather than in a folder
/// named after it.
fn owns_directory(file: &Path) -> bool {
    file.file_name()
        .is_some_and(|name| name == "mod.rs" || name == "lib.rs" || name == "main.rs")
}

/// The file `decl` names when `declaring` declares it, and whether that file
/// in turn owns its directory. `None` when it is not in the tree.
fn module_file(
    declaring: &Path,
    owns_dir: bool,
    decl: &ModDecl,
    known: &BTreeSet<PathBuf>,
) -> Option<(PathBuf, bool)> {
    let parent = declaring.parent()?;
    // `#[path]` outside an inline module is relative to the declaring
    // file's own directory, whatever kind of file that is.
    if let Some(path) = &decl.path {
        let file = normalize(&parent.join(path));
        return known.contains(&file).then_some((file, true));
    }
    let dir = if owns_dir {
        parent.to_path_buf()
    } else {
        parent.join(declaring.file_stem()?)
    };
    let flat = dir.join(format!("{}.rs", decl.name));
    if known.contains(&flat) {
        return Some((flat, false));
    }
    let nested = dir.join(&decl.name).join("mod.rs");
    known.contains(&nested).then_some((nested, true))
}

/// `path` with `.` and `..` folded away, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
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

/// The same code with comment lines BLANKED, not removed.
///
/// Blanked so line numbers still match the file. Removing them shifted every
/// declaration after the first doc comment, and this codebase's comments are
/// longer than its code — the first run reported line 12 for an item on line
/// 50, which sends a reader to the wrong place and makes the whole report
/// untrustworthy.
fn strip_comments(code: &str) -> String {
    code.lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `pub fn` names declared in this code, with their line numbers.
///
/// # Functions only, and that is the whole difference between a check and noise
///
/// The first version also took `struct`, `enum`, `const` and `trait`, and
/// reported 380 items against this crate — a number nobody reads, which means
/// a check nobody keeps. Almost all of them were fine: a type is normally
/// referred to by a path rather than a bare name, and a `pub const PATH` used
/// only by its own module is ordinary.
///
/// A function is different. Every one of the six capabilities this was written
/// for was an ACTION nobody took — `heartbeat`, `clear_session`, `with_deps`,
/// `Supervisor::start`. That is the shape worth reporting, and narrowing to it
/// is what makes the result small enough to act on.
fn declarations(code: &str) -> Vec<(String, usize)> {
    const KINDS: [&str; 1] = ["fn "];
    let mut out = Vec::new();

    // Test-only items are not here to be declared: `scan_source` already
    // blanked every one, so a helper that exists for tests never becomes a
    // permanent false entry on the list.
    for (index, line) in code.lines().enumerate() {
        let trimmed = line.trim_start();

        // `pub(crate)` and `pub(super)` count: a crate-visible item nothing in
        // the crate calls is the same defect, and more clearly so.
        let Some(rest) = trimmed.strip_prefix("pub") else {
            continue;
        };
        let rest = match rest.find(char::is_whitespace) {
            Some(cut) => rest[cut..].trim_start(),
            None => continue,
        };
        let Some(kind) = KINDS.iter().find(|k| rest.starts_with(**k)) else {
            continue;
        };
        let after = &rest[kind.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        out.push((name, index + 1));
    }
    out
}

/// How many times `code` uses `name` as a whole word.
fn count_mentions(code: &str, name: &str) -> usize {
    let boundary = |c: char| !(c.is_alphanumeric() || c == '_');
    let mut found = 0;
    let mut from = 0;
    while let Some(at) = code[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let before_ok = start == 0 || code[..start].chars().next_back().is_some_and(boundary);
        let after_ok = end >= code.len() || code[end..].chars().next().is_some_and(boundary);
        if before_ok && after_ok {
            found += 1;
        }
        from = end;
    }
    found
}

/// A path as a reader would type it.
fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
