//! What the CLI's stream-json lines become, one line at a time
//! (`map_line`) and as a whole turn through the stream loop (`consume`).
//! Fixtures are real lines captured from Claude Code 2.1.229.

use std::io::{BufRead, BufReader};

use super::tests::{outcome_of, usage_of};
use super::turn::{consume, consume_stream};
use super::wire::map_line;
use super::*;
use crate::tool_bridge::Incoming;

/// **A cancelled turn stops reading and says so.** Without a distinct
/// error the caller cannot tell "the user pressed Ctrl-C" from "the agent
/// crashed", and would report a crash.
#[test]
fn a_cancelled_turn_stops_and_names_itself() {
    let token = crate::runtime::cancel::CancelToken::new();
    token.cancel();
    let lines = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}\n";
    let mut session = None;
    let mut seen = Vec::new();
    let err = consume_stream(
        std::io::BufReader::new(lines.as_bytes()),
        &mut session,
        &mut |e: &AgentEvent| seen.push(e.clone()),
        Some(&token),
    )
    .expect_err("a cancelled turn must not report success");
    assert_eq!(err.to_string(), CANCELLED);
    assert!(seen.is_empty(), "it must stop BEFORE handling more output");
}

/// Without cancellation the same stream is read normally — the control
/// that proves the test above is measuring the flag and not the fixture.
#[test]
fn the_same_stream_reads_normally_when_not_cancelled() {
    let lines = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}\n";
    let mut session = None;
    let mut seen = Vec::new();
    consume_stream(
        std::io::BufReader::new(lines.as_bytes()),
        &mut session,
        &mut |e: &AgentEvent| seen.push(e.clone()),
        None,
    )
    .expect("it reads");
    assert_eq!(session.as_deref(), Some("s"));
    assert_eq!(seen.len(), 1);
}

/// **Captured from a real run, not composed here.** The delta shape is the
/// CLI's, and a parser written against an imagined one would pass its own
/// test and fail the first real line.
#[test]
fn a_text_delta_from_the_real_wire_becomes_a_delta_event() {
    let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"h"}},"session_id":"a77936b0","parent_tool_use_id":null,"uuid":"bfc09fc8"}"#;
    let mut outcome = None;
    let events = map_line(line, &mut outcome);
    assert_eq!(events, vec![AgentEvent::TextDelta("h".to_string())]);
}

/// A subagent's deltas are not the main conversation's text — the same rule
/// the whole-block branch already applies.
#[test]
fn a_subagents_delta_is_not_the_conversation() {
    let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"h"}},"parent_tool_use_id":"toolu_1"}"#;
    let mut outcome = None;
    assert!(map_line(line, &mut outcome).is_empty());
}

/// The other stream frames carry no user-facing text and must stay silent —
/// message_start and content_block_stop arrive for every single turn.
#[test]
fn stream_frames_that_are_not_text_produce_nothing() {
    let mut outcome = None;
    for line in [
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"m"}},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null}"#,
    ] {
        assert!(
            map_line(line, &mut outcome).is_empty(),
            "should be silent: {line}"
        );
    }
}

const REAL_ASSISTANT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5","id":"msg_011CdyZz5W5Z65fqwbCLUrvy","type":"message","role":"assistant","content":[{"type":"text","text":"OK"}],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"e2996c17-f358-46fa-89b9-e757a4742abe"}"#;

const REAL_RATE_LIMIT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1786575600,"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false},"session_id":"e2996c17-f358-46fa-89b9-e757a4742abe"}"#;

