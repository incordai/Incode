//! The guard's lexer: a command read roughly as the shell reads it — quotes,
//! operators, comments, command substitutions and heredoc bodies — into the
//! tokens [`super::collect`] looks for writes in.
//!
//! One character starts each step of [`lex`], and a step consumes as much as
//! that character begins: a whole quoted span, an operator, a substitution, or
//! the heredoc lines after a newline. Each kind has its own [`Lexer`] method.

/// One lexical piece of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Token {
    /// A word with the shell's quoting removed.
    Word(String),
    /// An output redirect: the next word is written.
    WriteTo,
    /// `>&`: the next word is written, unless it is a descriptor (`2>&1`).
    WriteOrDup,
    /// An input redirect or here-string: the next word is read, not written,
    /// and is not an argument of the command either.
    ReadFrom,
    /// A heredoc, by index into [`Lexed::bodies`].
    Heredoc(usize),
    /// `|`: the next command reads this one's output.
    Pipe,
    /// Anything that ends a pipeline: `;`, `&&`, `||`, `&`, a newline, `(`, `)`.
    End,
}

/// A command, read.
#[derive(Debug, Default)]
pub(super) struct Lexed {
    pub(super) tokens: Vec<Token>,
    /// Each heredoc's text, kept apart from the tokens: whether it is a script
    /// or data depends on which program reads it.
    pub(super) bodies: Vec<String>,
    /// The commands inside `$(...)` and backticks, which run whatever the
    /// command around them does.
    pub(super) substitutions: Vec<String>,
}

/// Read a command roughly as the shell does: quotes, operators, comments,
/// command substitutions and heredoc bodies. With `ansi_c`, `$'…'` is read
/// as bash reads it; without, as a POSIX `sh` does.
///
/// # Wired to
///
/// Called by [`super::collect_reading`], once for each reading of `$'`.
pub(super) fn lex(text: &str, ansi_c: bool) -> Lexed {
    let chars: Vec<char> = text.chars().collect();
    let mut lexer = Lexer {
        ansi_c,
        ..Lexer::default()
    };
    let mut index = 0;
    while index < chars.len() {
        index = lexer.step(&chars, index);
    }
    lexer.flush();
    lexer.lexed
}

/// Past a comment: the index of the newline that ends it, or the end.
///
/// Called by [`Lexer::step`] for a `#` that begins a word.
fn skip_comment(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() && chars[index] != '\n' {
        index += 1;
    }
    index
}

#[derive(Default)]
struct Lexer {
    lexed: Lexed,
    /// `$'…'` read as bash reads it, not as a POSIX `sh` does
    /// ([`super::collect`]).
    ansi_c: bool,
    word: String,
    /// A word has begun, even if it is empty so far (`""`).
    started: bool,
    /// Some of the word was quoted, so `"2">x` is a word and not a descriptor.
    quoted: bool,
    /// A heredoc whose delimiter is the next word.
    awaiting_delimiter: Option<usize>,
    /// Heredocs announced on this line, read from the next: (body, delimiter).
    pending: Vec<(usize, String)>,
}

impl Lexer {
    /// Read whatever starts at `index`, and return where the next step starts.
    ///
    /// Called by [`lex`] for every step.
    fn step(&mut self, chars: &[char], index: usize) -> usize {
        let next = chars.get(index + 1).copied();
        match chars[index] {
            '\'' => self.single_quoted(chars, index),
            '"' => self.double_quoted(chars, index),
            '\\' => self.escaped(next, index),
            '`' => self.substitution(chars, index),
            '$' => self.dollar(chars, index),
            '#' if !self.started => skip_comment(chars, index),
            '\n' => {
                self.operator(Token::End);
                self.heredoc_bodies(chars, index + 1)
            }
            c if c.is_whitespace() => {
                self.flush();
                index + 1
            }
            ';' | '(' | ')' => {
                self.operator(Token::End);
                index + 1
            }
            '|' => self.pipe(next, index),
            '&' => self.ampersand(chars, index),
            '>' => self.output_redirect(next, index),
            '<' => self.input_redirect(chars, index),
            other => {
                self.word.push(other);
                self.started = true;
                index + 1
            }
        }
    }

