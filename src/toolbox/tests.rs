use super::*;

fn toolbox(root: &Path) -> Toolbox {
    Toolbox::new(root, true)
}

#[test]
fn every_advertised_tool_can_actually_be_run() {
    // THE INVARIANT, in the direction that costs a model a wasted turn: a
    // schema whose name reaches no implementation answers "there is no tool
    // called X" to a model that was told there was.
    let dir = tempfile::tempdir().expect("tmp");
    let box_ = toolbox(dir.path());
    for name in box_.names() {
        let answer = box_.run(&name, &json!({}));
        // The call is expected to FAIL here — it is given no arguments. What
        // must never happen is failing because nothing is wired to the name.
        if let Err(e) = answer {
            assert!(
                !e.contains("there is no tool called"),
                "advertised but not wired: {name} -> {e}"
            );
        }
    }
}

#[test]
fn an_unknown_tool_is_an_error_the_model_can_read() {
    let dir = tempfile::tempdir().expect("tmp");
    let answer = toolbox(dir.path()).run("Teleport", &json!({}));
    let message = answer.expect_err("an unknown tool cannot succeed");
    assert!(message.contains("Teleport"), "{message}");
}

#[test]
fn the_control_for_that_invariant_works() {
    // Proves the check above can fail at all: an unwired name really does
    // produce the string the assertion looks for. Without this the loop
    // could be passing because the message changed.
    let dir = tempfile::tempdir().expect("tmp");
    let e = toolbox(dir.path())
        .run("NotATool", &json!({}))
        .expect_err("must fail");
    assert!(e.contains("there is no tool called"), "{e}");
}

#[test]
fn every_schema_is_shaped_the_way_a_model_expects() {
    let dir = tempfile::tempdir().expect("tmp");
    for def in toolbox(dir.path()).definitions() {
        let name = def.get("name").and_then(Value::as_str);
        assert!(name.is_some_and(|n| !n.is_empty()), "unnamed tool: {def}");
        let described = def
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|d| d.len() > 20);
        assert!(described, "{name:?} needs a description a model can act on");
        let schema = def.get("input_schema").and_then(|s| s.get("type"));
        assert_eq!(
            schema.and_then(Value::as_str),
            Some("object"),
            "{name:?} input_schema"
        );
    }
}

#[test]
fn no_tool_is_advertised_twice() {
    // Two entries under one name means the second is unreachable, and which
    // one a provider honours is not ours to decide.
    let dir = tempfile::tempdir().expect("tmp");
    let mut names = toolbox(dir.path()).names();
    let before = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), before, "duplicated: {names:?}");
}

#[test]
fn the_file_schema_names_the_field_the_implementation_reads() {
    // A schema saying `file_path` over an implementation reading `path`
    // fails on every single call, and nothing in either half would show it:
    // both are correct alone. This is the join, which is what was missing.
    let dir = tempfile::tempdir().expect("tmp");
    let defs = toolbox(dir.path()).definitions();
    let read = defs
        .iter()
        .find(|d| d["name"] == READ)
        .expect("Read is offered");
    let props = &read["input_schema"]["properties"];
    assert!(
        props.get("path").is_some(),
        "the implementation reads `path`: {props}"
    );
    assert!(
        props.get("file_path").is_none(),
        "the implementation does NOT read `file_path`: {props}"
    );
}

/// **THE BASH TOOL NAMES THE SHELL ITS COMMANDS RUN IN**, as offered — the
/// one this process chose for this session's sandbox — so the model writes
/// that shell's syntax rather than guessing.
#[test]
fn the_bash_description_names_the_shell_in_use() {
    let dir = tempfile::tempdir().expect("tmp");
    let defs = toolbox(dir.path()).definitions();
    let bash = defs
        .iter()
        .find(|d| d["name"] == BASH)
        .expect("Bash is offered");
    let text = bash["description"].as_str().expect("a description");
    let shell = crate::tools::shell_choice::describe(&crate::sandbox::Sandbox::off());
    assert!(text.ends_with(&shell), "{text}");
    assert!(shell.starts_with("Commands run in "), "{shell}");
    #[cfg(windows)]
    assert!(!text.contains("cmd"), "{text}");
}

#[test]
fn reading_a_file_works_through_the_toolbox() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("hello.txt"), "first\nsecond\n").expect("write");
    let out = toolbox(dir.path())
        .run(READ, &json!({"path": "hello.txt"}))
        .expect("read");
    assert!(out.contains("first"), "{out}");
    assert!(out.contains("second"), "{out}");
}

