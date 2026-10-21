//! Bash's ANSI-C quoting, `$'…'`, read the way bash reads it.
//!
//! Inside `$'…'` a backslash escapes the next character, a quote included, so
//! `$'\''` is ONE quote character and whatever follows the closing quote is
//! shell syntax again. Read as a plain `'…'`, where a backslash is just a
//! character, the same text looks like a quote that runs on: a `;` and the
//! command after it are taken for one quoted argument. Every reader of a
//! command line that decides what runs must agree with bash here, or a command
//! hides behind the quote.
//!
//! Used by the two such readers:
//! - `permissions::rules` — `split_into`, `substitution`, `words` and
//!   `unquote`: which commands a line holds, for the allow / ask / deny rules.
//! - `guards::shell` — `lex` and `Lexer::substitution`: which files a command
//!   writes, for the protected-path guard.
//!
//! `$"…"` (a double-quoted string translated by the locale) needs nothing
//! here: past its `$` it is read exactly as `"…"`.

/// The text of the `$'…'` whose body starts at `start` (just past `$'`),
/// decoded as bash decodes it (`man bash`, QUOTING), and the index just past
/// its closing quote. Unterminated: the rest of `chars`, decoded.
#[must_use]
pub fn quoted(chars: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c == '\'' {
            return (out, i);
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(&escaped) = chars.get(i) else {
            out.push('\\');
            break;
        };
        i += 1;
        match escaped {
            'a' => out.push('\u{7}'),
            'b' => out.push('\u{8}'),
            'e' | 'E' => out.push('\u{1b}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\u{b}'),
            '\\' | '\'' | '"' | '?' => out.push(escaped),
            // `\nnn`: one to three octal digits, the first already read.
            '0'..='7' => {
                let (rest, used) = digits(chars, i, 8, 2);
                i += used;
                let value = (escaped as u32 - '0' as u32) * 8u32.pow(used as u32) + rest;
                push_code(&mut out, value & 0xff);
            }
            'x' | 'u' | 'U' => {
                let most = match escaped {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let (value, used) = digits(chars, i, 16, most);
                if used == 0 {
                    // No digits: bash keeps the escape as written.
                    out.push('\\');
                    out.push(escaped);
                } else {
                    i += used;
                    push_code(&mut out, value);
                }
            }
            // `\cx`: the control character of `x`.
            'c' => match chars.get(i) {
                Some(&x) => {
                    i += 1;
                    push_code(&mut out, x as u32 & 0x1f);
                }
                None => out.push_str("\\c"),
            },
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    (out, chars.len())
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

fn push_code(out: &mut String, code: u32) {
    out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
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

    #[test]
    fn escapes_decode_as_bash_does() {
        for (body, meant) in [
            (r"a\tb'", "a\tb"),
            (r"\x72m'", "rm"),
            (r"\x7'", "\u{7}"),
            (r"\162m'", "rm"),
            (r"\0'", "\0"),
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
