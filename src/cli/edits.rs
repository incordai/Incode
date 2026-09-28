//! What an agent changed in a file, shown as the change itself.
//!
//! # Why a result panel was not enough
//!
//! An edit used to reach the screen as [`crate::cli::panel::result`] does:
//! `✓ Edit  src/foo.rs` and whatever confirmation string the tool returned.
//! That answers "a file changed" and refuses to answer the only two questions a
//! person watching actually has — WHICH FILE, and WHAT PART OF IT. The answers
//! were already in hand: the call carries the path and both sides of the
//! splice, and the file on disk carries the line numbers. None of it was read.
//!
//! # Named by SHAPE, never by tool name
//!
//! Nothing here matches `Edit` or `Write` or any other agent's word for the
//! same act. This harness drives several agents and is meant to drive ones
//! nobody has written yet, so a change is recognised by the KEYS a call carries
//! — a path, and either both sides of a splice or a whole body. An agent whose
//! editing tool is called something else gets this view for free; one that
//! invents a new key gets the plain panel, which is what it had before.
//!
//! # Shown only when the edit HAPPENED
//!
//! The block is printed on a successful result, never on the call. A write can
//! be refused — by this harness's own guards, by the agent's permission mode,
//! by the file system — and a diff printed at call time claims a change that
//! may never have landed. Everything else here follows the same rule the ledger
//! follows: show what happened, not what was asked for.

/// Lines of a change shown before the rest is counted.
const MAX_LINES: usize = 14;
/// Context lines kept either side of the change, when line numbers are known.
const CONTEXT: usize = 2;
/// The longest a single line is shown before it is cut.
const LINE_WIDTH: usize = 100;

/// One file change, as the agent asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    /// The lines being taken out. Empty for a file being created.
    pub removed: Vec<String>,
    /// The lines going in. Empty for a deletion.
    pub added: Vec<String>,
    /// Whether this replaces the WHOLE file rather than splicing part of it.
    ///
    /// Kept because the two read differently to a person: a splice into a known
    /// file is an edit, and a whole body is the file being written.
    pub whole_file: bool,
}

impl Change {
    /// How many lines this adds and removes.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        (self.added.len(), self.removed.len())
    }

    /// Name the file the way the person working in this tree would.
    ///
    /// An agent passes an ABSOLUTE path, and the first live run printed one
    /// 110 characters long — a header where the identifying part, the file
    /// name, sat off the end of the useful width behind directories the reader
    /// chose themselves. Relative to the tree in hand it is `src/cli/edits.rs`.
    ///
    /// A path OUTSIDE the tree keeps its full form: for those the leading
    /// directories are the whole point, and shortening one would make a write
    /// into a neighbouring project look like a local edit.
    #[must_use]
    pub fn under(mut self, root: &std::path::Path) -> Self {
        let path = std::path::Path::new(&self.path);
        if let Ok(inside) = path.strip_prefix(root) {
            // Separators normalised so one tree does not report two spellings
            // of the same file and count it twice.
            self.path = inside.to_string_lossy().replace('\\', "/");
        }
        self
    }
}