#[test]
fn writing_then_reading_returns_what_was_written() {
    // The round trip, not a return code: a write that reports success and
    // leaves nothing on disk is the failure worth catching.
    let dir = tempfile::tempdir().expect("tmp");
    let box_ = toolbox(dir.path());
    box_.run(
        WRITE,
        &json!({"path": "new.txt", "content": "the contents"}),
    )
    .expect("write");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.txt")).expect("on disk"),
        "the contents",
        "the file must actually hold it"
    );
    let back = box_.run(READ, &json!({"path": "new.txt"})).expect("read");
    assert!(back.contains("the contents"), "{back}");
}

#[test]
fn editing_changes_the_file_on_disk() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("e.txt"), "keep OLD keep").expect("write");
    toolbox(dir.path())
        .run(
            EDIT,
            &json!({"path": "e.txt", "old_string": "OLD", "new_string": "NEW"}),
        )
        .expect("edit");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("e.txt")).expect("read"),
        "keep NEW keep"
    );
}

#[test]
fn a_failing_call_reports_the_failure_rather_than_inventing_a_result() {
    // A model told a read succeeded when the file is missing will build on
    // contents that were never there.
    let dir = tempfile::tempdir().expect("tmp");
    let answer = toolbox(dir.path()).run(READ, &json!({"path": "absent.txt"}));
    assert!(answer.is_err(), "got: {answer:?}");
}

#[test]
fn searching_finds_a_file_that_is_there_and_not_one_that_is_not() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("found.rs"), "fn marker() {}").expect("write");
    let box_ = toolbox(dir.path());
    let hits = box_
        .run(GLOB, &json!({"pattern": "**/*.rs"}))
        .expect("glob");
    assert!(hits.contains("found.rs"), "{hits}");
    let none = box_
        .run(GLOB, &json!({"pattern": "**/*.nope"}))
        .expect("glob");
    assert!(!none.contains("found.rs"), "control: {none}");
}

#[test]
fn the_gate_that_demands_a_brief_offers_the_tool_that_saves_one() {
    // Watched live: the intake gate tells the model to "Save each part with
    // save_brief" while that tool was offered to nobody. An instruction to
    // call a tool that does not exist is a loop with no way out of it.
    let dir = tempfile::tempdir().expect("tmp");
    let names = toolbox(dir.path()).names();
    assert!(
        names.iter().any(|n| n == crate::brief::SAVE_TOOL),
        "the gate's own tool is missing: {names:?}"
    );
}

#[test]
fn asking_for_a_skill_without_naming_one_costs_no_request() {
    // An unnamed ask cannot match anything, so going to the store for it
    // spends a request and a four-second timeout to answer a question that
    // was already answered.
    // Blank, not missing: a missing `name` is now refused by the schema check
    // before the tool is reached; a blank one passes it and must still be
    // caught here.
    let dir = tempfile::tempdir().expect("tmp");
    let started = std::time::Instant::now();
    let refused = toolbox(dir.path())
        .run(SKILL, &json!({"name": "  "}))
        .expect_err("nothing to look up");
    assert!(refused.contains("say which skill"), "{refused}");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "it went to the store anyway: {:?}",
        started.elapsed()
    );
}

#[test]
fn delegation_stops_at_the_depth_limit() {
    // A subagent that can spawn subagents that can spawn subagents is a bill
    // with no ceiling. The refusal happens at the tool, because that is where
    // the new agent would be made.
    let dir = tempfile::tempdir().expect("tmp");
    let deep = Toolbox::new(dir.path(), true).at_depth(subagent::MAX_DEPTH);
    let refused = deep
        .run(AGENT, &json!({"task": "go and do a thing"}))
        .expect_err("past the limit");
    assert!(refused.contains("agents deep"), "{refused}");
    assert!(
        deep.children().is_empty(),
        "and nothing was spawned to record"
    );
}

#[test]
fn a_subagent_with_no_task_is_refused_before_anything_is_spawned() {
    // An empty task cannot be done, and finding that out by paying for a
    // conversation is the expensive way to learn it.
    let dir = tempfile::tempdir().expect("tmp");
    let box_ = toolbox(dir.path());
    let refused = box_
        .run(AGENT, &json!({"task": "   "}))
        .expect_err("nothing to do");
    assert!(refused.contains("needs a task"), "{refused}");
    assert!(box_.children().is_empty(), "nothing was spawned");
}

#[test]
fn the_control_for_the_depth_limit_works() {
    // Proves the limit is what refused, not the task check above it: the
    // SAME call one level shallower gets past the depth gate and fails
    // later, on something else.
    let dir = tempfile::tempdir().expect("tmp");
    let shallow = Toolbox::new(dir.path(), true).at_depth(subagent::MAX_DEPTH - 1);
    let refused = shallow
        .run(AGENT, &json!({"task": "go and do a thing"}))
        .expect_err("no endpoint is configured in a test");
    assert!(
        !refused.contains("agents deep"),
        "the depth gate fired at the wrong depth: {refused}"
    );
}

