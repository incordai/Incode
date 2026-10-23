//! Pins on what the codex backend does, taken from the code before it was
//! split into a module directory: what each `codex exec --json` line is read
//! as, the exact argv and environment each route builds, and the errors a
//! turn that cannot start reports. Every expectation below was produced by
//! the original `src/agent/codex.rs` and written down literally, so a split
//! that changes any of it fails here.

use std::path::{Path, PathBuf};

use super::{CodexBackend, CodexTools, Reading, MODEL_ONLY_OVERRIDES};
use crate::agent::{AgentBackend, AgentEvent, TurnOutcome};

// ---------------------------------------------------------------------------
// Reading: one line at a time
// ---------------------------------------------------------------------------

/// One event, every field shown.
fn event(e: &AgentEvent) -> String {
    match e {
        AgentEvent::SessionStarted { session_id, model } => {
            format!("session({session_id:?},{model:?})")
        }
        AgentEvent::Text(t) => format!("text({t:?})"),
        AgentEvent::TextDelta(t) => format!("delta({t:?})"),
        AgentEvent::ToolUse { id, name, input } => format!("use({id:?},{name:?},{input:?})"),
        AgentEvent::ToolResult {
            id,
            output,
            is_error,
        } => format!("result({id:?},{is_error},{output:?})"),
        other => format!("{other:?}"),
    }
}

fn outcome(o: Option<&TurnOutcome>) -> String {
    o.map_or_else(
        || "-".to_string(),
        |o| {
            format!(
                "outcome({:?},{:?},{},{:?},{:?})",
                o.session_id, o.stop_reason, o.is_error, o.detail, o.served_by
            )
        },
    )
}

/// Everything a run of `lines` leaves behind: the events, the thread the
/// backend now holds, the turn's result and its cost.
fn read_all(lines: &[&str], model: Option<&str>, bridged: bool) -> String {
    let mut thread = None;
    let mut reading = Reading::new(&mut thread, model.map(str::to_string), bridged);
    let events: Vec<String> = lines
        .iter()
        .flat_map(|l| reading.line(l))
        .map(|e| event(&e))
        .collect();
    let (result, usage) = reading.finish();
    format!(
        "[{}] thread={thread:?} {} usage({},{},{},{})",
        events.join(" "),
        outcome(result.as_ref()),
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens
    )
}

