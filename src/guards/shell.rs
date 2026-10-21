//! Which paths a shell command can WRITE, read off its text.
//!
//! # Why a lexer and not a word split
//!
//! The first version split the command on whitespace and matched every word,
//! and that was wrong in both directions at once. `printf x >>tests/it.rs` is
//! ONE word, `>>tests/it.rs`, whose first segment is `>>tests` — so the append
//! went through and only the sentinel noticed (incordai/Incode#4). And a heredoc
//! writing a document that merely MENTIONED `tests/it.rs` was refused, because
//! every word of the document was taken for a path (#11). Both come from not
//! knowing where a redirect ends or where data begins, and knowing that takes
//! reading the command the way the shell does — quotes, operators, heredoc
//! bodies — if only roughly.
//!
//! # What counts as written
//!
//! - the target of an output redirect: `>`, `>>`, `>|`, `&>`, `2>`, `<>`
//! - an argument of a command that writes the files it is handed (`rm`, `tee`,
//!   `sed -i`, `perl -pi`, `git checkout`, ...)
//! - a quoted string inside an inline program (`python -c`, `node -e`, a
//!   heredoc fed to `python3`): that program's syntax is not parsed, so a
//!   literal naming a protected path is taken to be one it writes
//! - a command handed to a shell (`bash -c`, `eval`, `$(...)`, a heredoc fed to
//!   `sh`), read the same way, recursively
//!
//! Heredoc text given to anything else is DATA: it is what gets written, not
//! where, so the paths it mentions are not targets.
//!
//! # Where it gives up, and which way
//!
//! A writer whose argument is not literal — `$f`, a `{}` from `find`, `xargs`
//! feeding it — cannot be resolved from text. Then every word of the command,
//! heredoc data excepted, is taken as a possible target: an ambiguity is
//! resolved towards refusing. Even so this is a textual check, and the module
//! header of `guards` says what it cannot catch; the sentinel is the guarantee.

/// Every string this command could write to, as the shell would hand it over.
///
/// # Wired to
///
/// Called by [`super::Guards::protecting_in_command`], which matches each one
/// against the protected fragments.
#[must_use]
pub(super) fn write_targets(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Whether it was unresolved no longer matters here: `collect` has already
    // added every word of a command it could not read precisely.
    let _ = collect(command, 0, &mut out);
    out
}

/// How deep `bash -c "eval '...'"` is followed. Each level is a strictly
/// shorter string, so this bounds the work of a hostile nesting, not a loop.
/// Past it, every word is a target: a command too deep to read is refused
/// rather than waved through.
const MAX_DEPTH: usize = 8;

/// Commands that write, change or remove every file they are given.
///
/// `cp`, `rsync`, `install` and `ln` are here whole, sources included: which
/// argument is the destination depends on flags (`cp -t`), and guessing it
/// wrong is a hole. Copying a test OUT is refused with them, which `cat` does
/// not need.
const WRITERS: [&str; 14] = [
    "rm", "rmdir", "mv", "cp", "truncate", "tee", "dd", "chmod", "chown", "install", "ln", "touch",
    "unlink", "rsync",
];

/// Programs whose inline code is scanned for quoted paths. `perl` is here and
/// also an in-place editor (`perl -pi`), and is read both ways.
const INTERPRETERS: [&str; 7] = ["python", "py", "node", "nodejs", "ruby", "perl", "php"];

/// Programs that run a command handed to them as text.
const SHELLS: [&str; 7] = ["bash", "sh", "zsh", "dash", "ksh", "ash", "fish"];

/// Words in front of the program that are not the program: the program is the
/// word after them. Flags after a wrapper are skipped with it.
const PREFIXES: [&str; 18] = [
    "sudo", "doas", "env", "command", "builtin", "exec", "nohup", "time", "nice", "then", "do",
    "else", "elif", "if", "while", "until", "!", "{",
];

