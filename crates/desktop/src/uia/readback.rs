//! What reading a field back after `set_value` can honestly say.
//!
//! `set_value` sends text through the accessibility interface and reads the
//! field back. Whether what comes back can be believed depends on the control,
//! and the three answers are kept apart because they mean different things to
//! the agent:
//!
//! - **Same**: the field reads back what was sent. Line endings are not
//!   differences: Windows edit controls return `\r\n`, RichEdit (Notepad) `\r`,
//!   and the text sent had `\n`. It is `verified`.
//! - **Unverified**: the read-back is not evidence either way. A browser's
//!   input reads back as EMPTY after the text was put in it (measured on
//!   Chrome: the page showed the text and `ValuePattern` said 0 characters).
//!   An empty read-back after a non-empty set is a control that does not
//!   report its value, not one that refused it. The reply says the text may be
//!   there and was not confirmed, and never claims it was set.
//! - **Differs**: the read-back is a real value and it is not what was sent:
//!   either it is exactly what the field held BEFORE (the control ignored the
//!   set), or it is other text. That is an error, not a success with a note.
//!
//! Errors are journalled word for word, so none quotes a value: only lengths
//! and the position of the first difference.
//!
//! Pure, so it is tested on every platform; the platforms call [`judge`].

/// `text` with every line ending as `\n`: `\r\n` and a lone `\r` (RichEdit's
/// paragraph mark) are both one `\n`.
#[must_use]
pub(crate) fn lines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// What reading the field back after the set means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadBack {
    /// Reads back as sent, line endings aside.
    Same,
    /// The read-back proves nothing; why, in words for the reply.
    Unverified(String),
    /// A real value that is not what was sent; why, without the values.
    Differs(String),
}

/// Judge `now`, what the field reads back after `sent` was set. `before` is
/// what it read before the set, when that could be read. `trim` is for a
/// platform whose read-back is trimmed (macOS), so the sent text is too.
#[must_use]
pub(crate) fn judge(sent: &str, before: Option<&str>, now: &str, trim: bool) -> ReadBack {
    let cut = |s: String| if trim { s.trim().to_string() } else { s };
    let (sent, now) = (cut(lines(sent)), cut(lines(now)));
    if sent == now {
        return ReadBack::Same;
    }
    if now.is_empty() {
        return ReadBack::Unverified(
            "it reads back empty, which a browser or custom control does when it does not \
             report its value, so the text may be there; look at the result"
                .to_string(),
        );
    }
    if let Some(before) = before.map(|b| cut(lines(b))).filter(|b| !b.is_empty()) {
        if before == now {
            return ReadBack::Differs(format!(
                "the control kept its old value: it reads back exactly as it did before the set \
                 ({} characters), not as sent ({} characters)",
                now.chars().count(),
                sent.chars().count()
            ));
        }
    }
    let at = sent
        .chars()
        .zip(now.chars())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| sent.chars().count().min(now.chars().count()));
    ReadBack::Differs(format!(
        "it reads back differently from what was sent (line endings aside): {} characters \
         sent, {} read back, the first difference at character {}",
        sent.chars().count(),
        now.chars().count(),
        at + 1
    ))
}

