//! agy as a model only: the agent file, the command a bridged turn starts,
//! the environment it carries, the setting, and whole turns through
//! `run_turn` on a stand-in agy whose tool calls go over the real bridge to
//! incode's toolbox.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::agent::agy::{AgyBackend, AgyTools, AGY_TOOLS_ENV, HOOKS_BY_ENV};
use crate::agent::{AgentBackend, AgentEvent};

use super::agent_file;

fn args_of(cmd: &std::process::Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// What the command SETS in the environment, by name.
fn set_env(cmd: &std::process::Command, name: &str) -> Option<String> {
    cmd.get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new(name))
        .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

// ---------------------------------------------------------------------------
// The agent file
// ---------------------------------------------------------------------------

/// **THE AGENT FILE MAKES AGY A MODEL WITH ONE SERVER**: `finish` as its only
/// tool of its own (the one name measured to be accepted there), incode's
/// tool server as its only MCP server, main agent, no sub-agent — and a
/// Windows path with a quote in it survives YAML intact.
#[test]
fn the_agent_file_names_incodes_server_and_nothing_else() {
    let exe = Path::new(r"C:\Program Files\it's\incode.exe");
    let text = agent_file::content(exe);
    let (front, body) = text
        .strip_prefix("---\n")
        .and_then(|t| t.split_once("\n---\n"))
        .expect("YAML frontmatter, then the body");
    let name = format!("name: {}", agent_file::agent_name(exe));
    assert_eq!(
        front.lines().collect::<Vec<_>>(),
        [
            name.as_str(),
            "description: incode's model-only agent; every tool is incode's, through its tool bridge.",
            "tools:",
            "  - finish",
            "mcpServers:",
            "  - name: incode",
            r"    command: 'C:\Program Files\it''s\incode.exe'",
            "    args: ['--tool-server']",
            "mainAgent: true",
            "subagent: false",
        ]
    );
    assert!(body.contains("call_mcp_tool"), "{body}");
    assert!(body.contains("ServerName `incode`"), "{body}");
    // `call_mcp_tool` under `tools:` is refused at startup (measured).
    assert!(!front.contains("call_mcp_tool"), "{front}");
}

/// **NO SECRET IN THE FILE**: the bridge's address and token reach the
/// server through agy's environment only. The file is the same bytes
/// whichever turn's bridge is open, and carries neither value nor name.
#[test]
fn the_agent_file_carries_no_secret() {
    let turn = crate::tool_bridge::Turn::open(None).expect("the channel opens");
    let text = agent_file::content(&turn.exe);
    for (name, value) in turn.bridge.env() {
        assert!(!text.contains(name), "{name} in the file: {text}");
        assert!(!text.contains(&value), "{name}'s value in the file");
    }
    let backend = AgyBackend::new(PathBuf::from(".")).with_tools(AgyTools::Incode);
    let config = backend.mcp_config().expect("bridged");
    let env = turn.bridge.env();
    assert!(!config.contains(env[1].1.as_str()));
}

/// **WRITTEN ONLY WHEN IT DIFFERS**, whole, by rename: a first turn writes
/// it, a second with the same binary leaves it alone (its bytes and its
/// modification time), a moved binary or a hand edit gets it rewritten, and
/// no temporary file is left beside it.
#[test]
fn the_agent_file_is_rewritten_only_when_it_differs() {
    let dir = tempfile::tempdir().expect("scratch");
    let agents = dir.path().join("config").join("agents");
    let one = Path::new("/opt/incode/bin/incode");
    let path = agents.join(format!("{}.md", agent_file::agent_name(one)));

    assert_eq!(agent_file::ensure(&agents, one), Ok(true), "first write");
    assert_eq!(
        std::fs::read_to_string(&path).expect("written"),
        agent_file::content(one)
    );
    let stamp = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("mtime");
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(agent_file::ensure(&agents, one), Ok(false), "unchanged");
    assert_eq!(
        std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .expect("mtime"),
        stamp,
        "an unchanged file was rewritten"
    );

    std::fs::write(&path, "---\nname: incode\ntools:\n  - run_command\n---\n").expect("edit");
    assert_eq!(
        agent_file::ensure(&agents, one),
        Ok(true),
        "hand edit undone"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("rewritten"),
        agent_file::content(one)
    );

    let mut left: Vec<_> = std::fs::read_dir(&agents)
        .expect("listed")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [format!("{}.md", agent_file::agent_name(one))],
        "a temporary file was left behind"
    );
}

