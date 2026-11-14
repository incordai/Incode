//! The routine tools and `watch` on a live server: the gates a replayed step
//! passes, stop-at-first-failure, the tiers, and what the journal and the
//! operator's hook are told.
//!
//! The "desktop" is the real one of the machine running the tests, as in
//! `selector_tests::live`, and every step is one that touches nothing: a wait
//! for a window that does not exist to be gone (found at once) or to appear (a
//! timeout of 100 ms).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::hooks::HookOutcome;
use crate::permission::OBSERVING;
use crate::routine::source::{Call, MemorySource, NoSource, Record, StepSource};
use crate::routine::store::{Routines, Store};
use crate::routine::{Routine, Step};

/// A window no program has: waiting for it to be gone is true at once.
const NO_SUCH_WINDOW: &str = "window=zz no such window 7f3a9c1e";

fn folder(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("incu-routine-tools-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn server_with(dir: &Path, source: Box<dyn StepSource>) -> Server {
    Server::new(InputThread::start().expect("input thread")).with_routines(RoutineKit::new(
        Store::at(dir.join("routines.json")),
        source,
    ))
}

fn server(dir: &Path) -> Server {
    server_with(dir, Box::new(NoSource))
}

fn call(server: &mut Server, name: &str, args: Value) -> Value {
    let line = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": args}
    })
    .to_string();
    let reply = server.handle_line(&line).expect("a reply was expected");
    serde_json::from_str(&reply).expect("the reply must be valid JSON")
}

fn said(reply: &Value) -> String {
    reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn refused(reply: &Value) -> bool {
    reply["result"]["isError"] == true
}

/// The JSON a tool answered.
fn answer(reply: &Value) -> Value {
    assert!(!refused(reply), "{reply}");
    serde_json::from_str(&said(reply)).expect("the answer is JSON")
}

fn gone() -> Value {
    json!({"condition": "window_gone", "selector": NO_SUCH_WINDOW})
}

fn never_appears() -> Value {
    json!({"condition": "window_appears", "selector": NO_SUCH_WINDOW, "timeout_ms": 100, "poll_ms": 50})
}

fn step(args: Value) -> Value {
    json!({"tool": "wait_for", "args": args})
}

fn save_steps(s: &mut Server, phrase: &str, steps: Vec<Value>) -> Value {
    call(s, "routine_save", json!({"phrase": phrase, "steps": steps}))
}

/// The tools a pre-tool hook was asked about.
type Pre = Arc<Mutex<Vec<String>>>;
/// The tools a post-tool hook was told about, and the answer it was handed.
type Post = Arc<Mutex<Vec<(String, String)>>>;

/// What a post-tool and a pre-tool hook were told, and a veto on request.
struct Recording {
    veto: Option<&'static str>,
    pre: Pre,
    post: Post,
    /// Written to after the first `wait_for`, to stop the desktop mid-routine.
    stop_after_wait: Option<PathBuf>,
}

impl Recording {
    fn new(veto: Option<&'static str>) -> (Self, Pre, Post) {
        let pre = Arc::new(Mutex::new(Vec::new()));
        let post = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                veto,
                pre: pre.clone(),
                post: post.clone(),
                stop_after_wait: None,
            },
            pre,
            post,
        )
    }
}

impl Hooks for Recording {
    fn run_pre_tool_use(&self, tool_name: &str, _input: &str) -> HookOutcome {
        self.pre.lock().unwrap().push(tool_name.to_string());
        if self.veto == Some(tool_name) {
            HookOutcome::deny(vec![format!("the operator does not allow {tool_name}")])
        } else {
            HookOutcome::allow(Vec::new())
        }
    }

    fn run_post_tool_use(
        &self,
        tool_name: &str,
        _input: &str,
        output: &str,
        _err: bool,
    ) -> HookOutcome {
        self.post
            .lock()
            .unwrap()
            .push((tool_name.to_string(), output.to_string()));
        if let (Some(stop), "wait_for") = (&self.stop_after_wait, tool_name) {
            crate::safety::request_stop(stop).expect("stop file");
        }
        HookOutcome::allow(Vec::new())
    }
}