/// [`judge`] for a platform whose read-back is `None` both when the value is
/// empty and when it could not be read at all (macOS `text_attribute` trims,
/// and drops an empty value and a failed call alike). Nothing read is nothing
/// proven: never `Same`, even when an empty value was sent, since a failed
/// read cannot be told from an emptied field.
#[cfg(any(target_os = "macos", test))]
#[must_use]
pub(crate) fn judge_optional(
    sent: &str,
    before: Option<&str>,
    now: Option<&str>,
    trim: bool,
) -> ReadBack {
    match now {
        Some(now) => judge(sent, before, now, trim),
        None if lines(sent).trim().is_empty() => ReadBack::Unverified(
            "its value could not be read back (an empty value and an unreadable one look \
             the same), so it is not known to be empty"
                .to_string(),
        ),
        None => judge(sent, before, "", trim),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS gives `None` for an empty value AND for a failed read, so neither
    /// is "verified": an empty value sent and nothing read back is unverified.
    #[test]
    fn nothing_read_back_is_never_verified() {
        for sent in ["", "   ", "\n"] {
            match judge_optional(sent, None, None, true) {
                ReadBack::Unverified(why) => assert!(why.contains("could not be read"), "{why}"),
                other => panic!("{sent:?}: {other:?}"),
            }
        }
        // Text sent and nothing read: the browser case, unverified too.
        assert!(matches!(
            judge_optional("hello", None, None, true),
            ReadBack::Unverified(_)
        ));
        // A value that was read is judged as before.
        assert_eq!(
            judge_optional("hello", None, Some("hello"), true),
            ReadBack::Same
        );
        assert!(matches!(
            judge_optional("hello", Some("old"), Some("old"), true),
            ReadBack::Differs(_)
        ));
    }

    #[test]
    fn line_endings_are_not_differences() {
        assert_eq!(lines("a\r\nb\rc\nd"), "a\nb\nc\nd");
        // Notepad: RichEdit reads paragraphs back with a bare CR.
        assert_eq!(
            judge("one\ntwo\nthree", None, "one\rtwo\rthree", false),
            ReadBack::Same
        );
        // A Windows edit control: CRLF.
        assert_eq!(judge("one\ntwo", None, "one\r\ntwo", false), ReadBack::Same);
        // And the other way round.
        assert_eq!(judge("one\r\ntwo", None, "one\ntwo", false), ReadBack::Same);
    }

    #[test]
    fn the_same_text_is_verified_and_a_trimmed_platform_trims_what_it_sent() {
        assert_eq!(judge("hello", None, "hello", false), ReadBack::Same);
        assert_eq!(judge(" hello \n", None, "hello", true), ReadBack::Same);
        assert!(matches!(
            judge(" hello ", None, "hello", false),
            ReadBack::Differs(_)
        ));
    }

    /// Chrome: the text was put in, the read-back is empty. Not an error, and
    /// not a success either.
    #[test]
    fn an_empty_read_back_after_a_set_is_unverified_not_an_error() {
        match judge("hello", Some(""), "", false) {
            ReadBack::Unverified(why) => assert!(why.contains("reads back empty"), "{why}"),
            other => panic!("{other:?}"),
        }
        // Even when the field held text before: a browser reads nothing at all.
        assert!(matches!(
            judge("hello", Some("old text"), "", false),
            ReadBack::Unverified(_)
        ));
        // Setting an empty value reads back empty, and that is the same.
        assert_eq!(judge("", Some("old"), "", false), ReadBack::Same);
    }

    #[test]
    fn a_field_that_still_reads_as_it_did_before_ignored_the_set() {
        match judge("new text", Some("old text"), "old text", false) {
            ReadBack::Differs(why) => {
                assert!(why.contains("kept its old value"), "{why}");
                assert!(
                    !why.contains("old text") && !why.contains("new text"),
                    "{why}"
                );
            }
            other => panic!("{other:?}"),
        }
        // The old value compared the way the new one is: line endings aside.
        assert!(matches!(
            judge("new", Some("a\r\nb"), "a\nb", false),
            ReadBack::Differs(_)
        ));
    }

    #[test]
    fn other_text_is_an_error_that_gives_numbers_and_never_the_text() {
        match judge("hello world", Some(""), "hello there!", false) {
            ReadBack::Differs(why) => {
                assert!(why.contains("11 characters sent"), "{why}");
                assert!(why.contains("12 read back"), "{why}");
                assert!(why.contains("character 7"), "{why}");
                assert!(!why.contains("hello"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        // A prefix: the first difference is where the shorter one ends.
        match judge("abc", None, "abcdef", false) {
            ReadBack::Differs(why) => assert!(why.contains("character 4"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unchanged_empty_field_is_not_taken_for_an_ignored_set() {
        // A field that was empty and reads back something else is just other
        // text; "kept its old value" needs an old value to have kept.
        match judge("hello", Some(""), "x", false) {
            ReadBack::Differs(why) => assert!(!why.contains("kept its old value"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
