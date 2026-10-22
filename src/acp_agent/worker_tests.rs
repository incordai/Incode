//! Workers as sub-agents, a command's output while it runs, the model's
//! reasoning, pictures in a prompt and `_meta.incode` — each driven by a fake
//! ACP client over a real pipe, with incode's own Toolbox doing the work.

use std::time::Instant;

use serde_json::{json, Value};

use super::tests::{updates_of, Behaviour, Client, Script};
use super::transport;
use crate::agent::AgentEvent;
use crate::permissions::PermissionMode;

/// The command [`Behaviour::Shell`] runs: a line, two seconds of nothing,
/// another line.
#[cfg(windows)]
const SHELL_COMMAND: &str = "echo first&& ping -n 3 127.0.0.1 >nul&& echo second";
#[cfg(not(windows))]
const SHELL_COMMAND: &str = "echo first; sleep 2; echo second";

/// One call through the toolbox, announced and answered as a backend does.
fn call(
    tools: &crate::toolbox::Toolbox,
    on_event: &mut dyn FnMut(&AgentEvent),
    id: &str,
    name: &str,
    input: &Value,
) {
    on_event(&AgentEvent::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: input.to_string(),
    });
    let (output, is_error) = match tools.run_call(id, name, input) {
        Ok(said) => (said.shown(), false),
        Err(why) => (why, true),
    };
    on_event(&AgentEvent::ToolResult {
        id: id.to_string(),
        output,
        is_error,
    });
}

/// What the fake backend does for the behaviours this file tests.
pub(super) fn act(
    behaviour: Behaviour,
    tools: Option<&crate::toolbox::Toolbox>,
    on_event: &mut dyn FnMut(&AgentEvent),
) {
    if let Behaviour::Thinks = behaviour {
        on_event(&AgentEvent::Thinking("weighing it".to_string()));
        on_event(&AgentEvent::Notice("[pre_compact hook] noted".to_string()));
        on_event(&AgentEvent::TextDelta("decided".to_string()));
        return;
    }
    let Some(tools) = tools else {
        on_event(&AgentEvent::TextDelta("no toolbox".to_string()));
        return;
    };
    match behaviour {
        Behaviour::SubAgent | Behaviour::SubAgentWaits => call(
            tools,
            on_event,
            "a1",
            crate::toolbox::AGENT,
            &json!({"task": "count the files", "role": "explore"}),
        ),
        Behaviour::Shell => call(
            tools,
            on_event,
            "b1",
            crate::toolbox::BASH,
            &json!({"command": SHELL_COMMAND}),
        ),
        Behaviour::Protected => {
            call(
                tools,
                on_event,
                "p1",
                crate::toolbox::WRITE,
                &json!({"path": "tests/probe.txt", "content": "x"}),
            );
            call(
                tools,
                on_event,
                "r1",
                crate::toolbox::READ,
                &json!({"path": "notes.txt"}),
            );
        }
        _ => {}
    }
}

/// A client that has said it follows sub-agents — or not.
fn started(script: Script, subagents: bool) -> Client {
    let mut client = Client::start(
        script,
        PermissionMode::WorkspaceWrite,
        transport::MAX_LINE_BYTES,
    );
    let capabilities = if subagents {
        json!({"subagents": {}})
    } else {
        json!({})
    };
    let reply = client.call(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": capabilities}),
    );
    assert!(reply.get("result").is_some(), "{reply}");
    client
}

fn prompt(client: &mut Client, session: &str, blocks: Value) -> (Vec<Value>, Value) {
    let id = client.request(
        "session/prompt",
        json!({"sessionId": session, "prompt": blocks}),
    );
    client.until_response(id)
}

/// The `session/update`s sent under `session`, in order.
fn under<'a>(messages: &'a [Value], session: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|m| m["method"] == "session/update" && m["params"]["sessionId"] == session)
        .map(|m| &m["params"]["update"])
        .collect()
}

fn position(messages: &[Value], found: impl Fn(&Value) -> bool) -> usize {
    messages
        .iter()
        .position(found)
        .unwrap_or_else(|| panic!("not found in {messages:#?}"))
}

