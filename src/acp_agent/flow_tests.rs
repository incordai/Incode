//! The pickers, the client's MCP servers, `/run` as a plan, and loading a
//! session — each driven by a fake ACP client over a real pipe.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::run::LedgerRuns;
use super::tests::{client, prompt_in, text_of, updates_of, Behaviour, Client, Script};
use super::transport;
use crate::permissions::PermissionMode;
use crate::runtime::cancel::CancelToken;

/// How long the fake ledger worker waits for a cancel before finishing on
/// its own — shorter than a test's own wait, so a cancel that never lands
/// shows up as the wrong answer rather than a hang.
const WORKER_PATIENCE: Duration = Duration::from_secs(8);

/// `/run` for the fake sessions: the ledger in the workspace, worked by
/// [`WaitingWorker`] through the same stop watch, loop and report the REPL's
/// `/run` uses.
pub(super) struct FakeRuns;

impl LedgerRuns for FakeRuns {
    fn run(
        &mut self,
        workspace: &Path,
        cancel: &CancelToken,
        _mode: PermissionMode,
        progress: &mut dyn crate::runner::Progress,
    ) {
        let mut ledger = match crate::ledger::Ledger::load(workspace) {
            Ok(ledger) => ledger,
            Err(e) => return crate::cli::render::print_error(&e),
        };
        let _stop = crate::cli::run_ledger::watch_for_stop(cancel, workspace);
        let mut worker = WaitingWorker {
            cancel: cancel.clone(),
            first: true,
        };
        let outcome = crate::cli::run_ledger::work(
            &mut ledger,
            crate::cli::run_ledger::Crew {
                worker: &mut worker,
                owner: &mut crate::runner::NoOwner,
                journal: &mut crate::runner::NoJournal,
                replanner: &mut crate::runner::NoReplan,
                goals: &mut crate::runner::NoGoals,
                lanes: &crate::runner::NoLanes,
            },
            workspace,
            &mut crate::budget::Budget::default(),
            cancel,
            progress,
        );
        crate::cli::run_ledger::report(&outcome, &ledger, workspace);
    }
}

/// Its first task lasts until the run is cancelled (bounded); every other
/// task is done at once.
struct WaitingWorker {
    cancel: CancelToken,
    first: bool,
}

impl crate::runner::TaskWorker for WaitingWorker {
    fn work(&mut self, task: &crate::ledger::Task) -> Result<String, String> {
        if std::mem::take(&mut self.first) {
            let deadline = Instant::now() + WORKER_PATIENCE;
            while !self.cancel.is_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(format!("worked {}", task.id))
    }
}

fn result_of(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

/// The select with `id` among `options`.
fn option<'a>(options: &'a Value, id: &str) -> &'a Value {
    options
        .as_array()
        .and_then(|all| all.iter().find(|o| o["id"] == id))
        .unwrap_or_else(|| panic!("no `{id}` option in {options}"))
}

fn new_session(client: &mut Client, cwd: &Path, servers: Value) -> (String, Value) {
    let reply = client.call("session/new", json!({"cwd": cwd, "mcpServers": servers}));
    let result = result_of(&reply).clone();
    let id = result["sessionId"].as_str().expect("an id").to_string();
    (id, result)
}