/// **EACH incode BINARY HAS AN AGENT OF ITS OWN.** The agents folder is
/// machine-wide: with one shared file, two incode binaries running at once
/// rewrote it in turn and pointed each other's next turn at the wrong tool
/// server. Two binaries now write two files and leave each other's alone;
/// the same binary always gets the same name (on Windows whatever the case
/// of its path), so its file is still written once.
#[test]
fn each_incode_binary_has_an_agent_of_its_own() {
    let dir = tempfile::tempdir().expect("scratch");
    let agents = dir.path().join("agents");
    let one = Path::new("/opt/incode/bin/incode");
    let two = Path::new("/usr/local/bin/incode");
    let (name_one, name_two) = (agent_file::agent_name(one), agent_file::agent_name(two));
    assert_ne!(name_one, name_two, "two binaries, two agents");
    assert_eq!(
        name_one,
        agent_file::agent_name(one),
        "stable for one binary"
    );
    assert!(name_one.starts_with("incode-") && name_one.len() == "incode-".len() + 8);
    #[cfg(windows)]
    assert_eq!(
        agent_file::agent_name(Path::new(r"C:\Tools\incode.exe")),
        agent_file::agent_name(Path::new(r"c:\tools\INCODE.EXE")),
        "a Windows path's case names the same binary"
    );

    assert_eq!(agent_file::ensure(&agents, one), Ok(true));
    assert_eq!(agent_file::ensure(&agents, two), Ok(true));
    assert_eq!(
        agent_file::ensure(&agents, one),
        Ok(false),
        "one's file left alone by two"
    );
    for (exe, name) in [(one, &name_one), (two, &name_two)] {
        let file = std::fs::read_to_string(agents.join(format!("{name}.md"))).expect("its file");
        assert_eq!(file, agent_file::content(exe));
        assert!(file.contains(&format!("name: {name}\n")), "{file}");
    }
}

/// **agy's saved results folder is named only for a plain conversation id**:
/// the id arrives on agy's output, and one holding `..` or a separator would
/// widen the folder incode's `Read` is allowed into.
#[test]
fn the_results_folder_is_named_only_for_a_plain_conversation_id() {
    let dir = agent_file::results_dir("643db012-39c5-419d-8868-3d41fc5f669b")
        .expect("a conversation id names a folder");
    assert!(
        dir.ends_with(
            Path::new("antigravity-cli")
                .join("brain")
                .join("643db012-39c5-419d-8868-3d41fc5f669b")
                .join(".system_generated")
                .join("steps")
        ),
        "{}",
        dir.display()
    );
    for bad in ["", "..", "../..", "a/b", r"a\b", "x y", "C:"] {
        assert_eq!(agent_file::results_dir(bad), None, "{bad:?}");
    }
}

/// agy's agents folder is under ITS home: `USERPROFILE` on Windows.
#[test]
fn the_agents_folder_is_agys() {
    let dir = agent_file::agents_dir().expect("a home");
    assert!(
        dir.ends_with(Path::new(".gemini").join("config").join("agents")),
        "{}",
        dir.display()
    );
    #[cfg(windows)]
    assert!(dir.starts_with(std::env::var_os("USERPROFILE").expect("USERPROFILE")));
}

// ---------------------------------------------------------------------------
// The command and the setting
// ---------------------------------------------------------------------------

