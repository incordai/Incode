//! A command line split into its simple commands, quote-aware, for the rules
//! to judge one by one: on `&&`, `||`, `;`, `|`, `&` and newlines outside
//! quotes, with the bodies of substitutions and of `sh -c '…'` split as
//! commands of their own.

use super::peel::peel;
use super::rule::collapse_whitespace;

/// One simple command of a command line, ready to be matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// As written, whitespace-collapsed. What an allow rule is matched with.
    pub text: String,
    /// The forms a deny or ask rule is matched with: `text`, and `text` with
    /// leading assignments and wrapper commands peeled off, one at a time;
    /// each as written and unquoted.
    pub peeled: Vec<String>,
    /// A part no allow rule may pass: it substitutes a command, redirects,
    /// or hands a string to a shell.
    pub opaque: bool,
}

/// Split a command line into its simple commands, quote-aware.
///
/// Substituted commands (`$(…)`, backticks, `<(…)`, `>(…)`) are returned as
/// segments of their own, after the one they appear in, which is marked
/// opaque. So is the string handed to `sh -c` / `bash -c` / `eval`.
#[must_use]
pub fn split_command(line: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    split_into(line, &mut out, 0);
    out
}

/// How deep `sh -c "sh -c '…'"` is followed before the rest is simply opaque.
const MAX_DEPTH: usize = 8;

/// `line`'s parts, appended to `out`.
///
/// `$'…'` is ANSI-C quoting to bash, zsh and ksh, but a `$` and a plain
/// quote to a POSIX `sh` such as dash — the `sh` of Debian and Ubuntu, and
/// incode runs `sh -c` (`tools::shell_choice`). The two readings can end the
/// quote in different places, so a command can hide from one of them. Which
/// shell runs is not known here, so a line with `$'` is read BOTH ways: a deny
/// or ask rule meets the parts of either, and every part is opaque, so no
/// allow rule passes it. `guards::shell::write_targets` does the same.
fn split_into(line: &str, out: &mut Vec<Segment>, depth: usize) {
    if depth > MAX_DEPTH {
        out.push(Segment {
            text: collapse_whitespace(line),
            peeled: vec![collapse_whitespace(line)],
            opaque: true,
        });
        return;
    }
    let first = out.len();
    split_reading(line, out, depth, true);
    if line.contains("$'") {
        split_reading(line, out, depth, false);
        for segment in &mut out[first..] {
            segment.opaque = true;
        }
    }
}

/// One reading of `line`: with `ansi_c`, `$'…'` as bash reads it; without,
/// as a POSIX `sh` does.
///
/// Called by [`split_into`], once for each reading.
fn split_reading(line: &str, out: &mut Vec<Segment>, depth: usize, ansi_c: bool) {
    let chars: Vec<char> = line.chars().collect();
    let mut splitter = Splitter {
        out,
        depth,
        ansi_c,
        raw: String::new(),
        bare: String::new(),
        opaque: false,
        inner: Vec::new(),
        quote: None,
    };
    let mut i = 0;
    while i < chars.len() {
        i = match splitter.quote {
            Some('\'') => splitter.single_quoted(chars[i], i),
            Some('"') => splitter.double_quoted(&chars, i),
            _ => splitter.unquoted(&chars, i),
        };
    }
    splitter.flush();
}

/// The state of one reading of a line: the part being read, and where the
/// finished parts go.
struct Splitter<'o> {
    out: &'o mut Vec<Segment>,
    depth: usize,
    /// `$'…'` as bash reads it ([`split_into`]).
    ansi_c: bool,
    /// The current part, as written.
    raw: String,
    /// The current part with quoted text blanked, to look for redirections
    /// outside quotes.
    bare: String,
    opaque: bool,
    /// The bodies of the substitutions in the current part.
    inner: Vec<String>,
    quote: Option<char>,
}

