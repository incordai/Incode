//! The rules' reading of every command in [`crate::parser_pins`], as it was
//! recorded before the splitter was restructured: the segments, the words
//! and the unquoted line, and the verdict of two rule sets.

use std::path::{Path, PathBuf};

use super::{
    split_command, unquote, words, Layer, PathContext, PermissionRules, RuleLists, Subject,
};
use crate::parser_pins::{assert_pinned, esc, list, record};

const RECORDING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/pinned/permissions_rules.txt"
));

fn rules(allow: &[&str], ask: &[&str], deny: &[&str]) -> PermissionRules {
    let owned = |texts: &[&str]| texts.iter().map(|s| (*s).to_string()).collect();
    let lists = RuleLists {
        allow: owned(allow),
        ask: owned(ask),
        deny: owned(deny),
    };
    let mut rules = PermissionRules::none();
    rules.add(&lists, Layer::User, Some(Path::new("/home/me")));
    rules
}

fn verdict(rules: &PermissionRules, command: &str) -> String {
    let ctx = PathContext {
        workspace: PathBuf::from("/ws"),
        home: Some(PathBuf::from("/home/me")),
    };
    rules
        .decide("Bash", &Subject::Command(command.to_string()), &ctx)
        .map_or_else(|| "none".to_string(), |v| v.describe())
}

fn reading(narrow: &PermissionRules, broad: &PermissionRules, command: &str) -> String {
    let segments: Vec<String> = split_command(command)
        .iter()
        .map(|s| {
            format!(
                "{} {} {}",
                esc(&s.text),
                if s.opaque { "opaque" } else { "clear" },
                list(&s.peeled)
            )
        })
        .collect();
    format!(
        "{} => [{}] | words {} | unquote {} | {} | {}",
        esc(command),
        segments.join("; "),
        list(&words(command)),
        esc(&unquote(command)),
        verdict(narrow, command),
        verdict(broad, command)
    )
}

/// **NO RESTRUCTURING CHANGES A VERDICT.** Every command of the corpus is
/// split, peeled and decided exactly as it was when the recording was made.
#[test]
fn every_pinned_command_is_split_and_decided_as_recorded() {
    let narrow = rules(
        &[
            "Bash(echo:*)",
            "Bash(ls:*)",
            "Bash(cat:*)",
            "Bash(npm test:*)",
            "Bash(git status)",
            "Bash(git * --dry-run)",
        ],
        &["Bash(git push:*)"],
        &["Bash(rm:*)", "Bash(curl:*)"],
    );
    let broad = rules(&["Bash"], &[], &["Bash(rm -rf:*)", "Bash(bash:*)"]);
    assert_pinned(
        RECORDING,
        &record(|command| reading(&narrow, &broad, command)),
    );
}
