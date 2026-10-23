//! The guard's reading of every command in [`crate::parser_pins`], as it was
//! recorded before the lexer was restructured: the write targets, and the
//! tokens, heredoc bodies and substitutions of both `$'` readings.

use super::{lex, write_targets, Lexed, Token};
use crate::parser_pins::{assert_pinned, esc, list, record};

const RECORDING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/pinned/guards_shell.txt"
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