#[test]
fn a_write_to_a_protected_path_is_refused() {
    // An agent CLI enforces this itself. Over an API these functions ARE
    // the write, so unenforced here means unenforced entirely.
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
    let box_ = toolbox(dir.path());
    let refused = box_
        .run(
            WRITE,
            &json!({"path": "tests/it.rs", "content": "assert!(true)"}),
        )
        .expect_err("a protected path must be refused");
    assert!(refused.contains("protected"), "{refused}");
    // BEFORE the write, not after: the file must not exist at all.
    assert!(
        !dir.path().join("tests/it.rs").exists(),
        "the refusal came too late — the file was written"
    );
    // The control: an ordinary path in the same workspace still works once
    // the spec set exists, or this proves only that writing is broken.
    for f in &crate::spec::SPEC_SET {
        let full = dir.path().join(f.path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, crate::spec::complete_text(f)).unwrap();
    }
    std::fs::create_dir_all(dir.path().join("src")).expect("dir");
    box_.run(
        WRITE,
        &json!({"path": "src/it.rs", "content": "fn main() {}"}),
    )
    .expect("an unprotected path is fine");
    assert!(dir.path().join("src/it.rs").exists());
}

#[test]
fn writing_source_without_spec_is_refused_by_toolbox() {
    let dir = tempfile::tempdir().expect("tmp");
    let box_ = toolbox(dir.path());
    let refused = box_
        .run(
            WRITE,
            &json!({"path": "src/it.rs", "content": "fn main() {}"}),
        )
        .expect_err("source write without spec set must be refused");
    assert!(refused.contains("not specified yet"), "{refused}");
}

#[test]
fn editing_a_protected_path_is_refused_too() {
    // Write and Edit both change a file; guarding one and not the other
    // leaves the door open beside the locked one.
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
    std::fs::write(dir.path().join("tests/it.rs"), "OLD").expect("write");
    let refused = toolbox(dir.path())
        .run(
            EDIT,
            &json!({"path": "tests/it.rs", "old_string": "OLD", "new_string": "NEW"}),
        )
        .expect_err("refused");
    assert!(refused.contains("protected"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tests/it.rs")).expect("read"),
        "OLD",
        "the file is untouched"
    );
}

#[test]
fn reading_a_protected_path_is_still_allowed() {
    // The guards protect against writes. An agent that cannot READ the
    // check it must satisfy cannot satisfy it.
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
    std::fs::write(dir.path().join("tests/it.rs"), "the check").expect("write");
    let out = toolbox(dir.path())
        .run(READ, &json!({"path": "tests/it.rs"}))
        .expect("reading is not writing");
    assert!(out.contains("the check"), "{out}");
}

#[test]
fn unreadable_guards_stop_writes_rather_than_permitting_them() {
    // The failure the guards module calls the worst one: protections that
    // quietly stop applying while everything keeps working.
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join(".incode")).expect("dir");
    std::fs::write(dir.path().join(crate::guards::GUARDS_FILE), "{ not json").expect("write");
    let refused = toolbox(dir.path())
        .run(WRITE, &json!({"path": "anything.txt", "content": "x"}))
        .expect_err("a broken guards file must not read as no guards");
    assert!(refused.contains("could not be read"), "{refused}");
    assert!(
        !dir.path().join("anything.txt").exists(),
        "and nothing was written"
    );
}

#[test]
fn the_shell_tool_runs_a_command_and_returns_its_output() {
    let dir = tempfile::tempdir().expect("tmp");
    let out = toolbox(dir.path())
        .run(BASH, &json!({"command": "echo toolbox_probe"}))
        .expect("bash");
    assert!(out.contains("toolbox_probe"), "{out}");
}

#[test]
fn the_orphaned_tools_are_all_reachable_again() {
    // Named individually rather than counted: a count passes while the
    // wrong six are present.
    let dir = tempfile::tempdir().expect("tmp");
    let names = toolbox(dir.path()).names();
    for expected in [
        READ,
        WRITE,
        EDIT,
        GLOB,
        GREP,
        BASH,
        crate::verify::VERIFY_TOOL,
        crate::recall::INDEX_TOOL,
        crate::recall::GET_TOOL,
        crate::knowledge::REMEMBER_TOOL,
        crate::knowledge::SEARCH_TOOL,
        crate::knowledge::FORGET_TOOL,
        SKILL,
        AGENT,
        crate::brief::SAVE_TOOL,
        crate::knowledge::SYMBOL_TOOL,
        crate::web::FETCH_TOOL,
        crate::web::SEARCH_TOOL,
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} is not offered; have {names:?}"
        );
    }
}

