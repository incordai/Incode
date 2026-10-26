//! `run_bridged` itself, without `run_turn` around it. The whole flow through
//! `run_turn`, with the argv codex is started with, is pinned in
//! `codex/tests.rs` `a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one`;
//! both use its stand-in codex and tool server (`stand_in_bridged_backend`).

use crate::agent::{AgentBackend, AgentEvent};

/// **`run_bridged` RUNS THE TURN IT IS HANDED**: given the bridged command
/// and an open turn as `run_turn` builds them (`codex/mod.rs`), it runs the
/// stand-in's calls in incode's toolbox — one written, one refused by a deny
/// rule — and its line handler turns each codex line into the events
/// asserted here.
#[test]
fn run_bridged_runs_the_turn_it_is_handed() {
    let ws = tempfile::tempdir().expect("ws");
    let bin = tempfile::tempdir().expect("bin");
    let (mut backend, server) =
        super::super::tests::stand_in_bridged_backend(ws.path(), bin.path());
    let turn = crate::tool_bridge::Turn::open(None).expect("the channel opens");
    let command = backend.bridged_command("write two files", &server);

    let mut seen = Vec::new();
    let outcome = backend
        .run_bridged(command, None, turn, &mut |e| seen.push(e.clone()))
        .expect("the turn completes");
    assert!(!outcome.is_error, "{outcome:?}");
    assert_eq!(outcome.session_id, "fake-thread");
    assert_eq!(backend.session_id(), Some("fake-thread"));
    assert_eq!(
        std::fs::read_to_string(ws.path().join("notes.txt")).expect("written by incode"),
        "written"
    );
    assert!(
        !ws.path().join("secret.txt").exists(),
        "the deny rule did not hold"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t == "done")),
        "codex's lines became events: {seen:?}"
    );
}
