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
//! shape this is looking for.
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
use std::path::{Path, PathBuf};

/// How many unwired capabilities this crate is known to carry.
///
/// A RATCHET, not a target. It may go down and never up: the check landed
/// with 68 already there, and a gate that cannot be green on the day it
/// arrives is a gate somebody deletes. What it enforces is that a seventh
/// `PermissionPolicy` cannot be added quietly — which is the thing that
/// actually happened, six times.
///
/// Lower it whenever the number falls. The test fails and tells you to.
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
         own, inside its own process. (The API route's compaction, \
         `agent::compact`, runs inside the backend, which holds no lifecycle \
         hooks.) The claude CLI's stream \
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

    // Every file's production half, keyed by path. Read once: this is quadratic
    // in the number of items otherwise, and the tree has thousands.
    let mut production: Vec<(PathBuf, String)> = Vec::new();
    let mut declared: Vec<(String, PathBuf, usize)> = Vec::new();

    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let code = strip_comments(production_half(&text));
        for (name, line) in declarations(&code) {
            declared.push((name, path.clone(), line));
        }
        production.push((path.clone(), code));
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

/// A file up to its first `#[cfg(test)]`.
///
/// Everything after it is the tests, and a capability whose only callers are
/// its own tests is exactly what this looks for — so counting them would make
/// the check report nothing, forever.
fn production_half(text: &str) -> &str {
    // THE MARKER MUST BE AT COLUMN ZERO.
    //
    // `text.find("#[cfg(test)]")` was wrong in a way that made the check lie
    // rather than merely miss things. `cli/agent_turn.rs` has a single
    // test-only method at line 177 — `#[cfg(test)] pub fn with_backend` —
    // and matching that threw away the remaining 1,028 lines of the file.
    // Every call site in it became invisible, so `checkpoint::begin`,
    // `has_undo`, `pending_label` and `undo_last` were all reported as having
    // no caller while `/undo` was working perfectly.
    //
    // That is a FALSE POSITIVE, which this module's own header calls the
    // thing that kills a check: four items were on the list for months
    // pointing at code that was fine, and the next reader has to verify by
    // hand before believing any entry.
    //
    // A test MODULE is written at column zero (`#[cfg(test)]\nmod tests`);
    // an attribute on an item inside an `impl` is indented. Anchoring to the
    // line start keeps the trailing test block out and keeps the file in.
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.starts_with("#[cfg(test)]") {
            return &text[..offset];
        }
        offset += line.len();
    }
    text
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

    // Whether the previous non-blank line marked the next item as test-only.
    // Now that `production_half` keeps the rest of a file after an item-level
    // `#[cfg(test)]`, those items are in range — and a helper that exists only
    // for tests is not a production capability, so declaring it would put a
    // permanent false entry on the list.
    let mut test_only = false;

    for (index, line) in code.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("#[cfg(test)]") {
            test_only = true;
            continue;
        }

        // `pub(crate)` and `pub(super)` count: a crate-visible item nothing in
        // the crate calls is the same defect, and more clearly so.
        let Some(rest) = trimmed.strip_prefix("pub") else {
            // Attributes and doc lines sit between the marker and the item, so
            // the flag survives them and clears on the first real statement.
            if !trimmed.starts_with('#') {
                test_only = false;
            }
            continue;
        };
        if test_only {
            test_only = false;
            continue;
        }
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
mod tests {
    use super::*;

    #[test]
    fn a_pub_item_nobody_calls_is_found() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("lonely.rs"),
            "pub fn never_called_anywhere() {}\npub fn called_from_elsewhere() {}\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("caller.rs"),
            "fn go() { called_from_elsewhere(); }\n",
        )
        .expect("write");

        let found = unwired_in(dir.path()).expect("scan");
        let names: Vec<&str> = found.iter().map(|u| u.name.as_str()).collect();

        assert!(names.contains(&"never_called_anywhere"), "got: {names:?}");
        assert!(
            !names.contains(&"called_from_elsewhere"),
            "a called item was reported: {names:?}"
        );
    }

    /// **THE TESTS ARE NOT CALLERS.**
    ///
    /// The whole point. Every one of the six capabilities this was written
    /// for had passing tests — that is what made them look used, and what
    /// kept them unwired for months.
    #[test]
    fn a_test_calling_it_does_not_count_as_wired() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("thing.rs"), "pub fn heartbeat() {}\n").expect("write");
        std::fs::write(
            dir.path().join("other.rs"),
            "fn unrelated() {}\n#[cfg(test)]\nmod tests {\n  #[test]\n  fn t() { heartbeat(); }\n}\n",
        )
        .expect("write");

        let names: Vec<String> = unwired_in(dir.path())
            .expect("scan")
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert!(
            names.contains(&"heartbeat".to_string()),
            "a test was counted as a caller: {names:?}"
        );
    }

    /// **A MENTION IN A DOC COMMENT IS NOT A CALL.**
    ///
    /// This codebase refers to items by name constantly. Counting those would
    /// mean the better-documented an unwired item is, the more wired it looks
    /// — and nothing would ever be reported.
    #[test]
    fn being_written_about_is_not_being_called() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("a.rs"), "pub fn orphan() {}\n").expect("write");
        std::fs::write(
            dir.path().join("b.rs"),
            "/// Calls [`orphan`] one day.\n// orphan();\nfn real() {}\n",
        )
        .expect("write");

        let names: Vec<String> = unwired_in(dir.path())
            .expect("scan")
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert!(
            names.contains(&"orphan".to_string()),
            "prose was counted as a caller: {names:?}"
        );
    }

    /// **A MODULE CALLING ITS OWN HELPER IS NOT A FINDING.**
    ///
    /// The rule this replaced was "no caller outside the defining file", and
    /// against the real tree it reported 225 items — almost all of them
    /// ordinary helpers used exactly where they were meant to be. A number
    /// that size is a check nobody reads.
    ///
    /// What the six real capabilities had in common is narrower: the
    /// declaration was the ONLY appearance of the name in production code
    /// anywhere.
    #[test]
    fn a_helper_its_own_module_calls_is_wired() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("only.rs"),
            "pub fn helper() {}\nfn local() { helper(); }\n",
        )
        .expect("write");

        let names: Vec<String> = unwired_in(dir.path())
            .expect("scan")
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert!(
            !names.contains(&"helper".to_string()),
            "an ordinary helper was reported as dead: {names:?}"
        );
    }

    /// **ONE TEST-ONLY METHOD MUST NOT BLIND THE REST OF ITS FILE.**
    ///
    /// The check used to cut a file at the first `#[cfg(test)]` anywhere in
    /// it. `cli/agent_turn.rs` has one test-only method 177 lines in, so the
    /// remaining 1,028 lines — every caller in the file — were invisible, and
    /// four working capabilities sat on the dead list for months:
    /// `checkpoint::begin`, `has_undo`, `pending_label` and `undo_last`, all
    /// of them reached every time anyone typed `/undo`.
    ///
    /// A false positive is worse than a miss here. It makes the list
    /// something a reader has to verify by hand before believing any of it,
    /// which is how a check stops being used.
    #[test]
    fn a_test_only_method_does_not_hide_the_callers_below_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn used_later() {}\n\
             impl Thing {\n\
             \x20   #[cfg(test)]\n\
             \x20   pub fn only_for_tests(&self) {}\n\
             }\n\
             fn real_caller() { used_later(); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20   fn t() { whatever(); }\n\
             }\n",
        )
        .expect("write");

        let names: Vec<String> = unwired_in(dir.path())
            .expect("scan")
            .into_iter()
            .map(|u| u.name)
            .collect();

        assert!(
            !names.contains(&"used_later".to_string()),
            "a caller below an item-level `#[cfg(test)]` was not seen: {names:?}"
        );
        // AND THE TEST-ONLY ITEM IS NOT A CAPABILITY. Keeping the rest of the
        // file in scope must not put every test helper onto the list instead.
        assert!(
            !names.contains(&"only_for_tests".to_string()),
            "a test-only method was reported as a dead capability: {names:?}"
        );
    }

    /// Every allowlist entry carries a reason somebody had to write.
    #[test]
    fn nothing_is_excused_without_saying_why() {
        for (name, why) in DELIBERATELY_UNWIRED {
            assert!(!name.trim().is_empty(), "an entry with no name");
            assert!(
                why.trim().len() > 30,
                "`{name}` is excused without a real reason: {why:?}"
            );
        }
    }

    /// **THIS CRATE, CHECKED AGAINST ITSELF — AS A RATCHET.**
    ///
    /// The first run over the real tree found 68. Demanding zero would mean
    /// wiring or excusing all of them in one unfocused change, and a gate that
    /// cannot be made green on the day it lands is a gate somebody deletes.
    ///
    /// So the number may go DOWN and never up. That is the property worth
    /// enforcing: it does not clear the debt, it stops a seventh
    /// `PermissionPolicy` being added without anyone noticing — which is the
    /// failure that actually happened, six times.
    ///
    /// When it goes down, lower [`MAX_UNWIRED`]. The test says so on failure.
    #[test]
    fn no_new_dead_capability_is_added() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let found = unwired_in(&src).expect("the source tree must be readable");

        assert!(
            found.len() <= MAX_UNWIRED,
            "{} capabilities have no caller, up from {MAX_UNWIRED}. Wire the new one \
             up, or add it to DELIBERATELY_UNWIRED with the reason:\n{}",
            found.len(),
            found
                .iter()
                .map(Unwired::line_report)
                .collect::<Vec<_>>()
                .join("\n")
        );

        assert!(
            found.len() >= MAX_UNWIRED,
            "good news: {} capabilities are unwired, down from {MAX_UNWIRED}. \
             Lower MAX_UNWIRED to {} so the ratchet holds the ground you just took.",
            found.len(),
            found.len()
        );
    }
}
