//! The settings' permission rules, applied at the toolbox's one gate.

use super::*;
use crate::hooks::engine::{Ask, Prompter, Verdict};
use crate::permissions::rules::{Layer, PermissionRules, RuleLists};
use crate::permissions::PermissionMode;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Answers every confirmation the same way, and counts them.
struct Answers {
    verdict: Verdict,
    asked: AtomicUsize,
}

impl Prompter for Answers {
    fn confirm(&self, _ask: &Ask<'_>, _settled: &dyn Fn() -> bool) -> Option<Verdict> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Some(self.verdict.clone())
    }
    fn choose(&self, _questions: &[crate::ask::Question]) -> Option<Vec<crate::ask::Answer>> {
        None
    }
}

fn person(verdict: Verdict) -> Arc<Answers> {
    Arc::new(Answers {
        verdict,
        asked: AtomicUsize::new(0),
    })
}

fn with_rules(root: &Path, allow: &[&str], ask: &[&str], deny: &[&str]) -> Toolbox {
    let lists = RuleLists {
        allow: allow.iter().map(|s| (*s).to_string()).collect(),
        ask: ask.iter().map(|s| (*s).to_string()).collect(),
        deny: deny.iter().map(|s| (*s).to_string()).collect(),
    };
    let mut rules = PermissionRules::none();
    rules.add(&lists, Layer::User, None);
    let mut toolbox = Toolbox::new(root, true);
    toolbox.set_permission_rules(rules);
    toolbox
}

/// **A DENY IS FINAL**, whatever the mode — even one that allows everything,
/// and even for a tool that only reads.
#[test]
fn a_deny_rule_refuses_the_call_in_every_mode() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join(".env"), "SECRET=1").expect("seed");
    let mut box_ = with_rules(dir.path(), &["Read"], &[], &["Read(.env)", "Bash(rm:*)"]);
    box_.set_permission_mode(PermissionMode::Allow);
    let refused = box_
        .run(READ, &json!({"path": ".env"}))
        .expect_err("denied");
    assert!(refused.contains("Read(.env)"), "{refused}");
    assert!(!refused.contains("SECRET"), "{refused}");
    let refused = box_
        .run(BASH, &json!({"command": "echo hi && rm -rf src"}))
        .expect_err("denied");
    assert!(refused.contains("Bash(rm:*)"), "{refused}");
}

/// An `ask` rule asks even where the mode would not, and the answer decides.
#[test]
fn an_ask_rule_asks_the_person_in_a_writing_mode() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut box_ = with_rules(dir.path(), &[], &["Write(notes/**)"], &[]);
    box_.set_permission_mode(PermissionMode::WorkspaceWrite);
    let no = person(Verdict::Deny("not there".to_string()));
    box_.set_prompter(no.clone());
    let refused = box_
        .run(WRITE, &json!({"path": "notes/a.txt", "content": "x"}))
        .expect_err("asked and refused");
    assert_eq!(refused, "not there");
    assert_eq!(no.asked.load(Ordering::SeqCst), 1);
    assert!(!dir.path().join("notes/a.txt").exists());
    // A write the rule does not name is not asked about.
    box_.run(WRITE, &json!({"path": "free.txt", "content": "x"}))
        .expect("not asked");
    assert_eq!(no.asked.load(Ordering::SeqCst), 1);
}

#[test]
fn an_ask_rule_with_nobody_to_ask_refuses() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut box_ = with_rules(dir.path(), &[], &["Write"], &[]);
    box_.set_permission_mode(PermissionMode::DangerFullAccess);
    let refused = box_
        .run(WRITE, &json!({"path": "a.txt", "content": "x"}))
        .expect_err("nobody to ask");
    assert!(refused.contains("nobody could be asked"), "{refused}");
}

/// An `allow` rule spares the `prompt` mode's question — for what it names,
/// and only when every part of a command line is named.
#[test]
fn an_allow_rule_runs_without_the_prompt_modes_question() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut box_ = with_rules(dir.path(), &["Write(out/**)", "Bash(echo:*)"], &[], &[]);
    box_.set_permission_mode(PermissionMode::Prompt);
    let no = person(Verdict::Deny("asked".to_string()));
    box_.set_prompter(no.clone());
    box_.run(WRITE, &json!({"path": "out/a.txt", "content": "x"}))
        .expect("allowed by rule");
    assert!(dir.path().join("out/a.txt").exists());
    assert_eq!(no.asked.load(Ordering::SeqCst), 0);
    // Outside what the rule names, the mode still asks.
    let refused = box_
        .run(WRITE, &json!({"path": "b.txt", "content": "x"}))
        .expect_err("asked");
    assert_eq!(refused, "asked");
    // A compound command is not covered by the allowed prefix.
    let refused = box_
        .run(BASH, &json!({"command": "echo ok && touch pwned"}))
        .expect_err("asked");
    assert_eq!(refused, "asked");
    assert!(!dir.path().join("pwned").exists());
}