// -----------------------------------------------------------------------
// The memory engine's gate: pre-tool, the ask, and questions.
// -----------------------------------------------------------------------

use crate::hooks::engine::fake::{self, Reply};
use crate::hooks::engine::{Ask, Prompter, Verdict};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ASK_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"git push needs the owner"}}"#;
const ALLOW_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;
const DENY_JSON: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"no"}}}"#;

/// A toolbox over `root` whose engine is the fake in `engine`.
fn guarded(root: &Path, engine: &Path, replies: &[(&str, Reply)], cap: u64) -> Toolbox {
    let bin = fake::write(engine, replies);
    let mut toolbox = Toolbox::new(root, true)
        .with_engine(Some(fake::engine(bin).capped(Duration::from_secs(cap))));
    toolbox.set_session("api-test-session");
    toolbox
}

/// Someone at the session, scripted.
#[derive(Default)]
struct Person {
    /// What they answer to a confirmation; `None` waits for the answer to
    /// come from elsewhere instead.
    confirm: Option<Verdict>,
    choose: Option<Vec<crate::ask::Answer>>,
    asked: AtomicUsize,
    chose: AtomicUsize,
}

impl Prompter for Person {
    fn confirm(&self, _ask: &Ask<'_>, settled: &dyn Fn() -> bool) -> Option<Verdict> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if self.confirm.is_some() {
            return self.confirm.clone();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while !settled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn choose(&self, _questions: &[crate::ask::Question]) -> Option<Vec<crate::ask::Answer>> {
        self.chose.fetch_add(1, Ordering::SeqCst);
        self.choose.clone()
    }
}

/// §3 test 1. **EXIT 2 IS A REFUSAL, AND NOTHING IS WRITTEN.**
#[test]
fn a_blocked_write_is_not_made_and_the_model_is_told_why() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    std::fs::write(ws.path().join("notes.txt"), "before").expect("seed");
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(2).stderr("no backup of notes.txt"))],
        30,
    );
    let refused = toolbox
        .run(WRITE, &json!({"path": "notes.txt", "content": "after"}))
        .expect_err("a block is an error");
    assert!(refused.contains("no backup"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("notes.txt")).expect("read"),
        "before",
        "the file is unchanged on disk"
    );
}

/// §3 test 2. **SILENCE IS AN ALLOW.**
#[test]
fn an_engine_that_says_nothing_lets_the_tool_run() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(0))],
        30,
    );
    toolbox
        .run(WRITE, &json!({"path": "made.txt", "content": "made"}))
        .expect("allowed");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("made.txt")).expect("the tool ran"),
        "made"
    );
    assert_eq!(fake::calls(engine.path(), "pre-tool"), 1);
}

/// The rules the engine attaches to an allowed action reach the model,
/// with the result it acts on.
#[test]
fn the_engines_guidance_travels_with_the_result() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    std::fs::write(ws.path().join("r.txt"), "the contents").expect("seed");
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[(
            "pre-tool",
            Reply::exit(0).stdout(
                r#"{"hookSpecificOutput":{"permissionDecision":"allow","additionalContext":"RULE-7: read before you write"}}"#,
            ),
        )],
        30,
    );
    let said = toolbox.run(READ, &json!({"path": "r.txt"})).expect("read");
    assert!(said.contains("the contents"), "{said}");
    assert!(said.contains("RULE-7"), "{said}");
}

/// §3 test 3 and §6's unattended case. **AN ASK WITH NOBODY TO ASK IS A
/// REFUSAL**, when the owner's app does not answer either — and the
/// engine's reason reaches the model.
#[test]
fn an_ask_nobody_answers_is_refused_with_the_reason() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[
            ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
            ("permission-request", Reply::exit(0)),
        ],
        30,
    );
    let refused = toolbox
        .run(WRITE, &json!({"path": "asked.txt", "content": "x"}))
        .expect_err("nobody confirmed it");
    assert!(refused.contains("git push needs the owner"), "{refused}");
    assert!(
        !ws.path().join("asked.txt").exists(),
        "the tool did not run"
    );
    assert_eq!(fake::calls(engine.path(), "permission-request"), 1);
}