/// **A BRIDGED TURN STARTS AGY AS THE `incode` AGENT**, permissions skipped
/// because it has no tools of its own, with the bridge's address and token
/// and `INCORD_HOOKS_BY=incode` in its environment — none of them on its
/// argv — and the flags agy's own route hands it, unchanged.
#[test]
fn a_bridged_turn_is_started_as_the_incode_agent_with_the_bridge_in_its_environment() {
    let turn = crate::tool_bridge::Turn::open(None).expect("the channel opens");
    let env = turn.bridge.env();
    let mut backend = AgyBackend::new(PathBuf::from("ws"))
        .with_binary("/opt/agy")
        .with_tools(AgyTools::Incode);
    let cmd = backend.bridged_command("p", "incode-0a1b2c3d", &env);
    assert_eq!(cmd.get_program(), "/opt/agy");
    assert_eq!(cmd.get_current_dir(), Some(Path::new("ws")));
    assert_eq!(
        args_of(&cmd),
        strings(&[
            "--print",
            "p",
            "--output-format",
            "stream-json",
            "--print-timeout",
            "30m",
            "--agent",
            "incode-0a1b2c3d",
            "--dangerously-skip-permissions",
            "--mode",
            "accept-edits",
        ])
    );
    for (name, value) in &env {
        assert_eq!(
            set_env(&cmd, name).as_deref(),
            Some(value.as_str()),
            "{name}"
        );
        assert!(!args_of(&cmd).iter().any(|a| a.contains(value.as_str())));
    }
    assert_eq!(set_env(&cmd, HOOKS_BY_ENV).as_deref(), Some("incode"));
    assert!(
        cmd.get_envs()
            .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
        "issue #3 on this route too"
    );

    // Everything set: the same flags as agy's own route, after the agent's.
    backend.set_models(&strings(&["gemini-3.1-pro-high"]));
    backend.set_permission_mode("read-only");
    backend.set_extra_dirs(vec![PathBuf::from("a")]);
    backend.set_session_id("c-1");
    assert_eq!(
        args_of(&backend.bridged_command("p", "incode-0a1b2c3d", &env))[6..].to_vec(),
        strings(&[
            "--agent",
            "incode-0a1b2c3d",
            "--dangerously-skip-permissions",
            "--model",
            "gemini-3.1-pro-high",
            "--mode",
            "plan",
            "--add-dir",
            "a",
            "--conversation",
            "c-1",
        ])
    );
}

/// **BRIDGED, THE PROMPT NAMES EACH TOOL'S INPUTS**, because agy hands the
/// model no schema it can open on this route (measured live: calls with `{}`
/// until a refusal named the input). Required inputs are starred, the
/// harness's context and the task still follow, and agy's own route sends no
/// such block.
#[test]
fn a_bridged_prompt_names_each_incode_tools_inputs() {
    let bridged = AgyBackend::new(PathBuf::from("."))
        .with_tools(AgyTools::Incode)
        .with_system_context(vec!["<guards>tests/ is protected</guards>".to_string()]);
    let sent = bridged.full_prompt("do the work").expect("sent");
    assert!(sent.starts_with("<incode-tools>\n"), "{sent}");
    for line in [
        "\n- Read(path*, ",
        "\n- Write(path*, content*)\n",
        "\n- Bash(command*",
    ] {
        assert!(sent.contains(line), "{line:?} in {sent}");
    }
    assert!(sent.contains("tests/ is protected"), "{sent}");
    assert!(sent.ends_with("do the work"), "{sent}");

    let own = AgyBackend::new(PathBuf::from(".")).full_prompt("do the work");
    assert_eq!(own.expect("sent"), "do the work");
}

/// Bridged, the backend says so to every caller that asks: its calls run
/// here, it enforces both halves, and it offers the toolbox's tools.
#[test]
fn bridged_agy_runs_its_tools_here() {
    let backend = AgyBackend::new(PathBuf::from(".")).with_tools(AgyTools::Incode);
    assert!(backend.runs_tools_here());
    assert_eq!(backend.enforces(), crate::guards::Enforcement::EVERYTHING);
    let tools = backend.offered_tools();
    for tool in ["Read", "Write", "Edit", "Bash"] {
        assert!(tools.iter().any(|t| t == tool), "{tool}: {tools:?}");
    }
}

