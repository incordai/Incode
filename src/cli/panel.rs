//! Tool results, rendered as something a person can read.
//!
//! # Why a result needs a shape
//!
//! A tool result used to print as `[Bash: OK] <the first two hundred bytes>…`.
//! Two hundred bytes of a build log is the part that says nothing, the tool's
//! name is buried in the middle of it, and success and failure differ only by a
//! word. On a screen holding a dozen of them, none of it is findable.
//!
//! So a result gets a rule with its name on it, its body indented beneath, and
//! an honest count of what was cut. The eye finds the boundary between one
//! result and the next without reading a character.
//!
//! # What is cut, and saying so
//!
//! Long output is truncated to [`BODY_LINES`] and the remainder is COUNTED, not
//! quietly dropped. `… 90 more lines` tells the reader there is more and how
//! much; a silent cut tells them the tool returned six lines when it returned
//! ninety-six.

/// Lines of a result body shown before the rest is counted.
const BODY_LINES: usize = 8;
/// The longest a single body line is allowed to be before it is cut.
const LINE_WIDTH: usize = 110;

/// Shown on the rule of a result that succeeded.
const OK_MARK: char = '\u{2713}';
/// Shown on the rule of a result that failed.
const FAIL_MARK: char = '\u{2717}';

/// One tool result, as the lines to print.
///
/// Returned rather than printed so the shape can be tested without a terminal:
/// every decision here — what the rule says, how much body survives, what the
/// remainder line claims — is a string, and strings can be asserted on.
#[must_use]
pub fn result(tool: &str, on: Option<&str>, body: &str, failed: bool, width: usize) -> Vec<String> {
    let mark = if failed { FAIL_MARK } else { OK_MARK };
    let head = match on {
        Some(what) if !what.is_empty() => format!("{mark} {tool}  {what}"),
        _ => format!("{mark} {tool}"),
    };
    let mut out = vec![rule(&head, width)];

    let lines: Vec<&str> = body.lines().collect();
    // Trailing blank lines are padding from the tool, not content; counting
    // them as "more lines" would promise the reader something that is not there.
    let meaningful = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |at| at + 1);
    let lines = &lines[..meaningful];

    for line in lines.iter().take(BODY_LINES) {
        out.push(format!("  {}", cut(line, LINE_WIDTH)));
    }
    if lines.len() > BODY_LINES {
        let hidden = lines.len() - BODY_LINES;
        out.push(format!("  \u{2026} {hidden} more line{}", plural(hidden)));
    }
    out
}

/// What a tool call is acting ON, for the rule beside its name.
///
/// A result headed only `Edit` says a file changed and not which one. The call
/// already carries the answer — `file_path` for the file tools, `command` for
/// the shell — and it was simply never read.
///
/// `None` when the tool takes nothing worth naming; the rule then shows the
/// tool alone rather than an empty gap or a guess.
#[must_use]
pub fn target(input: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(input).ok()?;
    // Ordered by how much the reader learns: a path beats a pattern, and a
    // pattern beats the raw command it happened to arrive in.
    for key in [
        "file_path",
        "path",
        "notebook_path",
        "pattern",
        "query",
        "command",
    ] {
        if let Some(found) = parsed.get(key).and_then(serde_json::Value::as_str) {
            let trimmed = found.trim();
            if !trimmed.is_empty() {
                return Some(shorten(trimmed, TARGET_WIDTH));
            }
        }
    }
    None
}

/// The widest a target is shown before it is shortened.
const TARGET_WIDTH: usize = 52;

/// Shorten a target, keeping the END — the part that identifies it.
///
/// A long path's tail names the file; its head is directories the reader
/// already knows. Cutting the tail leaves every result in a deep tree looking
/// identical.
#[must_use]
pub fn shorten(text: &str, width: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= width || width == 0 {
        return flat;
    }
    let kept: String = chars[chars.len() - (width - 1)..].iter().collect();
    format!("…{kept}")
}

/// A rule with a label on it, exactly `width` wide.
#[must_use]
pub fn rule(label: &str, width: usize) -> String {
    let lead = "\u{2500}\u{2500} ";
    let head = format!("{lead}{label} ");
    let dashes = width.saturating_sub(head.chars().count());
    format!("{head}{}", "\u{2500}".repeat(dashes))
}

