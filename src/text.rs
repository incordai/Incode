//! Character-safe string shortening.
//!
//! Slicing a `&str` at an arbitrary byte index panics when that index falls
//! inside a multi-byte character, and every value shortened for display here
//! comes from outside the process: tool output, shell stdout, file contents,
//! model text, branch names typed by a user. A single accented letter or emoji
//! landing on the cut point took the whole CLI down with
//! `byte index N is not a char boundary`.
//!
//! These helpers cut on a character boundary instead. Use [`truncate`] when the
//! limit is about how much a human should read, and [`head_bytes`] when it is
//! about how many bytes may be sent onward.
//!
//! [`read_authored`] is the reading side of the same concern: text a person
//! wrote by hand arrives with whatever their editor put in front of it.

/// The byte-order mark some Windows tools write at the start of a UTF-8 file.
const BOM: char = '\u{feff}';

/// A file a person writes by hand (a setting, a manifest, a definition), as
/// text with any leading byte-order mark dropped.
///
/// Windows PowerShell 5.1's `Set-Content -Encoding utf8` and some editors
/// start a UTF-8 file with U+FEFF. `str::trim` keeps it, and serde_json and
/// the toml crate refuse it, so without this a hand-written file is rejected
/// or, worse, half-read: a command's front matter stopped being recognised
/// and its `allowed-tools` were never applied.
///
/// Errors are `std::fs::read_to_string`'s, unchanged, so a caller's
/// `NotFound` handling still works.
///
/// Wired to: every reader of a file a person authors — admin.rs (admin.json),
/// mcp_config.rs (mcp.json), plugins.rs (plugin.toml, hooks.json, mcp.json,
/// SKILL.md), custom_commands.rs, styles.rs, cli/keys.rs, guards.rs,
/// contracts.rs, permissions/rules/set.rs (settings.json),
/// permissions/managed.rs, runtime/lifecycle.rs, harness.rs, spec.rs, goal.rs,
/// brief.rs, folders.rs (.gitignore), research.rs (project manifests).
/// Sibling: subagents.rs `parse` strips the mark from text it is handed.
/// Not for files incode writes itself, nor for content passed through as is.
///
/// # Errors
///
/// The file could not be read, or is not UTF-8.
pub fn read_authored(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let mut text = std::fs::read_to_string(path)?;
    if text.starts_with(BOM) {
        text.drain(..BOM.len_utf8());
    }
    Ok(text)
}

/// Longest prefix of `s` holding at most `max_chars` characters.
#[must_use]
pub fn head(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_index, _)) => &s[..byte_index],
        None => s,
    }
}

