//! The wire types and the reading of `codex exec --json`, moved here from
//! the tests of `src/agent/codex.rs` when it became a module directory.

use super::*;

fn parse(line: &str) -> Wire {
    serde_json::from_str(line).expect("a wire line")
}

#[test]
fn the_thread_started_line_sets_session_id() {
    let wire =
        parse(r#"{"type":"thread.started","thread_id":"01a081c2-bdb1-7c71-a922-9a10bbb2ccae"}"#);
    match wire {
        Wire::ThreadStarted { thread_id } => {
            assert_eq!(thread_id, "01a081c2-bdb1-7c71-a922-9a10bbb2ccae");
        }
        other => panic!("expected ThreadStarted: {other:?}"),
    }
}

#[test]
fn agent_message_item_is_parsed() {
    let wire = parse(
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"The package name is incord-code."}}"#,
    );
    match wire {
        Wire::ItemCompleted { item } => {
            assert_eq!(item.item_type, "agent_message");
            assert_eq!(
                item.text.as_deref(),
                Some("The package name is incord-code.")
            );
        }
        other => panic!("expected ItemCompleted: {other:?}"),
    }
}

#[test]
fn command_execution_item_is_parsed() {
    let wire = parse(
        r#"{"type":"item.completed","item":{"id":"item_5","type":"command_execution","command":"powershell.exe -Command Get-Location","aggregated_output":"C:\\Projects\\incord-code\n","exit_code":0,"status":"completed"}}"#,
    );
    match wire {
        Wire::ItemCompleted { item } => {
            assert_eq!(item.item_type, "command_execution");
            assert_eq!(
                item.command.as_deref(),
                Some("powershell.exe -Command Get-Location")
            );
            assert_eq!(item.exit_code, Some(0));
        }
        other => panic!("expected ItemCompleted: {other:?}"),
    }
}

#[test]
fn mcp_tool_call_item_is_parsed() {
    let wire = parse(
        r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"memory","tool":"watchman_lookup","arguments":{"task":"C:\\Projects\\incord-code"},"result":{"content":[{"type":"text","text":"{\"hit\":false}"}]},"error":null,"status":"completed"}}"#,
    );
    match wire {
        Wire::ItemCompleted { item } => {
            assert_eq!(item.item_type, "mcp_tool_call");
            assert_eq!(item.tool.as_deref(), Some("watchman_lookup"));
            assert_eq!(extract_mcp_output(&item.result), "{\"hit\":false}");
        }
        other => panic!("expected ItemCompleted: {other:?}"),
    }
}

#[test]
fn turn_completed_usage_is_parsed() {
    let wire = parse(
        r#"{"type":"turn.completed","usage":{"input_tokens":122070,"cached_input_tokens":112128,"cache_write_input_tokens":100,"output_tokens":438,"reasoning_output_tokens":83}}"#,
    );
    match wire {
        Wire::TurnCompleted { usage } => {
            let u = to_usage(&usage.expect("usage"));
            assert_eq!(u.input_tokens, 122070);
            assert_eq!(u.cache_read_input_tokens, 112128);
            assert_eq!(u.cache_creation_input_tokens, 100);
            assert_eq!(u.output_tokens, 438);
        }
        other => panic!("expected TurnCompleted: {other:?}"),
    }
}

#[test]
fn turn_failed_unpacks_json_error() {
    let wire = parse(
        r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'bad-model' model is not supported.\"}}"}}"#,
    );
    match wire {
        Wire::TurnFailed { error } => {
            let detail = parse_error_detail(&error.expect("error").message);
            assert_eq!(detail, "The 'bad-model' model is not supported.");
        }
        other => panic!("expected TurnFailed: {other:?}"),
    }
}

/// The events a sequence of `codex exec --json` lines stands for.
fn read(lines: &[&str], bridged: bool) -> (Vec<AgentEvent>, Option<String>) {
    let mut thread = None;
    let mut reading = Reading::new(&mut thread, Some("gpt-5.5".to_string()), bridged);
    let events = lines.iter().flat_map(|l| reading.line(l)).collect();
    drop(reading);
    (events, thread)
}