/// `git` subcommands that overwrite or remove the files they are given.
const GIT_WRITERS: [&str; 4] = ["checkout", "restore", "rm", "mv"];

/// One lexical piece of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
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
struct Lexed {
    tokens: Vec<Token>,
    /// Each heredoc's text, kept apart from the tokens: whether it is a script
    /// or data depends on which program reads it.
    bodies: Vec<String>,
    /// The commands inside `$(...)` and backticks, which run whatever the
    /// command around them does.
    substitutions: Vec<String>,
}

/// What a program does with its arguments, for this purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Program {
    /// Writes every argument.
    Writer,
    /// Writes every argument when an in-place flag is given.
    InPlaceEditor,
    /// Runs code given inline; its quoted strings are scanned.
    Interpreter,
    /// `perl`: an interpreter, and an in-place editor with `-i`.
    Perl,
    /// Runs a command given as text.
    Shell,
    Eval,
    Git,
    Find,
    /// Reads, or does something this module has no opinion about.
    Other,
}

/// Push every target of `command` into `out`. True when one of its writes could
/// not be read from the text, so a command wrapping this one — `bash -c`, a
/// `$(...)` — must count its own words too: `bash -c 'rm "$1"' _ tests/x`
/// names its target OUTSIDE the inner command.
#[must_use]
fn collect(command: &str, depth: usize, out: &mut Vec<String>) -> bool {
    if depth > MAX_DEPTH {
        out.extend(pieces(command));
        return true;
    }
    let lexed = lex(command);
    let mut unresolved = false;
    for inner in &lexed.substitutions {
        unresolved |= collect(inner, depth + 1, out);
    }
    for pipeline in lexed.tokens.split(|t| *t == Token::End) {
        let commands: Vec<&[Token]> = pipeline.split(|t| *t == Token::Pipe).collect();
        // A heredoc anywhere in `cat <<E | sh` is a script, because `sh` reads
        // it; the command it is attached to is not what decides.
        let reader = commands
            .iter()
            .map(|c| program_of(&words_of(c)).0)
            .find(|p| matches!(p, Program::Shell | Program::Interpreter | Program::Perl));
        for tokens in commands {
            for (index, token) in tokens.iter().enumerate() {
                let next = match tokens.get(index + 1) {
                    Some(Token::Word(word)) => Some(word),
                    _ => None,
                };
                match (token, next) {
                    (Token::WriteTo, Some(target)) => {
                        unresolved |= is_unresolved(target);
                        out.push(target.clone());
                    }
                    (Token::WriteOrDup, Some(target)) if !is_descriptor(target) => {
                        unresolved |= is_unresolved(target);
                        out.push(target.clone());
                    }
                    (Token::Heredoc(body), _) => {
                        let body = &lexed.bodies[*body];
                        match reader {
                            Some(Program::Shell) => unresolved |= collect(body, depth + 1, out),
                            Some(Program::Interpreter | Program::Perl) => {
                                out.extend(quoted_literals(body));
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            unresolved |= arguments_written(&words_of(tokens), depth, out);
        }
    }
    if unresolved {
        for token in &lexed.tokens {
            if let Token::Word(word) = token {
                out.extend(pieces(word));
            }
        }
    }
    unresolved
}

/// Push what this one command writes through its ARGUMENTS. True when an
/// argument it writes cannot be read from the text.
fn arguments_written(words: &[&str], depth: usize, out: &mut Vec<String>) -> bool {
    let (program, start, fed_by_xargs) = program_of(words);
    let args = words.get(start + 1..).unwrap_or_default();
    let in_place = args.iter().any(|a| is_in_place_flag(a));
    // Whether the program writes the files its arguments name.
    let mut writes = false;
    let mut unresolved = false;
    match program {
        Program::Writer => {
            writes = true;
            unresolved |= push_all(args, out);
        }
        Program::InPlaceEditor | Program::Perl => {
            if in_place {
                writes = true;
                unresolved |= push_all(args, out);
            }
            if program == Program::Perl {
                for arg in args {
                    out.extend(quoted_literals(arg));
                }
            }
        }
        Program::Interpreter => {
            // The program's own arguments are NOT targets: `python -m pytest
            // tests/` runs the tests, which is the whole job.
            for arg in args {
                out.extend(quoted_literals(arg));
            }
        }
        Program::Shell => {
            for arg in args.iter().filter(|a| !a.starts_with('-')) {
                unresolved |= collect(arg, depth + 1, out);
            }
        }
        Program::Eval => unresolved |= collect(&args.join(" "), depth + 1, out),
        Program::Git => {
            if let Some(sub) = git_subcommand(args) {
                if GIT_WRITERS.contains(&args[sub]) {
                    writes = true;
                    unresolved |= push_all(&args[sub + 1..], out);
                }
            }
        }
        Program::Find => {
            if find_writes(args, depth) {
                // What `-delete` or `-exec rm {}` reaches is whatever was
                // found, which no argument spells out.
                push_all(args, out);
                unresolved = true;
            }
        }
        Program::Other => {}
    }
    // `xargs rm` writes whatever arrives on stdin, which no argument names.
    unresolved || (fed_by_xargs && writes)
}

/// Every argument, plus the value of any `name=value` one (`dd of=x`,
/// `--target-directory=x`). True when one of them is decided at run time.
fn push_all(args: &[&str], out: &mut Vec<String>) -> bool {
    for arg in args {
        out.push((*arg).to_string());
        if let Some((_, value)) = arg.split_once('=') {
            out.push(value.to_string());
        }
    }
    args.iter().any(|a| is_unresolved(a))
}

/// The program a command runs, the index of its word, and whether `xargs`
/// stands in front of it.
fn program_of(words: &[&str]) -> (Program, usize, bool) {
    let mut index = 0;
    let mut xargs = false;
    while let Some(word) = words.get(index) {
        let name = program_name(word);
        if is_assignment(word) {
            index += 1;
        } else if PREFIXES.contains(&name.as_str()) || name == "xargs" {
            xargs |= name == "xargs";
            index += 1;
            while words.get(index).is_some_and(|w| w.starts_with('-')) {
                index += 1;
            }
        } else {
            break;
        }
    }
    let Some(word) = words.get(index) else {
        return (Program::Other, index, xargs);
    };
    let name = program_name(word);
    let program = if WRITERS.contains(&name.as_str()) {
        Program::Writer
    } else if name == "sed" {
        Program::InPlaceEditor
    } else if name == "perl" {
        Program::Perl
    } else if INTERPRETERS.contains(&name.as_str()) || name.starts_with("python") {
        Program::Interpreter
    } else if SHELLS.contains(&name.as_str()) {
        Program::Shell
    } else if name == "eval" {
        Program::Eval
    } else if name == "git" {
        Program::Git
    } else if name == "find" {
        Program::Find
    } else {
        Program::Other
    };
    (program, index, xargs)
}

/// `/usr/bin/python3.12` and `python.exe` are both `python3.12`/`python`.
fn program_name(word: &str) -> String {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let lower = base.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// `FOO=bar` in front of a command sets a variable; it is not the program.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// `-i`, `-i.bak`, `-pi`, `-ie`, `--in-place`: an `i` anywhere in a cluster of
/// short flags. Read broadly — `-ni` is in-place too, and a flag misread as
/// in-place costs a refusal where a miss costs the exam.
fn is_in_place_flag(word: &str) -> bool {
    if word.starts_with("--") {
        return word.starts_with("--in-place");
    }
    word.strip_prefix('-')
        .is_some_and(|flags| flags.split('.').next().is_some_and(|f| f.contains('i')))
}

/// The index of `git`'s subcommand among its arguments, past `-C <dir>` and
/// the other global options that take a value.
fn git_subcommand(args: &[&str]) -> Option<usize> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if matches!(
            *arg,
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            index += 2;
        } else if arg.starts_with('-') {
            index += 1;
        } else {
            return Some(index);
        }
    }
    None
}

/// Whether `find` deletes, or runs a writer on, what it finds.
fn find_writes(args: &[&str], depth: usize) -> bool {
    if args.contains(&"-delete") {
        return true;
    }
    args.iter().enumerate().any(|(index, arg)| {
        matches!(*arg, "-exec" | "-execdir" | "-ok" | "-okdir") && {
            let command: Vec<&str> = args[index + 1..]
                .iter()
                .take_while(|w| !matches!(**w, ";" | "+"))
                .copied()
                .collect();
            let mut reached = Vec::new();
            arguments_written(&command, depth + 1, &mut reached) || !reached.is_empty()
        }
    })
}

/// `1`, `2`, `-` after `>&` name a descriptor, not a file.
fn is_descriptor(word: &str) -> bool {
    word == "-" || (!word.is_empty() && word.chars().all(|c| c.is_ascii_digit()))
}

/// A word whose value the shell decides at run time: a variable, a command
/// substitution, a `find` placeholder.
fn is_unresolved(word: &str) -> bool {
    word.contains(['$', '`', '{'])
}

/// The words of one command, redirect operands and heredocs left out.
fn words_of(tokens: &[Token]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut operand = false;
    for token in tokens {
        match token {
            Token::Word(word) if !operand => words.push(word.as_str()),
            _ => {}
        }
        operand = matches!(token, Token::WriteTo | Token::WriteOrDup | Token::ReadFrom);
    }
    words
}

/// The strings quoted inside a piece of code: `'tests/a.rs'` in
/// `open('tests/a.rs','w')`.
fn quoted_literals(code: &str) -> Vec<String> {
    let chars: Vec<char> = code.chars().collect();
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let quote = chars[index];
        if !matches!(quote, '\'' | '"' | '`') {
            index += 1;
            continue;
        }
        let mut literal = String::new();
        index += 1;
        while index < chars.len() && chars[index] != quote {
            if chars[index] == '\\' && index + 1 < chars.len() {
                index += 1;
            }
            literal.push(chars[index]);
            index += 1;
        }
        index += 1;
        out.push(literal);
    }
    out
}

/// A word cut at everything a path does not contain, for when the command
/// could not be read precisely and every word has to count.
fn pieces(text: &str) -> Vec<String> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>')
            || matches!(c, '\'' | '"' | '`' | '$' | '=' | ',' | '{' | '}')
    })
    .filter(|p| !p.is_empty())
    .map(str::to_string)
    .collect()
}