/// The real result line puts `type` near the END of the object — a
/// deserializer that assumes `type` comes first will not parse it.
/// A FAILED result, in the wire shape the agent actually emits.
///
/// The `result` and `subtype` fields here were not deserialized at all, so
/// a turn that ended this way was reported to the operator as the single
/// word `stop_sequence` — for an entire day, while the sentence explaining
/// it sat unread in the same line.
const FAILED_RESULT: &str = r#"{"is_error":true,"duration_api_ms":900,"num_turns":3,"stop_reason":"stop_sequence","session_id":"e2996c17-f358-46fa-89b9-e757a4742abe","usage":{"input_tokens":1,"output_tokens":2},"subtype":"error_during_execution","result":"tool use was interrupted before it completed","type":"result","duration_ms":1200}"#;

const REAL_RESULT: &str = r#"{"is_error":false,"duration_api_ms":1846,"num_turns":1,"stop_reason":"end_turn","session_id":"393279e4-f6e9-4d64-bd17-57926443ef94","total_cost_usd":0.399,"usage":{"input_tokens":1,"cache_creation_input_tokens":36371,"cache_read_input_tokens":23432,"output_tokens":4},"subtype":"success","result":"OK","type":"result","duration_ms":31639}"#;

fn map(line: &str) -> (Vec<AgentEvent>, Option<TurnOutcome>) {
    let mut outcome = None;
    let events = map_line(line, &mut outcome);
    (events, outcome)
}

#[test]
fn parses_assistant_text_from_real_output() {
    let (events, _) = map(REAL_ASSISTANT);
    assert_eq!(events, vec![AgentEvent::Text("OK".to_string())]);
}

/// A subagent's messages must not be mistaken for the main conversation.
#[test]
fn subagent_output_is_not_emitted_as_main_text() {
    let sub = REAL_ASSISTANT.replace(
        r#""parent_tool_use_id":null"#,
        r#""parent_tool_use_id":"toolu_1""#,
    );
    assert_ne!(sub, REAL_ASSISTANT, "fixture must actually differ");
    let (events, _) = map(&sub);
    assert!(
        events.is_empty(),
        "subagent text must not surface as main-conversation output; got {events:?}"
    );
}

#[test]
fn parses_subscription_quota_signal() {
    let (events, _) = map(REAL_RATE_LIMIT);
    assert_eq!(
        events,
        vec![AgentEvent::Quota {
            status: "allowed".to_string(),
            kind: "five_hour".to_string(),
            resets_at: 1_786_575_600,
            using_overage: false,
        }]
    );
}

/// Guards the field-order hazard: `type` is not the first key on this line.
#[test]
fn a_failed_turn_reports_what_the_agent_said_about_it() {
    let (_, outcome) = map(FAILED_RESULT);
    let outcome = outcome.expect("the result line produces an outcome");
    assert!(outcome.is_error);
    let said = outcome.failure();
    assert!(
        said.contains("tool use was interrupted"),
        "the agent's own explanation must reach the operator: {said}"
    );
    assert!(
        said.contains("error_during_execution"),
        "and so must the category: {said}"
    );
    assert!(
        !said.contains("gave no reason"),
        "it DID give a reason: {said}"
    );
}

#[test]
fn a_successful_turn_carries_no_detail() {
    // On success `result` repeats the streamed reply; carrying it would
    // print every answer twice.
    let (_, outcome) = map(REAL_RESULT);
    assert_eq!(outcome.expect("outcome").detail, None);
}

#[test]
fn parses_result_line_despite_type_being_last() {
    let (events, outcome) = map(REAL_RESULT);
    let outcome = outcome.expect("result line must yield a turn outcome");
    assert_eq!(outcome.stop_reason, "end_turn");
    assert!(!outcome.is_error);
    assert_eq!(outcome.session_id, "393279e4-f6e9-4d64-bd17-57926443ef94");
    assert_eq!(
        events,
        vec![AgentEvent::Usage {
            input_tokens: 1,
            output_tokens: 4,
            cache_creation_tokens: 36371,
            cache_read_tokens: 23432,
        }]
    );
}