impl Splitter<'_> {
    /// Inside `'…'`: every character is text, up to the closing quote.
    ///
    /// Called by [`split_reading`].
    fn single_quoted(&mut self, c: char, i: usize) -> usize {
        self.raw.push(c);
        self.bare.push('_');
        if c == '\'' {
            self.quote = None;
        }
        i + 1
    }

    /// Inside `"…"`: an escape, the closing quote, a substitution — which
    /// happens inside double quotes too — or text.
    ///
    /// Called by [`split_reading`].
    fn double_quoted(&mut self, chars: &[char], i: usize) -> usize {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\\' {
            return self.escaped(c, next, i);
        }
        if c == '"' {
            self.quote = None;
            self.raw.push(c);
            self.bare.push('_');
            return i + 1;
        }
        if (c == '$' && next == Some('(')) || c == '`' {
            return self.substitute(chars, i, i);
        }
        self.raw.push(c);
        self.bare.push('_');
        i + 1
    }

    /// Outside quotes: an escape, a `$` construct, an opening quote, a
    /// substitution, a separator, or text.
    ///
    /// Called by [`split_reading`].
    fn unquoted(&mut self, chars: &[char], i: usize) -> usize {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' => self.escaped(c, next, i),
            '$' => self.dollar(chars, i),
            '\'' | '"' => {
                self.quote = Some(c);
                self.raw.push(c);
                self.bare.push('_');
                i + 1
            }
            '`' => self.substitute(chars, i, i),
            '<' | '>' if next == Some('(') => self.substitute(chars, i, i + 1),
            ';' | '\n' | '\r' => {
                self.flush();
                // `;;` in a case arm: still a separator.
                i + 1
            }
            '|' => {
                self.flush();
                // `||` and `|&` are one operator.
                i + if matches!(next, Some('|' | '&')) {
                    2
                } else {
                    1
                }
            }
            '&' => self.ampersand(next, i),
            _ => {
                self.raw.push(c);
                self.bare.push(c);
                i + 1
            }
        }
    }

    /// A backslash and the character after it, kept as written and blanked.
    ///
    /// Called by [`Splitter::double_quoted`] and [`Splitter::unquoted`].
    fn escaped(&mut self, c: char, next: Option<char>, i: usize) -> usize {
        self.raw.push(c);
        if let Some(n) = next {
            self.raw.push(n);
        }
        self.bare.push_str("__");
        i + 2
    }

    /// A `$` outside quotes: `$$`, `$'…'`, `$"…"`, `$(…)`, or itself.
    ///
    /// Called by [`Splitter::unquoted`].
    fn dollar(&mut self, chars: &[char], i: usize) -> usize {
        match chars.get(i + 1) {
            // `$$`, the shell's pid: a quote after it is an ordinary one.
            Some('$') => {
                self.raw.push_str("$$");
                self.bare.push_str("$$");
                i + 2
            }
            // `$'…'`: a backslash escapes the quote, so it ends where bash
            // says (`crate::ansi_c`). What it spells is decided by escapes,
            // so no allow rule passes the part.
            Some('\'') if self.ansi_c => {
                let (_, end) = crate::ansi_c::quoted(chars, i + 2);
                self.raw.extend(&chars[i..end]);
                self.bare.extend(std::iter::repeat_n('_', end - i));
                self.opaque = true;
                end
            }
            // `$"…"`: a double-quoted string the locale may translate into
            // other text; past the `$` it is read as `"…"`.
            Some('"') => {
                self.raw.push('$');
                self.bare.push('_');
                self.opaque = true;
                i + 1
            }
            Some('(') => self.substitute(chars, i, i),
            _ => {
                self.raw.push('$');
                self.bare.push('$');
                i + 1
            }
        }
    }

    /// `&` and `&&` separate; after a `>` or `<`, or before a `>`, it is part
    /// of a redirection (`>&2`, `2>&1`, `&>file`).
    ///
    /// Called by [`Splitter::unquoted`].
    fn ampersand(&mut self, next: Option<char>, i: usize) -> usize {
        let after_redirect = self.bare.ends_with('>') || self.bare.ends_with('<');
        if after_redirect || next == Some('>') {
            self.raw.push('&');
            self.bare.push('&');
            return i + 1;
        }
        self.flush();
        i + if next == Some('&') { 2 } else { 1 }
    }

    /// The substitution that opens at `i` and whose bracket or backtick is at
    /// `from`: kept in the part as written, its body kept to be split after
    /// the part, and the part made opaque.
    ///
    /// Called by every [`Splitter`] step that meets `$(`, a backtick, `<(` or
    /// `>(`.
    fn substitute(&mut self, chars: &[char], i: usize, from: usize) -> usize {
        let (body, end) = substitution(chars, from, self.ansi_c);
        self.raw.extend(&chars[i..end]);
        self.bare.push('_');
        self.inner.push(body);
        self.opaque = true;
        end
    }

    /// End the current part: add it, then the commands substituted in it.
    ///
    /// Called by [`split_reading`] at the end and by every separator.
    fn flush(&mut self) {
        finish_segment(&self.raw, &self.bare, self.opaque, self.out, self.depth);
        for body in self.inner.drain(..) {
            split_into(&body, self.out, self.depth + 1);
        }
        self.raw.clear();
        self.bare.clear();
        self.opaque = false;
    }
}