/// **THE `Agent` TOOL'S SUB-AGENT IS A TAB OF ITS OWN.** Announced in the
/// parent's session with `subagent_spawned` — its name from the role, its
/// task — then its reasoning and its reply arrive under ITS session id, and
/// `subagent_state_update` says `completed` in the parent's once it ends. The
/// parent's call gets the sub-agent's answer. A client that did not advertise
/// the extension is sent none of it, and the call still completes.
#[test]
fn a_sub_agent_is_announced_and_its_work_arrives_under_its_own_id() {
    let scratch = tempfile::tempdir().expect("scratch");
    let script = Script::new(Behaviour::SubAgent).with_sub_cli(
        crate::agent::claude_code::tests::stand_in_cli(scratch.path(), false),
    );
    let mut client = started(script.clone(), true);
    let ws = tempfile::tempdir().expect("ws");
    let session = client.open_session(ws.path());
    let (seen, reply) = prompt(
        &mut client,
        &session,
        json!([{"type": "text", "text": "hand it on"}]),
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");

    let parent = under(&seen, &session);
    let spawned = parent
        .iter()
        .find(|u| u["sessionUpdate"] == "subagent_spawned")
        .unwrap_or_else(|| panic!("no subagent_spawned: {seen:#?}"));
    assert_eq!(spawned["name"], "explore · sub-agent", "{spawned}");
    assert_eq!(spawned["task"], "count the files", "{spawned}");
    assert_eq!(spawned["capabilities"], json!({}), "{spawned}");
    let sub = spawned["subagentSessionId"]
        .as_str()
        .expect("an id")
        .to_string();
    assert_ne!(sub, session);

    let own = under(&seen, &sub);
    assert!(
        own.iter()
            .any(|u| u["sessionUpdate"] == "agent_thought_chunk"
                && u["content"]["text"] == "looking around"),
        "{own:#?}"
    );
    let said: String = own
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert_eq!(said, "three files", "streamed once, not twice: {own:#?}");

    let ended = parent
        .iter()
        .find(|u| u["sessionUpdate"] == "subagent_state_update")
        .unwrap_or_else(|| panic!("no state update: {seen:#?}"));
    assert_eq!(ended["subagentSessionId"], sub.as_str());
    assert_eq!(ended["state"], "completed", "{ended}");
    // Announced before its work, ended after it.
    let announced = position(&seen, |m| {
        m["params"]["update"]["sessionUpdate"] == "subagent_spawned"
    });
    let first_own = position(&seen, |m| m["params"]["sessionId"] == sub.as_str());
    let last_own = seen
        .iter()
        .rposition(|m| m["params"]["sessionId"] == sub.as_str())
        .expect("its work");
    let over = position(&seen, |m| {
        m["params"]["update"]["sessionUpdate"] == "subagent_state_update"
    });
    assert!(announced < first_own && last_own < over, "{seen:#?}");
    let result = parent
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "a1")
        .unwrap_or_else(|| panic!("no result: {seen:#?}"));
    assert_eq!(result["status"], "completed", "{result}");
    assert!(result.to_string().contains("three files"), "{result}");
    client.close();

    // Not advertised: nothing announced, and the work still done.
    let mut plain = started(script, false);
    let session = plain.open_session(ws.path());
    let (seen, reply) = prompt(
        &mut plain,
        &session,
        json!([{"type": "text", "text": "hand it on"}]),
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
    assert!(
        !seen.iter().any(|m| m.to_string().contains("subagent_")),
        "{seen:#?}"
    );
    assert!(
        updates_of(&seen)
            .iter()
            .any(|u| u["toolCallId"] == "a1" && u["status"] == "completed"),
        "{seen:#?}"
    );
    plain.close();
}

/// **A CANCEL FROM THE PARENT CANCELS ITS WORKER.** The sub-agent pauses
/// half a minute; the client cancels once it is announced; the worker is
/// stopped at once, its tab says `cancelled`, and the prompt answers
/// `cancelled` — long before the stand-in would have finished.
#[test]
fn cancelling_the_prompt_cancels_its_sub_agent() {
    let scratch = tempfile::tempdir().expect("scratch");
    let script = Script::new(Behaviour::SubAgentWaits).with_sub_cli(
        crate::agent::claude_code::tests::stand_in_cli(scratch.path(), true),
    );
    let mut client = started(script, true);
    let ws = tempfile::tempdir().expect("ws");
    let session = client.open_session(ws.path());
    let id = client.request(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "hand it on"}]}),
    );
    let mut seen = Vec::new();
    loop {
        let message = client.next();
        let spawned = message["params"]["update"]["sessionUpdate"] == "subagent_spawned";
        seen.push(message);
        if spawned {
            break;
        }
    }
    let asked = Instant::now();
    client.notify("session/cancel", json!({"sessionId": session}));
    let (rest, reply) = client.until_response(id);
    seen.extend(rest);
    assert_eq!(reply["result"]["stopReason"], "cancelled", "{reply}");
    assert!(
        asked.elapsed().as_secs() < 15,
        "the worker was waited out, not stopped: {:?}",
        asked.elapsed()
    );
    let ended = updates_of(&seen)
        .into_iter()
        .find(|u| u["sessionUpdate"] == "subagent_state_update")
        .unwrap_or_else(|| panic!("no state update: {seen:#?}"));
    assert_eq!(ended["state"], "cancelled", "{ended}");
    client.close();
}