/// The path a call is acting on, if it names one.
fn path_in(call: &serde_json::Value) -> Option<String> {
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(found) = call.get(key).and_then(serde_json::Value::as_str) {
            let trimmed = found.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Split into lines, dropping the single trailing empty that a body ending in a
/// newline produces. Counting that as a line inflates every count by one.
fn lines_of(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.lines().map(str::to_string).collect()
}

/// A file change exactly as the call carries it, text untouched.
///
/// [`Change`] is the same thing cut into lines for a terminal block, which
/// loses whether the text ended in a newline. A front end that shows the
/// before and after text itself — `incode --acp` — needs it as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    /// Part of the file replaced: `old` taken out, `new` put in its place.
    Splice {
        path: String,
        old: String,
        new: String,
    },
    /// The whole file written with `body`.
    Whole { path: String, body: String },
}

/// The change a tool call asks for, or `None` when it is not an edit.
///
/// See the module note: recognised by the keys present, never by tool name.
/// The one place those keys are known; [`change_from`] is built on it.
#[must_use]
pub fn asked_in(input: &str) -> Option<Asked> {
    let call: serde_json::Value = serde_json::from_str(input).ok()?;
    let path = path_in(&call)?;

    // A splice: both sides given. `new_string` may legitimately be empty — that
    // is a deletion — so presence is what matters, not emptiness.
    let old = call.get("old_string").and_then(serde_json::Value::as_str);
    let new = call.get("new_string").and_then(serde_json::Value::as_str);
    if old.is_some() || new.is_some() {
        return Some(Asked::Splice {
            path,
            old: old.unwrap_or_default().to_string(),
            new: new.unwrap_or_default().to_string(),
        });
    }

    // A whole body. `content` is the common key; `new_source` is a notebook's.
    for key in ["content", "new_source"] {
        if let Some(body) = call.get(key).and_then(serde_json::Value::as_str) {
            return Some(Asked::Whole {
                path,
                body: body.to_string(),
            });
        }
    }
    None
}

/// The change a tool call describes, or `None` when it is not an edit.
///
/// See the module note: recognised by the keys present, never by tool name.
#[must_use]
pub fn change_from(input: &str) -> Option<Change> {
    Some(match asked_in(input)? {
        Asked::Splice { path, old, new } => Change {
            path,
            removed: lines_of(&old),
            added: lines_of(&new),
            whole_file: false,
        },
        Asked::Whole { path, body } => Change {
            path,
            removed: Vec::new(),
            added: lines_of(&body),
            whole_file: true,
        },
    })
}

/// The 1-based line where `needle` begins in `text`, if it is there.
///
/// Used to number the block against the file that is actually on disk, so the
/// numbers a person reads are the ones their editor will show. `None` when the
/// text cannot be found — a file the harness cannot read, or an edit the agent
/// described differently from how it landed — and the block is then shown
/// unnumbered rather than with numbers that would be invented.
#[must_use]
pub fn located_in(text: &str, needle: &str) -> Option<usize> {
    let first = needle.lines().next()?;
    if first.trim().is_empty() {
        return None;
    }
    let wanted: Vec<&str> = needle.lines().collect();
    let haystack: Vec<&str> = text.lines().collect();
    haystack
        .windows(wanted.len().max(1))
        .position(|window| window == wanted.as_slice())
        .map(|at| at + 1)
}

/// Cut a line to `width`, marking the cut.
fn cut(line: &str, width: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= width {
        return line.to_string();
    }
    let kept: String = chars[..width.saturating_sub(1)].iter().collect();
    format!("{kept}\u{2026}")
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// What the change did, in words, for the line under the header.
fn tally_line(added: usize, removed: usize) -> String {
    match (added, removed) {
        (0, 0) => "No lines changed".to_string(),
        (a, 0) => format!("Added {a} line{}", plural(a)),
        (0, r) => format!("Removed {r} line{}", plural(r)),
        (a, r) => format!("Added {a} line{}, removed {r} line{}", plural(a), plural(r)),
    }
}

/// One change, as the lines to print.
///
/// `body` is the file as it stands AFTER the edit, when the harness could read
/// it; the added lines are located in it to number the block. Pass `None` and
/// the same block is produced without numbers.
///
/// Returned rather than printed so every decision here — what the header says,
/// which lines survive, what the remainder claims — can be asserted on without
/// a terminal, exactly as [`crate::cli::panel::result`] is.
#[must_use]
pub fn block(change: &Change, body: Option<&str>, width: usize) -> Vec<String> {
    let (added, removed) = change.counts();
    let verb = if change.whole_file { "Write" } else { "Update" };
    let mut out = vec![
        format!("{verb}({})", change.path),
        format!("  {}", tally_line(added, removed)),
    ];

    // Where the new text sits in the finished file. A whole-file write starts
    // at line 1 by definition; a splice has to be found.
    let start = if change.whole_file {
        Some(1)
    } else {
        body.and_then(|text| located_in(text, &change.added.join("\n")))
    };

    let width = width.min(LINE_WIDTH);
    let mut shown = 0usize;
    let push = |out: &mut Vec<String>, mark: char, number: Option<usize>, text: &str| {
        let numbered = match number {
            Some(n) => format!("{n:>6} "),
            None => "       ".to_string(),
        };
        out.push(format!("{numbered}{mark}{}", cut(text, width)));
    };

    // Context above, when the file could be read and the text found.
    if let (Some(at), Some(text)) = (start, body) {
        let before: Vec<&str> = text.lines().collect();
        let from = at.saturating_sub(1).saturating_sub(CONTEXT);
        for (offset, line) in before[from..at.saturating_sub(1)].iter().enumerate() {
            push(&mut out, ' ', Some(from + offset + 1), line);
        }
    }

    for line in &change.removed {
        if shown == MAX_LINES {
            break;
        }
        // A removed line has no number in the finished file — it is not there
        // any more. Numbering it with the line that replaced it would point a
        // reader at text that says something else.
        push(&mut out, '-', None, line);
        shown += 1;
    }
    for (offset, line) in change.added.iter().enumerate() {
        if shown == MAX_LINES {
            break;
        }
        push(&mut out, '+', start.map(|at| at + offset), line);
        shown += 1;
    }

    let total = added + removed;
    if total > shown {
        let hidden = total - shown;
        out.push(format!(
            "       \u{2026} {hidden} more line{}",
            plural(hidden)
        ));
    }

    // Context below.
    if let (Some(at), Some(text)) = (start, body) {
        let after: Vec<&str> = text.lines().collect();
        let from = at.saturating_sub(1) + change.added.len();
        let to = (from + CONTEXT).min(after.len());
        for (offset, line) in after[from.min(after.len())..to].iter().enumerate() {
            push(&mut out, ' ', Some(from + offset + 1), line);
        }
    }
    out
}

/// What a turn changed, across every file it touched.
///
/// One line at the end of a turn answering the question the per-change blocks
/// cannot: how much moved in total, and in how many files. Files are counted
/// ONCE however often they are edited — five splices into one file is one file
/// changed, and reporting it as five would overstate the blast radius of a
/// turn every time an agent works incrementally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    files: Vec<String>,
    added: usize,
    removed: usize,
}

impl Tally {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, change: &Change) {
        if !self.files.iter().any(|seen| seen == &change.path) {
            self.files.push(change.path.clone());
        }
        let (added, removed) = change.counts();
        self.added += added;
        self.removed += removed;
    }

    #[must_use]
    pub fn files_changed(&self) -> usize {
        self.files.len()
    }

    /// The summary line, or `None` when nothing was changed.
    ///
    /// `None` rather than "0 files changed": a turn that edited nothing should
    /// say nothing, not report a zero.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let files = self.files.len();
        let mut line = format!("{files} file{} changed", plural(files));
        if self.added > 0 {
            line.push_str(&format!("  +{}", self.added));
        }
        if self.removed > 0 {
            line.push_str(&format!("  -{}", self.removed));
        }
        Some(line)
    }
}

