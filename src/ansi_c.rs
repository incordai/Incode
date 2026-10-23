//! Bash's ANSI-C quoting, `$'…'`, read the way bash reads it.
//!
//! Inside `$'…'` a backslash escapes the next character, a quote included, so
//! `$'\''` is ONE quote character and whatever follows the closing quote is
//! shell syntax again. Read as a plain `'…'`, where a backslash is just a
//! character, the same text looks like a quote that runs on: a `;` and the
//! command after it are taken for one quoted argument.
//!
//! But a POSIX `sh` such as dash — what `sh -c` runs on Debian and Ubuntu —
//! DOES read `$'` as a `$` and a plain quote. So each reader of a command line
//! that decides what runs reads a line with `$'` both ways, and a command
//! either reading finds counts.
//!
//! Used by the two such readers:
//! - `permissions::rules` — `split::Splitter::dollar`,
//!   `split::ParenScan::unquoted`, `peel::words` and `peel::unquote`: which
//!   commands a line holds, for the allow / ask / deny rules.
//! - `guards::shell` — `lex::Lexer::dollar` and `lex::Lexer::skipped`: which
//!   files a command writes, for the protected-path guard.
//!
//! `$"…"` (a double-quoted string translated by the locale) needs nothing
//! here: past its `$` it is read exactly as `"…"`.

/// The text of the `$'…'` whose body starts at `start` (just past `$'`),
/// decoded as bash decodes it (`man bash`, QUOTING), and the index just past
/// its closing quote. Unterminated: the rest of `chars`, decoded.
///
/// The end is found FIRST, as bash's lexer finds it — a backslash pairs with
/// the one character after it, whatever escape that makes — and only the body
/// is decoded, so no escape (`\c'`, say) can move the closing quote.
#[must_use]
pub fn quoted(chars: &[char], start: usize) -> (String, usize) {
    let start = start.min(chars.len());
    let mut close = start;
    while close < chars.len() && chars[close] != '\'' {
        close += if chars[close] == '\\' { 2 } else { 1 };
    }
    let close = close.min(chars.len());
    (decode(&chars[start..close]), (close + 1).min(chars.len()))
}

/// A `$'…'` body, decoded. A NUL ends the text, as in bash: `$'rm\0x'` is
/// `rm`.
fn decode(body: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        i += 1;
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(&escaped) = body.get(i) else {
            out.push('\\');
            break;
        };
        i += 1;
        let (decoded, used) = escape(escaped, body, i);
        i += used;
        match decoded {
            Escape::Kept => {
                out.push('\\');
                out.push(escaped);
            }
            Escape::Code(0) => break,
            Escape::Code(code) => {
                out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        }
    }
    out
}

/// What one escape decodes to.
enum Escape {
    /// A character code; 0 ends the text.
    Code(u32),
    /// Not an escape bash decodes: the backslash and the character stay.
    Kept,
}

/// The escape `\<escaped>` whose operand, if it takes one, starts at
/// `at`: what it decodes to and how many characters of operand it used.
///
/// Called by [`decode`] for every backslash with a character after it.
fn escape(escaped: char, body: &[char], at: usize) -> (Escape, usize) {
    match escaped {
        // `\nnn`: one to three octal digits, the first already read.
        '0'..='7' => {
            let (rest, used) = digits(body, at, 8, 2);
            let first = escaped.to_digit(8).unwrap_or(0);
            (
                Escape::Code((first * 8u32.pow(used as u32) + rest) & 0xff),
                used,
            )
        }
        'x' | 'u' | 'U' => hex(escaped, body, at),
        // `\cx`: the control character of the next character, whatever
        // it is (`\c\` is 0x1c).
        'c' => match body.get(at) {
            Some(&x) => (Escape::Code(u32::from(x) & 0x1f), 1),
            None => (Escape::Kept, 0),
        },
        _ => (named(escaped).map_or(Escape::Kept, Escape::Code), 0),
    }
}

/// `\xHH`, `\uHHHH` or `\UHHHHHHHH`: up to that many hex digits. With none,
/// bash keeps the escape as written.
///
/// Called by [`escape`].
fn hex(escaped: char, body: &[char], at: usize) -> (Escape, usize) {
    let most = match escaped {
        'x' => 2,
        'u' => 4,
        _ => 8,
    };
    match digits(body, at, 16, most) {
        (_, 0) => (Escape::Kept, 0),
        (value, used) => (Escape::Code(value), used),
    }
}