/// An MCP call to incode's own server, in the shape `codex exec --json`
/// prints it (`exec/src/exec_events.rs` `McpToolCallItem`, 0.163.0): a
/// started item, then a completed one with the result's content blocks.
const INCODE_CALL: [&str; 2] = [
    r#"{"type":"item.started","item":{"id":"item_1","type":"mcp_tool_call","server":"incode","tool":"Write","arguments":{"path":"a.txt","content":"x"},"result":null,"error":null,"status":"in_progress"}}"#,
    r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"incode","tool":"Write","arguments":{"path":"a.txt","content":"x"},"result":{"content":[{"type":"text","text":"wrote a.txt"}],"structured_content":null},"error":null,"status":"completed"}}"#,
];

/// **BRIDGED, A CALL TO incode's SERVER IS SHOWN ONCE — AS INCODE RAN
/// IT.** The bridge shows the call under incode's id when it runs it;
/// codex's own record of the same call is not shown again. A call to any
/// other server is still shown from codex's record.
#[test]
fn bridged_codex_does_not_show_incodes_calls_twice() {
    let (events, _) = read(&INCODE_CALL, true);
    assert!(events.is_empty(), "{events:?}");

    let other = r#"{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"elsewhere","tool":"lookup","arguments":{},"result":{"content":[{"type":"text","text":"found"}]},"error":null,"status":"completed"}}"#;
    let (events, _) = read(&[other], true);
    assert!(
        matches!(&events[..], [AgentEvent::ToolUse { name, .. }, AgentEvent::ToolResult { output, is_error: false, .. }] if name == "lookup" && output == "found"),
        "{events:?}"
    );
}

/// On codex's own tools the same lines are its tool calls, mapped as
/// they always were: a use when the call starts, and the use and its
/// result when it completes.
#[test]
fn own_tools_codex_shows_an_mcp_call_from_its_record() {
    let (events, _) = read(&INCODE_CALL, false);
    assert!(
        matches!(
            &events[..],
            [
                AgentEvent::ToolUse { id: a, name: n1, .. },
                AgentEvent::ToolUse { id: b, name: n2, input },
                AgentEvent::ToolResult { output, is_error: false, .. },
            ] if a == "item_1" && b == "item_1" && n1 == "Write" && n2 == "Write"
                && input.contains("a.txt") && output == "wrote a.txt"
        ),
        "{events:?}"
    );
}

/// A failed MCP call carries its error as an object — `{"message": …}`,
/// not a string — and is shown as a failed result with that message.
#[test]
fn a_failed_mcp_call_shows_its_message() {
    let failed = r#"{"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"elsewhere","tool":"Write","arguments":{},"result":null,"error":{"message":"tool call timed out"},"status":"failed"}}"#;
    let (events, _) = read(&[failed], false);
    assert!(
        matches!(&events[..], [_, AgentEvent::ToolResult { output, is_error: true, .. }] if output == "tool call timed out"),
        "{events:?}"
    );
}

/// The rest of a turn reads the same on both routes: the thread, the
/// reply, and the result.
#[test]
fn a_turn_reads_the_same_on_both_routes() {
    let lines = [
        r#"{"type":"thread.started","thread_id":"th-1"}"#,
        r#"{"type":"turn.started"}"#,
        r#"Reading additional input from stdin..."#,
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"done"}}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":3,"cached_input_tokens":1,"output_tokens":2}}"#,
    ];
    let (own, own_thread) = read(&lines, false);
    let (bridged, bridged_thread) = read(&lines, true);
    assert_eq!(own, bridged);
    assert_eq!(own_thread.as_deref(), Some("th-1"));
    assert_eq!(bridged_thread.as_deref(), Some("th-1"));
    assert!(
        matches!(&own[0], AgentEvent::SessionStarted { session_id, .. } if session_id == "th-1")
    );
    assert!(own
        .iter()
        .any(|e| matches!(e, AgentEvent::Text(t) if t == "done")));
}
