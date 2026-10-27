//! The desktop server's tests, moved out of `desktop/mcp.rs` unchanged and
//! under the same path (`desktop::mcp::tests::protocol::*` and, on Windows,
//! `desktop::mcp::tests::live::*`): the pure protocol shapes, and the real
//! protocol path through [`Server::handle_line`] with the gates in order.

use super::*;

use super::guard::off_every_screen;
use super::tools::{image, point};
use super::transport::code;
use crate::desktop::capture::ShotFormat;
use crate::desktop::coords::PhysicalPoint;

/// Protocol tests that need no desktop: they exercise the pure shapes.
mod protocol {
    use super::*;

    #[test]
    fn an_unparseable_line_answers_with_a_null_id() {
        // There is no id to echo, and JSON-RPC prescribes null for exactly
        // this case. Inventing one would make a client match the reply to
        // the wrong request.
        let reply = super::super::transport::error_response(
            Value::Null,
            code::PARSE_ERROR,
            "invalid JSON: trailing characters",
        );
        let parsed: Value = serde_json::from_str(&reply).expect("valid JSON");
        assert_eq!(parsed["id"], Value::Null);
        assert_eq!(parsed["error"]["code"], code::PARSE_ERROR);
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    #[test]
    fn every_tool_definition_has_the_three_measured_fields() {
        // The live server's tools each had exactly name/description/
        // inputSchema. A tool missing one is a tool a client may skip.
        for tool in tool_definitions() {
            let name = tool["name"].as_str().expect("a name");
            assert!(
                tool["description"].as_str().is_some_and(|d| d.len() > 40),
                "{name} has no usable description"
            );
            assert_eq!(
                tool["inputSchema"]["type"], "object",
                "{name}'s inputSchema is not an object schema"
            );
        }
    }

    #[test]
    fn the_tools_that_touch_the_screen_carry_the_untrusted_warning() {
        // GLOBAL RULE 17: the warning has to reach the agent's context,
        // which means the description, not a document it will never read.
        let tools = tool_definitions();
        let shot = tools
            .iter()
            .find(|t| t["name"] == "screenshot")
            .expect("a screenshot tool");
        let description = shot["description"].as_str().expect("a description");

        assert!(
            description.contains("UNTRUSTED"),
            "the screenshot description does not warn about untrusted content"
        );
        assert!(
            description.contains("never an instruction"),
            "the warning does not say what to do about it"
        );
    }

    #[test]
    fn action_tools_require_their_coordinates() {
        for name in ["move_mouse", "click"] {
            let tools = tool_definitions();
            let tool = tools.iter().find(|t| t["name"] == name).expect(name);
            let required = tool["inputSchema"]["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} must declare required fields"));

            assert!(required.contains(&json!("x")), "{name} does not require x");
            assert!(required.contains(&json!("y")), "{name} does not require y");
        }
    }

    #[test]
    fn a_missing_coordinate_is_refused_rather_than_defaulted_to_zero() {
        // Defaulting would click the top-left corner of the screen, which
        // on Windows is very close to things that are hard to undo.
        assert!(point(&json!({"y": 10})).is_err());
        assert!(point(&json!({"x": 10})).is_err());
        assert!(point(&json!({"x": "20", "y": 10})).is_err());
        assert_eq!(
            point(&json!({"x": -1920, "y": 5})).expect("negative x is valid"),
            PhysicalPoint { x: -1920, y: 5 }
        );
    }

