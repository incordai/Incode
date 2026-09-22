//! What else this change touches.
//!
//! # The failure this exists to stop
//!
//! An agent changes a backend function and stops. The API layer that calls it,
//! the frontend that reads its response, and the tests that assert its old shape
//! are all still written against what it used to be. Every individual edit is
//! correct, the task's own check may even pass, and the system is broken in
//! three places nobody looked at.
//!
//! It is the most common way work LOOKS finished and is not, and it does not
//! announce itself: the file that was edited compiles, and the files that did
//! not change are the ones that are now wrong.
//!
//! # How this differs from [`crate::contracts`]
//!
//! Contracts are shapes frozen in ADVANCE, by a planner, before work is split
//! up. They catch `{user_id}` meeting `{userId}` across two tasks that never saw
//! each other. That only works for interfaces somebody thought to declare.
//!
//! This is the other half, and needs nothing declared: it reads the tree as it
//! is, records which files mention the names each file defines, and answers two
//! questions — *before* a turn, "what else touches this?", and *after* one,
//! "you changed a name three other files use and left them alone".
//!
//! # What this is NOT
//!
//! **It is a text index, not a compiler.** It does not resolve imports, scopes,
//! generics or dynamic dispatch. It will link two files that happen to share a
//! name, and it will miss a call made through a string or a reflection layer.
//!
//! That is stated plainly because the value is in the direction it points, not
//! in a guarantee: a list of five files worth checking, three of which matter,
//! is worth far more than the silence there is now. Every finding is phrased as
//! something to look at — never as a fact about the code, and never as something
//! that blocks work.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// How many files the index will walk.
///
/// Bounds the WALK, which is the action. What is reported is never truncated
/// silently: a tree over this is told the index is partial.
pub const MAX_INDEXED: usize = 20_000;

/// The largest file that will be read into the index.
///
/// A generated bundle or a vendored blob is megabytes of identifiers that link
/// everything to everything, which is the same as linking nothing.
pub const MAX_FILE_BYTES: u64 = 512 * 1024;

/// The shortest name worth indexing.
///
/// `id`, `get`, `new` and `run` appear in every file in every project, so
/// linking on them produces a graph where everything touches everything. Four
/// characters is where a name starts being about this codebase.
const MIN_SYMBOL_LEN: usize = 4;

/// Directories that hold no hand-written source.
const SKIP_DIRS: [&str; 9] = [
    "target",
    "node_modules",
    "dist",
    "build",
    ".git",
    ".incord",
    "vendor",
    "__pycache__",
    // Backups are COPIES of files that are already indexed. Left in, every
    // symbol resolves to its own history and the report names a dozen dead
    // paths for one live edit — observed listing eight `.backups/` files as
    // users of a name that had exactly one real caller.
    ".backups",
];

/// Extensions this can read usefully.
const SOURCE_EXTENSIONS: [&str; 16] = [
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "rb", "php", "cs", "c", "h", "cpp",
    "swift",
];

/// How a name is declared, per language family.
///
/// Deliberately a small set of prefixes rather than a parser: the goal is to
/// tell "this file DEFINES the name" from "this file MENTIONS it", which is the
/// difference between the thing that changed and the things that must follow.
const DEFINITION_MARKERS: [&str; 18] = [
    "fn ",
    "function ",
    "def ",
    "class ",
    "struct ",
    "enum ",
    "trait ",
    "interface ",
    "type ",
    "const ",
    "static ",
    "impl ",
    "func ",
    "public ",
    "private ",
    "protected ",
    "export ",
    "async ",
];

/// One file, as the index sees it.
#[derive(Debug, Clone, Default)]
struct FileFacts {
    /// Names this file declares.
    defines: BTreeSet<String>,
    /// Names this file uses.
    mentions: BTreeSet<String>,
    /// The contents, as one number, for spotting a change later.
    fingerprint: u64,
}

/// Which files are connected to which, by the names they share.
#[derive(Debug, Clone, Default)]
pub struct LinkMap {
    files: BTreeMap<String, FileFacts>,
    /// Whether the walk stopped early.
    truncated: bool,
}

/// A change that was left half-finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfinished {
    /// The file that changed.
    pub changed: String,
    /// The name it defines that others use.
    pub symbol: String,
    /// Files using that name which were NOT touched.
    pub untouched: Vec<String>,
}