    /// `'…'`: every character up to the closing quote, as it is.
    ///
    /// Called by [`Lexer::step`].
    fn single_quoted(&mut self, chars: &[char], mut index: usize) -> usize {
        self.started = true;
        self.quoted = true;
        index += 1;
        while index < chars.len() && chars[index] != '\'' {
            self.word.push(chars[index]);
            index += 1;
        }
        index + 1
    }

    /// `"…"`: a backslash escapes only `"`, `\`, `$` and a backtick, and a
    /// substitution inside still runs.
    ///
    /// Called by [`Lexer::step`].
    fn double_quoted(&mut self, chars: &[char], mut index: usize) -> usize {
        self.started = true;
        self.quoted = true;
        index += 1;
        while index < chars.len() && chars[index] != '"' {
            if chars[index] == '\\' && matches!(chars.get(index + 1), Some('"' | '\\' | '$' | '`'))
            {
                index += 1;
            } else if chars[index] == '`'
                || (chars[index] == '$' && matches!(chars.get(index + 1), Some('(')))
            {
                index = self.substitution(chars, index);
                continue;
            }
            self.word.push(chars[index]);
            index += 1;
        }
        index + 1
    }

    /// A backslash outside quotes: the next character, quoted; a newline after
    /// it joins the lines.
    ///
    /// Called by [`Lexer::step`].
    fn escaped(&mut self, next: Option<char>, index: usize) -> usize {
        match next {
            Some('\n') | None => {}
            Some(escaped) => {
                self.word.push(escaped);
                self.started = true;
                self.quoted = true;
            }
        }
        index + 2
    }

    /// A `$` outside quotes: a substitution, `$$`, `$'…'`, `$"…"`, or itself.
    ///
    /// Called by [`Lexer::step`].
    fn dollar(&mut self, chars: &[char], index: usize) -> usize {
        match chars.get(index + 1) {
            Some('(' | '{') => self.substitution(chars, index),
            // `$$`, the shell's pid: a quote after it is an ordinary one.
            Some('$') => {
                self.word.push_str("$$");
                self.started = true;
                index + 2
            }
            // `$'…'`: escapes decoded, ending where bash ends it
            // (`crate::ansi_c`) — a backslash escapes the quote.
            Some('\'') if self.ansi_c => {
                let (text, end) = crate::ansi_c::quoted(chars, index + 2);
                self.word.push_str(&text);
                self.started = true;
                self.quoted = true;
                end
            }
            // `$"…"` is read as `"…"`.
            Some('"') => index + 1,
            _ => {
                self.word.push('$');
                self.started = true;
                index + 1
            }
        }
    }

    /// `|` and `|&` continue the pipeline; `||` ends it.
    ///
    /// Called by [`Lexer::step`].
    fn pipe(&mut self, next: Option<char>, index: usize) -> usize {
        if next == Some('|') {
            self.operator(Token::End);
        } else {
            self.operator(Token::Pipe);
        }
        index
            + if matches!(next, Some('|' | '&')) {
                2
            } else {
                1
            }
    }

    /// `&>` and `&>>` write both output streams to a file; `&` and `&&` end
    /// the pipeline.
    ///
    /// Called by [`Lexer::step`].
    fn ampersand(&mut self, chars: &[char], index: usize) -> usize {
        let next = chars.get(index + 1).copied();
        if next == Some('>') {
            self.operator(Token::WriteTo);
            return index
                + if chars.get(index + 2) == Some(&'>') {
                    3
                } else {
                    2
                };
        }
        self.operator(Token::End);
        index + if next == Some('&') { 2 } else { 1 }
    }

    /// `>`, `>>`, `>|` and `>&`.
    ///
    /// Called by [`Lexer::step`].
    fn output_redirect(&mut self, next: Option<char>, index: usize) -> usize {
        self.drop_descriptor();
        let (token, width) = match next {
            Some('>' | '|') => (Token::WriteTo, 2),
            Some('&') => (Token::WriteOrDup, 2),
            _ => (Token::WriteTo, 1),
        };
        self.operator(token);
        index + width
    }