    /// The config has to name the flag that actually starts the server.
    ///
    /// If these two ever drift, the client launches the binary in its
    /// NORMAL mode — which starts the interactive harness on a pipe, prints
    /// human text to stdout, and looks to the client like a server speaking
    /// gibberish. That is a long debugging session for a one-word mistake.
    #[test]
    fn the_client_config_launches_this_binary_in_server_mode() {
        let config = client_config(
            std::path::Path::new("C:/bin/incode.exe"),
            None,
            crate::permissions::PermissionMode::WorkspaceWrite,
        );
        let server = &config["mcpServers"][CLIENT_KEY];

        assert_eq!(server["command"], "C:/bin/incode.exe");
        assert_eq!(
            server["args"],
            json!(["--desktop-mcp", "-p", "workspace-write"]),
            "the config must pass the flag that selects the server mode"
        );

        // With a journal, the path is passed as its own argument — one the
        // server can only receive this way, since it holds no workspace.
        let watched = client_config(
            std::path::Path::new("C:/bin/incode.exe"),
            Some(std::path::Path::new("C:/work/.incode/desktop.jsonl")),
            crate::permissions::PermissionMode::ReadOnly,
        );
        assert_eq!(
            watched["mcpServers"][CLIENT_KEY]["args"],
            json!([
                "--desktop-mcp",
                "--desktop-journal",
                "C:/work/.incode/desktop.jsonl",
                "-p",
                "read-only"
            ]),
            "the session's mode must reach the child process on its argv"
        );

        // The same string main.rs matches on, so a rename breaks here
        // rather than at a client six months later.
        assert!(
            USAGE_FLAG_IN_MAIN.contains("--desktop-mcp"),
            "the flag in the config no longer matches the one main.rs parses"
        );
    }

    /// The flag `src/main.rs` parses. Kept beside the config that emits it.
    const USAGE_FLAG_IN_MAIN: &str = "--desktop-mcp";

    #[test]
    fn the_config_is_valid_json_for_a_command_line() {
        let text = client_config_json(
            std::path::Path::new("/usr/local/bin/incode"),
            None,
            crate::permissions::PermissionMode::WorkspaceWrite,
        );
        let parsed: Value = serde_json::from_str(&text).expect("must be valid JSON");
        assert_eq!(
            parsed["mcpServers"]["desktop"]["command"],
            "/usr/local/bin/incode"
        );
        // Compact, because it is passed as one shell argument.
        assert!(!text.contains('\n'), "a multi-line config breaks the flag");
    }

    #[test]
    fn a_point_on_no_display_is_refused_and_one_on_any_display_is_not() {
        let screens = [
            crate::desktop::Rect::from_edges(-1920, 0, 0, 1080),
            crate::desktop::Rect::from_edges(0, 0, 2560, 1440),
        ];
        let at = |x, y| PhysicalPoint { x, y };
        for (x, y) in [(0, 0), (-1920, 500), (2559, 1439), (-1, 1079)] {
            assert_eq!(off_every_screen(at(x, y), &screens), None, "{x},{y}");
        }
        for (x, y) in [(0, -16079), (2560, 0), (-1921, 10), (-500, 1100)] {
            let why = off_every_screen(at(x, y), &screens)
                .unwrap_or_else(|| panic!("{x},{y} was not refused"));
            assert!(why.contains("on no display"), "{why}");
        }
    }

    #[test]
    fn an_image_block_is_base64_and_names_its_type() {
        let png = image(&[0x89, b'P', b'N', b'G'], ShotFormat::Png);
        assert_eq!(png["type"], "image");
        assert_eq!(png["mimeType"], "image/png");
        assert_eq!(png["data"], "iVBORw==");

        let jpeg = image(&[0xFF, 0xD8], ShotFormat::Jpeg);
        assert_eq!(jpeg["mimeType"], "image/jpeg");
    }
}

/// These need the input thread, so they are Windows-only — but none of them
/// move anything.
#[cfg(windows)]
mod live {
    use super::super::transport::{PROTOCOL_VERSION, SERVER_NAME};
    use super::*;

    fn server() -> Server {
        Server::new(InputThread::start().expect("input thread"))
    }

    fn call(server: &mut Server, line: &str) -> Value {
        let reply = server.handle_line(line).expect("a reply was expected");
        serde_json::from_str(&reply).expect("the reply must be valid JSON")
    }