/// **AGY'S OWN ROUTE IS AS IT WAS**: `own` — or a backend never told — has
/// no toolbox, enforces nothing, is handed no server, and is started with
/// the same command as before this route existed, with no `--agent` and no
/// hooks switch.
#[test]
fn the_own_route_is_unchanged() {
    let own = AgyBackend::new(PathBuf::from("ws"))
        .with_tools(AgyTools::Incode)
        .with_tools(AgyTools::Own);
    let never_told = AgyBackend::new(PathBuf::from("ws"));
    for backend in [&own, &never_told] {
        assert!(!backend.runs_tools_here());
        assert_eq!(backend.enforces(), crate::guards::Enforcement::NOTHING);
        assert_eq!(backend.mcp_config(), None);
        assert!(backend.offered_tools().is_empty());
        let cmd = backend.command("p");
        assert_eq!(
            args_of(&cmd),
            strings(&[
                "--print",
                "p",
                "--output-format",
                "stream-json",
                "--print-timeout",
                "30m",
                "--mode",
                "accept-edits",
            ])
        );
        assert_eq!(set_env(&cmd, HOOKS_BY_ENV), None);
    }
}

#[test]
fn the_setting_spells_own_and_incode() {
    assert_eq!(AgyTools::parse("own"), Some(AgyTools::Own));
    assert_eq!(AgyTools::parse(" INCODE "), Some(AgyTools::Incode));
    assert_eq!(AgyTools::parse("bridge"), None);
    assert_eq!(AgyTools::parse(""), None);
    assert_eq!(AgyTools::default(), AgyTools::Incode);
    for tools in [AgyTools::Own, AgyTools::Incode] {
        assert_eq!(AgyTools::parse(tools.as_str()), Some(tools));
    }
    assert_eq!(AGY_TOOLS_ENV, "INCODE_AGY_TOOLS");
}

// ---------------------------------------------------------------------------
// The flow, end to end, with a stand-in agy.
// ---------------------------------------------------------------------------

/// Set only in the stand-in agy: the directory it reads the agent file from.
const FAKE_AGY: &str = "INCODE_FAKE_AGY";
/// Set only in the stand-in tool server the stand-in agy starts.
const FAKE_TOOL_SERVER: &str = "INCODE_FAKE_AGY_TOOL_SERVER";
/// In the stand-in's directory: start no tool server, as an agy whose
/// server failed to start would.
const NO_SERVER: &str = "no-server";

/// The tool server half, as `incode --tool-server` runs it.
#[test]
fn tool_server_child() {
    if std::env::var_os(FAKE_TOOL_SERVER).is_none() {
        return;
    }
    let _ = crate::tool_bridge::run_stdio();
    std::process::exit(0);
}

/// The server agy was told to start, read off the agent file the way agy
/// reads it: the `command:` and `args:` of its one `mcpServers` entry.
fn server_in(agent_file: &str) -> (String, Vec<String>) {
    let unquote = |s: &str| {
        s.trim()
            .trim_start_matches('\'')
            .trim_end_matches('\'')
            .replace("''", "'")
    };
    let command = agent_file
        .lines()
        .find_map(|l| l.strip_prefix("    command: "))
        .map(unquote)
        .expect("a command");
    let args = agent_file
        .lines()
        .find_map(|l| l.strip_prefix("    args: "))
        .map(|a| {
            a.trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(unquote)
                .collect()
        })
        .expect("args");
    (command, args)
}

/// One `step_update` line, as agy prints them.
fn step(index: u64, kind: &str, extra: &serde_json::Value) -> serde_json::Value {
    let mut update = serde_json::json!({"step_index": index, "state": "DONE", "step_type": kind});
    if let (Some(update), Some(extra)) = (update.as_object_mut(), extra.as_object()) {
        update.extend(extra.clone());
    }
    serde_json::json!({"event": "step_update", "step_update": update})
}