/// Read-only is a promise that nothing changes; no allow rule breaks it.
#[test]
fn an_allow_rule_does_not_lift_read_only() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut box_ = with_rules(dir.path(), &["Write", "Bash"], &[], &[]);
    box_.set_permission_mode(PermissionMode::ReadOnly);
    let refused = box_
        .run(WRITE, &json!({"path": "a.txt", "content": "x"}))
        .expect_err("read-only");
    assert!(refused.contains("read-only"), "{refused}");
}

/// The toolbox and a session state it shares, held to `allowed` until the
/// returned guard is dropped.
fn restricted(root: &Path, allowed: &[&str]) -> (Toolbox, TurnRestriction) {
    let state = SessionTools::new(true);
    let mut box_ = Toolbox::new(root, true);
    box_.share_session(Arc::clone(&state));
    let texts: Vec<String> = allowed.iter().map(|s| (*s).to_string()).collect();
    let (list, warnings) = crate::permissions::rules::Allowlist::parse(&texts, None);
    assert!(warnings.is_empty(), "{warnings:?}");
    (box_, state.restrict_turn(list))
}

/// **A COMMAND'S `allowed-tools` HOLDS ITS TURN**: what the list covers runs,
/// anything else — another tool, a command line with one part outside the
/// list — is refused with the list named, and nothing it would do happens.
#[test]
fn a_turn_restriction_refuses_what_its_list_does_not_cover() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("a.txt"), "hello").expect("seed");
    let (box_, _held) = restricted(dir.path(), &["Read", "Bash(echo:*)"]);
    assert!(box_
        .run(READ, &json!({"path": "a.txt"}))
        .expect("listed")
        .contains("hello"));
    box_.run(BASH, &json!({"command": "echo ok"}))
        .expect("listed");
    let refused = box_
        .run(WRITE, &json!({"path": "b.txt", "content": "x"}))
        .expect_err("not listed");
    assert!(
        refused.contains("allowed-tools (Read, Bash(echo:*))"),
        "{refused}"
    );
    assert!(!dir.path().join("b.txt").exists());
    let refused = box_
        .run(BASH, &json!({"command": "echo ok && touch pwned"}))
        .expect_err("one part is not listed");
    assert!(refused.contains("allowed-tools"), "{refused}");
    assert!(!dir.path().join("pwned").exists());
}

/// A path pattern in the list narrows a family of tools to those paths.
#[test]
fn a_turn_restriction_narrows_writes_to_its_paths() {
    let dir = tempfile::tempdir().expect("tmp");
    let (box_, _held) = restricted(dir.path(), &["Edit(notes/**)"]);
    box_.run(WRITE, &json!({"path": "notes/a.md", "content": "x"}))
        .expect("inside notes");
    let refused = box_
        .run(WRITE, &json!({"path": "other.md", "content": "x"}))
        .expect_err("outside notes");
    assert!(refused.contains("Edit(notes/**)"), "{refused}");
}

/// **THE NEXT TURN IS NOT HELD TO IT**: once the guard is dropped, the same
/// toolbox runs what it refused.
#[test]
fn a_turn_restriction_is_lifted_when_the_turn_ends() {
    let dir = tempfile::tempdir().expect("tmp");
    let (box_, held) = restricted(dir.path(), &["Read"]);
    box_.run(WRITE, &json!({"path": "b.txt", "content": "x"}))
        .expect_err("restricted");
    drop(held);
    box_.run(WRITE, &json!({"path": "b.txt", "content": "x"}))
        .expect("unrestricted");
    assert!(dir.path().join("b.txt").exists());
}

/// Listed is not allowed: the settings' deny rules still refuse.
#[test]
fn a_turn_restriction_does_not_outrank_a_deny_rule() {
    let dir = tempfile::tempdir().expect("tmp");
    let state = SessionTools::new(true);
    let mut box_ = with_rules(dir.path(), &[], &[], &["Bash(rm:*)"]);
    box_.share_session(Arc::clone(&state));
    let (list, _) = crate::permissions::rules::Allowlist::parse(&["Bash".to_string()], None);
    let _held = state.restrict_turn(list);
    let refused = box_
        .run(BASH, &json!({"command": "rm -r gone"}))
        .expect_err("denied");
    assert!(refused.contains("Bash(rm:*)"), "{refused}");
}