/// §3 test 4. **A CALL THE OPERATOR'S GUARDS REFUSE NEVER STARTS THE
/// ENGINE.**
#[test]
fn a_native_refusal_never_spawns_the_engine() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    std::fs::create_dir_all(ws.path().join("tests")).expect("dir");
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(0))],
        30,
    );
    let refused = toolbox
        .run(WRITE, &json!({"path": "tests/it.rs", "content": "x"}))
        .expect_err("protected");
    assert!(refused.contains("protected"), "{refused}");
    assert_eq!(
        fake::calls(engine.path(), "pre-tool"),
        0,
        "the engine was spawned for a call already refused"
    );
}

/// The owner's failure policy: an engine that cannot decide stops a
/// change, and lets a read through.
#[test]
fn an_engine_that_cannot_decide_stops_writes_and_lets_reads_through() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    std::fs::write(ws.path().join("r.txt"), "readable").expect("seed");
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(1))],
        30,
    );
    let refused = toolbox
        .run(BASH, &json!({"command": "echo should-not-run > ran.txt"}))
        .expect_err("fail closed");
    assert!(refused.contains("could not decide"), "{refused}");
    assert!(!ws.path().join("ran.txt").exists());
    let read = toolbox
        .run(READ, &json!({"path": "r.txt"}))
        .expect("fail open");
    assert!(read.contains("readable"), "{read}");
}

/// The same policy for an engine that runs past its bound: killed, and
/// the write refused.
#[test]
fn an_engine_past_its_bound_stops_the_write() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(0).after(20))],
        2,
    );
    let started = Instant::now();
    let refused = toolbox
        .run(WRITE, &json!({"path": "late.txt", "content": "x"}))
        .expect_err("timed out");
    assert!(refused.contains("timed out"), "{refused}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert!(!ws.path().join("late.txt").exists());
}

/// A named engine that is not there is the same failure, not a quiet
/// "no guard".
#[test]
fn a_named_engine_that_is_missing_stops_the_write() {
    let ws = tempfile::tempdir().expect("ws");
    let toolbox = Toolbox::new(ws.path(), true)
        .with_engine(Some(fake::engine(ws.path().join("no-such-engine.exe"))));
    let refused = toolbox
        .run(WRITE, &json!({"path": "x.txt", "content": "x"}))
        .expect_err("missing engine");
    assert!(refused.contains("could not start"), "{refused}");
    assert!(!ws.path().join("x.txt").exists());
}

/// §6 test 1. **THE OWNER'S APP ANSWERS FIRST**: allowed there, the tool
/// runs, and the person at the session did not have to answer.
#[test]
fn an_allow_from_the_owners_app_runs_the_tool_with_no_local_answer() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let mut toolbox = guarded(
        ws.path(),
        engine.path(),
        &[
            ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
            ("permission-request", Reply::exit(0).stdout(ALLOW_JSON)),
        ],
        30,
    );
    let person = Arc::new(Person::default());
    toolbox.set_prompter(person.clone());
    toolbox
        .run(WRITE, &json!({"path": "ok.txt", "content": "ok"}))
        .expect("allowed in the app");
    assert!(ws.path().join("ok.txt").exists());
    assert_eq!(
        person.asked.load(Ordering::SeqCst),
        1,
        "the prompt was shown"
    );
}

/// §6 test 2. A refusal in the app is the model's error, in the app's
/// words.
#[test]
fn a_deny_from_the_owners_app_is_the_error() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[
            ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
            ("permission-request", Reply::exit(0).stdout(DENY_JSON)),
        ],
        30,
    );
    let refused = toolbox
        .run(WRITE, &json!({"path": "no.txt", "content": "x"}))
        .expect_err("denied");
    assert_eq!(refused, "no");
    assert!(!ws.path().join("no.txt").exists());
}

/// §6 test 4. **THE PERMISSION EVENT NAMES THE SAME CALL**: its
/// `tool_input` is byte-identical to the pre-tool event's, which is how the
/// engine finds the card the pre-tool raised.
#[test]
fn the_permission_event_carries_the_pre_tool_events_input() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[
            ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
            ("permission-request", Reply::exit(0).stdout(DENY_JSON)),
        ],
        30,
    );
    let _ = toolbox.run(
        EDIT,
        &json!({"path": "e.txt", "old_string": "a", "new_string": "b"}),
    );
    let pre = fake::events(engine.path(), "pre-tool");
    let permission = fake::events(engine.path(), "permission-request");
    assert_eq!(pre.len(), 1);
    assert_eq!(permission.len(), 1);
    assert_eq!(permission[0]["hook_event_name"], "PermissionRequest");
    assert_eq!(
        pre[0]["tool_input"].to_string(),
        permission[0]["tool_input"].to_string()
    );
    // And the file is named absolutely, since the engine keys by path.
    let named = pre[0]["tool_input"]["path"].as_str().expect("a path");
    assert!(Path::new(named).is_absolute(), "{named}");
}