/// A result line from a real two-request turn (`claude -p … --output-format
/// stream-json --verbose`, 2026-09-25), trimmed to the fields read here.
/// The top-level usage is the SUM of both requests; `iterations` holds the
/// last request only; `modelUsage` carries the window.
const REAL_TWO_REQUEST_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":2,"stop_reason":"end_turn","session_id":"sess-probe","total_cost_usd":0.19127059999999999,"usage":{"input_tokens":4,"cache_creation_input_tokens":22197,"cache_read_input_tokens":55293,"output_tokens":131,"iterations":[{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":38663,"cache_creation_input_tokens":164,"type":"message"}]},"modelUsage":{"claude-opus-5-5":{"inputTokens":4,"outputTokens":131,"cacheReadInputTokens":55293,"cacheCreationInputTokens":22197,"webSearchRequests":0,"costUSD":0.19127059999999999,"contextWindow":1000000,"maxOutputTokens":128000,"thinkingTokens":0,"canonicalModel":"claude-opus-5-5","provider":"firstParty","costBasis":"list"}},"result":"banana"}"#;

/// **CONTEXT USED IS THE LAST REQUEST, NOT THE TURN'S SUM.**
///
/// The sum counts the same cached prefix once per request — 77 494 input
/// tokens here for a context that held 38 829 — so it would show a window
/// twice as full as it is. The window is the one the CLI reported.
#[test]
fn a_result_reports_the_last_requests_context_and_the_reported_window() {
    let (events, _) = map(REAL_TWO_REQUEST_RESULT);
    let context: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Context { .. }))
        .collect();
    assert_eq!(
        context,
        vec![&AgentEvent::Context {
            used: 2 + 38_663 + 164 + 4,
            size: 1_000_000,
        }]
    );
}

/// With more than one model on the result, the window is the one of the
/// model that held the conversation: the one given the most input. A side
/// model sees a fragment of it. (The second entry is added to the real
/// line for this test.)
#[test]
fn the_window_is_the_conversations_model_not_a_side_one() {
    let line = REAL_TWO_REQUEST_RESULT.replace(
        r#""modelUsage":{"#,
        r#""modelUsage":{"side-model":{"inputTokens":900,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"contextWindow":200000},"#,
    );
    let (events, _) = map(&line);
    assert!(
        events.contains(&AgentEvent::Context {
            used: 38_833,
            size: 1_000_000
        }),
        "{events:?}"
    );
}

/// No window reported, no context event — the token counts still go out
/// as [`AgentEvent::Usage`].
#[test]
fn no_window_means_no_context_event() {
    let (events, _) = map(REAL_RESULT);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Context { .. })),
        "{events:?}"
    );
}

/// The final text must be emitted exactly once. The result line repeats it,
/// so counting both duplicates every reply the user sees.
#[test]
fn final_text_is_not_emitted_twice() {
    let mut outcome = None;
    let mut texts = Vec::new();
    for line in [REAL_ASSISTANT, REAL_RESULT] {
        for e in map_line(line, &mut outcome) {
            if let AgentEvent::Text(t) = e {
                texts.push(t);
            }
        }
    }
    assert_eq!(
        texts,
        vec!["OK".to_string()],
        "assistant blocks and the result line both carry the reply; only one may be emitted"
    );
}