/// Follows a turn's tool events and decides what an edit should print.
///
/// # Why this is a type and not four lines in a closure
///
/// It WAS four lines in one event closure, which put the rule this whole
/// module turns on — *show a change only when it actually landed* — somewhere
/// no test could reach: the closure lives inside a method that builds its own
/// backend, so there is nothing to script and nothing to assert. The rule was
/// live-tested and unit-untested, which is the wrong way round for the one
/// rule that decides whether the screen tells the truth.
///
/// Held here instead, both paths use the SAME decision — the interactive turn
/// and the ledger run — and a test can drive it directly.
pub struct Watcher {
    /// The tree paths are named relative to.
    root: std::path::PathBuf,
    /// Changes seen at CALL time, waiting for their result to say whether they
    /// happened. Keyed by the call id, which is all a result carries.
    pending: std::collections::HashMap<String, Change>,
    tally: Tally,
}

impl Watcher {
    #[must_use]
    pub fn watching(root: std::path::PathBuf) -> Self {
        Self {
            root,
            pending: std::collections::HashMap::new(),
            tally: Tally::new(),
        }
    }

    /// A tool was called. Remembered only if it describes a file change.
    pub fn called(&mut self, id: &str, input: &str) {
        if let Some(change) = change_from(input) {
            self.pending
                .insert(id.to_string(), change.under(&self.root));
        }
    }