#[test]
fn the_six_new_tools_are_offered_in_their_tiers() {
    let listed: Vec<Value> = tool_definitions();
    let names: Vec<&str> = listed.iter().filter_map(|t| t["name"].as_str()).collect();
    for tool in [
        "watch",
        "routine_save",
        "routine_run",
        "routine_forget",
        "routine_list",
        "routine_match",
    ] {
        assert!(names.contains(&tool), "{tool} is not offered: {names:?}");
    }
    for tool in ["watch", "routine_list", "routine_match"] {
        assert!(OBSERVING.contains(&tool), "{tool}");
    }
    for tool in ["routine_save", "routine_run", "routine_forget"] {
        assert!(ACTING.contains(&tool), "{tool}");
    }
    // The picture after an action is offered where it means something.
    let has_shot = |name: &str| {
        listed.iter().find(|t| t["name"] == name).is_some_and(|t| {
            t["inputSchema"]["properties"]
                .get("screenshot_after")
                .is_some()
        })
    };
    assert!(has_shot("routine_run"));
    assert!(!has_shot("routine_save"));
    assert!(!has_shot("routine_forget"));
    assert!(!has_shot("watch"));
    // `watch` takes `window` as wait_for does.
    let watch = listed.iter().find(|t| t["name"] == "watch").unwrap();
    assert!(watch["inputSchema"]["properties"].get("window").is_some());
    assert!(watch["description"].as_str().unwrap().contains("BLOCKS"));
}