/// An unrecognised line must be skipped, not abort the turn.
#[test]
fn unknown_event_type_is_skipped() {
    let (events, outcome) = map(r#"{"type":"some_future_event","payload":{}}"#);
    assert!(events.is_empty());
    assert!(outcome.is_none());
}

#[test]
fn malformed_line_does_not_panic() {
    let (events, outcome) = map("not json at all");
    assert!(events.is_empty());
    assert!(outcome.is_none());
}

/// Drive the stream loop the way a real turn does, over canned bytes.
fn drive(stream: &str) -> (Vec<AgentEvent>, Option<String>, Option<TurnOutcome>) {
    let mut session = None;
    let mut seen = Vec::new();
    let outcome = {
        let mut sink = |e: &AgentEvent| seen.push(e.clone());
        consume_stream(
            std::io::BufReader::new(std::io::Cursor::new(stream.as_bytes())),
            &mut session,
            &mut sink,
            None,
        )
        .expect("a well-formed stream must not error")
    };
    (seen, session, outcome)
}

/// A whole turn, in the order and shape the real CLI emits it — including
/// the hook events that precede `init` when the agent loads user config.
const REAL_SESSION: &str = concat!(
    r#"{"type":"system","subtype":"hook_started","hook_id":"b5d0"}"#,
    "\n",
    r#"{"type":"system","subtype":"hook_response","hook_id":"b5d0"}"#,
    "\n",
    r#"{"type":"system","subtype":"init","session_id":"sess-real","model":"claude-opus-5[1m]"}"#,
    "\n",
    "\n",
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OK"}]},"parent_tool_use_id":null}"#,
    "\n",
    r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1786575600,"rateLimitType":"five_hour","isUsingOverage":false}}"#,
    "\n",
    r#"{"is_error":false,"stop_reason":"end_turn","session_id":"sess-real","usage":{"input_tokens":1,"output_tokens":4,"cache_creation_input_tokens":36371,"cache_read_input_tokens":23432},"type":"result"}"#,
    "\n",
);

/// The stream loop must turn a real turn into the right events, in order,
/// and skip the blank line without treating it as data.
#[test]
fn consumes_a_real_turn_into_ordered_events() {
    let (events, session, outcome) = drive(REAL_SESSION);

    assert_eq!(
        events,
        vec![
            AgentEvent::SessionStarted {
                session_id: "sess-real".to_string(),
                model: "claude-opus-5[1m]".to_string(),
            },
            AgentEvent::Text("OK".to_string()),
            AgentEvent::Quota {
                status: "allowed".to_string(),
                kind: "five_hour".to_string(),
                resets_at: 1_786_575_600,
                using_overage: false,
            },
            AgentEvent::Usage {
                input_tokens: 1,
                output_tokens: 4,
                cache_creation_tokens: 36371,
                cache_read_tokens: 23432,
            },
        ],
        "hook events must be dropped and the rest emitted in stream order"
    );

    assert_eq!(session.as_deref(), Some("sess-real"));
    let outcome = outcome.expect("a result line must produce an outcome");
    assert_eq!(outcome.stop_reason, "end_turn");
    assert!(!outcome.is_error);
}

/// The session id must be captured even if the turn dies before the result
/// line — otherwise a crashed turn forces the next one to start fresh and
/// re-pay the whole prompt prefix.
#[test]
fn session_is_captured_even_when_the_turn_never_completes() {
    let truncated = REAL_SESSION
        .lines()
        .take(3) // hook, hook, init — then the agent dies
        .collect::<Vec<_>>()
        .join("\n");

    let (_, session, outcome) = drive(&truncated);

    assert_eq!(
        session.as_deref(),
        Some("sess-real"),
        "a turn that dies after init must still leave a resumable session"
    );
    assert!(
        outcome.is_none(),
        "no result line means no outcome — the caller must see the failure"
    );
}

/// Garbage in the middle of a stream must not abort the turn; the CLI adds
/// event types over time and an unknown line is not a failure.
#[test]
fn a_corrupt_line_does_not_abort_the_turn() {
    let mut polluted = String::from(REAL_SESSION);
    polluted = polluted.replace(
        r#"{"type":"rate_limit_event""#,
        r#"{{{NOT JSON}}}
{"type":"rate_limit_event""#,
    );

    let (events, _, outcome) = drive(&polluted);
    assert!(
        outcome.is_some(),
        "the turn must still complete despite an unparseable line"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Text(t) if t == "OK")));
}

#[test]
fn session_id_is_captured_for_resume() {
    let (events, _) = map(
        r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-opus-5[1m]"}"#,
    );
    assert_eq!(
        events,
        vec![AgentEvent::SessionStarted {
            session_id: "abc-123".to_string(),
            model: "claude-opus-5[1m]".to_string(),
        }]
    );
}