/// The body of the substitution that starts at `start` (`$(`, `(` or a
/// backtick), and the index just past its end. Unterminated: the rest.
/// `ansi_c`: whether `$'…'` is read as bash reads it ([`split_into`]).
///
/// Called by [`Splitter::substitute`].
fn substitution(chars: &[char], start: usize, ansi_c: bool) -> (String, usize) {
    if chars[start] == '`' {
        return backtick_substitution(chars, start);
    }
    // `$(` or `(`: find the matching close, quote-aware.
    let open = if chars[start] == '$' {
        start + 1
    } else {
        start
    };
    paren_substitution(chars, open, ansi_c)
}

/// A backtick substitution's body, its escapes removed, and the index just
/// past the closing backtick.
///
/// Called by [`substitution`].
fn backtick_substitution(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start + 1;
    let mut body = String::new();
    while i < chars.len() && chars[i] != '`' {
        if chars[i] == '\\' && i + 1 < chars.len() {
            body.push(chars[i + 1]);
            i += 2;
            continue;
        }
        body.push(chars[i]);
        i += 1;
    }
    (body, (i + 1).min(chars.len()))
}

/// The body of the bracket that opens at `open`, and the index just past the
/// bracket that closes it.
///
/// Called by [`substitution`].
fn paren_substitution(chars: &[char], open: usize, ansi_c: bool) -> (String, usize) {
    let mut scan = ParenScan {
        ansi_c,
        depth: 0,
        quote: None,
    };
    let mut i = open;
    while i < chars.len() {
        match scan.step(chars, i) {
            Scan::At(next) => i = next,
            Scan::Closed(close) => {
                let body: String = chars[open + 1..close].iter().collect();
                return (body, close + 1);
            }
        }
    }
    let body: String = chars[(open + 1).min(chars.len())..].iter().collect();
    (body, chars.len())
}

/// Where a step of [`ParenScan`] leaves the scan.
enum Scan {
    /// Keep reading at this index.
    At(usize),
    /// The bracket at this index closes the substitution.
    Closed(usize),
}

/// The brackets open, and the quote the scan is inside, while a `$(…)` is
/// read for the bracket that closes it.
struct ParenScan {
    ansi_c: bool,
    depth: usize,
    quote: Option<char>,
}

impl ParenScan {
    /// Called by [`paren_substitution`] for every step.
    fn step(&mut self, chars: &[char], i: usize) -> Scan {
        match self.quote {
            Some(quote) => Scan::At(self.quoted(quote, chars[i], i)),
            None => self.unquoted(chars, i),
        }
    }

    /// Inside a quote: only a backslash in `"…"` and the closing quote count.
    ///
    /// Called by [`ParenScan::step`].
    fn quoted(&mut self, quote: char, c: char, i: usize) -> usize {
        if c == '\\' && quote == '"' {
            return i + 2;
        }
        if c == quote {
            self.quote = None;
        }
        i + 1
    }