/// Longest prefix of `s` that fits within `max_bytes` without splitting a
/// character.
///
/// For limits that exist to bound transport or buffer size, where the unit
/// genuinely is bytes rather than characters.
#[must_use]
pub fn head_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` shortened to `max_chars` characters, with a trailing `...` only when
/// something was actually removed.
#[must_use]
pub fn truncate(s: &str, max_chars: usize) -> String {
    let kept = head(s, max_chars);
    if kept.len() == s.len() {
        s.to_string()
    } else {
        format!("{kept}...")
    }
}

/// Shorten `s` in place to at most `max_bytes`, cutting on a character
/// boundary and marking that it was cut.
///
/// `String::truncate` panics on a non-boundary index just as slicing does.
pub fn truncate_in_place(s: &mut String, max_bytes: usize, suffix: &str) {
    if s.len() <= max_bytes {
        return;
    }
    let end = head_bytes(s, max_bytes).len();
    s.truncate(end);
    s.push_str(suffix);
}

/// Whether a message is a greeting rather than an instruction.
///
/// # Why this is worth a function
///
/// Every turn assembles context first: the guard rules for this message and
/// whatever the shared memory holds about it. Both are network calls. For "hi"
/// there is nothing to guard and nothing to recall, so that work is pure delay
/// in front of the one reply where a person is most likely to be watching the
/// clock.
///
/// Kept DELIBERATELY narrow. It matches only when the whole message is a
/// greeting, so "hi, can you fix the parser" is an instruction and gets its
/// full context. Skipping context for real work to save a second would be a
/// bad trade — the rules exist for a reason.
#[must_use]
pub fn is_greeting(line: &str) -> bool {
    const GREETINGS: [&str; 14] = [
        "hi",
        "hey",
        "hello",
        "yo",
        "sup",
        "morning",
        "good morning",
        "good afternoon",
        "good evening",
        "how are you",
        "how's it going",
        "hows it going",
        "you there",
        "are you there",
    ];

    let cleaned: String = line
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '\'')
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    GREETINGS.contains(&cleaned.as_str())
}

/// What the harness says back to a greeting, without starting an agent.
///
/// Deliberately short and deliberately NOT a model call. It names the workspace,
/// because the one thing a person genuinely cannot see from a bare prompt is
/// which directory this session is pointed at — and pointing it at the wrong
/// one is the mistake that wastes a whole session.
#[must_use]
pub fn greeting_reply(workspace: &std::path::Path) -> String {
    let where_ = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| workspace.display().to_string());
    format!("Hello. Ready in `{where_}` — tell me what you want to work on.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A greeting skips the context, so the reply is quick.** Both context
    /// calls are network requests, and for "hi" there is nothing to guard and
    /// nothing to recall — it is pure delay in front of the one reply where a
    /// person is most likely watching the clock.
    #[test]
    fn a_bare_greeting_is_recognised() {
        for line in [
            "hi",
            "Hi!",
            "  hello  ",
            "Hey",
            "how are you?",
            "Good morning",
        ] {
            assert!(is_greeting(line), "should be a greeting: {line}");
        }
    }

    /// **A greeting carrying an instruction is NOT one.** Skipping the guard
    /// rules for real work to save a second would be a bad trade.
    #[test]
    fn a_greeting_with_work_attached_is_an_instruction() {
        for line in [
            "hi, can you fix the parser",
            "hello world program",
            "hey what is wrong with the build",
            "morning standup notes",
        ] {
            assert!(!is_greeting(line), "should be work: {line}");
        }
    }

    /// An empty line is not a greeting either.
    #[test]
    fn nothing_is_not_a_greeting() {
        assert!(!is_greeting(""));
        assert!(!is_greeting("   "));
    }

    /// The exact shape that panicked: a limit landing mid-character.
    ///
    /// "é" is two bytes, so a 3-character prefix of "aéb" ends at byte 4 while
    /// a naive `&s[..3]` cuts "é" in half.
    #[test]
    fn cutting_inside_a_multi_byte_character_does_not_panic() {
        let s = "aéb";
        assert_eq!(head(s, 2), "aé");
        assert_eq!(truncate(s, 2), "aé...");
    }

    /// Every one of the shortened values is attacker- or user-influenced text,
    /// so the panic was reachable from ordinary input. Walk every possible cut
    /// point of a string made of 1-, 2-, 3- and 4-byte characters: none may
    /// panic, and none may cut a character apart.
    #[test]
    fn no_cut_point_of_mixed_width_text_panics() {
        let s = "aé中🙂bñ漢字x";
        for limit in 0..=s.len() + 4 {
            let by_chars = head(s, limit);
            let by_bytes = head_bytes(s, limit);
            // Round-tripping proves the cut landed on a boundary: a split
            // character would not survive re-parsing as UTF-8.
            assert_eq!(std::str::from_utf8(by_chars.as_bytes()).unwrap(), by_chars);
            assert_eq!(std::str::from_utf8(by_bytes.as_bytes()).unwrap(), by_bytes);
            assert!(s.starts_with(by_chars));
            assert!(s.starts_with(by_bytes));
            assert!(by_bytes.len() <= limit.min(s.len()));
        }
    }

    #[test]
    fn a_short_string_is_returned_unchanged_with_no_ellipsis() {
        assert_eq!(truncate("hello", 20), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hell...");
    }

    #[test]
    fn head_bytes_respects_the_byte_budget() {
        // "中" is 3 bytes: a 2-byte budget can keep none of it.
        assert_eq!(head_bytes("中文", 2), "");
        assert_eq!(head_bytes("中文", 3), "中");
        assert_eq!(head_bytes("中文", 5), "中");
        assert_eq!(head_bytes("中文", 6), "中文");
    }

    #[test]
    fn truncate_in_place_cuts_on_a_boundary_and_marks_it() {
        let mut s = "中文中文".to_string();
        truncate_in_place(&mut s, 7, "\n... (output truncated)");
        assert_eq!(s, "中文\n... (output truncated)");

        let mut short = "ok".to_string();
        truncate_in_place(&mut short, 100, "CUT");
        assert_eq!(short, "ok", "an untruncated string must not be marked");
    }

    #[test]
    fn a_greeting_is_answered_without_an_agent() {
        // The reply must be produced from the workspace alone. If this ever
        // needs a model, a network call or a spawned process, saying hello goes
        // back to costing eighteen seconds.
        let reply = greeting_reply(std::path::Path::new("/tmp/some-project"));
        assert!(
            reply.contains("some-project"),
            "names the workspace: {reply}"
        );
        assert!(!reply.is_empty());
    }

    /// **A hand-written file reads the same with or without a byte-order
    /// mark**, and only a LEADING mark goes: one inside the text is the
    /// person's own and stays.
    #[test]
    fn a_leading_byte_order_mark_is_dropped_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let with = dir.path().join("with.json");
        let without = dir.path().join("without.json");
        std::fs::write(&with, b"\xEF\xBB\xBF{\"a\": \"x\xEF\xBB\xBFy\"}").unwrap();
        std::fs::write(&without, b"{\"a\": \"x\xEF\xBB\xBFy\"}").unwrap();
        let read = read_authored(&with).unwrap();
        assert_eq!(read, read_authored(&without).unwrap());
        assert_eq!(read, "{\"a\": \"x\u{feff}y\"}");
        assert!(serde_json::from_str::<serde_json::Value>(&read).is_ok());
    }

    /// A missing file is still `NotFound`, which callers treat as "none".
    #[test]
    fn a_missing_authored_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_authored(dir.path().join("absent.json")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn the_greeting_reply_names_a_workspace_with_no_file_name() {
        let reply = greeting_reply(std::path::Path::new("/"));
        assert!(!reply.trim().is_empty(), "still says something: {reply}");
    }
}