/// **What actually ran, from the billing record.** Captured from a real
/// turn: the request named a model that does not exist, the CLI warned, and
/// the FALLBACK answered — so `modelUsage` and the requested name disagree,
/// which is exactly the case attribution has to get right.
#[test]
fn the_served_model_comes_from_the_billing_record_not_the_request() {
    let real = r#"{"type":"result","is_error":false,"session_id":"s-1",
        "stop_reason":"end_turn","result":"OK",
        "usage":{"input_tokens":1,"output_tokens":4},
        "modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":1,"outputTokens":4}}}"#;
    let (_, outcome) = map(real);
    let outcome = outcome.expect("a result line produces an outcome");
    assert_eq!(
        outcome.served_by,
        vec!["claude-haiku-4-5-20251001".to_string()]
    );
}

/// **Not reported is empty, never guessed.** A backend that says nothing
/// about the model must not have the requested one filled in for it.
#[test]
fn a_result_without_a_billing_record_reports_no_model() {
    let (_, outcome) = map(REAL_RESULT);
    assert!(
        outcome.expect("an outcome").served_by.is_empty(),
        "an absent billing record must stay absent"
    );
}

/// A summarized thinking delta is the model's reasoning; an empty one —
/// the default display — is nothing.
#[test]
fn a_thinking_delta_becomes_a_thought_and_an_empty_one_nothing() {
    let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"This is a","estimated_tokens":null}},"session_id":"c2","parent_tool_use_id":null,"thinking_display":"summarized"}"#;
    let (events, _) = map(line);
    assert!(
        matches!(events.as_slice(), [AgentEvent::Thinking(t)] if t == "This is a"),
        "{events:?}"
    );
    let empty = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}},"session_id":"d4","parent_tool_use_id":null,"thinking_display":"updates"}"#;
    assert!(map(empty).0.is_empty());
}

/// One request to the bridge, as the tool server sends it.
fn raw_call(addr: &str, token: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    use std::io::Write as _;
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    let body = serde_json::json!({"token": token, "op": "call", "name": name, "input": input});
    writeln!(stream, "{body}").expect("send");
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).expect("reply");
    serde_json::from_str(&line).expect("json reply")
}

/// **A TOOL CALL MID-TURN IS RUN BY incode's TOOLBOX, AND SHOWN ONCE.**
///
/// The CLI's stream carries its own record of the call it forwarded — the
/// `tool_use` and `tool_result` blocks. Those are not shown: the call is
/// shown as incode ran it, under incode's name for the tool.
#[test]
fn a_tool_call_mid_turn_runs_in_incodes_toolbox_and_is_shown_once() {
    let ws = tempfile::tempdir().expect("ws");
    let mut tools = crate::toolbox::Toolbox::new(ws.path(), true);
    let (to, from) = std::sync::mpsc::channel();
    let bridge =
        crate::tool_bridge::Bridge::open(to.clone(), Incoming::Tool, None).expect("bridge");
    let [(_, addr), (_, token)] = bridge.env();

    let cli = std::thread::spawn(move || {
        let say = |line: &str| {
            let _ = to.send(Incoming::Line(line.to_string()));
        };
        say(r#"{"type":"system","subtype":"init","session_id":"s-bridge","model":"m"}"#);
        say(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"mcp__incode__Write","input":{"path":"hello.txt","content":"hi"}}]},"parent_tool_use_id":null}"#,
        );
        let reply = raw_call(
            &addr,
            &token,
            "Write",
            serde_json::json!({"path": "hello.txt", "content": "hi"}),
        );
        say(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"wrote it","is_error":false}]},"parent_tool_use_id":null}"#,
        );
        say(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]},"parent_tool_use_id":null}"#,
        );
        say(
            r#"{"type":"result","is_error":false,"stop_reason":"end_turn","session_id":"s-bridge","usage":{"input_tokens":1,"output_tokens":1}}"#,
        );
        let _ = to.send(Incoming::Ended);
        reply
    });

    let mut session = None;
    let mut seen = Vec::new();
    let mut calls = 0;
    let outcome = consume(
        &from,
        &mut session,
        &mut |e: &AgentEvent| seen.push(e.clone()),
        None,
        &mut tools,
        &mut calls,
    )
    .expect("the turn completes");
    drop(bridge);
    let reply = cli.join().expect("the fake CLI");

    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("hello.txt")).expect("written by incode"),
        "hi"
    );
    assert!(outcome.is_some_and(|o| !o.is_error));
    let uses: Vec<&AgentEvent> = seen
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolUse { .. }))
        .collect();
    assert_eq!(uses.len(), 1, "one call, shown once: {seen:?}");
    assert!(
        matches!(uses[0], AgentEvent::ToolUse { name, id, .. } if name == "Write" && id == "incode-1"),
        "shown under incode's own name and id: {seen:?}"
    );
    let results: Vec<&AgentEvent> = seen
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .collect();
    assert_eq!(results.len(), 1, "{seen:?}");
    assert!(
        matches!(results[0], AgentEvent::ToolResult { output, is_error: false, .. } if !output.contains("wrote it")),
        "the result is incode's, not the CLI's echo: {seen:?}"
    );
}