    /// Outside quotes: escapes, `$$` and `$'…'` are skipped whole, a quote
    /// opens, and the brackets are counted.
    ///
    /// Called by [`ParenScan::step`].
    fn unquoted(&mut self, chars: &[char], i: usize) -> Scan {
        let c = chars[i];
        let next = chars.get(i + 1);
        match c {
            '\\' => Scan::At(i + 2),
            '$' if next == Some(&'$') => Scan::At(i + 2),
            '$' if self.ansi_c && next == Some(&'\'') => {
                Scan::At(crate::ansi_c::quoted(chars, i + 2).1)
            }
            '\'' | '"' => {
                self.quote = Some(c);
                Scan::At(i + 1)
            }
            '(' => {
                self.depth += 1;
                Scan::At(i + 1)
            }
            ')' => {
                self.depth -= 1;
                if self.depth == 0 {
                    Scan::Closed(i)
                } else {
                    Scan::At(i + 1)
                }
            }
            _ => Scan::At(i + 1),
        }
    }
}

/// Redirections that only point output at the terminal's other stream or at
/// nothing: harmless, and too common to make every command opaque.
fn benign_redirections() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?:\d*>>?|&>>?)\s*/dev/null\b|\d*>&\s*\d+|\d*<&\s*\d+")
            .expect("static regex")
    })
}

fn finish_segment(raw: &str, bare: &str, opaque: bool, out: &mut Vec<Segment>, depth: usize) {
    // A subshell or a group's brackets, and a leading `!`, do not change
    // which command runs.
    let mut text = collapse_whitespace(raw);
    loop {
        let before = text.len();
        for open in ["(", "{ ", "! "] {
            if let Some(rest) = text.strip_prefix(open) {
                text = rest.trim_start().to_string();
            }
        }
        // Only a close with no open of its own in this part: `$(c)` keeps
        // its bracket, the end of `(a; b)` loses it.
        for (open, close) in [('(', ")"), ('{', " }"), ('{', "}")] {
            let unbalanced = text.matches(close.trim()).count() > text.matches(open).count();
            if unbalanced {
                if let Some(rest) = text.strip_suffix(close) {
                    text = rest.trim_end().to_string();
                }
            }
        }
        if text.len() == before {
            break;
        }
    }
    // Shell keywords that introduce the command after them.
    for keyword in ["then ", "do ", "else ", "elif ", "if ", "while ", "until "] {
        if let Some(rest) = text.strip_prefix(keyword) {
            text = rest.trim_start().to_string();
        }
    }
    if text.is_empty() || matches!(text.as_str(), "fi" | "done" | "esac" | "then" | "do") {
        return;
    }
    let cleaned = benign_redirections().replace_all(bare, "");
    let redirects = cleaned.contains('>') || cleaned.contains('<');
    let (peeled, hands_to_shell) = peel(&text);
    let mut segment = Segment {
        text: text.clone(),
        peeled,
        opaque: opaque || redirects || hands_to_shell.is_some(),
    };
    segment.peeled.dedup();
    out.push(segment);
    if let Some(script) = hands_to_shell {
        split_into(&script, out, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::{split_command, substitution};

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    /// A bracket inside quotes or a nested pair does not close `$(…)`.
    #[test]
    fn a_bracket_substitution_ends_at_its_own_close() {
        assert_eq!(
            substitution(&chars("$(a (b) 'c)') d"), 0, true),
            ("a (b) 'c)'".to_string(), 13)
        );
        assert_eq!(
            substitution(&chars("$(open"), 0, true),
            ("open".to_string(), 6)
        );
    }

    /// An escaped backtick does not close a backtick substitution.
    #[test]
    fn a_backtick_substitution_ends_at_an_unescaped_backtick() {
        assert_eq!(
            substitution(&chars(r"`a \` b` c"), 0, true),
            ("a ` b".to_string(), 8)
        );
    }

    /// A line with `$'` is read both ways, and every part is opaque.
    #[test]
    fn a_dollar_quote_line_is_read_both_ways_and_made_opaque() {
        let parts = split_command(r"echo $'\'' ; rm x");
        assert!(parts.iter().any(|s| s.text == "rm x"), "{parts:?}");
        assert!(parts.iter().all(|s| s.opaque), "{parts:?}");
    }
}
