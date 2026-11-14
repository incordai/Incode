//! Tests of `wait_for` and of acting by selector, as a client meets them: the
//! tool definitions, the argument rules, the journal line, and (on Windows,
//! where the existing live tests run) the real server answering over its own
//! protocol path. None of them moves the mouse or types.
//!
//! The timing rules themselves are in `crate::wait`'s tests, against a scripted
//! desktop with a virtual clock; the selector grammar and matching are in
//! `crate::selector`'s.

use super::tools::{element_target, Target};
use super::*;

#[cfg(windows)]
use crate::uia;

fn definition(name: &str) -> Value {
    tool_definitions()
        .into_iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no tool named {name}"))
}

fn required(tool: &Value) -> Vec<String> {
    tool["inputSchema"]["required"]
        .as_array()
        .map(|r| {
            r.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn wait_for_is_offered_with_its_conditions_and_its_caps() {
    let tool = definition("wait_for");
    assert_eq!(required(&tool), ["condition"]);
    let conditions: Vec<&str> = tool["inputSchema"]["properties"]["condition"]["enum"]
        .as_array()
        .expect("an enum")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        conditions,
        [
            "element_appears",
            "element_gone",
            "window_appears",
            "window_gone",
            "screen_changes"
        ]
    );
    for property in [
        "selector",
        "timeout_ms",
        "poll_ms",
        "baseline",
        "tolerance",
        "window",
    ] {
        assert!(
            tool["inputSchema"]["properties"][property].is_object(),
            "wait_for has no `{property}`"
        );
    }
    let said = tool["description"].as_str().unwrap();
    assert!(said.contains("60000"), "the cap is told: {said}");
    assert!(said.contains("UNTRUSTED"), "{said}");
    // It acts on nothing, so it does not offer a screenshot afterwards.
    assert!(tool["inputSchema"]["properties"]["screenshot_after"].is_null());
}

#[test]
fn the_element_tools_take_an_id_or_a_selector() {
    for (name, needs) in [
        ("click_element", vec![]),
        ("set_value", vec!["value"]),
        ("element_action", vec!["action"]),
    ] {
        let tool = definition(name);
        assert!(
            tool["inputSchema"]["properties"]["selector"].is_object(),
            "{name} takes no selector"
        );
        assert!(
            tool["inputSchema"]["properties"]["id"].is_object(),
            "{name} lost its id"
        );
        // Neither alone is required: one of them is, and the step says so.
        assert_eq!(required(&tool), needs, "{name}");
    }
}

#[test]
fn an_id_alone_is_the_old_behaviour_and_a_selector_alone_is_the_new() {
    assert!(matches!(
        element_target(&json!({"id": 5})).unwrap(),
        Target::Id(5)
    ));
    match element_target(&json!({"selector": "role=button;name=Save"})).unwrap() {
        Target::Selector(s) => assert_eq!(s.render(), "role=button;name=Save"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_id_and_a_selector_together_are_refused_and_so_is_neither() {
    let both = element_target(&json!({"id": 5, "selector": "role=button"})).unwrap_err();
    assert!(both.contains("not both"), "{both}");
    let neither = element_target(&json!({})).unwrap_err();
    assert!(neither.contains("selector"), "{neither}");
    assert!(neither.contains("id"), "{neither}");
}

#[test]
fn a_selector_that_does_not_name_a_control_is_refused_before_anything_is_read() {
    for bad in ["nth=2", "app=notepad", "colour=red", ""] {
        let why = element_target(&json!({"selector": bad})).unwrap_err();
        assert!(
            why.contains("selector"),
            "`{bad}` was not refused by name: {why}"
        );
    }
    let why = element_target(&json!({"selector": 7})).unwrap_err();
    assert!(why.contains("text"), "{why}");
}

#[test]
fn the_journal_names_the_selector_but_never_a_text_fields_name() {
    let line = |name: &str, args: Value| Server::describe(name, &args);
    // The id form is as it was.
    assert_eq!(line("click_element", json!({"id": 5})), "element e5");
    assert_eq!(
        line("set_value", json!({"id": 5, "value": "abc"})),
        "element e5, 3 characters"
    );
    // A button's name is journalled; a text field's is not (on macOS it is
    // what the user typed).
    assert_eq!(
        line(
            "click_element",
            json!({"selector": "role=button;name=Save"})
        ),
        "selector role=button;name=Save"
    );
    let edit = line(
        "set_value",
        json!({"selector": "role=edit;name=hunter2", "value": "x"}),
    );
    assert!(!edit.contains("hunter2"), "{edit}");
    assert!(edit.contains("1 characters"), "{edit}");
    let unroled = line(
        "element_action",
        json!({"selector": "name=hunter2", "action": "press"}),
    );
    assert!(!unroled.contains("hunter2"), "{unroled}");
    // A selector that does not parse is not echoed.
    let bad = line("click_element", json!({"selector": "name=hunter2;;;bad"}));
    assert!(!bad.contains("hunter2"), "{bad}");
}

fn window(app: &str, title: &str) -> crate::uia::WindowInfo {
    crate::uia::WindowInfo {
        id: "9".to_string(),
        title: title.to_string(),
        app: app.to_string(),
        pid: 1,
        focused: false,
        rect: crate::Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        has_tree: true,
    }
}

/// **A blocked application's window, named by id with a selector, is refused
/// by the same words the id path gives — before the selector is read.** The
/// guard here is the real `refuse_blocked` that `click_element`, `set_value`
/// and `element_action` hand to `window_for`.
#[test]
fn a_blocked_window_named_by_id_is_refused_as_the_id_path_refuses_it() {
    use super::guard::refuse_blocked;
    let keepass = || window("KeePassXC", "Bank login - entry");
    let the_id_path = refuse_blocked(&keepass()).unwrap_err();
    assert!(!the_id_path.contains("Bank login"), "{the_id_path}");
    for selector in [
        "window=x;role=button",
        "window~=bank;role=button",
        "app=notepad;role=button",
        "role=button;name=Save",
    ] {
        let selector = crate::selector::Selector::parse(selector).unwrap();
        let said = crate::uia::confirm_window(keepass(), &selector, &refuse_blocked)
            .unwrap_err()
            .to_string();
        assert_eq!(said, the_id_path, "`{}`", selector.render());
    }
    // And a window that is not blocked still reaches the selector's check.
    let selector = crate::selector::Selector::parse("window=x;role=button").unwrap();
    let said = crate::uia::confirm_window(window("notepad", "notes"), &selector, &refuse_blocked)
        .unwrap_err()
        .to_string();
    assert!(said.contains("does not name"), "{said}");
}

#[test]
fn what_a_selector_error_says_is_never_what_was_typed() {
    // The tool's own arguments: a stray word with no `=` is the case a
    // secret typed into the wrong field would come in as.
    for raw in [
        "role=edit;hunter2",
        "hunter2=1",
        "name=hunter2;nth=zz",
        "role=hunter2",
    ] {
        let why = element_target(&json!({"selector": raw})).unwrap_err();
        assert!(!why.contains("hunter2"), "`{raw}` was quoted: {why}");
    }
    // A selector that parses but names no control: shown redacted.
    let why = element_target(&json!({"selector": "name=hunter2;nth=1;app=notes"}))
        .map(|_| String::new())
        .unwrap_or_else(|e| e);
    assert!(!why.contains("hunter2"), "{why}");
}

/// **No message names an edit or text field, on either path.** Platform
/// errors (`set_value`, `element_action`: "`{what}` has no value to set") say
/// `Element::label`, the disabled refusal says the id, and on macOS the name of
/// such a field is what is in it. Every message goes to the journal as it was
/// returned.
#[test]
fn no_message_names_an_edit_or_a_text_field_whatever_the_platform() {
    use super::tools::disabled_error;
    use crate::uia::{Element, Role};
    let element = |role, name: &str| Element {
        id: 7,
        depth: 1,
        role,
        name: name.to_string(),
        rect: crate::Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        enabled: false,
        secret: false,
        automation_id: String::new(),
    };
    for role in [Role::Edit, Role::Text] {
        let typed = element(role, "hunter2");
        assert_eq!(typed.label(), format!("e7 ({role:?})"));
        assert!(!typed.label().contains("hunter2"));
        let why = disabled_error(&Target::Id(7), &typed);
        assert!(!why.contains("hunter2"), "{why}");
        assert!(why.contains("e7 is disabled"), "{why}");
    }
    // A control whose name is a label, not content, is named as before.
    for role in [Role::Button, Role::CheckBox, Role::MenuItem, Role::Link] {
        let named = element(role, "Save");
        assert_eq!(named.label(), format!("e7 \"Save\" ({role:?})"));
        assert!(disabled_error(&Target::Id(7), &named).contains("\"Save\""));
    }
}

/// The "is disabled" refusal is journalled word for word.
#[test]
fn a_disabled_control_is_refused_without_naming_a_selected_text_field() {
    use super::tools::disabled_error;
    use crate::uia::{Element, Role};
    let element = |role, name: &str| Element {
        id: 7,
        depth: 1,
        role,
        name: name.to_string(),
        rect: crate::Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        enabled: false,
        secret: false,
        automation_id: String::new(),
    };
    // By id, the message is what it always was.
    assert_eq!(
        disabled_error(&Target::Id(7), &element(Role::Button, "Save")),
        "e7 \"Save\" is disabled; clicking it would do nothing"
    );
    // By selector: redacted, and the control's own name is not repeated.
    let typed = crate::selector::Selector::parse("role=edit;name=hunter2").unwrap();
    let why = disabled_error(&Target::Selector(typed), &element(Role::Edit, "hunter2"));
    assert!(!why.contains("hunter2"), "{why}");
    assert!(why.contains("is disabled"), "{why}");
    let untyped = crate::selector::Selector::parse("name=hunter2").unwrap();
    let why = disabled_error(&Target::Selector(untyped), &element(Role::Edit, "hunter2"));
    assert!(!why.contains("hunter2"), "{why}");
    let button = crate::selector::Selector::parse("role=button;name=Save").unwrap();
    let why = disabled_error(&Target::Selector(button), &element(Role::Button, "Save"));
    assert!(why.contains("role=button;name=Save"), "{why}");
}

#[test]
fn wait_for_is_journalled_by_condition_selector_and_timeout() {
    let said = Server::describe(
        "wait_for",
        &json!({"condition": "element_appears", "selector": "role=button;name=Save", "timeout_ms": 3000}),
    );
    assert_eq!(
        said,
        "element_appears selector role=button;name=Save timeout 3000 ms"
    );
    let said = Server::describe("wait_for", &json!({"condition": "screen_changes"}));
    assert_eq!(said, "screen_changes  timeout default");
}

#[test]
fn a_tree_prints_each_selector_beside_its_element_and_the_window_part_once() {
    use crate::selector::Selector;
    use crate::uia::{Element, Role, Tree};
    use crate::Rect;
    let el = |id, role, name: &str, aid: &str, disabled: bool| Element {
        id,
        depth: 1,
        role,
        name: name.to_string(),
        rect: Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        },
        enabled: !disabled,
        secret: false,
        automation_id: aid.to_string(),
    };
    let tree = Tree {
        root_name: "Untitled - Notepad".to_string(),
        window: "42".to_string(),
        app: "notepad".to_string(),
        no_positions: None,
        elements: vec![
            el(1, Role::Button, "Save", "", false),
            el(2, Role::Button, "Save", "", true),
            el(3, Role::Edit, "", "editor", false),
        ],
        truncated: false,
        offscreen: 0,
    };
    let shown = tree.render();
    assert!(
        shown.contains("Selector window part: app=notepad;window=Untitled - Notepad"),
        "{shown}"
    );
    assert_eq!(shown.matches("Selector window part").count(), 1);
    let line = |id: u32| {
        shown
            .lines()
            .find(|l| l.contains(&format!("[e{id}]")))
            .unwrap()
            .to_string()
    };
    // The ids are as they were, and each line says how to name its control.
    assert!(
        line(1).contains("sel: role=button;name=Save;nth=0"),
        "{}",
        line(1)
    );
    assert!(
        line(2).contains("sel: role=button;name=Save;nth=1"),
        "{}",
        line(2)
    );
    assert!(line(3).contains("sel: role=edit;aid=editor"), "{}", line(3));
    // The flags still end the line.
    assert!(line(2).ends_with("  disabled"), "{}", line(2));
    // And what a line says is what the matcher resolves.
    for (id, said) in [
        (1, "role=button;name=Save;nth=0"),
        (3, "role=edit;aid=editor"),
    ] {
        let got = Selector::parse(said)
            .unwrap()
            .resolve_in(&tree.elements)
            .unwrap();
        assert_eq!(got.id, id);
    }
}

/// What needs the real input thread and the real desktop.
#[cfg(windows)]
mod live {
    use super::*;
    use std::time::Instant;

    fn server() -> Server {
        Server::new(InputThread::start().expect("input thread"))
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

    /// The JSON the tool answered, when it did not refuse.
    fn answer(reply: &Value) -> Option<Value> {
        if reply["result"]["isError"] == true {
            return None;
        }
        let text = reply["result"]["content"][0]["text"].as_str()?;
        // The JSON first, then the untrusted-data note.
        let end = text.rfind('}')?;
        serde_json::from_str(&text[..=end]).ok()
    }

    fn refusal(reply: &Value) -> String {
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    const NO_SUCH_WINDOW: &str = "window=zz no such window 7f3a9c1e";

    #[test]
    fn a_window_that_never_comes_is_a_timeout_reply_not_an_error() {
        let mut s = server();
        let started = Instant::now();
        let reply = call(
            &mut s,
            "wait_for",
            json!({"condition": "window_appears", "selector": NO_SUCH_WINDOW,
                   "timeout_ms": 1500, "poll_ms": 100}),
        );
        let waited = started.elapsed();
        let got = answer(&reply).unwrap_or_else(|| panic!("a timeout is a reply: {reply}"));
        assert_eq!(got["result"], "timeout", "{got}");
        assert_eq!(got["condition"], "window_appears");
        // Listing the windows takes a few hundred ms on a busy desktop, so
        // a 1.5 s wait is a handful of looks, not a dozen.
        assert!(got["polls"].as_u64().unwrap() >= 2, "{got}");
        assert!(got["last_seen"].as_str().is_some(), "{got}");
        assert!(waited.as_millis() >= 1500, "returned early: {waited:?}");
        assert!(
            waited.as_secs() < 10,
            "a 1.5 s wait took {waited:?}: nothing bounded it"
        );
    }

    #[test]
    fn a_window_already_gone_is_found_at_once_and_a_huge_timeout_is_held_to_the_cap() {
        let mut s = server();
        let started = Instant::now();
        let reply = call(
            &mut s,
            "wait_for",
            json!({"condition": "window_gone", "selector": NO_SUCH_WINDOW,
                   "timeout_ms": 99_999_999u64}),
        );
        let got = answer(&reply).unwrap_or_else(|| panic!("{reply}"));
        assert_eq!(got["result"], "found", "{got}");
        assert_eq!(got["polls"], 1);
        assert_eq!(got["timeout_ms"], 60_000, "held to the cap");
        assert!(
            got["notes"][0].as_str().unwrap().contains("over the cap"),
            "the reply says it was held: {got}"
        );
        assert!(started.elapsed().as_secs() < 10);
    }

    #[test]
    fn the_window_in_front_is_found_by_its_program() {
        let Ok(Some(front)) = uia::foreground() else {
            eprintln!("no window in front in this session");
            return;
        };
        if crate::limits::get().blocked(&front.app).is_some() {
            return;
        }
        let mut s = server();
        let selector = format!("app={}", front.app);
        let reply = call(
            &mut s,
            "wait_for",
            json!({"condition": "window_appears", "selector": selector, "timeout_ms": 1000}),
        );
        let got = answer(&reply).unwrap_or_else(|| panic!("{reply}"));
        assert_eq!(got["result"], "found", "{got}");
        let ids: Vec<&str> = got["windows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|w| w["id"].as_str())
            .collect();
        assert!(
            ids.contains(&front.id.as_str()),
            "{ids:?} lacks {}",
            front.id
        );
    }

    /// Every selector `ui_tree` prints for a REAL window picks its own element
    /// back out of the same tree, whatever odd names the application has.
    #[test]
    fn every_selector_a_real_tree_prints_resolves_to_its_own_element() {
        let tree = match uia::window_tree(None, 12) {
            Ok(tree) => tree,
            Err(e) => {
                eprintln!("no focused window in this session: {e}");
                return;
            }
        };
        if crate::limits::get().blocked(&tree.app).is_some() {
            return;
        }
        let selectors = crate::selector::selectors_for(&tree.elements, uia::NAMES_ARE_VALUES, None);
        for (e, s) in tree.elements.iter().zip(&selectors) {
            let again = crate::selector::Selector::parse(&s.render()).unwrap_or_else(|why| {
                panic!("`{}` for {e:?} does not read back: {why}", s.render())
            });
            assert_eq!(&again, s, "{e:?}");
            let got = again
                .resolve_in(&tree.elements)
                .unwrap_or_else(|why| panic!("`{}` for {e:?}: {why}", s.render()));
            assert_eq!(got.id, e.id, "`{}`", s.render());
        }
        let shown = tree.render();
        assert!(shown.contains("Selector window part:"), "{shown}");
        assert_eq!(
            shown.matches("  sel: ").count(),
            tree.elements.len(),
            "every element line carries its selector"
        );
    }

    #[test]
    fn a_selector_that_matches_nothing_is_a_clear_error_and_clicks_nothing() {
        let mut s = server();
        let reply = call(
            &mut s,
            "click_element",
            json!({"selector": "role=button;name=zz no such button 7f3a9c1e"}),
        );
        if reply["result"]["isError"] != true {
            panic!("a selector that matches nothing was acted on: {reply}");
        }
        let why = refusal(&reply);
        // A session with no focused window refuses earlier, in its own words;
        // otherwise the miss is named.
        assert!(
            why.contains("matches no element") || why.contains("window"),
            "{why}"
        );
        assert!(!why.contains("clicked"), "{why}");
    }

    #[test]
    fn a_read_only_session_may_wait_and_a_wait_is_stopped_by_the_stop() {
        let mut s = Server::new(InputThread::start().expect("input thread"))
            .with_tier(crate::permission::Tier::Observe);
        let safety = s.safety();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            safety.stop();
        });
        let started = Instant::now();
        let reply = call(
            &mut s,
            "wait_for",
            json!({"condition": "window_appears", "selector": NO_SUCH_WINDOW,
                   "timeout_ms": 30_000}),
        );
        stopper.join().unwrap();
        // It ran at the read-only tier (a permission refusal would have
        // named the mode), and the stop ended it long before its 30 s.
        let why = refusal(&reply);
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert!(!why.contains("read-only"), "refused by the tier: {why}");
        assert!(why.contains("stopped"), "{why}");
        assert!(
            started.elapsed().as_secs() < 5,
            "the stop took {:?} to end the wait",
            started.elapsed()
        );
    }

    #[test]
    fn the_screen_is_compared_with_a_baseline_the_caller_gives() {
        let mut s = server();
        // All ones: the real screen is not.
        let far = "f".repeat(32);
        let reply = call(
            &mut s,
            "wait_for",
            json!({"condition": "screen_changes", "baseline": far,
                   "tolerance": 0, "timeout_ms": 1000}),
        );
        let Some(got) = answer(&reply) else {
            eprintln!("no capture in this session: {}", refusal(&reply));
            return;
        };
        assert_eq!(got["result"], "found", "{got}");
        assert_eq!(got["polls"], 1);
        assert_eq!(got["hash"].as_str().unwrap().len(), 32);
        assert!(got["distance"].as_u64().unwrap() > 0);

        // And a tolerance of every bit can never be exceeded: a timeout that
        // hands back the last hash to go on from.
        let reply = call(
            &mut s,
            "wait_for",
            json!({"condition": "screen_changes", "tolerance": 128,
                   "timeout_ms": 300, "poll_ms": 100}),
        );
        let got = answer(&reply).unwrap_or_else(|| panic!("{reply}"));
        assert_eq!(got["result"], "timeout", "{got}");
        assert_eq!(got["hash"].as_str().unwrap().len(), 32, "{got}");
    }
}