#[test]
fn a_routine_is_saved_from_steps_listed_matched_and_forgotten() {
    let dir = folder("lifecycle");
    let mut s = server(&dir);
    let saved = answer(&save_steps(&mut s, "check the window", vec![step(gone())]));
    assert_eq!(saved["saved"], "check the window");
    assert_eq!(saved["replaced_an_older_routine"], false);

    let listed = answer(&call(&mut s, "routine_list", json!({})));
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["routines"][0]["phrase"], "check the window");
    assert_eq!(listed["routines"][0]["expires_in_days"], 30);

    // The same words in another order, with filler, are the same request.
    let matched = answer(&call(
        &mut s,
        "routine_match",
        json!({"phrase": "Window, check it please"}),
    ));
    assert_eq!(matched["matched"], true);
    assert_eq!(matched["routine"]["steps"].as_array().unwrap().len(), 1);
    let near = answer(&call(
        &mut s,
        "routine_match",
        json!({"phrase": "check the window and the door"}),
    ));
    assert_eq!(near["matched"], false);

    let forgotten = call(&mut s, "routine_forget", json!({"phrase": "window check"}));
    assert!(!refused(&forgotten), "{forgotten}");
    assert_eq!(answer(&call(&mut s, "routine_list", json!({})))["count"], 0);
    let again = call(&mut s, "routine_forget", json!({"phrase": "window check"}));
    assert!(
        refused(&again) && said(&again).contains("no routine matches"),
        "{again}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_run_that_cannot_be_replayed_is_refused_with_the_reason_and_nothing_is_written() {
    let dir = folder("refused");
    let mut s = server(&dir);
    let cases = [
        ("close this", vec![step(gone())], "this"),
        (
            "click the button",
            vec![json!({"tool": "click_element", "args": {"id": 16}})],
            "eN id",
        ),
        (
            "click the spot",
            vec![json!({"tool": "click", "args": {"x": 4, "y": 5}})],
            "coordinates",
        ),
        (
            "do nothing",
            vec![json!({"tool": "ui_tree", "args": {}})],
            "no desktop action",
        ),
    ];
    for (phrase, steps, why) in cases {
        let reply = save_steps(&mut s, phrase, steps);
        assert!(refused(&reply), "{phrase}: {reply}");
        assert!(said(&reply).contains(why), "{phrase}: {}", said(&reply));
    }
    assert!(
        !dir.join("routines.json").exists(),
        "a refusal wrote a file"
    );
    let neither = call(&mut s, "routine_save", json!({"phrase": "open it"}));
    assert!(said(&neither).contains("records"), "{}", said(&neither));
    let both = call(
        &mut s,
        "routine_save",
        json!({"phrase": "open it", "steps": [], "records": []}),
    );
    assert!(said(&both).contains("not both"), "{}", said(&both));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_routine_is_learned_from_watchman_records_in_the_order_they_happened() {
    let dir = folder("records");
    let source = MemorySource::new()
        // Named second, happened first.
        .with(Record {
            id: "w-b".to_string(),
            started_at_ms: 2_000,
            calls: vec![Call::ok("mcp__desktop__wait_for", never_gone_second())],
        })
        .with(Record {
            id: "w-a".to_string(),
            started_at_ms: 1_000,
            calls: vec![
                Call::ok("mcp__desktop__ui_tree", json!({})),
                Call::ok("mcp__desktop__wait_for", gone()),
                Call::ok("Bash", json!({"command": "ls"})),
            ],
        });
    let mut s = server_with(&dir, Box::new(source));
    let reply = call(
        &mut s,
        "routine_save",
        json!({"phrase": "wait for it", "records": ["w-b", "w-a"]}),
    );
    let saved = answer(&reply);
    assert_eq!(saved["looking_steps_dropped"], 1);
    assert_eq!(saved["left_out_not_desktop_tools"], json!(["Bash"]));
    assert!(saved["warning"].is_string());
    let steps = saved["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert!(
        steps[0].as_str().unwrap().contains("window_gone"),
        "{steps:?}"
    );
    assert!(steps[1].as_str().unwrap().contains("11111111"), "{steps:?}");

    let unknown = call(
        &mut s,
        "routine_save",
        json!({"phrase": "wait for it", "records": ["w-zzz"]}),
    );
    assert!(
        refused(&unknown) && said(&unknown).contains("w-zzz"),
        "{unknown}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn never_gone_second() -> Value {
    json!({"condition": "window_gone", "selector": "window=zz 11111111"})
}

#[test]
fn a_failed_recorded_run_is_not_learned() {
    let dir = folder("failed-record");
    let source = MemorySource::new().with(Record {
        id: "w-1".to_string(),
        started_at_ms: 1,
        calls: vec![Call::ok("wait_for", gone()).failed("selector matches nothing")],
    });
    let mut s = server_with(&dir, Box::new(source));
    let reply = call(
        &mut s,
        "routine_save",
        json!({"phrase": "wait for it", "records": ["w-1"]}),
    );
    assert!(
        refused(&reply) && said(&reply).contains("the run failed"),
        "{reply}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_saved_routine_runs_every_step_through_the_gates_and_the_journal() {
    let dir = folder("run");
    let journal = dir.join("desktop.jsonl");
    let (hooks, pre, post) = Recording::new(None);
    let mut s = server(&dir)
        .with_journal(journal.clone())
        .with_hook_runner(Box::new(hooks));
    answer(&save_steps(
        &mut s,
        "check the windows",
        vec![step(gone()), step(gone())],
    ));

    let ran = answer(&call(
        &mut s,
        "routine_run",
        json!({"phrase": "windows check"}),
    ));
    assert_eq!(ran["steps_run"], 2, "{ran}");
    assert_eq!(ran["ran"], "check the windows");

    // The pre-tool hook saw the routine and then EACH step, as a live call.
    assert_eq!(
        *pre.lock().unwrap(),
        ["routine_save", "routine_run", "wait_for", "wait_for"]
    );
    assert_eq!(
        post.lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == "wait_for")
            .count(),
        2
    );
    // And the journal has a line for each step and one for the routine.
    let lines = std::fs::read_to_string(&journal).unwrap();
    assert_eq!(lines.matches("\"t\":\"wait_for\"").count(), 2, "{lines}");
    assert!(lines.contains("\"t\":\"routine_run\""), "{lines}");
    assert!(lines.contains("check the windows"), "{lines}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_first_failing_step_stops_the_replay_and_the_rest_never_run() {
    let dir = folder("stop-first");
    let journal = dir.join("desktop.jsonl");
    let (hooks, pre, _post) = Recording::new(None);
    let mut s = server(&dir)
        .with_journal(journal.clone())
        .with_hook_runner(Box::new(hooks));
    answer(&save_steps(
        &mut s,
        "three waits",
        vec![step(gone()), step(never_appears()), step(gone())],
    ));
    let reply = call(&mut s, "routine_run", json!({"phrase": "three waits"}));
    assert!(refused(&reply), "{reply}");
    let said = said(&reply);
    assert!(said.contains("failed at step 2 of 3"), "{said}");
    assert!(said.contains("timed out"), "{said}");
    assert!(said.contains("nothing was retried or repaired"), "{said}");
    // Step 3 was never asked of the gate, let alone the desktop.
    let waits = pre
        .lock()
        .unwrap()
        .iter()
        .filter(|t| *t == "wait_for")
        .count();
    assert_eq!(waits, 2);
    let lines = std::fs::read_to_string(&journal).unwrap();
    assert_eq!(lines.matches("\"t\":\"wait_for\"").count(), 2, "{lines}");
    assert!(
        lines.contains("refused") && lines.contains("routine_run"),
        "{lines}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_operators_hook_can_refuse_a_step_of_a_replay() {
    let dir = folder("veto");
    let (hooks, _pre, _post) = Recording::new(Some("wait_for"));
    let mut s = server(&dir).with_hook_runner(Box::new(hooks));
    answer(&save_steps(&mut s, "check the window", vec![step(gone())]));
    let reply = call(&mut s, "routine_run", json!({"phrase": "check the window"}));
    assert!(refused(&reply), "a replay got past the hook: {reply}");
    assert!(
        said(&reply).contains("failed at step 1 of 1"),
        "{}",
        said(&reply)
    );
    assert!(
        said(&reply).contains("does not allow wait_for"),
        "{}",
        said(&reply)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_stop_file_ends_a_replay_between_steps() {
    let dir = folder("stop-file");
    let journal = dir.join("desktop.jsonl");
    let (mut hooks, _pre, _post) = Recording::new(None);
    hooks.stop_after_wait = Some(crate::safety::stop_file_beside(&journal));
    let mut s = server(&dir)
        .with_journal(journal)
        .with_hook_runner(Box::new(hooks));
    answer(&save_steps(
        &mut s,
        "two waits",
        vec![step(gone()), step(gone())],
    ));
    let reply = call(&mut s, "routine_run", json!({"phrase": "two waits"}));
    assert!(refused(&reply), "{reply}");
    let said = said(&reply);
    assert!(said.contains("failed at step 2 of 2"), "{said}");
    assert!(said.to_lowercase().contains("stop"), "{said}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_read_only_session_may_look_at_routines_and_may_not_change_or_run_them() {
    let dir = folder("tier");
    let mut s = server(&dir).with_tier(Tier::Observe);
    assert!(!refused(&call(&mut s, "routine_list", json!({}))));
    assert!(!refused(&call(
        &mut s,
        "routine_match",
        json!({"phrase": "open it"})
    )));
    for tool in ["routine_save", "routine_run", "routine_forget"] {
        let reply = call(
            &mut s,
            tool,
            json!({"phrase": "open it", "steps": [step(gone())]}),
        );
        assert!(refused(&reply), "{tool}: {reply}");
        assert!(
            said(&reply).contains("workspace-write"),
            "{tool}: {}",
            said(&reply)
        );
    }
    assert!(!dir.join("routines.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_routine_unused_for_thirty_days_is_gone_unless_it_was_taught() {
    let dir = folder("expiry");
    let old = |phrase: &str, taught: bool| Routine {
        phrase: phrase.to_string(),
        steps: vec![Step::new("wait_for", gone())],
        date: "2026-01-01".to_string(),
        last_used_ms: 1_000,
        taught,
    };
    let file = Routines {
        version: 1,
        routines: vec![old("forgotten task", false), old("taught task", true)],
    };
    let store = Store::at(dir.join("routines.json"));
    store.save(&file).unwrap();
    let mut s = server(&dir);
    let listed = answer(&call(&mut s, "routine_list", json!({})));
    assert_eq!(listed["count"], 1, "{listed}");
    assert_eq!(listed["routines"][0]["phrase"], "taught task");
    assert!(listed["routines"][0].get("expires_in_days").is_none());
    let run = call(&mut s, "routine_run", json!({"phrase": "forgotten task"}));
    assert!(
        refused(&run) && said(&run).contains("no routine matches"),
        "{run}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stored_step_the_rules_refuse_runs_nothing() {
    let dir = folder("tampered");
    let (hooks, pre, _post) = Recording::new(None);
    let file = Routines {
        version: 1,
        routines: vec![Routine {
            phrase: "edited by hand".to_string(),
            steps: vec![
                Step::new("wait_for", gone()),
                Step::new("click", json!({"x": 1, "y": 1})),
            ],
            date: String::new(),
            last_used_ms: crate::routine::now_ms(),
            taught: true,
        }],
    };
    Store::at(dir.join("routines.json")).save(&file).unwrap();
    let mut s = server(&dir).with_hook_runner(Box::new(hooks));
    let reply = call(&mut s, "routine_run", json!({"phrase": "edited by hand"}));
    assert!(
        refused(&reply) && said(&reply).contains("Nothing was done"),
        "{reply}"
    );
    assert_eq!(
        *pre.lock().unwrap(),
        ["routine_run"],
        "not even the first step was tried"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_server_without_routines_refuses_them_by_name() {
    let mut s = Server::new(InputThread::start().expect("input thread"));
    let reply = call(&mut s, "routine_list", json!({}));
    assert!(
        refused(&reply) && said(&reply).contains("not configured"),
        "{reply}"
    );
}

#[test]
fn a_click_on_a_link_or_button_is_replayed_as_an_invoke_and_other_clicks_are_not() {
    use super::routine_tool::invoke_for;
    let click = |args: Value| Step::new("click_element", args);
    let selector = "app=chrome;window~=Wikipedia;role=link;name=Honshu";
    let invoked = invoke_for(&click(json!({"selector": selector}))).expect("a link is invoked");
    assert_eq!(invoked, json!({"selector": selector, "action": "invoke"}));
    assert!(invoke_for(&click(
        json!({"selector": "app=a;role=button;name=OK", "button": "left"})
    ))
    .is_some());
    // A right click, a text field and anything else are clicked as recorded.
    assert!(invoke_for(&click(json!({"selector": selector, "button": "right"}))).is_none());
    assert!(invoke_for(&click(json!({"selector": "app=a;role=edit;name=Body"}))).is_none());
    assert!(
        invoke_for(&click(json!({"selector": "app=a;name=Save"}))).is_none(),
        "no role"
    );
    assert!(invoke_for(&Step::new("set_value", json!({"selector": selector}))).is_none());
}

#[test]
fn a_keyboard_steps_window_is_looked_up_at_replay_and_a_missing_one_is_a_failed_step() {
    use super::routine_tool::live_args;
    // Not a keyboard step: its arguments go through as they are.
    let wait = Step::new("wait_for", gone());
    assert_eq!(live_args(&wait).unwrap(), gone());
    // A keyboard step whose window does not exist fails, naming it, before any
    // key is sent; the tool is never asked.
    let typing = Step::new(
        "type_text",
        json!({"text": "hi", "in_window": NO_SUCH_WINDOW}),
    );
    let why = live_args(&typing).unwrap_err();
    assert!(why.contains("in_window"), "{why}");
    assert!(why.contains("zz no such window"), "{why}");
}

#[test]
fn a_replayed_typing_step_into_a_window_that_is_not_there_fails_and_types_nothing() {
    let dir = folder("typing");
    let (hooks, pre, _post) = Recording::new(None);
    let mut s = server(&dir).with_hook_runner(Box::new(hooks));
    let reply = save_steps(
        &mut s,
        "type a note",
        vec![json!({"tool": "type_text", "args": {"text": "hi", "in_window": NO_SUCH_WINDOW}})],
    );
    assert!(!refused(&reply), "{reply}");
    let run = call(&mut s, "routine_run", json!({"phrase": "type a note"}));
    assert!(refused(&run), "{run}");
    assert!(
        said(&run).contains("failed at step 1 of 1"),
        "{}",
        said(&run)
    );
    assert_eq!(
        *pre.lock().unwrap(),
        ["routine_save", "routine_run"],
        "type_text never reached the gate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn typed_text_and_steps_never_reach_the_journal_or_a_listing() {
    let dir = folder("redaction");
    let journal = dir.join("desktop.jsonl");
    let mut s = server(&dir).with_journal(journal.clone());
    let reply = call(
        &mut s,
        "routine_save",
        json!({"phrase": "type my token", "steps": [
            {"tool": "set_value", "args": {"selector": "app=vault;role=edit;name=Token", "value": "hunter2-secret"}}
        ]}),
    );
    assert!(!refused(&reply), "{reply}");
    assert!(!said(&reply).contains("hunter2"), "{}", said(&reply));
    let matched = said(&call(
        &mut s,
        "routine_match",
        json!({"phrase": "token type"}),
    ));
    assert!(
        !matched.contains("hunter2") && matched.contains("14 characters"),
        "{matched}"
    );
    // A text field's name is what was typed in it: not in a step's line either.
    assert!(!matched.contains("name=Token"), "{matched}");
    let lines = std::fs::read_to_string(&journal).unwrap();
    assert!(!lines.contains("hunter2"), "{lines}");
    // It is in the routines file, as the docs say.
    let file = std::fs::read_to_string(dir.join("routines.json")).unwrap();
    assert!(file.contains("hunter2-secret"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn watch_returns_at_once_for_a_condition_that_holds_and_tells_the_hook_what_it_saw() {
    let dir = folder("watch");
    let journal = dir.join("desktop.jsonl");
    let (hooks, _pre, post) = Recording::new(None);
    let mut s = server(&dir)
        .with_journal(journal.clone())
        .with_hook_runner(Box::new(hooks));
    let reply = call(
        &mut s,
        "watch",
        json!({"condition": "window_gone", "selector": NO_SUCH_WINDOW, "message": "the export finished"}),
    );
    let text = said(&reply);
    assert!(!refused(&reply), "{text}");
    let json_part = &text[..=text.rfind('}').unwrap()];
    let body: Value = serde_json::from_str(json_part).unwrap();
    assert_eq!(body["result"], "found");
    assert_eq!(body["message"], "the export finished");
    // The hook for `watch` was handed the reply; no other tool's hook gets one.
    let told = post.lock().unwrap().clone();
    let (name, output) = told.iter().find(|(n, _)| n == "watch").expect("watch hook");
    assert_eq!(name, "watch");
    assert!(
        output.contains("the export finished") && output.contains("found"),
        "{output}"
    );
    let lines = std::fs::read_to_string(&journal).unwrap();
    assert!(
        lines.contains("started:"),
        "the begin is journalled: {lines}"
    );
    assert!(
        !lines.contains("the export finished"),
        "the message is free text: {lines}"
    );

    call(&mut s, "wait_for", gone());
    let told = post.lock().unwrap().clone();
    assert!(told
        .iter()
        .filter(|(n, _)| n == "wait_for")
        .all(|(_, out)| out.is_empty()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_watch_that_times_out_is_a_reply_and_still_notifies() {
    let dir = folder("watch-timeout");
    let (hooks, _pre, post) = Recording::new(None);
    let mut s = server(&dir).with_hook_runner(Box::new(hooks));
    let reply = call(
        &mut s,
        "watch",
        json!({"condition": "window_appears", "selector": NO_SUCH_WINDOW, "timeout_ms": 300, "poll_ms": 250}),
    );
    assert!(!refused(&reply), "{reply}");
    assert!(
        said(&reply).contains("\"result\": \"timeout\""),
        "{}",
        said(&reply)
    );
    let told = post.lock().unwrap().clone();
    assert!(
        told.iter()
            .any(|(n, out)| n == "watch" && out.contains("timeout")),
        "{told:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_stop_file_ends_a_watch_that_has_a_day_to_run() {
    let dir = folder("watch-stop");
    let journal = dir.join("desktop.jsonl");
    let stop = crate::safety::stop_file_beside(&journal);
    let mut s = server(&dir).with_journal(journal);
    // The stop arrives while the watch is polling, not before it starts.
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        crate::safety::request_stop(&stop).expect("stop file");
    });
    let started = std::time::Instant::now();
    let reply = call(
        &mut s,
        "watch",
        json!({"condition": "window_appears", "selector": NO_SUCH_WINDOW, "timeout_ms": 86_400_000}),
    );
    stopper.join().expect("the stopper thread");
    assert!(refused(&reply), "{reply}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "a stop took {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_watch_message_that_is_too_long_or_not_text_is_refused() {
    let dir = folder("watch-message");
    let mut s = server(&dir);
    let long = "x".repeat(201);
    for message in [json!(long), json!(5)] {
        let reply = call(
            &mut s,
            "watch",
            json!({"condition": "window_gone", "selector": NO_SUCH_WINDOW, "message": message}),
        );
        assert!(
            refused(&reply) && said(&reply).contains("message"),
            "{reply}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
