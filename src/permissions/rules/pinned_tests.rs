//! The rules' reading of every command in [`crate::parser_pins`], as it was
//! recorded before the splitter was restructured: the segments, the words
//! and the unquoted line, and the verdict of two rule sets.

use std::path::{Path, PathBuf};

use super::{
    split_command, unquote, words, Layer, PathContext, PermissionRules, RuleLists, Subject,
};
use crate::parser_pins::{assert_pinned, digest, esc, list, nesting, record};

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

/// Allow rules for a few commands, an ask, and denies for `rm` and `curl`.
fn narrow() -> PermissionRules {
    rules(
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
    )
}

/// Every command allowed but `rm -rf` and `bash`.
fn broad() -> PermissionRules {
    rules(&["Bash"], &[], &["Bash(rm -rf:*)", "Bash(bash:*)"])
}

/// **NO RESTRUCTURING CHANGES A VERDICT.** Every command of the corpus is
/// split, peeled and decided exactly as it was when the recording was made.
#[test]
fn every_pinned_command_is_split_and_decided_as_recorded() {
    let (narrow, broad) = (narrow(), broad());
    assert_pinned(
        RECORDING,
        &record(|command| reading(&narrow, &broad, command)),
    );
}

const NESTING: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/pinned/permissions_rules_nesting.txt"
));

/// **THE DEPTH LIMIT IS WHERE IT WAS.** Commands that really recurse —
/// quoted `bash -c`, `sh -c`, `eval`, `$(…)`, heredocs fed to `sh` — at depths
/// around the limit and well past it are split and decided as recorded: the
/// count of parts and of opaque ones, a digest of the whole reading, and both
/// verdicts in full.
#[test]
fn every_pinned_nesting_is_split_and_decided_as_recorded() {
    let (narrow, broad) = (narrow(), broad());
    let lines: Vec<String> = nesting()
        .iter()
        .map(|(name, command)| {
            let parts = split_command(command);
            format!(
                "{name} input {} chars {} => parts {} opaque {} reading {} | {} | {}",
                command.chars().count(),
                digest(command),
                parts.len(),
                parts.iter().filter(|s| s.opaque).count(),
                digest(&reading(&narrow, &broad, command)),
                verdict(&narrow, command),
                verdict(&broad, command)
            )
        })
        .collect();
    assert_pinned(NESTING, &lines);
}