    /// `<<<`, a heredoc (`<<`, `<<-`), `<>`, `<` and `<&`.
    ///
    /// Called by [`Lexer::step`].
    fn input_redirect(&mut self, chars: &[char], index: usize) -> usize {
        self.drop_descriptor();
        let next = chars.get(index + 1).copied();
        let third = chars.get(index + 2).copied();
        match (next, third) {
            (Some('<'), Some('<')) => {
                self.operator(Token::ReadFrom);
                index + 3
            }
            (Some('<'), _) => {
                self.announce_heredoc();
                index + if third == Some('-') { 3 } else { 2 }
            }
            // `<>` opens the file for reading AND writing.
            (Some('>'), _) => {
                self.operator(Token::WriteTo);
                index + 2
            }
            _ => {
                self.operator(Token::ReadFrom);
                index + if next == Some('&') { 2 } else { 1 }
            }
        }
    }

    /// A heredoc operator: a body to fill from the next line, whose
    /// delimiter is the next word.
    ///
    /// Called by [`Lexer::input_redirect`].
    fn announce_heredoc(&mut self) {
        let body = self.lexed.bodies.len();
        self.lexed.bodies.push(String::new());
        self.operator(Token::Heredoc(body));
        self.awaiting_delimiter = Some(body);
    }

    /// End the current word, if one has begun: a heredoc's delimiter when one
    /// is awaited, a [`Token::Word`] otherwise.
    ///
    /// Called by [`lex`] at the end, by [`Lexer::step`] at whitespace, and by
    /// [`Lexer::operator`].
    fn flush(&mut self) {
        if !self.started {
            return;
        }
        let word = std::mem::take(&mut self.word);
        self.started = false;
        self.quoted = false;
        match self.awaiting_delimiter.take() {
            Some(body) => self.pending.push((body, word)),
            None => self.lexed.tokens.push(Token::Word(word)),
        }
    }

    /// End the current word, then add `token`.
    ///
    /// Called by every [`Lexer`] step that reads an operator.
    fn operator(&mut self, token: Token) {
        self.flush();
        self.awaiting_delimiter = None;
        self.lexed.tokens.push(token);
    }

    /// The `2` of `2>x` is the stream being redirected, not a word.
    ///
    /// Called by [`Lexer::output_redirect`] and [`Lexer::input_redirect`].
    fn drop_descriptor(&mut self) {
        if self.started && !self.quoted && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.started = false;
        }
    }

    /// Consume `$(...)`, `${...}` or a backtick span starting at `start` into
    /// the current word, keep the command inside it, and return where it ends.
    ///
    /// Called by [`Lexer::step`], [`Lexer::double_quoted`] and
    /// [`Lexer::dollar`].
    fn substitution(&mut self, chars: &[char], start: usize) -> usize {
        let (open, close, body_start) = delimiters(chars, start);
        let end = self
            .substitution_end(chars, body_start, open, close)
            .min(chars.len());
        let inner: String = chars[body_start.min(end)..end].iter().collect();
        let whole_end = (end + 1).min(chars.len());
        self.word.extend(&chars[start..whole_end]);
        self.started = true;
        // `${name}` is a variable, not a command.
        if open != '{' {
            self.lexed.substitutions.push(inner);
        }
        whole_end
    }

    /// The index of the delimiter that closes a substitution whose body starts
    /// at `body_start`; past the end when nothing closes it.
    ///
    /// Called by [`Lexer::substitution`].
    fn substitution_end(
        &self,
        chars: &[char],
        body_start: usize,
        open: char,
        close: char,
    ) -> usize {
        let mut depth = 1usize;
        let mut index = body_start;
        while index < chars.len() {
            if let Some(after) = self.skipped(chars, index, close) {
                index = after;
                continue;
            }
            let c = chars[index];
            if c == close {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if c == open && open != '`' {
                depth += 1;
            }
            index += 1;
        }
        index
    }

    /// Inside a substitution, where a span that cannot open or close it ends:
    /// an escape, `$$`, a `$'…'` (bash's reading only) or a quoted span.
    /// `None` when the character at `index` is not such a span.
    ///
    /// Called by [`Lexer::substitution_end`].
    fn skipped(&self, chars: &[char], index: usize, close: char) -> Option<usize> {
        let c = chars[index];
        let next = chars.get(index + 1);
        if c == '\\' || (c == '$' && next == Some(&'$')) {
            return Some(index + 2);
        }
        if self.ansi_c && c == '$' && next == Some(&'\'') {
            return Some(crate::ansi_c::quoted(chars, index + 2).1);
        }
        // A quoted `)` does not close `$(...)`: a quoted span is skipped
        // whole, or the rest of the command would be read off by one.
        if matches!(c, '\'' | '"') && c != close {
            return Some(past_quoted_span(chars, index, c));
        }
        None
    }

    /// After a newline, the lines of every heredoc announced on the line
    /// before it. Returns where ordinary commands resume.
    ///
    /// A delimiter line is matched with its whitespace trimmed, which bash does
    /// not do: ending a body early leaves MORE text read as commands, which can
    /// only refuse more, never less.
    ///
    /// Called by [`Lexer::step`] at a newline.
    fn heredoc_bodies(&mut self, chars: &[char], mut index: usize) -> usize {
        for (body, delimiter) in std::mem::take(&mut self.pending) {
            while index < chars.len() {
                let end = chars[index..]
                    .iter()
                    .position(|c| *c == '\n')
                    .map_or(chars.len(), |p| index + p);
                let line: String = chars[index..end].iter().collect();
                index = (end + 1).min(chars.len());
                if line.trim() == delimiter {
                    break;
                }
                let text = &mut self.lexed.bodies[body];
                text.push_str(&line);
                text.push('\n');
            }
        }
        index
    }
}

