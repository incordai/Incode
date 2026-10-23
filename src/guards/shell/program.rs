//! Which program a command runs, and what that program does with its
//! arguments, for the guard: the lists of writers, interpreters and shells,
//! and the flags and subcommands that make a program write.

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
pub(super) const GIT_WRITERS: [&str; 4] = ["checkout", "restore", "rm", "mv"];

/// What a program does with its arguments, for this purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Program {
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

/// The program a command runs, the index of its word, and whether `xargs`
/// stands in front of it.
///
/// Called by [`super::arguments_written`], and by
/// [`super::collect_reading`]'s pipelines to find the one that reads a
/// heredoc.
pub(super) fn program_of(words: &[&str]) -> (Program, usize, bool) {
    let (index, xargs) = program_index(words);
    let program = words
        .get(index)
        .map_or(Program::Other, |word| kind(&program_name(word)));
    (program, index, xargs)
}

/// The index of the program's word, past assignments and wrappers (and the
/// wrappers' flags), and whether one of the wrappers was `xargs`.
///
/// Called by [`program_of`].
fn program_index(words: &[&str]) -> (usize, bool) {
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
    (index, xargs)
}

/// What the program called `name` does with its arguments.
///
/// Called by [`program_of`].
fn kind(name: &str) -> Program {
    if WRITERS.contains(&name) {
        Program::Writer
    } else if name == "sed" {
        Program::InPlaceEditor
    } else if name == "perl" {
        Program::Perl
    } else if INTERPRETERS.contains(&name) || name.starts_with("python") {
        Program::Interpreter
    } else if SHELLS.contains(&name) {
        Program::Shell
    } else if name == "eval" {
        Program::Eval
    } else if name == "git" {
        Program::Git
    } else if name == "find" {
        Program::Find
    } else {
        Program::Other
    }
}

/// `/usr/bin/python3.12` and `python.exe` are both `python3.12`/`python`.
///
/// Called by [`program_index`] and [`program_of`].
fn program_name(word: &str) -> String {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let lower = base.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// `FOO=bar` in front of a command sets a variable; it is not the program.
///
/// Called by [`program_index`].
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// `-i`, `-i.bak`, `-pi`, `-ie`, `--in-place`: an `i` anywhere in a cluster of
/// short flags. Read broadly — `-ni` is in-place too, and a flag misread as
/// in-place costs a refusal where a miss costs the exam.
///
/// Called by [`super::arguments_written`].
pub(super) fn is_in_place_flag(word: &str) -> bool {
    if word.starts_with("--") {
        return word.starts_with("--in-place");
    }
    word.strip_prefix('-')
        .is_some_and(|flags| flags.split('.').next().is_some_and(|f| f.contains('i')))
}

/// The index of `git`'s subcommand among its arguments, past `-C <dir>` and
/// the other global options that take a value.
///
/// Called by [`super::arguments_written`].
pub(super) fn git_subcommand(args: &[&str]) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::{git_subcommand, program_of, Program};

    /// Assignments, wrappers and their flags are not the program; `xargs`
    /// in front of it is remembered.
    #[test]
    fn the_program_is_found_past_assignments_and_wrappers() {
        assert_eq!(
            program_of(&["env", "A=1", "xargs", "-0", "rm"]),
            (Program::Writer, 4, true)
        );
        assert_eq!(
            program_of(&["/usr/bin/Python3.EXE", "-c"]),
            (Program::Interpreter, 0, false)
        );
        assert_eq!(program_of(&["sudo"]), (Program::Other, 1, false));
    }

    #[test]
    fn git_global_options_and_their_values_are_skipped() {
        assert_eq!(
            git_subcommand(&["-C", "repo", "checkout", "--", "f"]),
            Some(2)
        );
        assert_eq!(git_subcommand(&["--version"]), None);
    }
}