/// **THE AGENT PICKER CHANGES WHO SERVES THE NEXT TURN.** `session/new`
/// offers the catalogue's usable agents and the roles; picking `codex` is
/// answered at once with the options as they now stand, and the next turn
/// is served by the agent picked — in this session, not the next one.
#[test]
fn a_picked_agent_serves_the_next_turn_of_this_session() {
    let script = Script::new(Behaviour::Named);
    let mut client = Client::start(
        script.clone(),
        PermissionMode::WorkspaceWrite,
        transport::MAX_LINE_BYTES,
    );
    let dir = tempfile::tempdir().expect("ws");
    let (session, opened) = new_session(&mut client, dir.path(), json!([]));
    let agent = option(&opened["configOptions"], "agent");
    assert_eq!(agent["type"], "select", "{agent}");
    assert_eq!(agent["currentValue"], "claude-code", "{agent}");
    let offered: Vec<&str> = agent["options"]
        .as_array()
        .expect("options")
        .iter()
        .filter_map(|o| o["value"].as_str())
        .collect();
    assert_eq!(offered, ["claude-code", "codex"], "{agent}");
    let role = option(&opened["configOptions"], "role");
    assert_eq!(role["currentValue"], "plan", "{role}");

    let (said, _) = prompt_in(&mut client, &session, "who is there");
    assert!(
        text_of(&said).contains("served by claude-code"),
        "{said:#?}"
    );

    let set = client.call(
        "session/set_config_option",
        json!({"sessionId": session, "configId": "agent", "value": "codex"}),
    );
    let now = option(&result_of(&set)["configOptions"], "agent").clone();
    assert_eq!(now["currentValue"], "codex", "{set}");

    let refused = client.call(
        "session/set_config_option",
        json!({"sessionId": session, "configId": "agent", "value": "not-installed"}),
    );
    assert_eq!(refused["error"]["code"], -32602, "{refused}");

    let (said, reply) = prompt_in(&mut client, &session, "who is there now");
    assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
    assert!(text_of(&said).contains("served by codex"), "{said:#?}");
    let ran = script.ran.lock().expect("ran").clone();
    assert!(
        ran.last().is_some_and(|r| r.starts_with("codex ")),
        "{ran:?}"
    );
    // The mode the session runs under reached the agent switched in.
    assert_eq!(
        script
            .modes
            .lock()
            .expect("modes")
            .last()
            .map(String::as_str),
        Some(PermissionMode::WorkspaceWrite.agent_mode())
    );
    client.close();
}

/// **THE ROLE PICKER DECIDES THE NEXT TURN'S MODEL, AND THE CLIENT IS
/// TOLD.** `explore` is assigned `small-model` in the session's role table;
/// once picked, the next turn asks for it, and a `config_option_update`
/// names it.
#[test]
fn a_picked_role_routes_the_next_turns_model() {
    let script = Script::new(Behaviour::Named);
    let mut client = Client::start(
        script.clone(),
        PermissionMode::WorkspaceWrite,
        transport::MAX_LINE_BYTES,
    );
    let dir = tempfile::tempdir().expect("ws");
    let (session, _) = new_session(&mut client, dir.path(), json!([]));
    let set = client.call(
        "session/set_config_option",
        json!({"sessionId": session, "configId": "role", "value": "explore"}),
    );
    assert_eq!(
        option(&result_of(&set)["configOptions"], "role")["currentValue"],
        "explore"
    );
    let (said, _) = prompt_in(&mut client, &session, "look around");
    assert!(
        text_of(&said).contains("on small-model"),
        "the turn after the pick runs on the role's model: {said:#?}"
    );
    let updates = updates_of(&said);
    let mut told = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "config_option_update");
    let told = told.next().unwrap_or_else(|| {
        // Applied while idle, before the prompt: the update may have come
        // ahead of it.
        panic!("no config_option_update with the prompt: {said:#?}")
    });
    let role = option(&told["configOptions"], "role");
    assert!(
        role["description"]
            .as_str()
            .is_some_and(|d| d.contains("small-model")),
        "{role}"
    );
    client.close();
}

/// **THE CLIENT'S STDIO MCP SERVER IS INCODE'S OWN TOOL.** Started with the
/// session, its tool is called through the Toolbox's gate — shown as
/// incode's own tool call — and refused once the session is read-only,
/// without reaching the server.
#[test]
fn a_clients_mcp_server_is_called_through_the_gate_and_refused_read_only() {
    let logs = tempfile::tempdir().expect("logs");
    let log = logs.path().join("calls.log");
    let spec = crate::toolbox::fake_server("fake", &log);
    let crate::toolbox::Transport::Stdio { command, args, env } = spec.transport else {
        panic!("the fake is a stdio server");
    };
    let server = json!({
        "name": spec.name,
        "command": command,
        "args": args,
        "env": env.iter().map(|(name, value)| json!({"name": name, "value": value})).collect::<Vec<_>>(),
    });
    called_through_the_gate_and_refused_read_only(server, "1 tool(s)", || {
        crate::toolbox::client_server_calls(&log).len()
    });
}

/// **THE CLIENT'S HTTP MCP SERVER IS INCODE'S OWN TOOL TOO** — the entry the
/// desktop app sends once `mcpCapabilities.http` is true: connected over
/// Streamable HTTP, called through the same gate, refused read-only.
#[test]
fn a_clients_http_mcp_server_is_called_through_the_gate_and_refused_read_only() {
    let fake = crate::toolbox::FakeHttp::start();
    let server = json!({
        "type": "http",
        "name": "fake",
        "url": fake.url,
        "headers": [{"name": "x-fake-token", "value": "t0ken-value"}],
    });
    called_through_the_gate_and_refused_read_only(server, "4 tool(s)", || fake.calls());
}