/// **A QUIET TURN STILL STOPS.** Nothing arrives — the model thinking, a
/// long command — and the stop lands within the poll, not at the next
/// line the CLI happens to print.
#[test]
fn a_quiet_turn_stops_promptly_when_cancelled() {
    let token = crate::runtime::cancel::CancelToken::new();
    let (to, from) = std::sync::mpsc::channel::<Incoming>();
    let stopper = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        stopper.cancel();
    });
    let mut tools = crate::toolbox::Toolbox::new(std::path::Path::new("."), true);
    let started = std::time::Instant::now();
    let err = consume(
        &from,
        &mut None,
        &mut |_| {},
        Some(&token),
        &mut tools,
        &mut 0,
    )
    .expect_err("a stopped turn is not a finished one");
    assert_eq!(err.to_string(), CANCELLED);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    drop(to);
}

// ---------------------------------------------------------------------------
// Pinned behaviour: every wire shape `map_line` reads, with the events and
// the outcome each produces.
// ---------------------------------------------------------------------------

/// One pinned line: the events it becomes and the outcome it writes.
type Case = (&'static str, Vec<AgentEvent>, Option<TurnOutcome>);

fn text(t: &str) -> AgentEvent {
    AgentEvent::Text(t.to_string())
}

fn started(s: &str, m: &str) -> AgentEvent {
    AgentEvent::SessionStarted {
        session_id: s.to_string(),
        model: m.to_string(),
    }
}

/// **EVERY WIRE SHAPE, AND WHAT IT BECOMES.** One line per branch of the
/// mapping — each event type, the subagent and empty cases, every way a
/// failed result can describe itself, and lines that do not parse.
#[test]
fn the_map_line_corpus_is_pinned() {
    let cases = [
        system_cases(),
        stream_cases(),
        message_and_quota_cases(),
        result_cases(),
        context_cases(),
        unparsed_cases(),
    ]
    .concat();
    assert_eq!(cases.len(), 47, "the corpus lost or gained a line");
    for (line, events, outcome) in cases {
        let mut got = None;
        assert_eq!(map_line(line, &mut got), events, "events of {line}");
        assert_eq!(got, outcome, "outcome of {line}");
    }
}

/// `system` lines: only `init` says anything.
fn system_cases() -> Vec<Case> {
    vec![
        (
            r#"{"type":"system","subtype":"init","session_id":"s1","model":"m1"}"#,
            vec![started("s1", "m1")],
            None,
        ),
        (
            r#"{"type":"system","subtype":"init"}"#,
            vec![started("", "unknown")],
            None,
        ),
        (
            r#"  {"type":"system","subtype":"init","session_id":"s2"}  "#,
            vec![started("s2", "unknown")],
            None,
        ),
        (
            r#"{"type":"system","subtype":"hook_started","hook_id":"h"}"#,
            Vec::new(),
            None,
        ),
        (r#"{"type":"system"}"#, Vec::new(), None),
    ]
}

/// Partial-message frames: only non-empty main-conversation deltas.
fn stream_cases() -> Vec<Case> {
    vec![
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}},"parent_tool_use_id":null}"#,
            vec![AgentEvent::TextDelta("hi".to_string())],
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":""}}}"#,
            Vec::new(),
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta"}}}"#,
            Vec::new(),
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"why"}}}"#,
            vec![AgentEvent::Thinking("why".to_string())],
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":""}}}"#,
            Vec::new(),
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}},"parent_tool_use_id":"toolu_9"}"#,
            Vec::new(),
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"x"}},"parent_tool_use_id":"toolu_9"}"#,
            Vec::new(),
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0},"parent_tool_use_id":null}"#,
            Vec::new(),
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}}"#,
            Vec::new(),
            None,
        ),
        (
            r#"{"type":"stream_event","event":{"type":"content_block_start","delta":{"type":"text_delta","text":"x"}}}"#,
            Vec::new(),
            None,
        ),
        (r#"{"type":"stream_event"}"#, Vec::new(), None),
    ]
}