/// The race, the terminal's way round: the person answers first, and the
/// hook still waiting on the owner's app is stopped rather than left to
/// run out its ten minutes.
#[test]
fn a_local_answer_wins_and_stops_the_hook() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let mut toolbox = guarded(
        ws.path(),
        engine.path(),
        &[
            ("pre-tool", Reply::exit(0).stdout(ASK_JSON)),
            (
                "permission-request",
                Reply::exit(0).stdout(DENY_JSON).after(20),
            ),
        ],
        60,
    );
    toolbox.set_prompter(Arc::new(Person {
        confirm: Some(Verdict::Allow),
        ..Person::default()
    }));
    let started = Instant::now();
    toolbox
        .run(WRITE, &json!({"path": "mine.txt", "content": "x"}))
        .expect("allowed here");
    assert!(ws.path().join("mine.txt").exists());
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "waited on the losing hook: {:?}",
        started.elapsed()
    );
}

/// §4 test 4. **THE EXACT NAME**: the engine recognises a question tool by
/// it, so a rename turns routing off without a sound.
#[test]
fn the_question_tool_is_offered_under_its_exact_name() {
    assert_eq!(ASK_USER_QUESTION, "AskUserQuestion");
    let dir = tempfile::tempdir().expect("tmp");
    assert!(toolbox(dir.path())
        .names()
        .iter()
        .any(|n| n == "AskUserQuestion"));
}

fn database_question() -> Value {
    json!({"questions": [{
        "question": "Which database should this use?",
        "header": "Database",
        "options": [
            {"label": "Postgres", "description": "relational; the document implies joins"},
            {"label": "SQLite", "description": "one file, nothing to operate"}
        ],
        "multiSelect": false
    }]})
}

/// §4 test 1. **AN ANSWER GIVEN IN THE APP IS THE RESULT, NOT AN ERROR**,
/// and nobody is asked again here.
#[test]
fn a_question_answered_in_the_app_returns_the_answer() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let answered = "The owner answered your question: Postgres\nContinue with that answer — do not ask it again.";
    let mut toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(2).stderr(answered))],
        30,
    );
    let person = Arc::new(Person::default());
    toolbox.set_prompter(person.clone());
    let said = toolbox
        .run(ASK_USER_QUESTION, &database_question())
        .expect("an answer is not an error");
    assert_eq!(said, answered);
    assert_eq!(person.chose.load(Ordering::SeqCst), 0, "no local prompt");
}

/// §4 test 2. Not answered in the app, and nobody here who can answer —
/// the ACP client, which has no question request, or a run with nobody at
/// it: the model is told so, with the options.
#[test]
fn a_question_nobody_can_answer_says_so_with_its_options() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let mut toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(0))],
        30,
    );
    toolbox.set_prompter(Arc::new(Person::default()));
    let said = toolbox
        .run(ASK_USER_QUESTION, &database_question())
        .expect("a note, not an error");
    assert!(said.contains("nobody is at the prompt"), "{said}");
    assert!(
        said.contains("Postgres") && said.contains("SQLite"),
        "{said}"
    );
}

/// Answered at the terminal: the pick goes back as the result.
#[test]
fn a_question_answered_here_returns_the_pick() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let mut toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(0))],
        30,
    );
    toolbox.set_prompter(Arc::new(Person {
        choose: Some(vec![crate::ask::Answer::Chose("SQLite".to_string())]),
        ..Person::default()
    }));
    let said = toolbox
        .run(ASK_USER_QUESTION, &database_question())
        .expect("answered");
    assert!(said.contains("A: SQLite"), "{said}");
}

/// §4 test 3. The labels reach the engine unchanged: it turns them into
/// the buttons in the owner's app.
#[test]
fn the_questions_labels_reach_the_engine_unchanged() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let toolbox = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(0))],
        30,
    );
    let _ = toolbox.run(ASK_USER_QUESTION, &database_question());
    let events = fake::events(engine.path(), "pre-tool");
    assert_eq!(events[0]["tool_name"], "AskUserQuestion");
    assert_eq!(
        events[0]["tool_input"]["questions"][0]["options"],
        database_question()["questions"][0]["options"]
    );
}

