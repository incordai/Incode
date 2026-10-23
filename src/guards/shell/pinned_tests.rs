//! The guard's reading of every command in [`crate::parser_pins`], as it was
//! recorded before the lexer was restructured: the write targets, and the
//! tokens, heredoc bodies and substitutions of both `$'` readings.

use super::{lex, write_targets, Lexed, Token};
use crate::parser_pins::{assert_pinned, digest, esc, list, nesting, record};

const RECORDING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/pinned/guards_shell.txt"
));

const NESTING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/pinned/guards_shell_nesting.txt"
));

fn token(token: &Token) -> String {
    match token {
        Token::Word(word) => esc(word),
        Token::WriteTo => ">".to_string(),
        Token::WriteOrDup => ">&".to_string(),
        Token::ReadFrom => "<".to_string(),
        Token::Heredoc(body) => format!("<<{body}"),
        Token::Pipe => "|".to_string(),
        Token::End => ";".to_string(),
    }
}

fn lexed(lexed: &Lexed) -> String {
    let tokens: Vec<String> = lexed.tokens.iter().map(token).collect();
    format!(
        "tokens [{}] bodies {} subs {}",
        tokens.join(" "),
        list(&lexed.bodies),
        list(&lexed.substitutions)
    )
}

fn reading(command: &str) -> String {
    format!(
        "{} => targets {} | bash {} | sh {}",
        esc(command),
        list(&write_targets(command)),
        lexed(&lex(command, true)),
        lexed(&lex(command, false))
    )
}

/// **NO RESTRUCTURING CHANGES WHAT THE GUARD SEES.** Every command of the
/// corpus is read exactly as it was when the recording was made.
#[test]
fn every_pinned_command_is_read_as_recorded() {
    assert_pinned(RECORDING, &record(reading));
}

/// One nested command, too long to record whole: its size and digest, what
/// it is found to write (and whether the innermost writes are among it),
/// and a digest of both lexer readings.
fn nested_reading(name: &str, command: &str) -> String {
    let targets = write_targets(command);
    let has = |target: &str| targets.iter().any(|t| t == target);
    let readings = format!(
        "{}\n{}",
        lexed(&lex(command, true)),
        lexed(&lex(command, false))
    );
    format!(
        "{name} input {} chars {} => targets {} {} | tests/a {} | tests/b {} | lexed {}",
        command.chars().count(),
        digest(command),
        targets.len(),
        digest(&list(&targets)),
        has("tests/a"),
        has("tests/b"),
        digest(&readings)
    )
}

/// **THE DEPTH LIMIT IS WHERE IT WAS.** Commands that really recurse —
/// quoted `bash -c`, `sh -c`, `eval`, `$(…)`, heredocs fed to `sh` — at depths
/// around the limit and well past it are read exactly as recorded.
#[test]
fn every_pinned_nesting_is_read_as_recorded() {
    let lines: Vec<String> = nesting()
        .iter()
        .map(|(name, command)| nested_reading(name, command))
        .collect();
    assert_pinned(NESTING, &lines);
}