/// Whole messages, the tool-result lines incode already showed, and the
/// subscription quota.
fn message_and_quota_cases() -> Vec<Case> {
    vec![
        (
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"},{"type":"tool_use","id":"t1","name":"Write","input":{}},{"type":"thinking","thinking":"hm"},{"type":"text","text":"b"}]},"parent_tool_use_id":null}"#,
            vec![text("a"), text("b")],
            None,
        ),
        (
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":""}]}}"#,
            vec![text("")],
            None,
        ),
        (r#"{"type":"assistant","message":{}}"#, Vec::new(), None),
        (
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"sub"}]},"parent_tool_use_id":"toolu_2"}"#,
            Vec::new(),
            None,
        ),
        (r#"{"type":"assistant"}"#, Vec::new(), None),
        // user lines carry tool results incode already showed
        (
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            Vec::new(),
            None,
        ),
        // rate_limit_event
        (
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#,
            vec![AgentEvent::Quota {
                status: "rejected".to_string(),
                kind: String::new(),
                resets_at: 0,
                using_overage: false,
            }],
            None,
        ),
        (
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":17,"rateLimitType":"seven_day","isUsingOverage":true}}"#,
            vec![AgentEvent::Quota {
                status: "allowed_warning".to_string(),
                kind: "seven_day".to_string(),
                resets_at: 17,
                using_overage: true,
            }],
            None,
        ),
        (r#"{"type":"rate_limit_event"}"#, Vec::new(), None),
    ]
}