/// Real `codex exec --json` line shapes (0.145.0 to 0.163.0) and the broken
/// ones a stream can carry, by name.
const CORPUS: &[(&str, &str)] = &[
    ("thread", r#"{"type":"thread.started","thread_id":"th-1"}"#),
    ("turn_started", r#"{"type":"turn.started"}"#),
    (
        "start_cmd",
        r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
    ),
    (
        "start_cmd_no_command",
        r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution"}}"#,
    ),
    (
        "start_mcp_elsewhere",
        r#"{"type":"item.started","item":{"id":"item_2","type":"mcp_tool_call","server":"memory","tool":"recall","arguments":{"q":"x"},"result":null,"error":null,"status":"in_progress"}}"#,
    ),
    (
        "start_mcp_incode",
        r#"{"type":"item.started","item":{"id":"item_3","type":"mcp_tool_call","server":"incode","tool":"Write","arguments":{"path":"a.txt"},"result":null,"error":null,"status":"in_progress"}}"#,
    ),
    (
        "start_mcp_no_arguments",
        r#"{"type":"item.started","item":{"id":"item_4","type":"mcp_tool_call","server":"memory","tool":"recall"}}"#,
    ),
    (
        "start_mcp_no_tool",
        r#"{"type":"item.started","item":{"id":"item_5","type":"mcp_tool_call","server":"memory"}}"#,
    ),
    (
        "start_message",
        r#"{"type":"item.started","item":{"id":"item_6","type":"agent_message","text":"hi"}}"#,
    ),
    (
        "start_reasoning",
        r#"{"type":"item.started","item":{"id":"item_7","type":"reasoning","text":"thinking"}}"#,
    ),
    (
        "done_message",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"The package name is incord-code."}}"#,
    ),
    (
        "done_message_blank",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"  \n "}}"#,
    ),
    (
        "done_message_no_text",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message"}}"#,
    ),
    (
        "done_cmd_ok",
        r#"{"type":"item.completed","item":{"id":"item_5","type":"command_execution","command":"powershell.exe -Command Get-Location","aggregated_output":"C:\\Projects\\incord-code\n","exit_code":0,"status":"completed"}}"#,
    ),
    (
        "done_cmd_failed",
        r#"{"type":"item.completed","item":{"id":"item_6","type":"command_execution","command":"false","aggregated_output":"oops","exit_code":1,"status":"failed"}}"#,
    ),
    (
        "done_cmd_bare",
        r#"{"type":"item.completed","item":{"id":"item_7","type":"command_execution","command":"true"}}"#,
    ),
    (
        "done_cmd_no_command",
        r#"{"type":"item.completed","item":{"id":"item_8","type":"command_execution","aggregated_output":"x","exit_code":2}}"#,
    ),
    (
        "done_mcp_texts",
        r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"memory","tool":"watchman_lookup","arguments":{"task":"C:\\Projects\\incord-code"},"result":{"content":[{"type":"text","text":"one"},{"type":"image","data":"x"},{"type":"text","text":"two"}]},"error":null,"status":"completed"}}"#,
    ),
    (
        "done_mcp_structured",
        r#"{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"memory","tool":"t","arguments":{},"result":{"structured_content":{"n":1}},"error":null,"status":"completed"}}"#,
    ),
    (
        "done_mcp_empty_content",
        r#"{"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"memory","tool":"t","arguments":null,"result":{"content":[]},"status":"completed"}}"#,
    ),
    (
        "done_mcp_error_string",
        r#"{"type":"item.completed","item":{"id":"item_4","type":"mcp_tool_call","server":"memory","tool":"t","result":null,"error":"boom","status":"failed"}}"#,
    ),
    (
        "done_mcp_error_object",
        r#"{"type":"item.completed","item":{"id":"item_5","type":"mcp_tool_call","server":"memory","tool":"t","result":null,"error":{"message":"tool call timed out"},"status":"failed"}}"#,
    ),
    (
        "done_mcp_error_nested",
        r#"{"type":"item.completed","item":{"id":"item_6","type":"mcp_tool_call","server":"memory","tool":"t","error":{"error":{"message":"inner"},"message":"outer"}}}"#,
    ),
    (
        "done_mcp_error_unshaped",
        r#"{"type":"item.completed","item":{"id":"item_7","type":"mcp_tool_call","server":"memory","tool":"t","error":{"code":5}}}"#,
    ),
    (
        "done_mcp_error_beside_result",
        r#"{"type":"item.completed","item":{"id":"item_8","type":"mcp_tool_call","server":"memory","tool":"t","result":{"content":[{"type":"text","text":"partial"}]},"error":{"message":"late"},"status":"completed"}}"#,
    ),
    (
        "done_mcp_status_failed",
        r#"{"type":"item.completed","item":{"id":"item_9","type":"mcp_tool_call","server":"memory","tool":"t","result":{"content":[{"type":"text","text":"denied"}]},"error":null,"status":"failed"}}"#,
    ),
    (
        "done_mcp_no_tool",
        r#"{"type":"item.completed","item":{"id":"item_10","type":"mcp_tool_call","server":"memory","result":{"content":[{"type":"text","text":"x"}]}}}"#,
    ),
    (
        "done_mcp_incode",
        r#"{"type":"item.completed","item":{"id":"item_1","type":"mcp_tool_call","server":"incode","tool":"Write","arguments":{"path":"a.txt","content":"x"},"result":{"content":[{"type":"text","text":"wrote a.txt"}],"structured_content":null},"error":null,"status":"completed"}}"#,
    ),
    (
        "done_mcp_incode_failed",
        r#"{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"incode","tool":"Write","result":null,"error":{"message":"refused"},"status":"failed"}}"#,
    ),
    (
        "done_cmd_server_incode",
        r#"{"type":"item.completed","item":{"id":"item_3","type":"command_execution","server":"incode","command":"ls","exit_code":0}}"#,
    ),
    (
        "done_error_item",
        r#"{"type":"item.completed","item":{"id":"item_11","type":"error","text":"stream hiccup"}}"#,
    ),
    (
        "done_error_item_no_text",
        r#"{"type":"item.completed","item":{"id":"item_12","type":"error"}}"#,
    ),
    (
        "done_reasoning",
        r#"{"type":"item.completed","item":{"id":"item_13","type":"reasoning","text":"**Thinking**"}}"#,
    ),
    (
        "done_file_change",
        r#"{"type":"item.completed","item":{"id":"item_14","type":"file_change","changes":[{"path":"a.rs","kind":"update"}],"status":"completed"}}"#,
    ),
    (
        "done_todo_list",
        r#"{"type":"item.completed","item":{"id":"item_15","type":"todo_list","items":[{"text":"a","completed":false}]}}"#,
    ),
    (
        "done_untyped_item",
        r#"{"type":"item.completed","item":{"id":"item_16"}}"#,
    ),
    ("done_empty_item", r#"{"type":"item.completed","item":{}}"#),
    (
        "turn_done_usage",
        r#"{"type":"turn.completed","usage":{"input_tokens":122070,"cached_input_tokens":112128,"cache_write_input_tokens":100,"output_tokens":438,"reasoning_output_tokens":83}}"#,
    ),
    (
        "turn_done_partial_usage",
        r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":2}}"#,
    ),
    ("turn_done_no_usage", r#"{"type":"turn.completed"}"#),
    (
        "turn_done_null_usage",
        r#"{"type":"turn.completed","usage":null}"#,
    ),
    (
        "turn_failed_nested",
        r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'bad-model' model is not supported.\"}}"}}"#,
    ),
    (
        "turn_failed_top_message",
        r#"{"type":"turn.failed","error":{"message":"{\"message\":\"You've hit your usage limit.\"}"}}"#,
    ),
    (
        "turn_failed_plain",
        r#"{"type":"turn.failed","error":{"message":"stream disconnected before completion"}}"#,
    ),
    (
        "turn_failed_empty_error",
        r#"{"type":"turn.failed","error":{}}"#,
    ),
    ("turn_failed_no_error", r#"{"type":"turn.failed"}"#),
    (
        "error_plain",
        r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
    ),
    (
        "error_nested",
        r#"{"type":"error","message":"{\"error\":{\"message\":\"Unauthorized\"}}"}"#,
    ),
    (
        "error_json_not_shaped",
        r#"{"type":"error","message":"[1,2]"}"#,
    ),
    ("error_no_message", r#"{"type":"error"}"#),
    (
        "unknown_type",
        r#"{"type":"session.configured","model":"gpt-5.5"}"#,
    ),
    ("not_json", "Reading additional input from stdin..."),
    ("empty", ""),
    ("untyped_json", r#"{"thread_id":"th-x"}"#),
    ("thread_without_id", r#"{"type":"thread.started"}"#),
    ("completed_without_item", r#"{"type":"item.completed"}"#),
    (
        "wrong_field_type",
        r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"ls","exit_code":"0"}}"#,
    ),
    ("json_array", r#"[{"type":"turn.started"}]"#),
    ("numeric_type", r#"{"type":7}"#),
    ("truncated", r#"{"type":"thread.started","thread_id":"th"#),
];

/// What each line of [`CORPUS`] is read as on codex's own tools, by a fresh
/// reading whose model is `gpt-5.5`.
const OWN: &[(&str, &str)] = &[
    (
        "thread",
        "[session(\"th-1\",\"gpt-5.5\")] thread=Some(\"th-1\") - usage(0,0,0,0)",
    ),
    ("turn_started", NOTHING),
    (
        "start_cmd",
        "[use(\"item_1\",\"Bash\",\"bash -lc ls\")] thread=None - usage(0,0,0,0)",
    ),
    ("start_cmd_no_command", NOTHING),
    (
        "start_mcp_elsewhere",
        "[use(\"item_2\",\"recall\",\"{\\\"q\\\":\\\"x\\\"}\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "start_mcp_incode",
        "[use(\"item_3\",\"Write\",\"{\\\"path\\\":\\\"a.txt\\\"}\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "start_mcp_no_arguments",
        "[use(\"item_4\",\"recall\",\"\")] thread=None - usage(0,0,0,0)",
    ),
    ("start_mcp_no_tool", NOTHING),
    ("start_message", NOTHING),
    ("start_reasoning", NOTHING),
    (
        "done_message",
        "[delta(\"The package name is incord-code.\") text(\"The package name is incord-code.\")] thread=None - usage(0,0,0,0)",
    ),
    ("done_message_blank", NOTHING),
    ("done_message_no_text", NOTHING),
    (
        "done_cmd_ok",
        "[use(\"item_5\",\"Bash\",\"powershell.exe -Command Get-Location\") result(\"item_5\",false,\"C:\\\\Projects\\\\incord-code\\n\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_cmd_failed",
        "[use(\"item_6\",\"Bash\",\"false\") result(\"item_6\",true,\"oops\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_cmd_bare",
        "[use(\"item_7\",\"Bash\",\"true\") result(\"item_7\",false,\"\")] thread=None - usage(0,0,0,0)",
    ),
    ("done_cmd_no_command", NOTHING),
    (
        "done_mcp_texts",
        "[use(\"item_1\",\"watchman_lookup\",\"{\\\"task\\\":\\\"C:\\\\\\\\Projects\\\\\\\\incord-code\\\"}\") result(\"item_1\",false,\"one\\ntwo\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_structured",
        "[use(\"item_2\",\"t\",\"{}\") result(\"item_2\",false,\"{\\\"structured_content\\\":{\\\"n\\\":1}}\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_empty_content",
        "[use(\"item_3\",\"t\",\"\") result(\"item_3\",false,\"{\\\"content\\\":[]}\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_error_string",
        "[use(\"item_4\",\"t\",\"\") result(\"item_4\",true,\"boom\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_error_object",
        "[use(\"item_5\",\"t\",\"\") result(\"item_5\",true,\"tool call timed out\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_error_nested",
        "[use(\"item_6\",\"t\",\"\") result(\"item_6\",true,\"inner\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_error_unshaped",
        "[use(\"item_7\",\"t\",\"\") result(\"item_7\",true,\"{\\\"code\\\":5}\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_error_beside_result",
        "[use(\"item_8\",\"t\",\"\") result(\"item_8\",true,\"partial\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_status_failed",
        "[use(\"item_9\",\"t\",\"\") result(\"item_9\",true,\"denied\")] thread=None - usage(0,0,0,0)",
    ),
    ("done_mcp_no_tool", NOTHING),
    (
        "done_mcp_incode",
        "[use(\"item_1\",\"Write\",\"{\\\"path\\\":\\\"a.txt\\\",\\\"content\\\":\\\"x\\\"}\") result(\"item_1\",false,\"wrote a.txt\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_mcp_incode_failed",
        "[use(\"item_2\",\"Write\",\"\") result(\"item_2\",true,\"refused\")] thread=None - usage(0,0,0,0)",
    ),
    (
        "done_cmd_server_incode",
        "[use(\"item_3\",\"Bash\",\"ls\") result(\"item_3\",false,\"\")] thread=None - usage(0,0,0,0)",
    ),
    ("done_error_item", NOTHING),
    ("done_error_item_no_text", NOTHING),
    ("done_reasoning", NOTHING),
    ("done_file_change", NOTHING),
    ("done_todo_list", NOTHING),
    ("done_untyped_item", NOTHING),
    ("done_empty_item", NOTHING),
    (
        "turn_done_usage",
        "[] thread=None outcome(\"\",\"SUCCESS\",false,None,[\"gpt-5.5\"]) usage(122070,438,100,112128)",
    ),
    (
        "turn_done_partial_usage",
        "[] thread=None outcome(\"\",\"SUCCESS\",false,None,[\"gpt-5.5\"]) usage(3,2,0,0)",
    ),
    ("turn_done_no_usage", SUCCEEDED),
    ("turn_done_null_usage", SUCCEEDED),
    (
        "turn_failed_nested",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"The 'bad-model' model is not supported.\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "turn_failed_top_message",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"You've hit your usage limit.\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "turn_failed_plain",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"stream disconnected before completion\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "turn_failed_empty_error",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "turn_failed_no_error",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"turn failed\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "error_plain",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"Reconnecting... 1/5\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "error_nested",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"Unauthorized\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "error_json_not_shaped",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"[1,2]\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    (
        "error_no_message",
        "[] thread=None outcome(\"\",\"ERROR\",true,Some(\"codex error\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
    ),
    ("unknown_type", NOTHING),
    ("not_json", NOTHING),
    ("empty", NOTHING),
    ("untyped_json", NOTHING),
    ("thread_without_id", NOTHING),
    ("completed_without_item", NOTHING),
    ("wrong_field_type", NOTHING),
    ("json_array", NOTHING),
    ("numeric_type", NOTHING),
    ("truncated", NOTHING),
];

/// A line that stands for nothing and changes nothing.
const NOTHING: &str = "[] thread=None - usage(0,0,0,0)";

/// A turn that completed and reported no usage.
const SUCCEEDED: &str =
    "[] thread=None outcome(\"\",\"SUCCESS\",false,None,[\"gpt-5.5\"]) usage(0,0,0,0)";

/// The lines read differently when bridged: codex's record of a call to
/// incode's own server is not shown. Every other line reads as [`OWN`].
const BRIDGED: &[(&str, &str)] = &[
    ("start_mcp_incode", NOTHING),
    ("done_mcp_incode", NOTHING),
    ("done_mcp_incode_failed", NOTHING),
    ("done_cmd_server_incode", NOTHING),
];

#[test]
fn every_corpus_line_reads_as_it_did_on_both_routes() {
    let mut wrong = Vec::new();
    for (name, line) in CORPUS {
        let own = read_all(&[line], Some("gpt-5.5"), false);
        let bridged = read_all(&[line], Some("gpt-5.5"), true);
        let want_own = OWN.iter().find(|(n, _)| n == name).map(|(_, w)| *w);
        let want_bridged = BRIDGED
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, w)| *w)
            .or(want_own);
        if want_own != Some(own.as_str()) {
            wrong.push(format!("    ({name:?}, {own:?}),"));
        }
        if want_bridged != Some(bridged.as_str()) {
            wrong.push(format!("BRIDGED ({name:?}, {bridged:?}),"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
    assert_eq!(OWN.len(), CORPUS.len(), "one reading per line");
}

/// Whole turns: the state a line leaves is what the next one reads.
#[test]
fn whole_turns_read_as_they_did() {
    let line = |name: &str| {
        CORPUS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, l)| *l)
            .expect("a corpus line")
    };
    let turns: [(&str, &[&str], Option<&str>, bool); 7] = [
        (
            "own_turn",
            &[
                "thread",
                "turn_started",
                "not_json",
                "start_cmd",
                "done_cmd_ok",
                "start_mcp_elsewhere",
                "done_mcp_texts",
                "done_message",
                "turn_done_usage",
            ],
            Some("gpt-5.5"),
            false,
        ),
        (
            "bridged_turn",
            &[
                "thread",
                "turn_started",
                "start_mcp_incode",
                "done_mcp_incode",
                "done_mcp_incode_failed",
                "done_message",
                "turn_done_partial_usage",
            ],
            Some("gpt-5.5"),
            true,
        ),
        (
            "failed_after_thread",
            &["thread", "turn_failed_nested"],
            Some("gpt-5.5"),
            false,
        ),
        (
            "error_overwrites_success",
            &["thread", "turn_done_usage", "error_plain"],
            Some("gpt-5.5"),
            false,
        ),
        (
            "success_overwrites_error",
            &["error_no_message", "thread", "turn_done_no_usage"],
            Some("gpt-5.5"),
            false,
        ),
        (
            "no_model",
            &["thread", "done_message", "turn_done_partial_usage"],
            None,
            false,
        ),
        (
            "usage_kept_when_a_later_turn_has_none",
            &["turn_done_usage", "turn_done_no_usage"],
            None,
            false,
        ),
    ];
    let want: &[(&str, &str)] = &[
        (
            "own_turn",
            "[session(\"th-1\",\"gpt-5.5\") use(\"item_1\",\"Bash\",\"bash -lc ls\") use(\"item_5\",\"Bash\",\"powershell.exe -Command Get-Location\") result(\"item_5\",false,\"C:\\\\Projects\\\\incord-code\\n\") use(\"item_2\",\"recall\",\"{\\\"q\\\":\\\"x\\\"}\") use(\"item_1\",\"watchman_lookup\",\"{\\\"task\\\":\\\"C:\\\\\\\\Projects\\\\\\\\incord-code\\\"}\") result(\"item_1\",false,\"one\\ntwo\") delta(\"The package name is incord-code.\") text(\"The package name is incord-code.\")] thread=Some(\"th-1\") outcome(\"th-1\",\"SUCCESS\",false,None,[\"gpt-5.5\"]) usage(122070,438,100,112128)",
        ),
        (
            "bridged_turn",
            "[session(\"th-1\",\"gpt-5.5\") delta(\"The package name is incord-code.\") text(\"The package name is incord-code.\")] thread=Some(\"th-1\") outcome(\"th-1\",\"SUCCESS\",false,None,[\"gpt-5.5\"]) usage(3,2,0,0)",
        ),
        (
            "failed_after_thread",
            "[session(\"th-1\",\"gpt-5.5\")] thread=Some(\"th-1\") outcome(\"th-1\",\"ERROR\",true,Some(\"The 'bad-model' model is not supported.\"),[\"gpt-5.5\"]) usage(0,0,0,0)",
        ),
        (
            "error_overwrites_success",
            "[session(\"th-1\",\"gpt-5.5\")] thread=Some(\"th-1\") outcome(\"th-1\",\"ERROR\",true,Some(\"Reconnecting... 1/5\"),[\"gpt-5.5\"]) usage(122070,438,100,112128)",
        ),
        (
            "success_overwrites_error",
            "[session(\"th-1\",\"gpt-5.5\")] thread=Some(\"th-1\") outcome(\"th-1\",\"SUCCESS\",false,None,[\"gpt-5.5\"]) usage(0,0,0,0)",
        ),
        (
            "no_model",
            "[session(\"th-1\",\"\") delta(\"The package name is incord-code.\") text(\"The package name is incord-code.\")] thread=Some(\"th-1\") outcome(\"th-1\",\"SUCCESS\",false,None,[]) usage(3,2,0,0)",
        ),
        (
            "usage_kept_when_a_later_turn_has_none",
            "[] thread=None outcome(\"\",\"SUCCESS\",false,None,[]) usage(122070,438,100,112128)",
        ),
    ];
    let mut wrong = Vec::new();
    for (name, names, model, bridged) in turns {
        let lines: Vec<&str> = names.iter().map(|n| line(n)).collect();
        let got = read_all(&lines, model, bridged);
        if want.iter().find(|(n, _)| *n == name).map(|(_, w)| *w) != Some(got.as_str()) {
            wrong.push(format!("({name:?}, {got:?}),"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

// ---------------------------------------------------------------------------
// argv and environment
// ---------------------------------------------------------------------------

/// Everything a built command says about how it starts.
fn spawn(cmd: &std::process::Command) -> String {
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let set: Vec<(String, String)> = cmd
        .get_envs()
        .filter_map(|(k, v)| {
            Some((
                k.to_string_lossy().into_owned(),
                v?.to_string_lossy().into_owned(),
            ))
        })
        .collect();
    format!(
        "program={:?} dir={:?} args={args:?} env={set:?}",
        cmd.get_program().to_string_lossy(),
        cmd.get_current_dir()
            .map(|d| d.to_string_lossy().into_owned())
    )
}

/// The parent session's variables are removed on every spawn, both routes.
fn removes_the_parent_session(cmd: &std::process::Command) -> bool {
    [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_SESSION_INGRESS_TOKEN_FILE",
    ]
    .iter()
    .all(|name| cmd.get_envs().any(|(k, v)| k == *name && v.is_none()))
}

/// A backend with every setting the own route reads.
fn configured(tools: CodexTools) -> CodexBackend {
    let mut backend = CodexBackend::new(PathBuf::from("ws"))
        .with_binary("codex-x")
        .with_tools(tools)
        .with_mcp_overrides(vec![
            "mcp_servers.team.command=\"incode\"".to_string(),
            "mcp_servers.desktop.args=[\"--desktop\"]".to_string(),
        ]);
    backend.set_models(&["claude-x".to_string(), "gpt-5.5".to_string()]);
    backend.set_permission_mode("acceptEdits");
    backend.set_extra_dirs(vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    backend.set_team_identity("review");
    backend.image_files = vec![PathBuf::from("one.png"), PathBuf::from("two.jpg")];
    backend.tool_server = Some(PathBuf::from("/opt/incode/bin/incode"));
    backend
}

#[test]
fn every_spawn_is_built_as_it_was() {
    let mut cases: Vec<(String, String)> = Vec::new();
    let mut add = |name: &str, backend: &CodexBackend| {
        let cmd = backend.command("the task");
        assert!(removes_the_parent_session(&cmd), "{name}");
        cases.push((name.to_string(), spawn(&cmd)));
    };
    let mut bare = CodexBackend::new(PathBuf::from("."));
    add("bare", &bare);
    bare.thread_id = Some("t-9".to_string());
    add("bare_resumed", &bare);

    let mut own = configured(CodexTools::Own);
    add("own", &own);
    own.thread_id = Some("t-1".to_string());
    add("own_resumed", &own);
    for mode in [
        "plan",
        "read-only",
        "accept-edits",
        "workspace-write",
        "bypassPermissions",
    ] {
        let mut backend = CodexBackend::new(PathBuf::from("."));
        backend.set_permission_mode(mode);
        add(mode, &backend);
    }

    let mut bridged = configured(CodexTools::Incode);
    add("bridged", &bridged);
    bridged.thread_id = Some("t-2".to_string());
    add("bridged_resumed", &bridged);

    const AUTO: &str = "mcp_servers.memory.default_tools_approval_mode=\"auto\"";
    const TEAM: &str = "mcp_servers.team.command=\"incode\"";
    const DESKTOP: &str = "mcp_servers.desktop.args=[\"--desktop\"]";
    const LEAD: [&str; 4] = ["--skip-git-repo-check", "--json", "--disable", "hooks"];
    const IMAGES: [&str; 4] = ["--image", "one.png", "--image", "two.jpg"];
    const BYPASS: &str = "--dangerously-bypass-approvals-and-sandbox";
    let opt = bridged_overrides("/opt/incode/bin/incode");
    let plain = |tail: &[&str]| -> Vec<String> {
        let mut args = vec!["exec"];
        args.extend(LEAD);
        args.extend(["-c", AUTO]);
        args.extend(tail);
        args.iter().map(|a| (*a).to_string()).collect()
    };
    let mut want: Vec<(&str, String)> = vec![
        (
            "bare",
            expect("codex", ".", &plain(&[BYPASS, "-C", ".", "the task"]), &[]),
        ),
        (
            "bare_resumed",
            expect(
                "codex",
                ".",
                &strings(
                    &[
                        &["exec", "resume"][..],
                        &LEAD[..],
                        &[BYPASS, "-c", AUTO, "t-9", "the task"],
                    ]
                    .concat(),
                ),
                &[],
            ),
        ),
        (
            "own",
            expect(
                "codex-x",
                "ws",
                &strings(
                    &[
                        &["exec"][..],
                        &IMAGES[..],
                        &LEAD[..],
                        &["-c", AUTO, "-c", TEAM, "-c", DESKTOP],
                        &["-s", "workspace-write", "-m", "gpt-5.5"],
                        &["--add-dir", "/a", "--add-dir", "/b", "-C", "ws", "the task"],
                    ]
                    .concat(),
                ),
                &[("INCODE_TEAM_AS", "review")],
            ),
        ),
        (
            "own_resumed",
            expect(
                "codex-x",
                "ws",
                &strings(
                    &[
                        &["exec", "resume"][..],
                        &IMAGES[..],
                        &LEAD[..],
                        &[BYPASS, "-c", AUTO, "-c", TEAM, "-c", DESKTOP],
                        &["-m", "gpt-5.5", "t-1", "the task"],
                    ]
                    .concat(),
                ),
                &[("INCODE_TEAM_AS", "review")],
            ),
        ),
    ];
    for (mode, flags) in [
        ("plan", &["-s", "read-only"][..]),
        ("read-only", &["-s", "read-only"][..]),
        ("accept-edits", &["-s", "workspace-write"][..]),
        ("workspace-write", &["-s", "workspace-write"][..]),
        ("bypassPermissions", &[BYPASS][..]),
    ] {
        let tail: Vec<&str> = flags
            .iter()
            .copied()
            .chain(["-C", ".", "the task"])
            .collect();
        want.push((mode, expect("codex", ".", &plain(&tail), &[])));
    }
    for (name, lead, tail) in [
        ("bridged", &["exec"][..], &["-C", "ws"][..]),
        ("bridged_resumed", &["exec", "resume"][..], &["t-2"][..]),
    ] {
        let mut args = strings(&[lead, &IMAGES[..], &LEAD[..], &["--ignore-user-config"]].concat());
        args.extend(opt.iter().cloned());
        args.extend(strings(
            &[&["-m", "gpt-5.5"][..], tail, &["the task"]].concat(),
        ));
        want.push((name, expect("codex-x", "ws", &args, &[])));
    }
    assert_eq!(cases.len(), want.len());
    for ((name, got), (want_name, want)) in cases.iter().zip(&want) {
        assert_eq!(name, want_name);
        assert_eq!(got, want, "{name}");
    }
}

/// `strs` as owned strings.
fn strings(strs: &[&str]) -> Vec<String> {
    strs.iter().map(|s| (*s).to_string()).collect()
}

/// What [`spawn`] reports for a command built this way.
fn expect(program: &str, dir: &str, args: &[String], env: &[(&str, &str)]) -> String {
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    format!(
        "program={:?} dir={:?} args={args:?} env={env:?}",
        program,
        Some(dir)
    )
}

/// The bridged route's `-c` pairs, written out, for a tool server at `exe`
/// as it appears in the TOML (backslashes already doubled).
fn bridged_overrides(exe: &str) -> Vec<String> {
    let values = [
        "features.shell_tool=false".to_string(),
        "features.view_image=false".to_string(),
        "web_search=\"disabled\"".to_string(),
        "features.goals=false".to_string(),
        "tools.experimental_request_user_input.enabled=false".to_string(),
        "agents.enabled=false".to_string(),
        "features.multi_agent=false".to_string(),
        "features.apps=false".to_string(),
        "features.plugins=false".to_string(),
        "features.browser_use=false".to_string(),
        "features.computer_use=false".to_string(),
        "features.image_generation=false".to_string(),
        "sandbox_mode=\"read-only\"".to_string(),
        "approval_policy=\"never\"".to_string(),
        format!("mcp_servers.incode.command=\"{exe}\""),
        "mcp_servers.incode.args=[\"--tool-server\"]".to_string(),
        "mcp_servers.incode.env_vars=[\"INCODE_TOOL_BRIDGE\",\"INCODE_TOOL_TOKEN\"]".to_string(),
        "mcp_servers.incode.default_tools_approval_mode=\"approve\"".to_string(),
        "mcp_servers.incode.required=true".to_string(),
        "mcp_servers.incode.tool_timeout_sec=3600".to_string(),
        "mcp_servers.incode.omit_tools_from=[\"deferred\"]".to_string(),
    ];
    values
        .into_iter()
        .flat_map(|v| ["-c".to_string(), v])
        .collect()
}

/// A Windows path with spaces, as the tool server, on the bridged command.
#[test]
fn the_bridged_command_escapes_a_windows_server_path() {
    let mut backend = CodexBackend::new(PathBuf::from("."))
        .with_tools(CodexTools::Incode)
        .with_binary("codex");
    let exe = Path::new(r"C:\Program Files\incode\incode.exe");
    let fresh = spawn(&backend.bridged_command("p", exe));
    backend.thread_id = Some("t".to_string());
    let resumed = spawn(&backend.bridged_command("p", exe));
    let overrides = bridged_overrides(r"C:\\Program Files\\incode\\incode.exe");
    for (got, lead, tail) in [
        (fresh, &["exec"][..], &["-C", ".", "p"][..]),
        (resumed, &["exec", "resume"][..], &["t", "p"][..]),
    ] {
        let mut args = strings(
            &[
                lead,
                &["--skip-git-repo-check", "--json", "--disable", "hooks"],
                &["--ignore-user-config"],
            ]
            .concat(),
        );
        args.extend(overrides.iter().cloned());
        args.extend(strings(tail));
        assert_eq!(got, expect("codex", ".", &args, &[]));
    }
}

#[test]
fn the_model_only_overrides_are_the_fourteen() {
    assert_eq!(
        MODEL_ONLY_OVERRIDES,
        [
            "features.shell_tool=false",
            "features.view_image=false",
            "web_search=\"disabled\"",
            "features.goals=false",
            "tools.experimental_request_user_input.enabled=false",
            "agents.enabled=false",
            "features.multi_agent=false",
            "features.apps=false",
            "features.plugins=false",
            "features.browser_use=false",
            "features.computer_use=false",
            "features.image_generation=false",
            "sandbox_mode=\"read-only\"",
            "approval_policy=\"never\"",
        ]
    );
}

/// What each route says it was handed.
#[test]
fn the_mcp_config_each_route_reports_is_unchanged() {
    assert_eq!(CodexBackend::new(PathBuf::from(".")).mcp_config(), None);
    let own = configured(CodexTools::Own);
    assert_eq!(
        own.mcp_config().as_deref(),
        Some("mcp_servers.team.command=\"incode\"\nmcp_servers.desktop.args=[\"--desktop\"]")
    );
    let bridged = configured(CodexTools::Incode);
    assert_eq!(
        bridged.mcp_config().as_deref(),
        Some(
            "mcp_servers.incode.command=\"/opt/incode/bin/incode\"\n\
             mcp_servers.incode.args=[\"--tool-server\"]\n\
             mcp_servers.incode.env_vars=[\"INCODE_TOOL_BRIDGE\",\"INCODE_TOOL_TOKEN\"]\n\
             mcp_servers.incode.default_tools_approval_mode=\"approve\"\n\
             mcp_servers.incode.required=true\n\
             mcp_servers.incode.tool_timeout_sec=3600\n\
             mcp_servers.incode.omit_tools_from=[\"deferred\"]"
        )
    );
}

// ---------------------------------------------------------------------------
// A turn that cannot start
// ---------------------------------------------------------------------------

/// A binary that is not there fails the turn with the same words on both
/// routes, and the images it was handed are let go.
#[test]
fn a_missing_binary_fails_the_turn_with_the_same_words() {
    for tools in [CodexTools::Own, CodexTools::Incode] {
        let dir = tempfile::tempdir().expect("dir");
        let mut backend = CodexBackend::new(dir.path().to_path_buf())
            .with_binary("incode-no-such-codex-binary")
            .with_tools(tools);
        let mut seen = Vec::new();
        let err = backend
            .run_turn("hi", &mut |e| seen.push(e.clone()))
            .expect_err("nothing to start");
        assert!(
            err.to_string()
                .starts_with("could not start `incode-no-such-codex-binary`: "),
            "{tools:?}: {err}"
        );
        assert!(seen.is_empty(), "{tools:?}: {seen:?}");
        assert!(backend.image_files.is_empty(), "{tools:?}");
        assert_eq!(backend.session_id(), None, "{tools:?}");
    }
}