/// Read a command roughly as the shell does: quotes, operators, comments,
/// command substitutions and heredoc bodies.
fn lex(text: &str) -> Lexed {
    let chars: Vec<char> = text.chars().collect();
    let mut lexer = Lexer::default();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let next = chars.get(index + 1).copied();
        match c {
            '\'' => {
                lexer.started = true;
                lexer.quoted = true;
                index += 1;
                while index < chars.len() && chars[index] != '\'' {
                    lexer.word.push(chars[index]);
                    index += 1;
                }
                index += 1;
            }
            '"' => {
                lexer.started = true;
                lexer.quoted = true;
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    if chars[index] == '\\'
                        && matches!(chars.get(index + 1), Some('"' | '\\' | '$' | '`'))
                    {
                        index += 1;
                    } else if chars[index] == '`'
                        || (chars[index] == '$' && matches!(chars.get(index + 1), Some('(')))
                    {
                        index = lexer.substitution(&chars, index);
                        continue;
                    }
                    lexer.word.push(chars[index]);
                    index += 1;
                }
                index += 1;
            }
            '\\' => {
                match next {
                    Some('\n') => {}
                    Some(escaped) => {
                        lexer.word.push(escaped);
                        lexer.started = true;
                        lexer.quoted = true;
                    }
                    None => {}
                }
                index += 2;
            }
            '`' => index = lexer.substitution(&chars, index),
            '$' if matches!(next, Some('(' | '{')) => index = lexer.substitution(&chars, index),
            // `$$`, the shell's pid: a quote after it is an ordinary one.
            '$' if next == Some('$') => {
                lexer.word.push_str("$$");
                lexer.started = true;
                index += 2;
            }
            // `$'…'`: escapes decoded, ending where bash ends it
            // (`crate::ansi_c`) — a backslash escapes the quote.
            '$' if next == Some('\'') => {
                let (text, end) = crate::ansi_c::quoted(&chars, index + 2);
                lexer.word.push_str(&text);
                lexer.started = true;
                lexer.quoted = true;
                index = end;
            }
            // `$"…"` is read as `"…"`.
            '$' if next == Some('"') => index += 1,
            '#' if !lexer.started => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '\n' => {
                lexer.operator(Token::End);
                index = lexer.heredoc_bodies(&chars, index + 1);
            }
            c if c.is_whitespace() => {
                lexer.flush();
                index += 1;
            }
            ';' | '(' | ')' => {
                lexer.operator(Token::End);
                index += 1;
            }
            '|' => {
                // `||` ends the pipeline; `|` and `|&` continue it.
                if next == Some('|') {
                    lexer.operator(Token::End);
                } else {
                    lexer.operator(Token::Pipe);
                }
                index += if matches!(next, Some('|' | '&')) {
                    2
                } else {
                    1
                };
            }
            '&' => {
                if next == Some('>') {
                    // `&>` and `&>>`: both output streams into a file.
                    lexer.operator(Token::WriteTo);
                    index += if chars.get(index + 2) == Some(&'>') {
                        3
                    } else {
                        2
                    };
                } else {
                    lexer.operator(Token::End);
                    index += if next == Some('&') { 2 } else { 1 };
                }
            }
            '>' => {
                lexer.drop_descriptor();
                let (token, width) = match next {
                    Some('>' | '|') => (Token::WriteTo, 2),
                    Some('&') => (Token::WriteOrDup, 2),
                    _ => (Token::WriteTo, 1),
                };
                lexer.operator(token);
                index += width;
            }
            '<' => {
                lexer.drop_descriptor();
                if next == Some('<') && chars.get(index + 2) == Some(&'<') {
                    lexer.operator(Token::ReadFrom);
                    index += 3;
                } else if next == Some('<') {
                    let body = lexer.lexed.bodies.len();
                    lexer.lexed.bodies.push(String::new());
                    lexer.operator(Token::Heredoc(body));
                    lexer.awaiting_delimiter = Some(body);
                    index += if chars.get(index + 2) == Some(&'-') {
                        3
                    } else {
                        2
                    };
                } else if next == Some('>') {
                    // `<>` opens the file for reading AND writing.
                    lexer.operator(Token::WriteTo);
                    index += 2;
                } else {
                    lexer.operator(Token::ReadFrom);
                    index += if next == Some('&') { 2 } else { 1 };
                }
            }
            other => {
                lexer.word.push(other);
                lexer.started = true;
                index += 1;
            }
        }
    }
    lexer.flush();
    lexer.lexed
}