/// **READ-ONLY IS DECIDED HERE.** A write is refused and nothing lands; a
/// read still runs. The control is the same write with no mode set.
#[test]
fn a_read_only_session_refuses_changes_and_allows_reads() {
    use crate::permissions::PermissionMode;
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("r.txt"), "readable").expect("seed");
    let mut box_ = toolbox(dir.path());
    box_.set_permission_mode(PermissionMode::ReadOnly);

    let refused = box_
        .run(WRITE, &json!({"path": "w.txt", "content": "x"}))
        .expect_err("read-only refuses a write");
    assert!(refused.contains("read-only"), "{refused}");
    assert!(!dir.path().join("w.txt").exists(), "nothing was written");
    let refused = box_
        .run(BASH, &json!({"command": "echo ran > ran.txt"}))
        .expect_err("read-only refuses the shell");
    assert!(refused.contains("read-only"), "{refused}");
    assert!(!dir.path().join("ran.txt").exists());
    assert!(box_
        .run(READ, &json!({"path": "r.txt"}))
        .expect("reading is not changing")
        .contains("readable"));

    // The control: no mode, the same write lands.
    toolbox(dir.path())
        .run(WRITE, &json!({"path": "w.txt", "content": "x"}))
        .expect("no mode, no refusal");
    assert!(dir.path().join("w.txt").exists());
}

/// **`prompt` ASKS THE PERSON**, and their answer decides.
#[test]
fn a_prompt_session_asks_before_a_write_and_obeys_the_answer() {
    use crate::permissions::PermissionMode;
    let dir = tempfile::tempdir().expect("tmp");

    let yes = Arc::new(Person {
        confirm: Some(Verdict::Allow),
        ..Person::default()
    });
    let mut box_ = toolbox(dir.path());
    box_.set_permission_mode(PermissionMode::Prompt);
    box_.set_prompter(yes.clone());
    box_.run(WRITE, &json!({"path": "yes.txt", "content": "x"}))
        .expect("allowed by the person");
    assert!(dir.path().join("yes.txt").exists());
    assert_eq!(yes.asked.load(Ordering::SeqCst), 1, "asked once");

    let no = Arc::new(Person {
        confirm: Some(Verdict::Deny("not now".to_string())),
        ..Person::default()
    });
    box_.set_prompter(no.clone());
    let refused = box_
        .run(WRITE, &json!({"path": "no.txt", "content": "x"}))
        .expect_err("refused by the person");
    assert_eq!(refused, "not now");
    assert!(!dir.path().join("no.txt").exists());

    // A read is not asked about.
    let _ = box_.run(GLOB, &json!({"pattern": "*.txt"}));
    assert_eq!(
        no.asked.load(Ordering::SeqCst),
        1,
        "only the write was asked"
    );
}

/// A subagent would not carry the mode, so a restricted session keeps
/// the work.
#[test]
fn a_restricted_session_does_not_delegate() {
    use crate::permissions::PermissionMode;
    let dir = tempfile::tempdir().expect("tmp");
    let mut box_ = toolbox(dir.path());
    box_.set_permission_mode(PermissionMode::ReadOnly);
    let refused = box_
        .run(AGENT, &json!({"task": "go and do a thing"}))
        .expect_err("no delegation");
    assert!(refused.contains("subagent"), "{refused}");
    assert!(box_.children().is_empty());
}

#[test]
fn writes_and_edits_via_toolbox_are_snapshotted_and_undoable() {
    let dir = tempfile::tempdir().expect("tmp");
    let ws = dir.path();
    let store = std::sync::Arc::new(std::sync::Mutex::new(
        crate::tools::checkpoint::CheckpointStore::new(),
    ));
    let box_ = toolbox(ws).with_checkpoints(store.clone());

    store.lock().unwrap().begin("create and modify a file");

    box_.run(
        WRITE,
        &json!({"path": "sample.txt", "content": "initial line\n"}),
    )
    .expect("write");
    assert_eq!(
        std::fs::read_to_string(ws.join("sample.txt")).unwrap(),
        "initial line\n"
    );

    box_.run(
        EDIT,
        &json!({
            "path": "sample.txt",
            "old_string": "initial line\n",
            "new_string": "modified line\n"
        }),
    )
    .expect("edit");
    assert_eq!(
        std::fs::read_to_string(ws.join("sample.txt")).unwrap(),
        "modified line\n"
    );

    let outcome = store.lock().unwrap().undo_last(ws).expect("undo");
    assert_eq!(outcome.restored.len(), 1);
    assert!(outcome.failed.is_empty());
    assert!(!ws.join("sample.txt").exists());
}