/// **A RUNNING COMMAND'S OUTPUT ARRIVES WHILE IT RUNS.** The first line is
/// shown as an `in_progress` update of the call before the command has
/// finished, and the result follows with the whole output.
#[test]
fn a_running_commands_output_arrives_before_its_result() {
    let mut client = started(Script::new(Behaviour::Shell), false);
    let ws = tempfile::tempdir().expect("ws");
    let session = client.open_session(ws.path());
    let (seen, reply) = prompt(
        &mut client,
        &session,
        json!([{"type": "text", "text": "run it"}]),
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
    let calls: Vec<&Value> = updates_of(&seen)
        .into_iter()
        .filter(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "b1")
        .collect();
    let live = calls
        .iter()
        .position(|u| {
            u["status"] == "in_progress"
                && u.to_string().contains("first")
                && !u.to_string().contains("second")
        })
        .unwrap_or_else(|| panic!("no output while it ran: {calls:#?}"));
    let done = calls
        .iter()
        .position(|u| u["status"] == "completed")
        .unwrap_or_else(|| panic!("no result: {calls:#?}"));
    assert!(live < done, "{calls:#?}");
    assert!(calls[done].to_string().contains("second"), "{calls:#?}");
    client.close();
}

/// **THE MODEL'S REASONING IS A THOUGHT, NOT THE REPLY.**
#[test]
fn reasoning_arrives_as_a_thought_chunk() {
    let mut client = started(Script::new(Behaviour::Thinks), false);
    let ws = tempfile::tempdir().expect("ws");
    let session = client.open_session(ws.path());
    let (seen, _) = prompt(
        &mut client,
        &session,
        json!([{"type": "text", "text": "think"}]),
    );
    let updates = updates_of(&seen);
    assert!(
        updates
            .iter()
            .any(|u| u["sessionUpdate"] == "agent_thought_chunk"
                && u["content"]["text"] == "weighing it"),
        "{updates:#?}"
    );
    let said: String = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert!(
        said.contains("decided") && !said.contains("weighing"),
        "{said}"
    );
    client.close();
}

/// **incode's OWN NOTICE IS SHOWN, AND IS NOT THE REPLY** — what an
/// operator's `pre_compact` hook printed mid-turn arrives as a thought.
///
/// Exercises the `AgentEvent::Notice` arm in `src/acp_agent/workers.rs`
/// (`acp_agent::workers`), which turns it into an `agent_thought_chunk`, and
/// the REPL arm in `cli::agent_turn` standing aside for the observer.
#[test]
fn a_notice_arrives_as_a_thought_chunk_not_as_the_reply() {
    let mut client = started(Script::new(Behaviour::Thinks), false);
    let ws = tempfile::tempdir().expect("ws");
    let session = client.open_session(ws.path());
    let (seen, _) = prompt(
        &mut client,
        &session,
        json!([{"type": "text", "text": "think"}]),
    );
    let updates = updates_of(&seen);
    assert!(
        updates
            .iter()
            .any(|u| u["sessionUpdate"] == "agent_thought_chunk"
                && u["content"]["text"] == "[pre_compact hook] noted"),
        "{updates:#?}"
    );
    let said: String = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert!(!said.contains("pre_compact"), "{said}");
    client.close();
}

fn details(update: &Value) -> &Value {
    &update["_meta"]["incode"]
}

/// **`_meta.incode` CARRIES WHAT INCODE KNOWS, AND NOTHING IT DOES NOT.** A
/// write the guards refused says so — who, the pattern, the path; a read the
/// guards never look at carries no decision; the model is left out when
/// none was chosen; the prompt's answer names the role and the agent.
///
/// The protected write is put to the client first, naming the file, and the
/// client rejects it — so the refusal is still the guards'.
#[test]
fn incode_meta_is_present_only_when_known() {
    let mut client = started(Script::new(Behaviour::Protected), false);
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("notes.txt"), "hello\n").expect("notes");
    let session = client.open_session(ws.path());
    let id = client.request(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "write the probe"}]}),
    );
    let mut seen = Vec::new();
    let asked = loop {
        let message = client.next();
        if message["method"] == "session/request_permission" {
            break message;
        }
        seen.push(message);
    };
    assert!(
        asked["params"]["toolCall"]["title"]
            .as_str()
            .is_some_and(|t| t.contains("tests/probe.txt")),
        "{asked}"
    );
    client.send_raw(
        &json!({"jsonrpc": "2.0", "id": asked["id"],
                "result": {"outcome": {"outcome": "selected", "optionId": "reject"}}})
        .to_string(),
    );
    let (rest, reply) = client.until_response(id);
    seen.extend(rest);
    let updates = updates_of(&seen);
    let find = |kind: &str, id: &str| -> &Value {
        updates
            .iter()
            .find(|u| u["sessionUpdate"] == kind && u["toolCallId"] == id)
            .unwrap_or_else(|| panic!("no {kind} {id}: {updates:#?}"))
    };
    let asked = details(find("tool_call", "p1"));
    assert_eq!(asked["role"], "plan", "{asked}");
    assert_eq!(asked["agent"], "claude-code", "{asked}");
    for unknown in ["model", "lane", "guard", "watchman"] {
        assert!(asked.get(unknown).is_none(), "{unknown} invented: {asked}");
    }
    let refused = find("tool_call_update", "p1");
    assert_eq!(refused["status"], "failed", "{refused}");
    assert_eq!(
        details(refused)["guard"],
        json!({"by": "guards", "decision": "blocked", "rule": "tests/", "path": "tests/probe.txt"}),
        "{refused}"
    );
    let read = details(find("tool_call_update", "r1"));
    assert_eq!(read["role"], "plan", "{read}");
    assert!(read.get("guard").is_none(), "{read}");
    assert!(read.get("watchman").is_none(), "{read}");
    let answered = &reply["result"]["_meta"]["incode"];
    assert_eq!(answered["role"], "plan", "{reply}");
    assert_eq!(answered["agent"], "claude-code", "{reply}");
    assert!(answered.get("model").is_none(), "{reply}");
    assert!(!ws.path().join("tests").join("probe.txt").exists());
    client.close();
}