/// A substitution's opening and closing characters, and where its body
/// starts: a backtick, `${` or `$(`.
///
/// Called by [`Lexer::substitution`].
fn delimiters(chars: &[char], start: usize) -> (char, char, usize) {
    match chars[start] {
        '`' => ('`', '`', start + 1),
        _ => match chars.get(start + 1) {
            Some('{') => ('{', '}', start + 2),
            _ => ('(', ')', start + 2),
        },
    }
}

/// Just past the span quoted by `quote` that opens at `index`; in a double
/// quote a backslash takes the next character with it.
///
/// Called by [`Lexer::skipped`].
fn past_quoted_span(chars: &[char], index: usize, quote: char) -> usize {
    let mut index = index + 1;
    while index < chars.len() && chars[index] != quote {
        index += if quote == '"' && chars[index] == '\\' {
            2
        } else {
            1
        };
    }
    index + 1
}

#[cfg(test)]
mod tests {
    use super::{lex, Token};

    fn word(text: &str) -> Token {
        Token::Word(text.to_string())
    }

    /// A redirect operator ends where it ends, space after it or not.
    #[test]
    fn a_redirect_with_no_space_is_an_operator_and_a_word() {
        assert_eq!(
            lex("echo x >>tests/a.rs", true).tokens,
            [word("echo"), word("x"), Token::WriteTo, word("tests/a.rs")]
        );
    }

    /// A heredoc's lines are its body, not tokens, and the pipe after the
    /// operator is still read.
    #[test]
    fn a_heredoc_body_is_kept_apart_from_the_tokens() {
        let lexed = lex("cat <<'E' | sh\nrm tests/a.rs\nE\n", true);
        assert_eq!(
            lexed.tokens,
            [
                word("cat"),
                Token::Heredoc(0),
                Token::Pipe,
                word("sh"),
                Token::End
            ]
        );
        assert_eq!(lexed.bodies, ["rm tests/a.rs\n"]);
    }

    /// bash ends `$'\''` after one quote character; a POSIX `sh` reads a `$`
    /// and a plain quote that runs on.
    #[test]
    fn dollar_quote_is_read_both_ways() {
        let text = r"echo $'\'' ; x";
        assert_eq!(
            lex(text, true).tokens,
            [word("echo"), word("'"), Token::End, word("x")]
        );
        assert_eq!(lex(text, false).tokens, [word("echo"), word(r"$\ ; x")]);
    }

    /// `$(…)` and backticks run commands; `${…}` names a variable.
    #[test]
    fn substitutions_are_kept_and_variables_are_not() {
        let lexed = lex("echo $(rm a) `rm b` ${H}", true);
        assert_eq!(lexed.substitutions, ["rm a", "rm b"]);
    }
}