/// A stand-in agy: reads the agent file it was pointed at, starts the MCP
/// server that file names with its OWN environment (agy's, measured), and
/// over MCP stdio lists the tools and calls `Write` twice, printing each as
/// agy prints a `call_mcp_tool` step. It says which hooks switch it was
/// handed, then ends the turn with a result line.
#[test]
fn fake_agy_child() {
    use std::io::Write as _;
    let Some(dir) = std::env::var_os(FAKE_AGY) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let say = |line: serde_json::Value| println!("{line}");
    say(
        serde_json::json!({"event":"init","conversation_id":"fake-conv","init":{"tools":["run_command","invoke_subagent"]}}),
    );
    let hooks_by = std::env::var(HOOKS_BY_ENV).unwrap_or_default();
    let result = serde_json::json!({"event":"result","result":{"conversation_id":"fake-conv","status":"SUCCESS","usage":{"input_tokens":1,"output_tokens":1}}});
    if let Ok(how) = std::fs::read_to_string(dir.join(NO_SERVER)) {
        if how == "error" {
            // As agy refuses a model it does not know: at once, no server.
            say(
                serde_json::json!({"event":"result","result":{"conversation_id":"","status":"ERROR","error":"model x is not recognized"}}),
            );
            std::process::exit(1);
        }
        say(step(
            1,
            "agent_response",
            &serde_json::json!({"text_delta": "no tools"}),
        ));
        say(result);
        std::process::exit(0);
    }
    // The one agent file in the folder it was pointed at: this binary's.
    let agent = std::fs::read_dir(dir.join("agents"))
        .expect("agents folder")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "md"))
        .expect("an agent file");
    let file = std::fs::read_to_string(agent).expect("agent file");
    let (command, args) = server_in(&file);
    let mut server = std::process::Command::new(&command)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the tool server");
    let mut to = server.stdin.take().expect("stdin");
    let mut from = BufReader::new(server.stdout.take().expect("stdout"));
    let mut ask = |id: u64, method: &str, params: serde_json::Value| -> serde_json::Value {
        let request = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(to, "{request}").expect("send");
        to.flush().expect("flush");
        loop {
            let mut line = String::new();
            if from.read_line(&mut line).expect("read") == 0 {
                panic!("the tool server went away");
            }
            // The test harness's own banner is not a reply.
            if let Ok(reply) = serde_json::from_str::<serde_json::Value>(&line) {
                if reply["id"] == id {
                    return reply;
                }
            }
        }
    };
    ask(
        0,
        "initialize",
        serde_json::json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake-agy","version":"0"}}),
    );
    let listed = ask(1, "tools/list", serde_json::json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    say(step(
        1,
        "agent_response",
        &serde_json::json!({"text_delta": format!("tools: {}", names.join(","))}),
    ));
    for (n, path) in [(2, "notes.txt"), (3, "secret.txt")] {
        let arguments = serde_json::json!({"path": path, "content": "written"});
        let reply = ask(
            n,
            "tools/call",
            serde_json::json!({"name":"Write","arguments":arguments}),
        );
        say(step(
            n,
            "tool",
            &serde_json::json!({"tool_name":"call_mcp_tool","tool_info":{"name":"call_mcp_tool",
                "parameters":{"ServerName":"incode","ToolName":"Write","Arguments":arguments},
                "output": reply["result"]["content"][0]["text"]}}),
        ));
    }
    drop(to);
    let _ = server.wait();
    say(step(
        9,
        "agent_response",
        &serde_json::json!({"text_delta": format!("done hooks_by={hooks_by}")}),
    ));
    say(result);
    std::process::exit(0);
}