/// Open a session handed `server` — a fake whose tools include `echo` —
/// call `mcp__fake__echo` through the gate, then again once read-only.
/// `calls` counts the calls that reached the server.
fn called_through_the_gate_and_refused_read_only(
    server: Value,
    listed: &str,
    calls: impl Fn() -> usize,
) {
    let mut client = client(Behaviour::CallMcp);
    let dir = tempfile::tempdir().expect("ws");
    let id = client.request(
        "session/new",
        json!({"cwd": dir.path(), "mcpServers": [server]}),
    );
    let (_, reply) = client.until_response(id);
    let session = result_of(&reply)["sessionId"]
        .as_str()
        .expect("id")
        .to_string();
    // The commands, then what the servers did.
    let mut said = String::new();
    for _ in 0..2 {
        let message = client.next();
        if let Some(text) = message["params"]["update"]["content"]["text"].as_str() {
            said.push_str(text);
        }
    }
    assert!(
        said.contains(&format!("MCP server `fake`: {listed}")),
        "{said}"
    );

    let (streamed, reply) = prompt_in(&mut client, &session, "call the echo");
    assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
    let updates = updates_of(&streamed);
    let result = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "m1")
        .unwrap_or_else(|| panic!("no result: {streamed:#?}"));
    assert_eq!(result["status"], "completed", "{result}");
    assert!(
        result.to_string().contains("echo: from the model"),
        "{result}"
    );
    assert_eq!(calls(), 1);

    let set = client.call(
        "session/set_mode",
        json!({"sessionId": session, "modeId": "read-only"}),
    );
    assert!(set.get("result").is_some(), "{set}");
    let (streamed, _) = prompt_in(&mut client, &session, "call it again");
    let updates = updates_of(&streamed);
    let refused = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "m1")
        .unwrap_or_else(|| panic!("no result: {streamed:#?}"));
    assert_eq!(refused["status"], "failed", "{refused}");
    assert!(refused.to_string().contains("read-only"), "{refused}");
    assert_eq!(calls(), 1, "a refused call reached the server");
    client.close();
}

/// A ledger of three tasks, each proved by a command that passes.
fn three_tasks(workspace: &Path) {
    let tasks = (1..=3)
        .map(|n| {
            crate::ledger::Task::new(format!("t-{n}"), format!("r-{n}"), format!("task {n}"))
                .with_verify(["cargo", "--version"])
        })
        .collect();
    crate::ledger::Ledger { version: 1, tasks }
        .save(workspace)
        .expect("ledger");
}

/// The plan entries of one `plan` update, as `(status, content)`.
fn entries(update: &Value) -> Vec<(String, String)> {
    update["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| {
            (
                e["status"].as_str().unwrap_or_default().to_string(),
                e["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// **`/run` IS A LIVE PLAN, AND A CANCEL STOPS IT.** One plan entry per
/// ledger task, the running one `in_progress`; the cancel lands when that
/// task ends, the plan shows 1 of 3 done, the run says why it stopped, and
/// the prompt answers `cancelled`.
#[test]
fn run_streams_the_ledger_as_a_plan_and_stops_when_cancelled() {
    let mut client = client(Behaviour::Stream);
    let dir = tempfile::tempdir().expect("ws");
    three_tasks(dir.path());
    let session = client.open_session(dir.path());
    let prompt = client.request(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "/run"}]}),
    );
    let mut seen = Vec::new();
    // Until the first task is running.
    loop {
        let message = client.next();
        let update = &message["params"]["update"];
        let running = update["sessionUpdate"] == "plan"
            && entries(update)
                .first()
                .is_some_and(|(s, _)| s == "in_progress");
        seen.push(message);
        if running {
            break;
        }
    }
    let started = Instant::now();
    client.notify("session/cancel", json!({"sessionId": session}));
    let (rest, reply) = client.until_response(prompt);
    seen.extend(rest);
    assert_eq!(reply["result"]["stopReason"], "cancelled", "{reply}");
    assert!(
        started.elapsed() < WORKER_PATIENCE,
        "the cancel must stop the run, not wait it out: {:?}",
        started.elapsed()
    );

    let plans: Vec<Vec<(String, String)>> = updates_of(&seen)
        .into_iter()
        .filter(|u| u["sessionUpdate"] == "plan")
        .map(entries)
        .collect();
    let first = plans.first().expect("a plan");
    assert_eq!(first.len(), 3, "{plans:?}");
    assert!(
        first.iter().all(|(status, _)| status == "pending"),
        "{plans:?}"
    );
    assert_eq!(first[0].1, "task 1", "{plans:?}");
    let last = plans.last().expect("a plan");
    let statuses: Vec<&str> = last.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(statuses, ["completed", "pending", "pending"], "{plans:?}");
    let said = text_of(&seen);
    assert!(said.contains("Stopped: cancelled."), "{said}");
    assert!(said.contains("1 completed, 1 attempt(s) made."), "{said}");
    client.close();
}