#[test]
fn pre_existing_file_modified_via_toolbox_is_restored_on_undo() {
    let dir = tempfile::tempdir().expect("tmp");
    let ws = dir.path();
    std::fs::write(ws.join("existing.txt"), "before edit\n").expect("write");

    let store = std::sync::Arc::new(std::sync::Mutex::new(
        crate::tools::checkpoint::CheckpointStore::new(),
    ));
    let box_ = toolbox(ws).with_checkpoints(store.clone());

    store.lock().unwrap().begin("edit existing");

    box_.run(
        EDIT,
        &json!({
            "path": "existing.txt",
            "old_string": "before edit\n",
            "new_string": "after edit\n"
        }),
    )
    .expect("edit");
    assert_eq!(
        std::fs::read_to_string(ws.join("existing.txt")).unwrap(),
        "after edit\n"
    );

    let outcome = store.lock().unwrap().undo_last(ws).expect("undo");
    assert_eq!(outcome.restored.len(), 1);
    assert_eq!(
        std::fs::read_to_string(ws.join("existing.txt")).unwrap(),
        "before edit\n"
    );
}

/// How many whole events the fake was given for `sub`, waiting up to
/// `bound` for `want` — the code-edit hook is not waited on by the
/// toolbox, so it lands a moment after the call returns.
fn calls_within(dir: &Path, sub: &str, want: usize, bound: Duration) -> usize {
    let deadline = Instant::now() + bound;
    loop {
        let whole = fake::events_raw(dir, sub)
            .iter()
            .filter(|line| serde_json::from_str::<Value>(line).is_ok())
            .count();
        if whole >= want || Instant::now() >= deadline {
            return whole;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// **CODE-EDIT FIRES ONLY FOR A WRITE THAT LANDED.** One spawn for a
/// successful Write, carrying the session and the absolute path; none for
/// a Write the guards refused, an Edit that failed, or a Read.
#[test]
fn code_edit_fires_only_for_a_successful_write() {
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    let tools = guarded(
        ws.path(),
        engine.path(),
        &[("pre-tool", Reply::exit(0)), ("code-edit", Reply::exit(0))],
        20,
    );
    tools
        .run(WRITE, &json!({"path": "notes.txt", "content": "hi"}))
        .expect("the write lands");
    assert_eq!(
        calls_within(engine.path(), "code-edit", 1, Duration::from_secs(20)),
        1
    );
    let event = &fake::events(engine.path(), "code-edit")[0];
    assert_eq!(event["hook_event_name"], "PostToolUse");
    assert_eq!(event["session_id"], "api-test-session");
    assert_eq!(event["tool_name"], WRITE);
    assert_eq!(
        event["tool_input"]["path"],
        ws.path().join("notes.txt").to_string_lossy().as_ref()
    );

    // Refused by the guards, failed in the tool, and a read: none of them
    // changed a file, so none of them is news.
    assert!(tools
        .run(WRITE, &json!({"path": "tests/x.txt", "content": "no"}))
        .is_err());
    assert!(tools
        .run(
            EDIT,
            &json!({"path": "notes.txt", "old_string": "absent", "new_string": "x"})
        )
        .is_err());
    tools
        .run(READ, &json!({"path": "notes.txt"}))
        .expect("the read works");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        fake::calls(engine.path(), "code-edit"),
        1,
        "only the write that landed"
    );
}

/// **AN ADDED DIRECTORY IS WRITABLE, AND GUARDED.** A write inside it by
/// absolute path lands there; a protected path inside it is refused as it
/// would be in the workspace, and nothing is written; a directory nobody
/// added stays out of reach.
#[test]
fn an_extra_directory_is_writable_under_the_same_guards() {
    let (ws, extra, elsewhere) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("extra"),
        tempfile::tempdir().expect("elsewhere"),
    );
    let mut tools = toolbox(ws.path());
    tools.set_extra_dirs(vec![extra.path().to_path_buf()]);

    let allowed = extra.path().join("notes").join("plan.txt");
    tools
        .run(
            WRITE,
            &json!({"path": allowed.to_string_lossy(), "content": "added root"}),
        )
        .expect("a write inside an added directory lands");
    assert_eq!(
        std::fs::read_to_string(&allowed).expect("written"),
        "added root"
    );
    let read = tools
        .run(READ, &json!({"path": allowed.to_string_lossy()}))
        .expect("and reads back");
    assert!(read.contains("added root"), "{read}");

    let guarded_path = extra.path().join("tests").join("check.txt");
    let refused = tools
        .run(
            WRITE,
            &json!({"path": guarded_path.to_string_lossy(), "content": "rewritten"}),
        )
        .expect_err("a protected path in an added directory is refused");
    assert!(refused.contains("protected"), "{refused}");
    assert!(!guarded_path.exists(), "nothing was written");

    let outside = elsewhere.path().join("x.txt");
    assert!(tools
        .run(
            WRITE,
            &json!({"path": outside.to_string_lossy(), "content": "no"})
        )
        .is_err());
    assert!(!outside.exists());
}