/// Cut to `width` characters, never inside one.
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// `s` unless there is exactly one.
const fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_is_headed_by_its_tool_and_outcome() {
        let out = result("Bash", None, "hello", false, 60);
        assert!(out[0].contains("Bash"), "the tool is named: {}", out[0]);
        assert!(out[0].contains(OK_MARK), "success is marked: {}", out[0]);
        assert_eq!(out[1], "  hello");
    }

    #[test]
    fn a_failure_is_marked_differently_from_a_success() {
        // They used to differ by the words OK and ERROR buried mid-line.
        let ok = result("Bash", None, "x", false, 40);
        let bad = result("Bash", None, "x", true, 40);
        assert_ne!(ok[0], bad[0]);
        assert!(bad[0].contains(FAIL_MARK), "{}", bad[0]);
        assert!(!bad[0].contains(OK_MARK), "{}", bad[0]);
    }

    #[test]
    fn a_result_names_the_file_it_touched() {
        // `Edit` alone says a file changed and not which one.
        let on = target(r#"{"file_path":"src/cli/footer.rs"}"#);
        let out = result("Edit", on.as_deref(), "ok", false, 80);
        assert!(out[0].contains("Edit"), "{}", out[0]);
        assert!(out[0].contains("footer.rs"), "{}", out[0]);
    }

    #[test]
    fn a_path_beats_the_command_it_arrived_with() {
        // Both keys present: the path is what the reader learns from.
        let found = target(r#"{"command":"cat x","file_path":"src/main.rs"}"#);
        assert_eq!(found.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn a_shell_call_is_named_by_its_command() {
        assert_eq!(
            target(r#"{"command":"cargo test --release"}"#).as_deref(),
            Some("cargo test --release")
        );
    }

    #[test]
    fn a_tool_with_nothing_worth_naming_gets_no_target() {
        // Better an unadorned rule than an empty gap or a guess.
        assert_eq!(target(r#"{"limit":20}"#), None);
        assert_eq!(target("not json"), None);
        assert_eq!(target(r#"{"file_path":"   "}"#), None);
    }

    #[test]
    fn a_long_path_keeps_the_end_that_identifies_it() {
        let deep = "a/very/deeply/nested/directory/structure/that/goes/on/src/cli/footer.rs";
        let short = shorten(deep, 30);
        assert!(short.ends_with("footer.rs"), "{short}");
        assert!(short.starts_with('…'), "{short}");
        assert_eq!(short.chars().count(), 30);
    }

    #[test]
    fn a_multi_line_command_is_flattened_onto_the_rule() {
        // A newline in the target would break the rule across two rows.
        let found = target(r#"{"command":"echo one\n  echo two"}"#).expect("a target");
        assert!(!found.contains('\n'), "{found}");
    }

    #[test]
    fn the_rule_keeps_its_width_with_a_target_on_it() {
        let on = target(r#"{"file_path":"src/cli/footer.rs"}"#);
        for width in [40usize, 80, 100] {
            let out = result("Edit", on.as_deref(), "", false, width);
            assert_eq!(out[0].chars().count(), width, "width {width}");
        }
    }

    #[test]
    fn the_rule_is_exactly_as_wide_as_asked() {
        for width in [30usize, 60, 100] {
            assert_eq!(rule("✓ Bash", width).chars().count(), width);
        }
    }

    #[test]
    fn a_label_longer_than_the_rule_does_not_widen_it() {
        // A tool with a very long name must not push the rule past the screen
        // and wrap, which would put a stray line under every result.
        let long = "✓ ".to_string() + &"tool-with-a-very-long-name".repeat(4);
        assert!(rule(&long, 30).chars().count() >= 30);
        assert!(!rule(&long, 30).contains('\n'));
    }

    #[test]
    fn long_output_is_cut_and_the_remainder_is_counted() {
        // Counted, not quietly dropped: a silent cut says the tool returned
        // eight lines when it returned fifty.
        let body: String = (0..50).map(|n| format!("line {n}\n")).collect();
        let out = result("Bash", None, &body, false, 60);
        assert_eq!(out.len(), 1 + BODY_LINES + 1, "rule + body + remainder");
        assert!(
            out.last().expect("last").contains("42 more lines"),
            "{:?}",
            out.last()
        );
    }

    #[test]
    fn output_that_fits_has_no_remainder_line() {
        let out = result("Read", None, "one\ntwo", false, 60);
        assert_eq!(out.len(), 3);
        assert!(!out.iter().any(|l| l.contains("more line")), "{out:?}");
    }

    #[test]
    fn exactly_the_limit_is_not_reported_as_truncated() {
        // Off by one here claims a line was hidden when none was.
        let body: String = (0..BODY_LINES).map(|n| format!("l{n}\n")).collect();
        let out = result("Bash", None, &body, false, 60);
        assert!(!out.iter().any(|l| l.contains("more line")), "{out:?}");
    }

    #[test]
    fn one_hidden_line_is_singular() {
        let body: String = (0..BODY_LINES + 1).map(|n| format!("l{n}\n")).collect();
        let out = result("Bash", None, &body, false, 60);
        let last = out.last().expect("last");
        assert!(last.contains("1 more line"), "{last}");
        assert!(!last.contains("1 more lines"), "{last}");
    }

    #[test]
    fn trailing_blank_lines_are_not_counted_as_content() {
        // Tools pad their output. Counting the padding promises the reader
        // lines that hold nothing.
        let out = result("Bash", None, "one\ntwo\n\n\n\n\n\n\n\n\n\n\n", false, 60);
        assert!(!out.iter().any(|l| l.contains("more line")), "{out:?}");
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn a_very_long_line_is_cut_without_splitting_a_character() {
        // An em dash is three bytes; cutting on bytes panics.
        let body = "x—".repeat(200);
        let out = result("Bash", None, &body, false, 60);
        assert!(out[1].chars().count() <= LINE_WIDTH + 2);
        assert!(out[1].ends_with('\u{2026}'), "{}", out[1]);
    }

    #[test]
    fn empty_output_still_shows_that_the_tool_ran() {
        let out = result("Bash", None, "", false, 40);
        assert_eq!(out.len(), 1, "the rule alone: {out:?}");
        assert!(out[0].contains("Bash"));
    }
}