impl LinkMap {
    /// Read every source file under these roots.
    ///
    /// Never fails: a file that cannot be read is skipped. An index that
    /// refused to build would take the harness down over one unreadable file,
    /// and this is an advisory, not a law.
    #[must_use]
    pub fn build(roots: &[PathBuf]) -> Self {
        let mut files = BTreeMap::new();
        let mut walked = 0usize;
        let mut truncated = false;
        let mut stack: Vec<PathBuf> = roots.to_vec();

        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !is_source(&path) {
                    continue;
                }
                walked += 1;
                if walked > MAX_INDEXED {
                    truncated = true;
                    break;
                }
                // A generated bundle links everything to everything, which is
                // the same as linking nothing.
                if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                files.insert(shown(&path), facts_of(&text));
            }
            if truncated {
                break;
            }
        }

        Self { files, truncated }
    }

    /// How many files were indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Whether the walk stopped before the tree did.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Indexed files whose path ends with what a person typed.
    ///
    /// Matched by suffix because nobody types the absolute path the index
    /// stores, and the file they mean is almost always the one whose tail they
    /// wrote.
    #[must_use]
    pub fn files_matching(&self, needle: &str) -> Vec<String> {
        let needle = needle.trim_start_matches("./");
        self.files
            .keys()
            .filter(|path| path.ends_with(needle))
            .cloned()
            .collect()
    }

    /// Files that use a name this one declares, with the name.
    ///
    /// This is the "who calls me" direction — the one that matters when a
    /// definition changes, because those are the files that must follow.
    #[must_use]
    pub fn dependents(&self, file: &str) -> Vec<(String, String)> {
        let Some(facts) = self.files.get(file) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for symbol in &facts.defines {
            for (other, other_facts) in &self.files {
                if other == file {
                    continue;
                }
                if other_facts.mentions.contains(symbol) {
                    out.push((other.clone(), symbol.clone()));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Files declaring names this one uses — the "what I depend on" direction.
    #[must_use]
    pub fn dependencies(&self, file: &str) -> Vec<(String, String)> {
        let Some(facts) = self.files.get(file) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (other, other_facts) in &self.files {
            if other == file {
                continue;
            }
            for symbol in other_facts.defines.intersection(&facts.mentions) {
                out.push((other.clone(), symbol.clone()));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Which indexed files have different contents now.
    #[must_use]
    pub fn changed_since(&self, roots: &[PathBuf]) -> Vec<String> {
        let now = Self::build(roots);
        let mut out = Vec::new();
        for (path, facts) in &self.files {
            match now.files.get(path) {
                Some(current) if current.fingerprint == facts.fingerprint => {}
                // A file that was deleted changed as surely as one that was
                // edited, and its dependents are just as stranded.
                _ => out.push(path.clone()),
            }
        }
        out
    }

    /// Changes whose dependents were left alone.
    ///
    /// The half-finished edit: a name was redefined and the files using it were
    /// not opened. Advisory — it says what to look at, never that the code is
    /// wrong, because a text index cannot know that.
    #[must_use]
    pub fn unfinished(&self, changed: &[String]) -> Vec<Unfinished> {
        let touched: BTreeSet<&String> = changed.iter().collect();
        let mut out = Vec::new();

        for file in changed {
            let mut by_symbol: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (dependent, symbol) in self.dependents(file) {
                if touched.contains(&dependent) {
                    // It was updated in the same turn, which is the whole point.
                    continue;
                }
                by_symbol.entry(symbol).or_default().push(dependent);
            }
            for (symbol, untouched) in by_symbol {
                out.push(Unfinished {
                    changed: file.clone(),
                    symbol,
                    untouched,
                });
            }
        }
        out
    }

    /// What to tell the agent BEFORE it works, given the files in hand.
    ///
    /// Named as something to check rather than as a list of files to edit: most
    /// changes do not need every caller touched, and an instruction to edit them
    /// all would be worse than silence.
    #[must_use]
    pub fn context_for(&self, files: &[String]) -> Option<String> {
        let mut lines = Vec::new();
        for file in files {
            let dependents = self.dependents(file);
            if dependents.is_empty() {
                continue;
            }
            lines.push(format!("  {file} is used by:"));
            for (dependent, symbol) in dependents.iter().take(12) {
                lines.push(format!("    {dependent} (uses `{symbol}`)"));
            }
            if dependents.len() > 12 {
                lines.push(format!("    …and {} more", dependents.len() - 12));
            }
        }
        if lines.is_empty() {
            return None;
        }
        Some(format!(
            "<connected_files>\nIf you change what a name MEANS — its arguments, its return shape, \
             its behaviour — the files below use that name and may need the same change in the \
             same pass. Check them; change the ones that actually need it.\n\n{}\n\nThis is a \
             text index, not a compiler: it can link files that merely share a word, and it can \
             miss a call made through a string. Use it as a place to look.\n</connected_files>",
            lines.join("\n")
        ))
    }

    /// What to hand back to the AGENT, so it finishes the job in the same pass.
    ///
    /// # Why this goes to the agent and not only to the person
    ///
    /// Printing "you left three callers behind" to a human means they have to
    /// come back, re-explain the task, and pay for a second turn. The agent is
    /// still there, still holding the context, and is the thing that can act on
    /// it. Telling it directly is the difference between a warning and a fix.
    ///
    /// Phrased as a CHECK, not an order: most changes do not need every caller
    /// touched — a renamed local, a comment, a new overload beside the old one.
    /// An instruction to update them all would cause the damage it is meant to
    /// prevent, so it asks for judgement and explicitly permits doing nothing.
    #[must_use]
    pub fn follow_up(unfinished: &[Unfinished]) -> Option<String> {
        if unfinished.is_empty() {
            return None;
        }
        let mut out = String::from(
            "Before this is finished: you changed names that other files use, and those files were not opened.
",
        );
        for item in unfinished {
            out.push_str(&format!(
                "
  `{}` in {} is still used by:
",
                item.symbol, item.changed
            ));
            for file in item.untouched.iter().take(8) {
                out.push_str(&format!(
                    "    {file}
"
                ));
            }
            if item.untouched.len() > 8 {
                out.push_str(&format!(
                    "    …and {} more
",
                    item.untouched.len() - 8
                ));
            }
        }
        out.push_str(
            "
Open each one and decide: does it need the same change? If the meaning of the name changed — its arguments, its return shape, its behaviour — update it now, in this pass. If it does not need changing, say so and leave it alone; a needless edit is worse than none.

This list comes from a text index, not a compiler, so it can name a file that merely shares a word. Judge each one.",
        );
        Some(out)
    }

    /// What to tell a person AFTER the turn.
    #[must_use]
    pub fn report(unfinished: &[Unfinished]) -> Option<String> {
        if unfinished.is_empty() {
            return None;
        }
        let mut out = String::from(
            "Changed names whose users were not touched. Worth a look before calling this done:\n",
        );
        for item in unfinished {
            out.push_str(&format!(
                "\n  {} redefines `{}`, still used by:\n",
                item.changed, item.symbol
            ));
            for file in item.untouched.iter().take(8) {
                out.push_str(&format!("    {file}\n"));
            }
            if item.untouched.len() > 8 {
                out.push_str(&format!("    …and {} more\n", item.untouched.len() - 8));
            }
        }
        out.push_str(
            "\nA text index cannot tell a signature change from a comment edit, so this is a \
             prompt to check rather than a fault.",
        );
        Some(out)
    }
}

/// Whether this file is worth reading.
fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// A path as it reads in a report.
fn shown(path: &Path) -> String {
    path.to_string_lossy().replace(char::from(92), "/")
}

/// What one file declares and what it uses.
fn facts_of(text: &str) -> FileFacts {
    let mut facts = FileFacts {
        fingerprint: fingerprint(text),
        ..FileFacts::default()
    };

    for line in text.lines() {
        let trimmed = line.trim_start();
        // A comment mentioning a name is not a use of it, and a commented-out
        // definition is not a definition.
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        let defining = DEFINITION_MARKERS
            .iter()
            .any(|marker| trimmed.starts_with(marker) || trimmed.contains(&format!(" {marker}")));

        let mut first = true;
        for word in identifiers(line) {
            // The first significant identifier after a definition marker is the
            // name being declared; the rest of the line is its signature, which
            // MENTIONS other names rather than defining them.
            if defining && first {
                facts.defines.insert(word);
                first = false;
            } else {
                facts.mentions.insert(word);
            }
        }
    }
    facts
}

/// The identifiers on one line that are specific enough to link on.
fn identifiers(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            push_if_significant(&mut out, &current);
            current.clear();
        }
    }
    push_if_significant(&mut out, &current);
    out
}

fn push_if_significant(out: &mut Vec<String>, word: &str) {
    if word.len() < MIN_SYMBOL_LEN {
        return;
    }
    // A number is not a name.
    if word.chars().next().is_some_and(char::is_numeric) {
        return;
    }
    if is_keyword(word) {
        return;
    }
    out.push(word.to_string());
}

/// Words every language uses, which link nothing when indexed.
fn is_keyword(word: &str) -> bool {
    // The list below covers C-family, Python and JavaScript. It did NOT cover
    // Rust, so in a Rust codebase `impl` and `struct` were treated as ordinary
    // identifiers: the impact report told the owner that `src/cli/mod.rs`
    // "redefines `impl`, still used by" eighteen files. Every language whose
    // source this indexes needs its keywords here, or its most common words
    // become its most connected symbols.
    const KEYWORDS: &[&str] = &[
        "self",
        "this",
        "true",
        "false",
        "null",
        "none",
        "return",
        "import",
        "from",
        "export",
        "const",
        "class",
        "function",
        "async",
        "await",
        "yield",
        "while",
        "break",
        "continue",
        "match",
        "case",
        "else",
        "elif",
        "then",
        "public",
        "private",
        "protected",
        "static",
        "final",
        "void",
        "string",
        "number",
        "boolean",
        "object",
        "array",
        "print",
        "console",
        "value",
        "result",
        "error",
        // Rust
        "impl",
        "struct",
        "enum",
        "trait",
        "unsafe",
        "crate",
        "super",
        "type",
        "move",
        "where",
        "dyn",
        "mod",
        "pub",
        "let",
        "mut",
        "ref",
        "loop",
        "fn",
        "use",
        "extern",
    ];
    KEYWORDS.contains(&word.to_ascii_lowercase().as_str())
}

/// A file's contents as one number.
///
/// Hand-rolled FNV-1a for the same reason as everywhere else in this crate: the
/// standard library's hasher does not promise a stable value between versions,
/// and an index taken before a toolchain upgrade must compare against one taken
/// after it.
fn fingerprint(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **The agent is told, not just the person.** Printing a warning to a
    /// human means they come back, re-explain the task and pay for another
    /// turn; the agent is still there and can act now.
    #[test]
    fn the_follow_up_goes_to_the_agent_and_names_the_files() {
        let unfinished = vec![Unfinished {
            changed: "backend.rs".to_string(),
            symbol: "calculate_invoice_total".to_string(),
            untouched: vec!["api.rs".to_string(), "frontend.ts".to_string()],
        }];
        let text = LinkMap::follow_up(&unfinished).expect("something to send");
        assert!(text.contains("calculate_invoice_total"), "got: {text}");
        assert!(
            text.contains("api.rs") && text.contains("frontend.ts"),
            "got: {text}"
        );
    }

    /// **It asks for judgement and permits doing nothing.** Most changes do not
    /// need every caller touched, and an order to update them all would cause
    /// the damage it exists to prevent.
    #[test]
    fn the_follow_up_permits_leaving_a_file_alone() {
        let text = LinkMap::follow_up(&[Unfinished {
            changed: "a.rs".to_string(),
            symbol: "some_name".to_string(),
            untouched: vec!["b.rs".to_string()],
        }])
        .expect("text");
        assert!(text.contains("leave it alone"), "got: {text}");
        assert!(text.contains("needless edit is worse"), "got: {text}");
        assert!(
            text.contains("not a compiler"),
            "its limits must travel: {text}"
        );
    }

    /// Nothing left behind means nothing is sent — no turn is spent saying
    /// "well done".
    #[test]
    fn a_complete_change_sends_no_follow_up() {
        assert!(LinkMap::follow_up(&[]).is_none());
    }

    /// A tiny project: a backend that defines a name, and two files that use it.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("backend.rs"),
            "pub fn calculate_invoice_total(items: &[Item]) -> Money { total(items) }",
        )
        .expect("backend");
        std::fs::write(
            dir.path().join("api.rs"),
            "fn handler() { let t = calculate_invoice_total(&items); respond(t) }",
        )
        .expect("api");
        std::fs::write(
            dir.path().join("frontend.ts"),
            "const total = await fetchJson('/invoice'); render(calculate_invoice_total);",
        )
        .expect("frontend");
        std::fs::write(
            dir.path().join("unrelated.rs"),
            "pub fn something_else() -> bool { true }",
        )
        .expect("unrelated");
        dir
    }

    /// **The question the agent should have asked.** Change the backend, and
    /// these are the files that use what it defines.
    #[test]
    fn a_definition_knows_which_files_use_it() {
        let dir = project();
        let map = LinkMap::build(&[dir.path().to_path_buf()]);

        let dependents = map.dependents(&shown(&dir.path().join("backend.rs")));
        let files: Vec<&str> = dependents.iter().map(|(f, _)| f.as_str()).collect();

        assert!(
            files.iter().any(|f| f.ends_with("api.rs")),
            "the API layer calls it: {dependents:?}"
        );
        assert!(
            files.iter().any(|f| f.ends_with("frontend.ts")),
            "the frontend names it too: {dependents:?}"
        );
        assert!(
            !files.iter().any(|f| f.ends_with("unrelated.rs")),
            "a file sharing nothing must not be linked: {dependents:?}"
        );
    }

    /// **The half-finished edit.** The backend changed and its callers did not —
    /// which is exactly the state that looks done and is not.
    #[test]
    fn changing_a_definition_without_its_users_is_reported() {
        let dir = project();
        let roots = vec![dir.path().to_path_buf()];
        let map = LinkMap::build(&roots);

        // The signature changes. Nothing else is touched.
        std::fs::write(
            dir.path().join("backend.rs"),
            "pub fn calculate_invoice_total(items: &[Item], tax: Rate) -> Money { total(items) }",
        )
        .expect("edit");

        let changed = map.changed_since(&roots);
        assert_eq!(changed.len(), 1, "only the backend moved: {changed:?}");

        let unfinished = map.unfinished(&changed);
        assert!(!unfinished.is_empty(), "the callers were left behind");
        let names: Vec<&str> = unfinished
            .iter()
            .flat_map(|u| u.untouched.iter().map(String::as_str))
            .collect();
        assert!(
            names.iter().any(|f| f.ends_with("api.rs")),
            "got: {names:?}"
        );

        let report = LinkMap::report(&unfinished).expect("something to say");
        assert!(report.contains("calculate_invoice_total"), "got: {report}");
        assert!(
            report.contains("prompt to check rather than a fault"),
            "it must not claim the code is wrong: {report}"
        );
    }

    /// **Updating the callers in the same pass reports nothing.** A guard that
    /// fired on correct work would be switched off within a day.
    #[test]
    fn updating_everything_together_reports_nothing() {
        let dir = project();
        let roots = vec![dir.path().to_path_buf()];
        let map = LinkMap::build(&roots);

        for (name, body) in [
            (
                "backend.rs",
                "pub fn calculate_invoice_total(items: &[Item], tax: Rate) -> Money { total(items) }",
            ),
            (
                "api.rs",
                "fn handler() { let t = calculate_invoice_total(&items, tax); respond(t) }",
            ),
            (
                "frontend.ts",
                "const total = await fetchJson('/invoice'); render(calculate_invoice_total, tax);",
            ),
        ] {
            std::fs::write(dir.path().join(name), body).expect("edit");
        }

        let changed = map.changed_since(&roots);
        assert_eq!(changed.len(), 3);
        assert!(
            map.unfinished(&changed).is_empty(),
            "everything moved together, so there is nothing to say"
        );
    }

    /// The context handed to the agent points at files, and says what it is not.
    #[test]
    fn the_context_says_check_rather_than_edit() {
        let dir = project();
        let map = LinkMap::build(&[dir.path().to_path_buf()]);
        let block = map
            .context_for(&[shown(&dir.path().join("backend.rs"))])
            .expect("there are connected files");

        assert!(block.contains("api.rs"), "got: {block}");
        assert!(
            block.contains("Check them"),
            "it must ask for a check, not an edit: {block}"
        );
        assert!(
            block.contains("not a compiler"),
            "its limits must travel with it: {block}"
        );
    }

    /// **Short and common names are not indexed.** `id`, `get` and `new` appear
    /// everywhere, so linking on them produces a graph where every file touches
    /// every other — which is the same as no graph at all.
    #[test]
    fn common_words_do_not_link_everything_to_everything() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("a.rs"),
            "pub fn get(id: u32) -> Value { new() }",
        )
        .expect("a");
        std::fs::write(dir.path().join("b.rs"), "fn other() { get(id); new(); }").expect("b");

        let map = LinkMap::build(&[dir.path().to_path_buf()]);
        assert!(
            map.dependents(&shown(&dir.path().join("a.rs"))).is_empty(),
            "nothing specific enough to link on"
        );
    }

    /// A comment naming a function is not a use of it.
    #[test]
    fn a_mention_in_a_comment_is_not_a_use() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("def.rs"),
            "pub fn reconcile_ledger_entries() {}",
        )
        .expect("def");
        std::fs::write(
            dir.path().join("prose.rs"),
            "// one day this should call reconcile_ledger_entries\nfn unrelated() {}",
        )
        .expect("prose");

        let map = LinkMap::build(&[dir.path().to_path_buf()]);
        assert!(
            map.dependents(&shown(&dir.path().join("def.rs")))
                .is_empty(),
            "a comment is not a caller"
        );
    }

    /// A deleted file's users are as stranded as an edited one's.
    #[test]
    fn deleting_a_definition_counts_as_changing_it() {
        let dir = project();
        let roots = vec![dir.path().to_path_buf()];
        let map = LinkMap::build(&roots);

        std::fs::remove_file(dir.path().join("backend.rs")).expect("delete");

        let changed = map.changed_since(&roots);
        assert!(
            changed.iter().any(|f| f.ends_with("backend.rs")),
            "got: {changed:?}"
        );
        assert!(!map.unfinished(&changed).is_empty());
    }

    /// Build output is never indexed: it links everything to everything and is
    /// nobody's source.
    #[test]
    fn rust_keywords_are_not_symbols() {
        // The impact report told the owner that a file "redefines `impl`,
        // still used by" eighteen others. `impl` is a keyword, not a name.
        for word in [
            "impl", "struct", "enum", "trait", "unsafe", "crate", "where",
        ] {
            assert!(
                identifiers(&format!("pub {word} Thing"))
                    .iter()
                    .all(|s| s != word),
                "`{word}` is a Rust keyword and must never be linked on"
            );
        }
    }

    #[test]
    fn real_names_still_link() {
        // The control: if the keyword list swallowed everything, the test above
        // would pass while the feature did nothing.
        let found = identifiers("pub struct LinkMap { roots: Vec<PathBuf> }");
        assert!(found.iter().any(|s| s == "LinkMap"), "found: {found:?}");
    }

    #[test]
    fn backup_copies_are_not_indexed() {
        // Backups are required by the working rules, so they exist in every
        // project here. Indexed, every symbol appears to have a dozen extra
        // users that are all its own history.
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join(".backups")).expect("dir");
        std::fs::write(
            dir.path().join(".backups").join("old.rs"),
            "pub fn distinctive_backup_name() {}",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("live.rs"),
            "pub fn distinctive_backup_name() {}",
        )
        .expect("write");
        let map = LinkMap::build(&[dir.path().to_path_buf()]);
        let report = format!("{:?}", map.files_matching("distinctive_backup_name"));
        assert!(!report.contains(".backups"), "backups leaked in: {report}");
    }

    #[test]
    fn generated_trees_are_not_indexed() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("node_modules")).expect("dir");
        std::fs::write(
            dir.path().join("node_modules").join("lib.js"),
            "function calculate_invoice_total() {}",
        )
        .expect("write");
        std::fs::write(dir.path().join("real.rs"), "fn main() {}").expect("write");

        let map = LinkMap::build(&[dir.path().to_path_buf()]);
        assert_eq!(map.len(), 1, "only the real source is indexed");
    }
}