    /// A result came back. The lines to print INSTEAD of the ordinary panel,
    /// or `None` when the ordinary panel is what the reader needs.
    ///
    /// `None` on a FAILED result even when a change was pending: the write did
    /// not happen, and the error text is the part worth reading. This is the
    /// rule the type exists to make testable.
    pub fn returned(&mut self, id: &str, is_error: bool, width: usize) -> Option<Vec<String>> {
        let change = self.pending.remove(id)?;
        if is_error {
            return None;
        }
        // The file as it stands AFTER the write, for its line numbers. Tried
        // under the tree first, then as given: an agent may name a file outside
        // the workspace, and that path is already absolute.
        let on_disk = std::fs::read_to_string(self.root.join(&change.path))
            .or_else(|_| std::fs::read_to_string(&change.path))
            .ok();
        let lines = block(&change, on_disk.as_deref(), width);
        self.tally.record(&change);
        Some(lines)
    }

    /// What the whole turn changed, or `None` if it changed nothing.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        self.tally.summary()
    }

    #[must_use]
    pub fn files_changed(&self) -> usize {
        self.tally.files_changed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A splice is recognised, and both sides of it are read.**
    #[test]
    fn a_splice_gives_the_lines_going_out_and_the_lines_coming_in() {
        let input =
            r#"{"file_path":"src/a.rs","old_string":"one\ntwo","new_string":"one\ntwo\nthree"}"#;
        let change = change_from(input).expect("a call naming a path and both sides is a change");

        assert_eq!(change.path, "src/a.rs");
        assert_eq!(change.removed, vec!["one", "two"]);
        assert_eq!(change.added, vec!["one", "two", "three"]);
        assert!(!change.whole_file, "a splice is not a whole-file write");
        assert_eq!(change.counts(), (3, 2));
    }

    /// A whole body is a write, and reads as one.
    #[test]
    fn a_whole_body_is_a_write() {
        let change = change_from(r#"{"file_path":"src/new.rs","content":"a\nb\n"}"#)
            .expect("a path and a body is a change");

        assert!(change.whole_file);
        assert_eq!(
            change.added,
            vec!["a", "b"],
            "the trailing newline is not a line"
        );
        assert!(change.removed.is_empty());
    }

    /// **Recognised by SHAPE, so a tool nobody has named yet still works.**
    ///
    /// This harness drives more than one agent and is meant to drive ones that
    /// do not exist yet. Matching on a tool name would give the view to one
    /// vendor's word for editing and silently withhold it from every other.
    #[test]
    fn an_unknown_tool_name_is_irrelevant_only_the_keys_matter() {
        // No tool name is passed in at all — there is nowhere to put one.
        let odd = r#"{"path":"lib/x.py","old_string":"x = 1","new_string":"x = 2"}"#;
        let change = change_from(odd).expect("`path` names a file just as `file_path` does");
        assert_eq!(change.path, "lib/x.py");

        // A call with no path is not an edit, whatever it is called.
        assert!(change_from(r#"{"command":"ls -la"}"#).is_none());
        // A path with neither a splice nor a body is a READ, not an edit.
        assert!(change_from(r#"{"file_path":"src/a.rs"}"#).is_none());
        // Malformed input is not an edit either, and must not panic.
        assert!(change_from("not json").is_none());
    }

    /// A deletion is a change, not a non-change.
    #[test]
    fn removing_text_is_recognised_even_though_nothing_is_added() {
        let change = change_from(r#"{"file_path":"a.rs","old_string":"gone","new_string":""}"#)
            .expect("an empty new side is a deletion, not an absent key");
        assert_eq!(change.counts(), (0, 1));
        assert_eq!(tally_line(0, 1), "Removed 1 line");
    }

    /// **The numbers shown are the file's own.**
    ///
    /// The whole point of the view is that a person can look at the same lines
    /// in their editor. Numbering from the change alone would produce numbers
    /// that match nothing.
    #[test]
    fn the_block_numbers_the_change_against_the_file_on_disk() {
        let file = "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\n";
        let change = Change {
            path: "src/lib.rs".to_string(),
            removed: Vec::new(),
            added: vec!["mod c;".to_string()],
            whole_file: false,
        };

        let lines = block(&change, Some(file), 80);
        let text = lines.join("\n");

        assert!(lines[0].contains("Update(src/lib.rs)"), "{:?}", lines[0]);
        assert!(lines[1].contains("Added 1 line"), "{:?}", lines[1]);
        assert!(
            text.contains("     3 +mod c;"),
            "the added line must carry ITS OWN number in the finished file:\n{text}"
        );
        assert!(
            text.contains("     2  mod b;") && text.contains("     4  mod d;"),
            "context either side must be numbered too:\n{text}"
        );
    }

    /// Text that cannot be found is shown WITHOUT numbers rather than with
    /// invented ones.
    #[test]
    fn a_change_that_cannot_be_located_is_shown_unnumbered() {
        let change = Change {
            path: "src/lib.rs".to_string(),
            removed: Vec::new(),
            added: vec!["something not in the file".to_string()],
            whole_file: false,
        };

        let text = block(&change, Some("mod a;\n"), 80).join("\n");
        assert!(text.contains("+something not in the file"));
        assert!(
            !text.contains(" 1 +"),
            "a line that could not be located must not be given a number: {text}"
        );

        // And with no file at all, the block still renders.
        let text = block(&change, None, 80).join("\n");
        assert!(text.contains("+something not in the file"));
    }

    /// A huge change is CUT and the remainder counted, never silently dropped.
    #[test]
    fn a_long_change_says_how_much_it_is_not_showing() {
        let added: Vec<String> = (0..40).map(|n| format!("line {n}")).collect();
        let change = Change {
            path: "big.rs".to_string(),
            removed: Vec::new(),
            added,
            whole_file: false,
        };

        let lines = block(&change, None, 80);
        let shown = lines.iter().filter(|l| l.contains('+')).count();
        assert_eq!(shown, MAX_LINES, "the block is bounded");
        assert!(
            lines.iter().any(|l| l.contains("26 more lines")),
            "the remainder must be counted: {lines:?}"
        );
    }

    /// **One file edited five times is ONE file changed.**
    ///
    /// Counting each splice would report a turn that worked incrementally in
    /// one file as having touched five, which overstates what moved every time
    /// an agent does the careful thing.
    #[test]
    fn the_tally_counts_files_once_however_often_they_are_edited() {
        let mut tally = Tally::new();
        let splice = |path: &str, added: usize| Change {
            path: path.to_string(),
            removed: vec!["gone".to_string()],
            added: (0..added).map(|n| n.to_string()).collect(),
            whole_file: false,
        };

        tally.record(&splice("a.rs", 3));
        tally.record(&splice("a.rs", 2));
        tally.record(&splice("b.rs", 1));

        assert_eq!(
            tally.files_changed(),
            2,
            "a.rs was edited twice, not two files"
        );
        assert_eq!(tally.summary().as_deref(), Some("2 files changed  +6  -3"));
    }

    /// A turn that changed nothing says nothing.
    #[test]
    fn an_untouched_turn_reports_no_summary_rather_than_a_zero() {
        assert_eq!(Tally::new().summary(), None);
    }

    /// **A REFUSED write shows no diff and counts for nothing.**
    ///
    /// The rule the whole module turns on, and until it was pulled out of an
    /// event closure there was nowhere to test it from. A write can be refused
    /// by the guards, by the agent's permission mode or by the file system;
    /// printing a diff for one would put a change on screen that is not in the
    /// file, and counting it would overstate the turn.
    ///
    /// Put the bug back by dropping the `is_error` guard in
    /// [`Watcher::returned`] and this fails on the first assertion.
    #[test]
    fn a_refused_write_is_not_shown_as_a_change() {
        let dir = std::env::temp_dir().join(format!("incode-edits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let call = r#"{"file_path":"a.rs","old_string":"one","new_string":"two"}"#;

        let mut watcher = Watcher::watching(dir.clone());
        watcher.called("c1", call);
        assert_eq!(
            watcher.returned("c1", true, 80),
            None,
            "a write that was refused must not be drawn as though it landed"
        );
        assert_eq!(watcher.files_changed(), 0, "nor counted");
        assert_eq!(watcher.summary(), None);

        // CONTROL: the same call, the same watcher, succeeding — so the None
        // above is the failure being honoured, not the parser quietly missing.
        std::fs::write(dir.join("a.rs"), "two\n").expect("write the scratch file");
        watcher.called("c2", call);
        let shown = watcher
            .returned("c2", false, 80)
            .expect("a write that landed IS drawn");
        assert!(shown[0].contains("Update(a.rs)"), "{:?}", shown[0]);
        assert_eq!(watcher.files_changed(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A result for a call that was never an edit is left to the ordinary
    /// panel, rather than swallowed.
    #[test]
    fn a_result_with_no_edit_behind_it_is_left_alone() {
        let mut watcher = Watcher::watching(std::path::PathBuf::from("."));
        watcher.called("c1", r#"{"command":"cargo test"}"#);
        assert_eq!(
            watcher.returned("c1", false, 80),
            None,
            "a shell result must still reach the reader as a result"
        );
        // And a result whose call was never seen at all must not panic.
        assert_eq!(watcher.returned("never-seen", false, 80), None);
    }

    /// **A file in the tree is named the way the person working in it would.**
    ///
    /// MEASURED on the first live run of this view: the header came back as
    /// `Update(C:\Users\…\scratchpad\editview\lib.rs)` — 110 characters, of
    /// which the identifying part was the last seven.
    #[test]
    fn a_path_inside_the_tree_is_shown_relative_to_it() {
        let root = std::path::Path::new("C:/work/proj");
        let change = Change {
            path: "C:/work/proj/src/cli/edits.rs".to_string(),
            removed: Vec::new(),
            added: vec!["x".to_string()],
            whole_file: false,
        }
        .under(root);

        assert_eq!(change.path, "src/cli/edits.rs");
        assert!(block(&change, None, 80)[0].contains("Update(src/cli/edits.rs)"));
    }

    /// **A path OUTSIDE the tree keeps every directory.**
    ///
    /// For those the leading path is the whole point — it is what tells the
    /// reader the agent just wrote into a different project.
    #[test]
    fn a_path_outside_the_tree_is_left_whole() {
        let change = Change {
            path: "D:/other-project/src/main.rs".to_string(),
            removed: Vec::new(),
            added: vec!["x".to_string()],
            whole_file: false,
        }
        .under(std::path::Path::new("C:/work/proj"));

        assert_eq!(
            change.path, "D:/other-project/src/main.rs",
            "shortening this would make a write into another tree read as a local edit"
        );
    }
}
