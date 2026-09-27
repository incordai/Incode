//! The tools of an ACP client's MCP servers, run through the toolbox's gate.

use super::*;
use crate::hooks::engine::fake::{self, Reply};
use crate::permissions::PermissionMode;
use std::time::Duration;

/// Session state whose client servers are `specs`, waited for until each has
/// connected or been dropped; returns what was said about them.
fn connect(specs: Vec<ServerSpec>, ws: &Path) -> (Arc<SessionTools>, Vec<String>) {
    let state = SessionTools::new(false);
    let (on, later) = client_servers::tests::collector();
    let mut said = state.connect_client_servers(specs, ws, &on);
    assert!(
        client_servers::tests::wait_connected(&state.client, Duration::from_secs(60)),
        "still connecting after 60s"
    );
    said.extend(later.lock().expect("said").iter().cloned());
    (state, said)
}

/// A toolbox sharing `state`, whose client servers are the fake one.
fn connected(ws: &Path, log: &Path) -> (Arc<SessionTools>, Vec<String>) {
    connect(vec![fake_server("fake", log)], ws)
}

/// **SESSION STATE IS READY AT ONCE; A SLOW SERVER'S TOOL JOINS LATER.**
/// Connecting returns while the server is still listing its tools, and every
/// toolbox sharing the state offers the tool from the first list after it
/// has — which is what the next turn asks for.
#[test]
fn a_slow_servers_tool_is_offered_from_the_next_list_after_it_connects() {
    let ws = tempfile::tempdir().expect("ws");
    let logs = tempfile::tempdir().expect("logs");
    let log = logs.path().join("calls.log");
    let state = SessionTools::new(false);
    let (on, _) = client_servers::tests::collector();
    let began = std::time::Instant::now();
    state.connect_client_servers(
        vec![client_servers::tests::slow_fake_server("slow", &log, 3000)],
        ws.path(),
        &on,
    );
    assert!(
        began.elapsed() < Duration::from_millis(1500),
        "{:?}",
        began.elapsed()
    );
    let mut tools = Toolbox::new(ws.path(), true);
    tools.share_session(Arc::clone(&state));
    assert!(!tools.names().iter().any(|n| n == "mcp__slow__echo"));
    assert_eq!(state.client_servers_connecting(), ["slow"]);
    assert!(client_servers::tests::wait_connected(
        &state.client,
        Duration::from_secs(60)
    ));
    assert!(tools.names().iter().any(|n| n == "mcp__slow__echo"));
    assert!(state.client_servers_connecting().is_empty());
    tools.set_permission_mode(PermissionMode::WorkspaceWrite);
    assert_eq!(
        tools
            .run("mcp__slow__echo", &json!({"text": "late"}))
            .expect("runs"),
        "echo: late"
    );
}

/// **A CLIENT'S MCP TOOL RUNS THROUGH THE ONE GATE.** Offered under its
/// prefix, it runs in a writing session; a call that breaks its own schema,
/// a call in a read-only session and a call in a prompting session with
/// nobody to ask are each refused — and none of them reaches the server.
#[test]
fn a_client_mcp_tool_runs_through_the_gate_and_is_refused_read_only() {
    let ws = tempfile::tempdir().expect("ws");
    let logs = tempfile::tempdir().expect("logs");
    let log = logs.path().join("calls.log");
    let (state, said) = connected(ws.path(), &log);
    let mut tools = Toolbox::new(ws.path(), true);
    tools.share_session(Arc::clone(&state));
    assert!(
        tools.names().iter().any(|n| n == "mcp__fake__echo"),
        "offered: {:?}; connecting said {said:?}",
        tools.names()
    );

    tools.set_permission_mode(PermissionMode::WorkspaceWrite);
    let ran = tools
        .run("mcp__fake__echo", &json!({"text": "hi"}))
        .expect("runs in a writing session");
    assert_eq!(ran, "echo: hi");
    assert_eq!(client_server_calls(&log).len(), 1);

    let malformed = tools
        .run("mcp__fake__echo", &json!({"text": 5}))
        .expect_err("its own schema");
    assert!(
        malformed.contains("does not match its schema"),
        "{malformed}"
    );

    tools.set_permission_mode(PermissionMode::ReadOnly);
    let read_only = tools
        .run("mcp__fake__echo", &json!({"text": "again"}))
        .expect_err("refused read-only");
    assert!(read_only.contains("read-only"), "{read_only}");

    tools.set_permission_mode(PermissionMode::Prompt);
    let unasked = tools
        .run("mcp__fake__echo", &json!({"text": "again"}))
        .expect_err("nobody to ask");
    assert!(unasked.contains("nobody could be asked"), "{unasked}");

    assert_eq!(
        client_server_calls(&log).len(),
        1,
        "a refused call reached the server"
    );
}