#[derive(Default)]
struct Lexer {
    lexed: Lexed,
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

    fn operator(&mut self, token: Token) {
        self.flush();
        self.awaiting_delimiter = None;
        self.lexed.tokens.push(token);
    }

    /// The `2` of `2>x` is the stream being redirected, not a word.
    fn drop_descriptor(&mut self) {
        if self.started && !self.quoted && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.started = false;
        }
    }

    /// Consume `$(...)`, `${...}` or a backtick span starting at `start` into
    /// the current word, keep the command inside it, and return where it ends.
    fn substitution(&mut self, chars: &[char], start: usize) -> usize {
        let (open, close, body_start) = match chars[start] {
            '`' => ('`', '`', start + 1),
            _ => match chars.get(start + 1) {
                Some('{') => ('{', '}', start + 2),
                _ => ('(', ')', start + 2),
            },
        };
        let mut depth = 1usize;
        let mut index = body_start;
        while index < chars.len() {
            let c = chars[index];
            if c == '\\' {
                index += 2;
                continue;
            }
            if c == '$' && chars.get(index + 1) == Some(&'$') {
                index += 2;
                continue;
            }
            if c == '$' && chars.get(index + 1) == Some(&'\'') {
                index = crate::ansi_c::quoted(chars, index + 2).1;
                continue;
            }
            // A quoted `)` does not close `$(...)`: a quoted span is skipped
            // whole, or the rest of the command would be read off by one.
            if matches!(c, '\'' | '"') && c != close {
                index += 1;
                while index < chars.len() && chars[index] != c {
                    index += if c == '"' && chars[index] == '\\' {
                        2
                    } else {
                        1
                    };
                }
                index += 1;
                continue;
            }
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
        let end = index.min(chars.len());
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

    /// After a newline, the lines of every heredoc announced on the line
    /// before it. Returns where ordinary commands resume.
    ///
    /// A delimiter line is matched with its whitespace trimmed, which bash does
    /// not do: ending a body early leaves MORE text read as commands, which can
    /// only refuse more, never less.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(command: &str) -> Vec<String> {
        write_targets(command)
    }

    /// A redirect ends where the operator does, whitespace or not — the word
    /// after `>>` with no space is the file, not part of an operator.
    #[test]
    fn a_redirect_target_is_read_with_or_without_a_space() {
        for command in [
            "echo x >tests/a.rs",
            "echo x > tests/a.rs",
            "echo x >>tests/a.rs",
            "echo x 2>tests/a.rs",
            "echo x &>tests/a.rs",
            "echo x >|tests/a.rs",
        ] {
            assert_eq!(
                targets(command),
                vec!["tests/a.rs".to_string()],
                "the redirect target is the file written: {command}"
            );
        }
    }

    /// `2>&1` duplicates a stream; there is no file called `1`.
    #[test]
    fn a_descriptor_duplication_writes_no_file() {
        assert!(
            targets("cargo test 2>&1").is_empty(),
            "`&1` is a stream, not a path"
        );
    }

    /// A quoted `>` is text, not a redirect.
    #[test]
    fn a_quoted_operator_is_only_text() {
        assert!(
            targets(r#"echo "a > tests/a.rs""#).is_empty(),
            "a quoted `>` redirects nothing"
        );
    }

    /// **`$'…'` ENDS WHERE BASH SAYS.** A backslash escapes its quote, so
    /// `$'\''` is one character and the redirect after the `;` is real. Read
    /// as a plain `'…'`, the rest of the line looked quoted and the write to a
    /// protected file went unseen.
    #[test]
    fn an_ansi_c_quote_does_not_hide_a_redirect_after_it() {
        for command in [
            r"echo $'\'' ; echo x > tests/a.rs #'",
            r"echo $'it\'s' && echo x >tests/a.rs #'",
            r"echo $(echo $'\'') ; echo x > tests/a.rs #'",
        ] {
            assert_eq!(
                targets(command),
                vec!["tests/a.rs".to_string()],
                "{command}"
            );
        }
        assert!(
            targets(r"echo $'a > tests/a.rs'").is_empty(),
            "a `>` inside `$'…'` is text"
        );
    }

    /// A target spelled with `$'…'` or `$"…"` is the file bash writes.
    #[test]
    fn a_target_in_ansi_c_or_locale_quotes_is_the_file_written() {
        for command in [
            r"echo x > $'tests/a.rs'",
            r"echo x > $'tests\x2fa.rs'",
            r#"echo x > $"tests/a.rs""#,
        ] {
            assert_eq!(
                targets(command),
                vec!["tests/a.rs".to_string()],
                "{command}"
            );
        }
    }

    /// The body of a heredoc given to `cat` is what is written, not where.
    #[test]
    fn heredoc_data_is_not_a_target() {
        let found = targets("cat > docs/a.md <<'E'\nsee tests/it.rs\nE\n");
        assert_eq!(found, vec!["docs/a.md".to_string()], "only the redirect");
    }

    /// A heredoc fed to a shell is a script, and its commands count.
    #[test]
    fn a_heredoc_fed_to_a_shell_is_read_as_commands() {
        let found = targets("bash <<'E'\nrm tests/a.rs\nE\n");
        assert!(
            found.contains(&"tests/a.rs".to_string()),
            "the body is run, so its rm writes: {found:?}"
        );
        let piped = targets("cat <<'E' | sh\nrm tests/a.rs\nE\n");
        assert!(
            piped.contains(&"tests/a.rs".to_string()),
            "piped into sh it runs just the same: {piped:?}"
        );
    }

    /// Commands after a heredoc's delimiter are commands again.
    #[test]
    fn the_command_after_a_heredoc_is_read() {
        let found = targets("cat > a.md <<E\ntext\nE\nrm tests/a.rs\n");
        assert!(
            found.contains(&"tests/a.rs".to_string()),
            "the body ended at E: {found:?}"
        );
    }

    /// `$(...)` runs, so what it writes is written.
    #[test]
    fn a_command_substitution_is_read_as_a_command() {
        let found = targets("echo $(rm tests/a.rs)");
        assert!(found.contains(&"tests/a.rs".to_string()), "{found:?}");
    }

    /// A writer handed a variable cannot be resolved, so every word counts.
    #[test]
    fn an_unresolved_writer_argument_makes_every_word_count() {
        let found = targets("for f in tests/*; do rm \"$f\"; done");
        assert!(
            found.iter().any(|t| t.starts_with("tests/")),
            "`$f` could be any of tests/*: {found:?}"
        );
        let fed = targets("ls tests/ | xargs rm");
        assert!(
            fed.contains(&"tests/".to_string()),
            "xargs feeds rm from stdin: {fed:?}"
        );
    }

    /// A program's ordinary arguments are not targets, only its quoted code.
    #[test]
    fn an_interpreters_arguments_are_not_what_it_writes() {
        assert!(
            targets("python3 -m pytest tests/").is_empty(),
            "running the tests is the job"
        );
        assert_eq!(
            targets(r#"python3 -c "open('tests/a.rs','w')""#),
            vec!["tests/a.rs".to_string(), "w".to_string()],
            "quoted literals in inline code are scanned"
        );
    }

    #[test]
    fn in_place_flags_are_recognised_in_every_spelling() {
        for flag in ["-i", "-i.bak", "-pi", "-ie", "--in-place", "-ni"] {
            assert!(is_in_place_flag(flag), "{flag} edits in place");
        }
        for flag in ["-n", "-e", "-E", "--expression"] {
            assert!(!is_in_place_flag(flag), "{flag} does not");
        }
    }

    /// `git checkout main` names no file; `git -C x checkout -- f` does.
    #[test]
    fn git_writes_only_through_its_file_subcommands() {
        assert!(targets("git checkout main").contains(&"main".to_string()));
        assert!(targets("git -C repo checkout -- tests/a.rs").contains(&"tests/a.rs".to_string()));
        assert!(
            targets("git log -- tests/a.rs").is_empty(),
            "log only reads history"
        );
    }

    /// `find -exec` is a writer only when what it runs is.
    #[test]
    fn find_writes_only_when_it_deletes_or_runs_a_writer() {
        assert!(targets("find tests -name '*.rs' -exec grep -l x {} +").is_empty());
        assert!(targets(r"find tests -exec rm {} \;").contains(&"tests".to_string()));
        assert!(targets("find tests -delete").contains(&"tests".to_string()));
    }

    /// A comment is not run.
    #[test]
    fn a_comment_writes_nothing() {
        assert!(targets("ls # rm tests/a.rs").is_empty());
    }
}
