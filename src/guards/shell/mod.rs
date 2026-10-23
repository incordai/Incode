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

mod lex;
mod program;

use lex::{lex, Lexed, Token};
use program::{git_subcommand, is_in_place_flag, program_of, Program, GIT_WRITERS};

/// Push every target of `command` into `out`. True when one of its writes could
/// not be read from the text, so a command wrapping this one — `bash -c`, a
/// `$(...)` — must count its own words too: `bash -c 'rm "$1"' _ tests/x`
/// names its target OUTSIDE the inner command.
///
/// A command with `$'` is read twice: as bash reads it (ANSI-C quoting) and as
/// a POSIX `sh` such as dash does (a `$` and a plain quote). incode runs
/// `sh -c`, which is dash on Debian and Ubuntu, and the two readings can end
/// the quote in different places; a write either one sees is a target.
/// `permissions::rules::split_into` does the same.
#[must_use]
fn collect(command: &str, depth: usize, out: &mut Vec<String>) -> bool {
    if depth > MAX_DEPTH {
        out.extend(pieces(command));
        return true;
    }
    let mut unresolved = collect_reading(command, depth, out, true);
    if command.contains("$'") {
        unresolved |= collect_reading(command, depth, out, false);
    }
    unresolved
}

/// [`collect`] for one reading: with `ansi_c`, `$'…'` as bash reads it.
///
/// Called by [`collect`], once for each reading.
#[must_use]
fn collect_reading(command: &str, depth: usize, out: &mut Vec<String>, ansi_c: bool) -> bool {
    let lexed = lex(command, ansi_c);
    let mut unresolved = false;
    for inner in &lexed.substitutions {
        unresolved |= collect(inner, depth + 1, out);
    }
    for pipeline in lexed.tokens.split(|t| *t == Token::End) {
        unresolved |= pipeline_targets(pipeline, &lexed.bodies, depth, out);
    }
    if unresolved {
        every_word(&lexed, out);
    }
    unresolved
}

/// Push what one pipeline writes. True when a write cannot be read from the
/// text.
///
/// Called by [`collect_reading`] for each pipeline.
fn pipeline_targets(
    pipeline: &[Token],
    bodies: &[String],
    depth: usize,
    out: &mut Vec<String>,
) -> bool {
    let commands: Vec<&[Token]> = pipeline.split(|t| *t == Token::Pipe).collect();
    // A heredoc anywhere in `cat <<E | sh` is a script, because `sh` reads
    // it; the command it is attached to is not what decides.
    let reader = commands
        .iter()
        .map(|c| program_of(&words_of(c)).0)
        .find(|p| matches!(p, Program::Shell | Program::Interpreter | Program::Perl));
    let mut unresolved = false;
    for tokens in commands {
        unresolved |= redirect_targets(tokens, bodies, reader, depth, out);
        unresolved |= arguments_written(&words_of(tokens), depth, out);
    }
    unresolved
}

/// Push what one command writes through its redirects, and through the
/// heredocs `reader` runs. True when a write cannot be read from the text.
///
/// Called by [`pipeline_targets`] for each command of a pipeline.
fn redirect_targets(
    tokens: &[Token],
    bodies: &[String],
    reader: Option<Program>,
    depth: usize,
    out: &mut Vec<String>,
) -> bool {
    let mut unresolved = false;
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
                unresolved |= heredoc_targets(&bodies[*body], reader, depth, out);
            }
            _ => {}
        }
    }
    unresolved
}

/// Push what a heredoc writes: a script when a shell reads it, the quoted
/// strings of a program an interpreter reads, nothing when it is data.
///
/// Called by [`redirect_targets`].
fn heredoc_targets(
    body: &str,
    reader: Option<Program>,
    depth: usize,
    out: &mut Vec<String>,
) -> bool {
    match reader {
        Some(Program::Shell) => collect(body, depth + 1, out),
        Some(Program::Interpreter | Program::Perl) => {
            out.extend(quoted_literals(body));
            false
        }
        _ => false,
    }
}

/// Every word of a command that could not be read precisely, cut into
/// [`pieces`]: each one counts as a target.
///
/// Called by [`collect_reading`].
fn every_word(lexed: &Lexed, out: &mut Vec<String>) {
    for token in &lexed.tokens {
        if let Token::Word(word) = token {
            out.extend(pieces(word));
        }
    }
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
            // Each reading of `$'` may find it; the caller asks only whether
            // any target is protected.
            assert!(
                targets(command).contains(&"tests/a.rs".to_string()),
                "{command}: {:?}",
                targets(command)
            );
        }
        assert!(
            targets(r"echo $'a > tests/a.rs'").is_empty(),
            "a `>` inside `$'…'` is text"
        );
    }

    /// A target spelled with `$'…'` or `$"…"` is the file bash writes; a NUL
    /// ends it (`> $'a.txt\0junk'` writes `a.txt`).
    #[test]
    fn a_target_in_ansi_c_or_locale_quotes_is_the_file_written() {
        for command in [
            r"echo x > $'tests/a.rs'",
            r"echo x > $'tests\x2fa.rs'",
            r#"echo x > $"tests/a.rs""#,
            r"echo x > $'tests/a.rs\0junk'",
        ] {
            assert!(
                targets(command).contains(&"tests/a.rs".to_string()),
                "{command}: {:?}",
                targets(command)
            );
        }
    }

    /// **TWO SHELLS, TWO READINGS.** dash — what `sh -c` runs on Debian and
    /// Ubuntu — reads `$'` as a `$` and a plain quote, and bash ends `$'\c'`
    /// at its quote. A write hidden from either reading is still seen.
    #[test]
    fn a_redirect_hidden_from_either_reading_of_dollar_quote_is_seen() {
        for command in [
            r"echo $'\' ; echo x > tests/a.rs # '",
            r"echo $'\c' ; echo x > tests/a.rs #'",
            r"echo $'\c\'' ; echo x > tests/a.rs #'",
        ] {
            assert!(
                targets(command).contains(&"tests/a.rs".to_string()),
                "{command}: {:?}",
                targets(command)
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

#[cfg(test)]
mod pinned_tests;