/// `result` lines: the outcome, and every way a failure describes itself.
fn result_cases() -> Vec<Case> {
    vec![
        (
            r#"{"type":"result","is_error":false,"session_id":"r1"}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r1", "unknown", false, None, &[])),
        ),
        (
            r#"{"type":"result","is_error":false,"session_id":"r1","stop_reason":"max_tokens","result":"the reply","subtype":"success","usage":{"input_tokens":3,"output_tokens":4,"cache_creation_input_tokens":5,"cache_read_input_tokens":6}}"#,
            vec![usage_of(3, 4, 5, 6)],
            Some(outcome_of("r1", "max_tokens", false, None, &[])),
        ),
        (
            r#"{"type":"result","is_error":true,"session_id":"r2","stop_reason":"stop_sequence","subtype":"error_x","result":"said"}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of(
                "r2",
                "stop_sequence",
                true,
                Some("error_x: said"),
                &[],
            )),
        ),
        (
            r#"{"type":"result","is_error":true,"session_id":"r2","subtype":"error_max_turns"}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of(
                "r2",
                "unknown",
                true,
                Some("error_max_turns"),
                &[],
            )),
        ),
        (
            r#"{"type":"result","is_error":true,"session_id":"r2","result":"boom"}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r2", "unknown", true, Some("boom"), &[])),
        ),
        (
            r#"{"type":"result","is_error":true,"session_id":"r2"}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r2", "unknown", true, None, &[])),
        ),
        (
            r#"{"type":"result","is_error":true,"session_id":"r2","subtype":"  ","result":" "}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r2", "unknown", true, None, &[])),
        ),
        (
            r#"{"type":"result","is_error":true,"session_id":"r2","subtype":" k ","result":" t "}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r2", "unknown", true, Some("k: t"), &[])),
        ),
    ]
}

/// `result` lines with a context window or without one, the billed models,
/// and results missing a required field.
fn context_cases() -> Vec<Case> {
    vec![
        (
            r#"{"type":"result","is_error":false,"session_id":"r3","usage":{"input_tokens":1,"iterations":[{"input_tokens":9},{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}]},"modelUsage":{"b-model":{"inputTokens":50,"contextWindow":200000},"a-model":{"inputTokens":1,"contextWindow":1000}}}"#,
            vec![
                usage_of(1, 0, 0, 0),
                AgentEvent::Context {
                    used: 135,
                    size: 200_000,
                },
            ],
            Some(outcome_of(
                "r3",
                "unknown",
                false,
                None,
                &["a-model", "b-model"],
            )),
        ),
        (
            r#"{"type":"result","is_error":false,"session_id":"r4","usage":{"iterations":[]},"modelUsage":{"m":{"contextWindow":200000}}}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r4", "unknown", false, None, &["m"])),
        ),
        (
            r#"{"type":"result","is_error":false,"session_id":"r5","usage":{"iterations":[{"input_tokens":1}]},"modelUsage":{"m":{"contextWindow":0}}}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r5", "unknown", false, None, &["m"])),
        ),
        (
            r#"{"type":"result","is_error":false,"session_id":"r6","usage":{"iterations":[{"input_tokens":1}]},"modelUsage":{"m":{"inputTokens":1}}}"#,
            vec![usage_of(0, 0, 0, 0)],
            Some(outcome_of("r6", "unknown", false, None, &["m"])),
        ),
        (r#"{"type":"result","is_error":false}"#, Vec::new(), None),
        (r#"{"type":"result","session_id":"r7"}"#, Vec::new(), None),
    ]
}

/// Lines that are unknown or do not parse: nothing, and no outcome.
fn unparsed_cases() -> Vec<Case> {
    vec![
        (
            r#"{"type":"some_future_event","payload":{}}"#,
            Vec::new(),
            None,
        ),
        ("not json", Vec::new(), None),
        ("", Vec::new(), None),
        ("[]", Vec::new(), None),
        ("null", Vec::new(), None),
        ("{}", Vec::new(), None),
        (r#"{"type":42}"#, Vec::new(), None),
        (
            r#"{"type":"assistant","message":{"content":[{"type":"text"}]}}"#,
            Vec::new(),
            None,
        ),
    ]
}

/// Only a result line touches the outcome; every other line leaves an
/// earlier one in place.
#[test]
fn only_a_result_line_writes_the_outcome() {
    let earlier = outcome_of("before", "end_turn", false, None, &[]);
    for line in [
        r#"{"type":"system","subtype":"init","session_id":"s"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"}]}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        "not json",
    ] {
        let mut outcome = Some(earlier.clone());
        map_line(line, &mut outcome);
        assert_eq!(outcome.as_ref(), Some(&earlier), "{line}");
    }
}