/// A PNG of `width` x `height`, as a prompt's image block.
fn picture(width: u32, height: u32) -> Value {
    json!({"type": "image", "mimeType": "image/png", "data": super::present::tests::png_base64(width, height)})
}

/// **A PICTURE REACHES THE AGENT; ONE PAST THE BOUNDS IS REFUSED, AND NO
/// TURN RUNS.** The same for a picture sent with a slash command.
#[test]
fn a_picture_reaches_the_agent_and_an_oversized_one_is_refused() {
    let script = Script::new(Behaviour::Named);
    let mut client = started(script.clone(), false);
    let ws = tempfile::tempdir().expect("ws");
    let session = client.open_session(ws.path());
    let (_, reply) = prompt(
        &mut client,
        &session,
        json!([{"type": "text", "text": "what is this"}, picture(4, 3)]),
    );
    assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
    assert_eq!(
        script.pictures.lock().expect("pictures").clone(),
        ["[image 4x3 png]"]
    );
    let turns = script.ran.lock().expect("ran").len();

    let (_, refused) = prompt(
        &mut client,
        &session,
        json!([{"type": "text", "text": "and this"}, picture(crate::toolbox::MAX_IMAGE_EDGE + 1, 1)]),
    );
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(
        refused["error"]["data"]
            .as_str()
            .is_some_and(|d| d.contains("pixels on a side")),
        "{refused}"
    );
    let (_, command) = prompt(
        &mut client,
        &session,
        json!([{"type": "text", "text": "/diff"}, picture(2, 2)]),
    );
    assert_eq!(command["error"]["code"], -32602, "{command}");
    assert_eq!(script.pictures.lock().expect("pictures").len(), 1);
    assert_eq!(script.ran.lock().expect("ran").len(), turns, "a turn ran");
    client.close();
}