/// A script at `path` that runs this test binary's `test` with `marker` set
/// to `value`, first recording its own arguments to `records_argv` when
/// given: NUL-separated on Unix (`printf '%s\0' "$@"`), as cmd's `%*` on
/// Windows — enough to find a flag in, if not to rebuild a quoted argv.
fn child_script(path: &Path, records_argv: Option<&Path>, marker: &str, value: &Path, test: &str) {
    let exe = std::env::current_exe().expect("test binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let before = records_argv
            .map(|argv| format!("printf '%s\\0' \"$@\" > '{}'", argv.display()))
            .unwrap_or_default();
        let body = format!(
            "#!/bin/sh\n{before}\n{marker}='{}' exec '{}' --exact '{test}' --nocapture -q --test-threads=1\n",
            value.display(),
            exe.display()
        );
        std::fs::write(path, body).expect("write the script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("runnable");
    }
    #[cfg(windows)]
    {
        let before = records_argv
            .map(|argv| format!("echo %* > \"{}\"\r\n", argv.display()))
            .unwrap_or_default();
        let body = format!(
            "@echo off\r\n{before}set \"{marker}={}\"\r\n\"{}\" --exact {test} --nocapture -q --test-threads=1\r\n",
            value.display(),
            exe.display()
        );
        std::fs::write(path, body).expect("write the script");
    }
}

/// A bridged backend in `ws` whose "agy" is the stand-in in `bin` — which
/// records its arguments to `bin/argv` and reads the agent file from
/// `bin/agents` — and whose tool server is the stand-in server; with a deny
/// rule on `Write(secret.txt)` in incode's toolbox.
///
/// Its turns are run with `run_bridged`, not `run_turn`: `run_turn` opens the
/// prompt with the multi-line `<incode-tools>` block, and on Windows the
/// stand-in is a batch file, to which Rust refuses to pass an argument holding
/// a newline. The block is pinned by `a_bridged_prompt_names_each_incode_tools_inputs`
/// and `run_turn`'s choice of route by `run_turn_on_incodes_tools_takes_the_bridged_route`.
fn stand_in_bridged_backend(ws: &Path, bin: &Path) -> AgyBackend {
    let script = if cfg!(windows) { "cmd" } else { "sh" };
    let agy = bin.join(format!("agy.{script}"));
    let server = bin.join(format!("server.{script}"));
    child_script(
        &agy,
        Some(&bin.join("argv")),
        FAKE_AGY,
        bin,
        "agent::agy::bridged::tests::fake_agy_child",
    );
    child_script(
        &server,
        None,
        FAKE_TOOL_SERVER,
        Path::new("1"),
        "agent::agy::bridged::tests::tool_server_child",
    );
    let mut backend = AgyBackend::new(ws.to_path_buf())
        .with_binary(agy.to_string_lossy())
        .with_tools(AgyTools::Incode)
        .with_stand_ins(server, bin.join("agents"));
    let mut rules = crate::permissions::rules::PermissionRules::none();
    rules.add(
        &crate::permissions::rules::RuleLists {
            deny: vec!["Write(secret.txt)".to_string()],
            ..Default::default()
        },
        crate::permissions::rules::Layer::User,
        None,
    );
    backend
        .tools
        .as_mut()
        .expect("bridged")
        .set_permission_rules(rules);
    backend
}

/// **A BRIDGED AGY TURN RUNS ITS TOOL CALLS IN incode's TOOLBOX**, end to
/// end: the backend writes the agent file, opens the bridge and starts "agy"
/// as the `incode` agent; the stand-in starts the server the file names,
/// which reaches the bridge through the environment it inherited, and calls
/// `Write` twice over MCP. The first lands — written by incode. The second
/// is refused by a deny rule in incode's toolbox and nothing is written.
/// Each call is shown once, under incode's id; agy's own record of it is
/// not shown again. agy was handed the hooks switch.
#[test]
fn a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one() {
    let ws = tempfile::tempdir().expect("ws");
    let bin = tempfile::tempdir().expect("bin");
    let mut backend = stand_in_bridged_backend(ws.path(), bin.path());

    let mut seen = Vec::new();
    let outcome = backend
        .run_bridged("write two files", &mut |e| seen.push(e.clone()))
        .expect("the turn completes");
    assert!(!outcome.is_error, "{outcome:?}");
    assert_eq!(outcome.session_id, "fake-conv");
    assert_eq!(backend.session_id(), Some("fake-conv"));

    // The arguments agy was really started with: the bridged ones.
    let argv = String::from_utf8_lossy(&std::fs::read(bin.path().join("argv")).expect("argv"))
        .replace('\0', " ");
    for flag in [
        "--agent incode",
        "--dangerously-skip-permissions",
        "--output-format stream-json",
    ] {
        assert!(argv.contains(flag), "{flag}: {argv:?}");
    }

    // The model saw incode's tools.
    assert!(
        seen.iter().any(|e| matches!(e, AgentEvent::Text(t) if t.starts_with("tools: ") && t.contains("Write") && t.contains("Read"))),
        "{seen:?}"
    );
    // The first call ran in incode's toolbox; the second was refused there.
    assert_eq!(
        std::fs::read_to_string(ws.path().join("notes.txt")).expect("written by incode"),
        "written"
    );
    assert!(
        !ws.path().join("secret.txt").exists(),
        "the deny rule did not hold"
    );
    let uses: Vec<(&str, &str)> = seen
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        uses,
        [("incode-1", "Write"), ("incode-2", "Write")],
        "each call once, under incode's id: {seen:?}"
    );
    let results: Vec<(&str, bool, &str)> = seen
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error,
                output,
            } => Some((id.as_str(), *is_error, output.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "{seen:?}");
    assert_eq!((results[0].0, results[0].1), ("incode-1", false));
    assert_eq!((results[1].0, results[1].1), ("incode-2", true));
    assert!(
        results[1].2.contains("Write(secret.txt)"),
        "refused by the rule, by name: {}",
        results[1].2
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::Text(t) if t == "done hooks_by=incode")),
        "{seen:?}"
    );
}