/// **FAIL CLOSED.** When the memory engine cannot decide, a client's MCP
/// tool is refused like any other tool that runs something.
#[test]
fn a_client_mcp_tool_is_refused_when_the_engine_cannot_decide() {
    let ws = tempfile::tempdir().expect("ws");
    let logs = tempfile::tempdir().expect("logs");
    let engine = tempfile::tempdir().expect("engine");
    let log = logs.path().join("calls.log");
    let (state, _) = connected(ws.path(), &log);
    let bin = fake::write(engine.path(), &[("pre-tool", Reply::exit(1))]);
    let mut tools = Toolbox::new(ws.path(), true)
        .with_engine(Some(fake::engine(bin).capped(Duration::from_secs(30))));
    tools.share_session(state);
    tools.set_permission_mode(PermissionMode::WorkspaceWrite);
    let refused = tools
        .run("mcp__fake__echo", &json!({"text": "hi"}))
        .expect_err("fail closed");
    assert!(refused.contains("could not decide"), "{refused}");
    assert!(client_server_calls(&log).is_empty());
    assert_eq!(fake::calls(engine.path(), "pre-tool"), 1);
}

/// **AN HTTP SERVER'S TOOL FAILS CLOSED TOO.** Reached over HTTP rather than
/// a pipe, it is the same kind of tool: run in a writing session, refused
/// when the engine cannot decide, and refused read-only — neither refusal
/// reaching the server.
#[test]
fn a_client_http_mcp_tool_is_gated_like_a_stdio_one() {
    let ws = tempfile::tempdir().expect("ws");
    let engine = tempfile::tempdir().expect("engine");
    let server = FakeHttp::start();
    let spec = mcp_http::tests::http_spec("web", &server.url);
    let (state, said) = connect(vec![spec], ws.path());
    let mut open = Toolbox::new(ws.path(), true);
    open.share_session(Arc::clone(&state));
    open.set_permission_mode(PermissionMode::WorkspaceWrite);
    let ran = open
        .run("mcp__web__echo", &json!({"text": "hi"}))
        .unwrap_or_else(|e| panic!("{e}; connecting said {said:?}"));
    assert_eq!(ran, "echo: hi");

    let bin = fake::write(engine.path(), &[("pre-tool", Reply::exit(1))]);
    let mut undecided = Toolbox::new(ws.path(), true)
        .with_engine(Some(fake::engine(bin).capped(Duration::from_secs(30))));
    undecided.share_session(Arc::clone(&state));
    undecided.set_permission_mode(PermissionMode::WorkspaceWrite);
    let refused = undecided
        .run("mcp__web__echo", &json!({"text": "hi"}))
        .expect_err("fail closed");
    assert!(refused.contains("could not decide"), "{refused}");

    open.set_permission_mode(PermissionMode::ReadOnly);
    let read_only = open
        .run("mcp__web__echo", &json!({"text": "hi"}))
        .expect_err("refused read-only");
    assert!(read_only.contains("read-only"), "{read_only}");
    assert_eq!(server.calls(), 1, "a refused call reached the server");
}