    /// **`read-only` MUST MEAN THE AGENT CANNOT TYPE.**
    ///
    /// The mode governed the agent's file writing and stopped there: a
    /// session started `-p read-only` got a desktop server with no
    /// restriction at all, so it could click a button and type into a
    /// login form while being unable to edit a file. `PermissionPolicy`
    /// had been written for exactly this and was never called.
    ///
    /// Both directions in one test. A gate that refuses everything is not
    /// a gate either — looking at the screen is precisely what a
    /// read-only session should still be able to do.
    /// # Why the typing half runs on a STOPPED server
    ///
    /// The first version of this test did not, and its control run —
    /// deleting the gate to prove the test notices — typed `hunter2` into
    /// whatever window happened to have focus on the machine running the
    /// suite. A test whose FAILURE mode is "type a password into the
    /// operator's screen" is not one to leave lying around.
    ///
    /// Stopping safety first makes that impossible: with the gate gone the
    /// call is refused as stopped instead, and nothing reaches the
    /// desktop. The assertion still distinguishes the two, because the
    /// permission refusal names the mode and the stop refusal does not —
    /// and permission is checked BEFORE the stop, so "read-only" in the
    /// message can only have come from the gate.
    #[test]
    fn a_read_only_desktop_may_look_and_may_not_touch() {
        let mut s = Server::new(InputThread::start().expect("input thread"))
            .with_permission_mode(crate::permissions::PermissionMode::ReadOnly);
        s.safety().stop();

        let typed = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"type_text","arguments":{"text":"hunter2"}}}"#,
        );
        assert_eq!(
            typed["result"]["isError"], true,
            "a read-only session typed into the desktop: {typed}"
        );
        let refusal = typed["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            refusal.contains("read-only"),
            "the refusal must name the mode that caused it: {refusal}"
        );
        // THE REFUSAL MUST NOT QUOTE WHAT WAS TYPED. The text reaches the
        // journal and the browser stream, and a refused password is still
        // a password.
        assert!(
            !refusal.contains("hunter2"),
            "the refusal leaked the typed text: {refusal}"
        );

        // AND THE READS STILL WORK — on a server that is not stopped,
        // since a stop halts reads too and would prove nothing about the
        // permission mode.
        let mut looking = Server::new(InputThread::start().expect("input thread"))
            .with_permission_mode(crate::permissions::PermissionMode::ReadOnly);
        let looked = call(
            &mut looking,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":"screen_info","arguments":{}}}"#,
        );
        assert_eq!(
            looked["result"]["isError"], false,
            "read-only must still be able to look at the screen: {looked}"
        );
    }

    /// **AN OPERATOR'S HOOK CAN STOP A KEYSTROKE, AND IS SHOWN WHAT IT
    /// WOULD BE.**
    ///
    /// `hooks` was the other capability that existed, was tested, and had
    /// nobody to call it: the harness stopped dispatching tools, so there
    /// was no before-and-after left in it. The desktop server is the one
    /// that remained, and it is the one where a veto is worth the most —
    /// a click reaches a machine the operator is signed into, and no
    /// checkpoint undoes it.
    ///
    /// The hook denies by exit code 2. Its stdout reaches the agent, so a
    /// refusal can say what to do instead.
    ///
    /// # Why this one cannot stop safety, and uses the mouse instead
    ///
    /// A hook is deliberately NOT run for a call that a mode or a stop has
    /// already refused — spawning a process to ask about work that is not
    /// going to happen is waste. So a stopped server never reaches the
    /// hook, and the stop refusal is what comes back.
    ///
    /// That leaves the control run genuinely able to reach the desktop, so
    /// the tool is `move_mouse` rather than `type_text`: with the wiring
    /// deleted the pointer moves a pixel, which leaks nothing and undoes
    /// itself the moment anyone touches the mouse.
    #[test]
    fn a_hook_can_refuse_an_action_before_it_reaches_the_desktop() {
        let deny = if cfg!(windows) {
            // Echoes what it was handed, then denies — proving both halves
            // of the contract in one command.
            "cmd /C echo blocked: %HOOK_TOOL_NAME% & exit 2"
        } else {
            "echo \"blocked: $HOOK_TOOL_NAME\"; exit 2"
        };

        let mut s = Server::new(InputThread::start().expect("input thread")).with_hooks(
            crate::hooks::HookConfig {
                pre_tool_use: vec![deny.to_string()],
                post_tool_use: Vec::new(),
                user_prompt_submit: Vec::new(),
                pre_compact: Vec::new(),
                stop: Vec::new(),
                notification: Vec::new(),
                session_start: Vec::new(),
                session_end: Vec::new(),
                subagent_stop: Vec::new(),
            },
        );

        let blocked = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"move_mouse","arguments":{"x":1,"y":1}}}"#,
        );
        assert_eq!(blocked["result"]["isError"], true, "{blocked}");

        let refusal = blocked["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            refusal.contains("hook"),
            "the refusal must say a hook caused it: {refusal}"
        );
        assert!(
            refusal.contains("move_mouse"),
            "the hook was not told which tool it was refusing: {refusal}"
        );
    }

    /// A hook that allows must not change anything, or every operator who
    /// adds a logging hook silently loses their desktop.
    #[test]
    fn a_hook_that_allows_leaves_the_call_alone() {
        let allow = if cfg!(windows) {
            "cmd /C exit 0"
        } else {
            "exit 0"
        };
        let mut s = Server::new(InputThread::start().expect("input thread")).with_hooks(
            crate::hooks::HookConfig {
                pre_tool_use: vec![allow.to_string()],
                post_tool_use: Vec::new(),
                user_prompt_submit: Vec::new(),
                pre_compact: Vec::new(),
                stop: Vec::new(),
                notification: Vec::new(),
                session_start: Vec::new(),
                session_end: Vec::new(),
                subagent_stop: Vec::new(),
            },
        );

        let moved = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"move_mouse","arguments":{"x":1,"y":1}}}"#,
        );
        assert_eq!(
            moved["result"]["isError"], false,
            "a permitting hook refused the call: {moved}"
        );
    }

    /// The ordinary mode is unchanged — the gate is a restriction people
    /// opt into, not a new obstacle in the default path.
    #[test]
    fn the_default_mode_can_still_drive_the_desktop() {
        let mut s = Server::new(InputThread::start().expect("input thread"));
        let moved = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"move_mouse","arguments":{"x":1,"y":1}}}"#,
        );
        assert_eq!(
            moved["result"]["isError"], false,
            "the default session lost a capability it had: {moved}"
        );
    }

    #[test]
    fn initialize_answers_the_shape_the_live_server_answers() {
        let mut s = server();
        let reply = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        );

        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(reply["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(reply["result"]["serverInfo"]["version"].is_string());
        assert_eq!(
            reply["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
    }

    #[test]
    fn a_notification_is_never_answered() {
        // It has no id. Answering one is a protocol error some clients
        // treat as fatal.
        let mut s = server();
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
    }

    #[test]
    fn an_unknown_method_gets_the_code_the_live_server_used() {
        let mut s = server();
        let reply = call(&mut s, r#"{"jsonrpc":"2.0","id":9,"method":"no/such"}"#);
        assert_eq!(reply["error"]["code"], code::METHOD_NOT_FOUND);
        assert_eq!(reply["id"], 9);
    }

    #[test]
    fn unparseable_input_does_not_kill_the_server() {
        let mut s = server();
        let reply = call(&mut s, "{not json");
        assert_eq!(reply["error"]["code"], code::PARSE_ERROR);

        // Still serving afterwards.
        let after = call(&mut s, r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#);
        assert!(after["result"].is_object());
    }

    #[test]
    fn tools_list_returns_the_five_built_so_far() {
        let mut s = server();
        let reply = call(&mut s, r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
        let tools = reply["result"]["tools"].as_array().expect("an array");

        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        for expected in [
            "screen_info",
            "screenshot",
            "ui_tree",
            "click_element",
            "type_text",
            "paste_text",
            "key",
            "scroll",
            "drag",
            "cursor_position",
            "move_mouse",
            "click",
        ] {
            assert!(names.contains(&expected), "{expected} is not listed");
        }
    }

    /// An agent must be able to READ which keys exist.
    ///
    /// Without the vocabulary in the description it would have to guess
    /// names and collect refusals one at a time.
    #[test]
    fn the_key_tool_lists_the_names_it_accepts() {
        let tools = tool_definitions();
        let key = tools
            .iter()
            .find(|t| t["name"] == "key")
            .expect("a key tool");
        let description = key["description"].as_str().expect("a description");

        for name in ["control", "enter", "escape", "pagedown", "f1-f12"] {
            assert!(
                description.contains(name),
                "'{name}' is not documented: {description}"
            );
        }
        assert!(
            description.contains("single character"),
            "the description must say a bare character works: {description}"
        );
    }

    /// paste_text uses the clipboard and puts it back, and says so.
    #[test]
    fn paste_says_it_puts_the_clipboard_back() {
        let tools = tool_definitions();
        let paste = tools
            .iter()
            .find(|t| t["name"] == "paste_text")
            .expect("a paste tool");
        let description = paste["description"].as_str().expect("a description");
        assert!(
            description.contains("clipboard") && description.contains("put back"),
            "an agent must know what happens to the clipboard: {description}"
        );
    }

    /// The tree is offered as the BETTER path, not merely another one.
    ///
    /// An agent choosing between `screenshot` and `ui_tree` reads only the
    /// descriptions, so the preference has to be stated there.
    #[test]
    fn the_tree_tells_the_agent_to_prefer_it_over_guessing_pixels() {
        let tools = tool_definitions();
        let tree = tools
            .iter()
            .find(|t| t["name"] == "ui_tree")
            .expect("a ui_tree tool");
        let description = tree["description"].as_str().expect("a description");

        assert!(
            description.contains("PREFER THIS OVER screenshot"),
            "got: {description}"
        );
        assert!(
            description.contains("UNTRUSTED"),
            "the tree is screen content too: {description}"
        );
    }

    /// A stale id must be refused, not clicked at its old position.
    #[test]
    fn clicking_a_vanished_element_is_refused_with_a_reason() {
        let mut s = server();
        // An id far beyond anything a real walk produces.
        let reply = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"click_element","arguments":{"id":999999}}}"#,
        );

        assert!(
            reply.get("error").is_none(),
            "must be a tool error: {reply}"
        );
        assert_eq!(reply["result"]["isError"], true);

        let why = reply["result"]["content"][0]["text"]
            .as_str()
            .expect("a reason");
        // An id no tree showed is refused before any walk: either no
        // tree of that window was read, or it was not in the last one.
        assert!(
            why.contains("ui_tree")
                || why.contains("no longer")
                || why.contains("no focused")
                || why.contains("unavailable"),
            "the refusal must explain, not just fail: {why}"
        );
    }

    #[test]
    fn screen_info_comes_back_through_the_wire_as_real_geometry() {
        let mut s = server();
        let reply = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"screen_info","arguments":{}}}"#,
        );

        assert_eq!(reply["result"]["isError"], false);
        let body = reply["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        let info: Value = serde_json::from_str(body).expect("screen_info must be JSON");

        assert_eq!(info["coordinates_are_physical"], true);
        assert!(
            info["monitors"][0]["rect"]["width"].as_i64().unwrap_or(0) > 0,
            "no usable display came back: {body}"
        );
    }

    /// A TOOL FAILURE IS NOT A PROTOCOL FAILURE.
    ///
    /// If a refused click came back as a JSON-RPC error, the agent would
    /// conclude the transport is broken and retry it, instead of reading
    /// why it was refused.
    #[test]
    fn a_refused_action_is_a_tool_error_not_a_transport_error() {
        let mut s = server();
        s.safety().stop();

        let reply = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"click","arguments":{"x":10,"y":10}}}"#,
        );

        assert!(
            reply.get("error").is_none(),
            "a refused click must not be a JSON-RPC error: {reply}"
        );
        assert_eq!(reply["result"]["isError"], true);

        let why = reply["result"]["content"][0]["text"]
            .as_str()
            .expect("a reason");
        assert!(
            why.contains("permanent"),
            "the refusal reason did not survive the wire: {why}"
        );
    }

    /// **A CLICK ON NO DISPLAY IS REFUSED** (#52): sent, it would be
    /// clamped to a screen edge and click whatever is there. Through the
    /// real protocol path, on a running (not stopped) desktop, with this
    /// machine's real displays. (The pointer is not compared before and
    /// after: on a desktop a person is using, that measures the person.)
    #[test]
    fn a_click_on_no_display_is_refused() {
        let mut s = server();
        let reply = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"click","arguments":{"x":0,"y":-16079}}}"#,
        );
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        let why = reply["result"]["content"][0]["text"]
            .as_str()
            .expect("a reason");
        assert!(why.contains("0,-16079 is on no display"), "{why}");
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error_too() {
        let mut s = server();
        let reply = call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"launch_missiles","arguments":{}}}"#,
        );
        assert!(reply.get("error").is_none());
        assert_eq!(reply["result"]["isError"], true);
    }

    /// **A REFUSAL MUST REACH THE OPERATOR'S JOURNAL.**
    ///
    /// The whole point of the semantic channel (`COMPUTER_USE.md` 14c) is
    /// that an operator sees the agent being STOPPED, not only the agent
    /// succeeding. A journal that records successes and swallows refusals
    /// would show a quiet desktop during exactly the incident someone is
    /// watching for.
    ///
    /// Driven through `handle_line`, so it is the real protocol path
    /// rather than a direct call to the recorder.
    #[test]
    fn a_refused_call_is_written_to_the_journal() {
        let path = std::env::temp_dir().join(format!(
            "incode-mcp-journal-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let mut s =
            Server::new(InputThread::start().expect("input thread")).with_journal(path.clone());
        s.safety().stop();

        call(
            &mut s,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"click","arguments":{"x":10,"y":10}}}"#,
        );

        let entries = crate::desktop::journal::since(&path, None);
        assert_eq!(entries.len(), 1, "the refusal was not journalled");
        assert_eq!(entries[0].t, "refused");
        assert!(!entries[0].ok);
        assert!(
            entries[0].detail.starts_with("click:"),
            "the journal must name the tool that was refused: {}",
            entries[0].detail
        );

        let _ = std::fs::remove_file(&path);
    }

    /// **TYPED TEXT MUST NOT REACH THE JOURNAL.**
    ///
    /// The journal goes to disk and to every browser holding the stream
    /// open, and the agent will be typing into real accounts. This drives
    /// a recognisable secret through the real protocol path and then reads
    /// the file back looking for it.
    ///
    /// There are two ways the text could get out, and both are checked:
    /// the REFUSAL path, which formats its own message from the tool's
    /// error, and the SUCCESS path, which formats through
    /// [`Server::describe`].
    ///
    /// The desktop is stopped first so nothing is actually typed. That is
    /// not only politeness toward whatever window has focus on the machine
    /// running the suite — a test that really pressed twenty-nine keys into
    /// an unknown application is a test nobody can run twice.
    #[test]
    fn a_typed_secret_never_reaches_the_journal() {
        const SECRET: &str = "hunter2-correct-horse-battery";

        let path = std::env::temp_dir().join(format!(
            "incode-mcp-secret-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let mut s =
            Server::new(InputThread::start().expect("input thread")).with_journal(path.clone());
        s.safety().stop();

        let line = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {"name": "type_text", "arguments": {"text": SECRET}},
        })
        .to_string();
        call(&mut s, &line);

        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !raw.is_empty(),
            "nothing was journalled at all, so this proved nothing"
        );
        assert!(
            !raw.contains(SECRET),
            "the refusal path wrote the typed text to the journal: {raw}"
        );

        // The success path formats through `describe`, which the stopped
        // desktop above never reaches. Checked directly rather than by
        // really typing.
        let described = Server::describe("type_text", &json!({"text": SECRET}));
        assert!(
            !described.contains(SECRET),
            "the success path would write the typed text: {described}"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The redaction is a LENGTH, not a silence — an operator still needs
    /// to see that typing happened and roughly how much.
    #[test]
    fn typing_is_journalled_as_a_length() {
        assert_eq!(
            Server::describe("type_text", &json!({"text": "hello"})),
            "5 characters"
        );
        // Characters, not bytes: the same rule `thread::key_from_name`
        // follows. "é" is two bytes and one character.
        assert_eq!(
            Server::describe("paste_text", &json!({"text": "café"})),
            "4 characters"
        );
    }

    /// A chord is a closed vocabulary of key names, so it is safe to show
    /// in full — and it is the detail that makes a journal line readable.
    #[test]
    fn a_chord_is_journalled_by_name() {
        assert_eq!(
            Server::describe("key", &json!({"keys": ["ctrl", "c"]})),
            "ctrl+c"
        );
    }

    /// **THE STOP FILE MUST HALT A RUNNING SERVER.**
    ///
    /// This is the operator's half of the kill switch. `Safety::stop` is
    /// only reachable from inside this process, so without a file there is
    /// no way to halt a desktop that is already driving a machine — the
    /// switch existed but nothing could press it.
    ///
    /// Driven through `handle_line`, and the SAME tool call is made before
    /// and after, so the only thing that changed is the file.
    #[test]
    fn a_stop_file_halts_a_server_that_was_working() {
        let journal = std::env::temp_dir().join(format!(
            "incode-stop-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let stop = crate::desktop::safety::stop_file_beside(&journal);
        let _ = std::fs::remove_file(&journal);
        let _ = std::fs::remove_file(&stop);

        let mut s =
            Server::new(InputThread::start().expect("input thread")).with_journal(journal.clone());

        // A harmless read, to prove the server is working BEFORE the stop.
        // Without this the test could pass on a server that never worked.
        let probe = r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"screen_info","arguments":{}}}"#;
        assert_eq!(
            call(&mut s, probe)["result"]["isError"],
            false,
            "the server must work before the stop, or this proves nothing"
        );

        crate::desktop::safety::request_stop(&stop).expect("request the stop");

        let after = call(&mut s, probe);
        assert_eq!(
            after["result"]["isError"], true,
            "the stop file did not reach the running server"
        );
        let why = after["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            why.contains("permanent"),
            "a stop must read as permanent, not as a transient failure: {why}"
        );

        // And it is permanent: removing the file does not un-press it.
        std::fs::remove_file(&stop).expect("remove the stop file");
        assert_eq!(
            call(&mut s, probe)["result"]["isError"],
            true,
            "deleting the stop file un-pressed the kill switch"
        );

        // The operator sees their own stop land in the channel they were
        // watching — that is what makes it confirmable.
        let entries = crate::desktop::journal::since(&journal, None);
        assert!(
            entries.iter().any(|e| e.t == "refused" && !e.ok),
            "the stop was not journalled: {entries:?}"
        );

        let _ = std::fs::remove_file(&journal);
    }

    /// The stop file sits beside the journal, and both are derived once.
    ///
    /// Two independently-built paths is how a desktop ends up reporting to
    /// one directory and listening in another.
    #[test]
    fn watching_and_stopping_name_the_same_place() {
        let workspace = std::path::Path::new("C:/work");
        let journal = crate::desktop::journal::path_in(workspace);
        assert_eq!(
            crate::desktop::safety::stop_path_in(workspace),
            crate::desktop::safety::stop_file_beside(&journal),
            "the workspace path and the beside-the-journal path disagree"
        );
        assert_eq!(
            crate::desktop::safety::stop_path_in(workspace).parent(),
            journal.parent(),
            "the stop file must live beside the journal"
        );
    }

    /// A tool nobody wrote a description for is named, not dumped.
    ///
    /// The default arm must not fall back to printing `args`, or the next
    /// tool that takes text leaks it the day it is added.
    #[test]
    fn an_undescribed_tool_does_not_dump_its_arguments() {
        let described = Server::describe("some_future_tool", &json!({"text": "a-secret-value"}));
        assert_eq!(described, "some_future_tool");
        assert!(!described.contains("a-secret-value"));
    }
}