/// The code of a one-letter escape (`\n`, `\t`, `\\`, …), if it is one.
///
/// Called by [`escape`].
fn named(escaped: char) -> Option<u32> {
    let code = match escaped {
        'a' => 0x07,
        'b' => 0x08,
        'e' | 'E' => 0x1b,
        'f' => 0x0c,
        'n' => u32::from('\n'),
        'r' => u32::from('\r'),
        't' => u32::from('\t'),
        'v' => 0x0b,
        '\\' | '\'' | '"' | '?' => u32::from(escaped),
        _ => return None,
    };
    Some(code)
}

/// Up to `most` digits in `radix` from `start`: their value and how many.
fn digits(chars: &[char], start: usize, radix: u32, most: usize) -> (u32, usize) {
    let mut value = 0u32;
    let mut used = 0;
    while used < most {
        let Some(digit) = chars.get(start + used).and_then(|c| c.to_digit(radix)) else {
            break;
        };
        value = value.saturating_mul(radix).saturating_add(digit);
        used += 1;
    }
    (value, used)
}

#[cfg(test)]
mod tests {
    use super::quoted;

    fn decode(body: &str) -> (String, usize) {
        let chars: Vec<char> = body.chars().collect();
        quoted(&chars, 0)
    }

    #[test]
    fn an_escaped_quote_does_not_close_the_string() {
        assert_eq!(decode(r"\'' ; rm"), ("'".to_string(), 3));
        assert_eq!(decode(r"it\'s' tail"), ("it's".to_string(), 6));
    }

    /// **NO ESCAPE MOVES THE CLOSING QUOTE.** bash finds the end first — a
    /// backslash pairs with one character — and decodes after. `\c` takes
    /// the next character as its operand, but that is decoding: `$'\c'`
    /// still ends at its quote (checked with bash: `\c`, then the command
    /// after it runs), and `$'\c\''` is 0x1c and `'`.
    #[test]
    fn the_end_is_found_before_any_escape_is_decoded() {
        assert_eq!(decode(r"\c' ; rm"), (r"\c".to_string(), 3));
        assert_eq!(decode(r"\c\'' ; rm"), ("\u{1c}'".to_string(), 5));
        assert_eq!(decode(r"\x' ; rm").1, 3);
        assert_eq!(decode(r"\1' ; rm").1, 3);
    }

    /// A NUL ends the text, whichever escape spells it, as in bash
    /// (`> $'a.txt\0junk'` writes `a.txt`); the end is still the quote.
    #[test]
    fn a_nul_ends_the_text_but_not_the_string() {
        for body in [
            r"rm\0x' tail",
            r"rm\x00x' tail",
            r"rm\000x' tail",
            r"rm\c@x' tail",
            concat!("rm", "\\", "u0000x' tail"),
        ] {
            assert_eq!(
                decode(body),
                ("rm".to_string(), body.find('\'').unwrap() + 1),
                "{body}"
            );
        }
    }

    #[test]
    fn escapes_decode_as_bash_does() {
        for (body, meant) in [
            (r"a\tb'", "a\tb"),
            (r"\x72m'", "rm"),
            (r"\x7'", "\u{7}"),
            (r"\162m'", "rm"),
            (r"\0'", ""),
            (concat!("\\", "u0072m'"), "rm"),
            (r"\U00000072m'", "rm"),
            (r"\cA'", "\u{1}"),
            (r"\\'", "\\"),
            (r#"\"\?'"#, "\"?"),
            (r"\e\E'", "\u{1b}\u{1b}"),
            (r"\z'", "\\z"),
            (r"\xg'", "\\xg"),
        ] {
            assert_eq!(decode(body).0, meant, "`$'{body}`");
        }
    }

    #[test]
    fn an_unterminated_string_runs_to_the_end() {
        assert_eq!(decode(r"abc\"), ("abc\\".to_string(), 4));
        assert_eq!(decode("abc"), ("abc".to_string(), 3));
    }
}

#[cfg(test)]
#[path = "ansi_c_pinned_tests.rs"]
mod pinned_tests;