/// **A TURN WHOSE TOOL SERVER NEVER ASKED FOR THE TOOLS IS REFUSED**: its
/// model had none, so its reply — and its `SUCCESS` — is not a result.
#[test]
fn a_bridged_turn_without_incodes_tools_is_refused() {
    let ws = tempfile::tempdir().expect("ws");
    let bin = tempfile::tempdir().expect("bin");
    let mut backend = stand_in_bridged_backend(ws.path(), bin.path());
    std::fs::write(bin.path().join(NO_SERVER), "").expect("mark");
    let err = backend
        .run_bridged("write a file", &mut |_| {})
        .expect_err("no tools were listed");
    assert!(
        err.to_string()
            .starts_with("Antigravity never started incode's tool server"),
        "{err}"
    );
}

/// **`run_turn` ON incode's TOOLS TAKES THE BRIDGED ROUTE**: the agent file
/// is written before agy is started — here an agy that cannot be — and on
/// agy's own tools it is not.
#[test]
fn run_turn_on_incodes_tools_takes_the_bridged_route() {
    let ws = tempfile::tempdir().expect("ws");
    let bin = tempfile::tempdir().expect("bin");
    let missing = bin.path().join("no-such-agy");
    let agents = bin.path().join("agents");
    let mut bridged = AgyBackend::new(ws.path().to_path_buf())
        .with_binary(missing.to_string_lossy())
        .with_tools(AgyTools::Incode)
        .with_stand_ins(PathBuf::from("/opt/incode/bin/incode"), agents.clone());
    let err = bridged
        .run_turn("hi", &mut |_| {})
        .expect_err("nothing to start");
    assert!(err.to_string().starts_with("could not start"), "{err}");
    let exe = Path::new("/opt/incode/bin/incode");
    assert_eq!(
        std::fs::read_to_string(agents.join(format!("{}.md", agent_file::agent_name(exe))))
            .expect("the agent file"),
        agent_file::content(exe)
    );

    let mut own = AgyBackend::new(ws.path().to_path_buf()).with_binary(missing.to_string_lossy());
    own.agents_dir = Some(bin.path().join("own-agents"));
    let _ = own.run_turn("hi", &mut |_| {});
    assert!(!bin.path().join("own-agents").exists());
}

/// **A FAILURE AGY REPORTED KEEPS ITS OWN REASON**, though no server was
/// started: agy refuses an unknown model at once (measured live), and that
/// reason — not "never started incode's tool server" — is what moves the
/// work to another agent and what the person needs to read.
#[test]
fn a_failure_agy_reported_keeps_its_own_reason() {
    let ws = tempfile::tempdir().expect("ws");
    let bin = tempfile::tempdir().expect("bin");
    let mut backend = stand_in_bridged_backend(ws.path(), bin.path());
    std::fs::write(bin.path().join(NO_SERVER), "error").expect("mark");
    let outcome = backend
        .run_bridged("write a file", &mut |_| {})
        .expect("agy's own failure is an outcome");
    assert!(outcome.is_error);
    assert_eq!(outcome.stop_reason, "ERROR");
    assert_eq!(outcome.detail.as_deref(), Some("model x is not recognized"));
    assert!(backend.agent_failed(&outcome), "the work moves");
}