/// **A SESSION IS LOADED AGAIN: ITS HISTORY IS REPLAYED, AND ITS MODEL'S
/// CONVERSATION IS RESUMED WHERE THE AGENT HOLDS IT.** On the same agent the
/// conversation id is handed back; opened on another agent, the transcript is
/// carried into the next turn as context instead. `session/list` names both.
#[test]
fn a_loaded_session_replays_its_history_and_resumes_the_model() {
    let script = Script::new(Behaviour::Named);
    let mut client = Client::start(
        script.clone(),
        PermissionMode::WorkspaceWrite,
        transport::MAX_LINE_BYTES,
    );
    let dir = tempfile::tempdir().expect("ws");

    // One session on the lead, one moved to `codex` before its turn.
    let (kept, _) = new_session(&mut client, dir.path(), json!([]));
    let _ = prompt_in(&mut client, &kept, "remember the parser");
    let (moved, _) = new_session(&mut client, dir.path(), json!([]));
    let _ = client.call(
        "session/set_config_option",
        json!({"sessionId": moved, "configId": "agent", "value": "codex"}),
    );
    let _ = prompt_in(&mut client, &moved, "remember the lexer");
    for id in [&kept, &moved] {
        let closed = client.call("session/close", json!({"sessionId": id}));
        assert!(closed.get("result").is_some(), "{closed}");
    }

    let listed = client.call("session/list", json!({"cwd": dir.path()}));
    let titles: Vec<&str> = result_of(&listed)["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|s| s["title"].as_str())
        .collect();
    assert!(titles.contains(&"remember the parser"), "{listed}");
    assert!(titles.contains(&"remember the lexer"), "{listed}");

    // The same agent: history replayed BEFORE the answer, conversation
    // resumed, nothing replayed into the next turn.
    let id = client.request(
        "session/load",
        json!({"sessionId": kept, "cwd": dir.path(), "mcpServers": []}),
    );
    let (replayed, reply) = client.until_response(id);
    let loaded = result_of(&reply);
    assert_eq!(
        loaded["modes"]["currentModeId"], "workspace-write",
        "{reply}"
    );
    assert!(loaded["configOptions"].is_array(), "{reply}");
    let kinds: Vec<&str> = updates_of(&replayed)
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    assert_eq!(
        kinds,
        ["user_message_chunk", "agent_message_chunk"],
        "{replayed:#?}"
    );
    assert_eq!(
        updates_of(&replayed)[0]["content"]["text"],
        "remember the parser"
    );
    assert!(
        script
            .resumed
            .lock()
            .expect("resumed")
            .contains(&"conv-claude-code".to_string()),
        "the model's own conversation is resumed"
    );
    let (said, _) = prompt_in(&mut client, &kept, "and now?");
    assert!(
        !text_of(&said).contains("earlier-conversation"),
        "a resumed conversation is not replayed as well: {said:#?}"
    );

    // Opened on another agent than the one holding its conversation: the
    // transcript rides the next turn instead.
    let id = client.request(
        "session/load",
        json!({"sessionId": moved, "cwd": dir.path(), "mcpServers": []}),
    );
    let (_, reply) = client.until_response(id);
    let _ = result_of(&reply);
    assert!(
        !script
            .resumed
            .lock()
            .expect("resumed")
            .contains(&"conv-codex".to_string()),
        "another agent's conversation id must not be handed over"
    );
    let (said, _) = prompt_in(&mut client, &moved, "and now?");
    let said = text_of(&said);
    assert!(said.contains("earlier-conversation"), "{said}");
    assert!(said.contains("Person: remember the lexer"), "{said}");

    let unknown = client.call(
        "session/load",
        json!({"sessionId": "no-such-session", "cwd": dir.path(), "mcpServers": []}),
    );
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    client.close();
}
