use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::*;

/// Issue #3: launched from inside another agent session, a child that
/// inherits its variables reports that session as its own — on both the
/// fresh and the resumed spawn.
#[test]
fn the_cli_never_joins_the_session_that_launched_incode() {
    let mut backend = CodexBackend::new(PathBuf::from("."));
    for resumed in [false, true] {
        if resumed {
            backend.thread_id = Some("t-1".to_string());
        }
        let cmd = backend.command("p");
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
            "CLAUDECODE is inherited (resumed={resumed}), so the child joins the parent's session"
        );
    }
}

#[test]
fn foreign_model_id_is_dropped() {
    let mut backend = CodexBackend::new(PathBuf::from("."));
    backend.set_models(&[
        "claude-sonnet-4-6".to_string(),
        "gemini-3.8-flash-high".to_string(),
    ]);
    assert_eq!(backend.model(), None);

    backend.set_models(&["claude-sonnet-4-6".to_string(), "gpt-5.6-terra".to_string()]);
    assert_eq!(backend.model(), Some("gpt-5.6-terra"));
}

#[test]
fn role_ladder_contains_supported_models() {
    let backend = CodexBackend::new(PathBuf::from("."));
    for role in crate::roles::KNOWN_ROLES {
        for id in backend.ladder_for(role) {
            assert!(
                backend.serves(&id),
                "role {role} drew model {id} which codex cannot serve"
            );
        }
    }
    assert_ne!(
        backend.ladder_for("explore"),
        backend.ladder_for("implement")
    );
}

#[test]
fn harness_context_is_carried_in_prompt() {
    let bare = CodexBackend::new(PathBuf::from("."));
    assert_eq!(
        bare.full_prompt("fix the bug").expect("sent"),
        ("fix the bug".to_string(), false)
    );

    let briefed = CodexBackend::new(PathBuf::from("."))
        .with_system_context(vec!["Guards: no writes outside src/".to_string()]);
    let (sent, on_stdin) = briefed.full_prompt("fix the bug").expect("sent");
    assert!(sent.contains("Guards: no writes outside src/"));
    assert!(sent.ends_with("fix the bug"));
    assert!(!on_stdin, "a short prompt stays an argument");
}

/// **A PROMPT TOO LONG FOR THE COMMAND LINE GOES ON STDIN, WHOLE** — `codex
/// exec -` — never cut and never spilled into a file the model must read.
#[test]
fn a_prompt_too_long_for_the_command_line_goes_on_stdin_whole() {
    let dir = tempfile::tempdir().expect("scratch");
    let briefed = CodexBackend::new(dir.path().to_path_buf())
        .with_system_context(vec!["Guards: no writes outside src/".to_string()]);
    let long = format!(
        "{}\nthe question itself",
        "context ".repeat(crate::agent::MAX_CMDLINE_PAYLOAD_LEN / 8 + 100)
    );
    let (sent, on_stdin) = briefed.full_prompt(&long).expect("sent");
    assert!(on_stdin, "over the cap it goes on stdin");
    assert!(
        sent.starts_with("Guards: no writes outside src/"),
        "context first"
    );
    assert!(
        sent.ends_with("the question itself"),
        "and the whole prompt, last line kept"
    );
    assert!(
        !dir.path().join(crate::hooks::memory::CONTEXT).exists(),
        "nothing was spilled to a file"
    );
}

#[test]
fn enforces_claims_nothing() {
    let backend = CodexBackend::new(PathBuf::from("."));
    assert_eq!(backend.enforces(), crate::guards::Enforcement::NOTHING);
}

/// **A TURN'S PICTURES GO AS `--image` FILES, BEFORE ANY OTHER FLAG**, on
/// a new thread and on a resumed one, so the prompt is never read as one
/// more file; the files are removed when the turn's hold on them ends.
#[test]
fn pictures_go_first_as_image_files_and_are_removed_after() {
    let picture = crate::toolbox::Image::from_base64(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
    )
    .expect("a 1x1 png");
    let files = ImageFiles::write(std::slice::from_ref(&picture)).expect("written");
    let path = files.paths[0].clone();
    assert_eq!(std::fs::read(&path).expect("on disk"), picture.bytes());
    let mut backend = CodexBackend::new(PathBuf::from("."));
    backend.image_files.clone_from(&files.paths);
    for resumed in [false, true] {
        if resumed {
            backend.thread_id = Some("t-1".to_string());
        }
        let args: Vec<String> = backend
            .command("the prompt")
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = if resumed { 2 } else { 1 };
        assert_eq!(args[at], "--image", "{args:?}");
        assert_eq!(args[at + 1], path.to_string_lossy(), "{args:?}");
        assert!(
            args[at + 2].starts_with("--"),
            "the list is ended: {args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some("the prompt"));
    }
    drop(files);
    assert!(!path.exists(), "the picture outlived its turn");
}

#[test]
fn command_includes_mcp_auto_approval_config() {
    let backend = CodexBackend::new(PathBuf::from("."));
    let cmd = backend.command("test prompt");
    let args: Vec<std::ffi::OsString> = cmd.get_args().map(|a| a.to_os_string()).collect();
    let args_str: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert!(
        args_str
            .windows(2)
            .any(|w| w[0] == "-c" && w[1].contains("default_tools_approval_mode=\"auto\"")),
        "codex command must include auto approval for memory MCP tools so calls are not cancelled"
    );
}

/// **THE TEAM IDENTITY MUST REACH THE SPAWNED PROCESS'S ENVIRONMENT.**
///
/// The team MCP server is a child of the agent and reads who it speaks
/// for from this variable. It cannot come from the MCP registration —
/// that is written once and the role changes every task — so if it does
/// not land on the spawn, every mid-turn message is filed as `unrouted`
/// and the channel stops being addressable.
#[test]
fn the_team_identity_is_set_on_the_spawn() {
    let mut backend = CodexBackend::new(PathBuf::from("."));
    backend.set_team_identity("implement");

    let cmd = backend.command("test prompt");
    let found = cmd
        .get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new(crate::team_mcp::IDENTITY_ENV));

    let (_, value) = found.expect("the identity never reached the environment");
    assert_eq!(
        value.expect("a value").to_string_lossy(),
        "implement",
        "the agent would speak under the wrong name"
    );

    // And a backend nobody told stays silent rather than inventing one.
    let plain = CodexBackend::new(PathBuf::from("."));
    assert!(
        !plain
            .command("p")
            .get_envs()
            .any(|(k, _)| k == std::ffi::OsStr::new(crate::team_mcp::IDENTITY_ENV)),
        "an identity was invented for a task that had no role"
    );
}

/// **THE DESKTOP MUST REACH ARGV, ON BOTH SPAWN PATHS.**
///
/// `mcp_config()` is an accessor — it reads the field back and would keep
/// answering after the `-c` loops were deleted. What the spawned process
/// actually receives is argv, so that is what this asserts. Both paths,
/// because `command` (`argv.rs`) builds a *different* argument list when
/// resuming a thread than when starting fresh: wiring one and forgetting
/// the other is a desktop that works until the first resume.
#[test]
fn the_desktop_overrides_reach_argv_on_both_spawn_paths() {
    let overrides = crate::desktop::mcp::codex_config_overrides(
        std::path::Path::new(r"C:\Users\someone\incode.exe"),
        Some(std::path::Path::new(r"C:\work\.incode\desktop.jsonl")),
        crate::permissions::PermissionMode::WorkspaceWrite,
    );

    let argv = |backend: &CodexBackend| -> Vec<String> {
        backend
            .command("test prompt")
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    };
    let carries = |args: &[String]| {
        args.windows(2)
            .filter(|w| w[0] == "-c" && w[1].starts_with("mcp_servers.desktop."))
            .count()
    };

    let fresh = CodexBackend::new(PathBuf::from(".")).with_mcp_overrides(overrides.clone());
    assert_eq!(
        carries(&argv(&fresh)),
        overrides.len(),
        "fresh spawn dropped the desktop: {:?}",
        argv(&fresh)
    );

    let mut resumed = CodexBackend::new(PathBuf::from(".")).with_mcp_overrides(overrides.clone());
    resumed.thread_id = Some("thread-abc".to_string());
    assert_eq!(
        carries(&argv(&resumed)),
        overrides.len(),
        "resume spawn dropped the desktop: {:?}",
        argv(&resumed)
    );

    // And without it, nothing: the flag is not unconditional.
    let plain = CodexBackend::new(PathBuf::from("."));
    assert_eq!(carries(&argv(&plain)), 0);
}

/// **CODEX'S OWN HOOKS ARE OFF ON BOTH SPAWN PATHS.** incode fires the
/// memory engine's lifecycle itself; codex loading its hooks file too
/// would record every session twice, under two ids.
#[test]
fn codex_is_started_with_its_own_hooks_off_on_both_paths() {
    let off = |backend: &CodexBackend| {
        let args: Vec<String> = backend
            .command("hi")
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        args.windows(2)
            .any(|w| w[0] == "--disable" && w[1] == "hooks")
    };
    let fresh = CodexBackend::new(PathBuf::from("."));
    assert!(off(&fresh));
    let mut resumed = CodexBackend::new(PathBuf::from("."));
    resumed.thread_id = Some("thread-abc".to_string());
    assert!(off(&resumed));
    for mut backend in [fresh, resumed] {
        backend = backend.with_tools(CodexTools::Incode);
        assert!(off(&backend), "bridged too");
    }
}

// -----------------------------------------------------------------------
// Bridged: codex as a model, incode's toolbox as its tools.
// -----------------------------------------------------------------------

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// Every `-c` value on an argv.
fn overrides_in(args: &[String]) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == "-c")
        .map(|w| w[1].clone())
        .collect()
}

/// **CODEX ON ITS OWN TOOLS IS STARTED EXACTLY AS BEFORE.** The whole argv,
/// both spawn paths, pinned: the bridged route must not leak a flag onto
/// the route that has run live.
#[test]
fn the_own_tools_argv_is_unchanged() {
    let mut backend = CodexBackend::new(PathBuf::from("."))
        .with_mcp_overrides(vec!["mcp_servers.team.command=\"incode\"".to_string()]);
    backend.set_models(&["gpt-5.5".to_string()]);
    backend.set_permission_mode("plan");
    backend.set_extra_dirs(vec![PathBuf::from("/more")]);
    let auto = "mcp_servers.memory.default_tools_approval_mode=\"auto\"";
    let team = "mcp_servers.team.command=\"incode\"";
    assert_eq!(
        args_of(&backend.command("the task")),
        [
            "exec",
            "--skip-git-repo-check",
            "--json",
            "--disable",
            "hooks",
            "-c",
            auto,
            "-c",
            team,
            "-s",
            "read-only",
            "-m",
            "gpt-5.5",
            "--add-dir",
            "/more",
            "-C",
            ".",
            "the task",
        ]
    );
    backend.thread_id = Some("t-1".to_string());
    assert_eq!(
        args_of(&backend.command("again")),
        [
            "exec",
            "resume",
            "--skip-git-repo-check",
            "--json",
            "--disable",
            "hooks",
            "--dangerously-bypass-approvals-and-sandbox",
            "-c",
            auto,
            "-c",
            team,
            "-m",
            "gpt-5.5",
            "t-1",
            "again",
        ]
    );
    assert!(!backend.runs_tools_here());
    assert!(backend.offered_tools().is_empty());
}

/// **BRIDGED, CODEX IS STARTED AS A MODEL AND NOTHING ELSE**, on both
/// spawn paths: its user config unread, every tool switch off, read-only
/// and never asking, and incode's tool server its one MCP server — and
/// none of the flags the other route hands it, because incode's toolbox
/// decides what they decided.
#[test]
fn bridged_codex_is_started_as_a_model_with_incodes_server_only() {
    let exe = Path::new(r"C:\Program Files\incode\incode.exe");
    let mut backend = CodexBackend::new(PathBuf::from("."))
        .with_mcp_overrides(vec!["mcp_servers.team.command=\"incode\"".to_string()])
        .with_tools(CodexTools::Incode);
    backend.set_models(&["gpt-5.5".to_string()]);
    backend.set_permission_mode("acceptEdits");
    backend.set_extra_dirs(vec![PathBuf::from("/more")]);
    backend.set_team_identity("implement");
    for resumed in [false, true] {
        if resumed {
            backend.thread_id = Some("t-1".to_string());
        }
        let cmd = backend.bridged_command("the task", exe);
        let args = args_of(&cmd);
        let lead: &[&str] = if resumed {
            &["exec", "resume"]
        } else {
            &["exec"]
        };
        assert_eq!(&args[..lead.len()], lead, "{args:?}");
        for flag in ["--skip-git-repo-check", "--json", "--ignore-user-config"] {
            assert!(args.iter().any(|a| a == flag), "{flag}: {args:?}");
        }
        assert!(args.windows(2).any(|w| w == ["--disable", "hooks"]));
        let overrides = overrides_in(&args);
        let mut expected: Vec<String> = MODEL_ONLY_OVERRIDES
            .iter()
            .map(|o| (*o).to_string())
            .collect();
        expected.extend([
            r#"mcp_servers.incode.command="C:\\Program Files\\incode\\incode.exe""#.to_string(),
            r#"mcp_servers.incode.args=["--tool-server"]"#.to_string(),
            r#"mcp_servers.incode.env_vars=["INCODE_TOOL_BRIDGE","INCODE_TOOL_TOKEN"]"#.to_string(),
            r#"mcp_servers.incode.default_tools_approval_mode="approve""#.to_string(),
            "mcp_servers.incode.required=true".to_string(),
            "mcp_servers.incode.tool_timeout_sec=3600".to_string(),
            r#"mcp_servers.incode.omit_tools_from=["deferred"]"#.to_string(),
        ]);
        assert_eq!(overrides, expected, "resumed={resumed}");
        for gone in [
            "--dangerously-bypass-approvals-and-sandbox",
            "-s",
            "--add-dir",
        ] {
            assert!(!args.iter().any(|a| a == gone), "{gone}: {args:?}");
        }
        assert!(
            !overrides
                .iter()
                .any(|o| o.starts_with("mcp_servers.team.") || o.starts_with("mcp_servers.memory.")),
            "a server beside the bridge: {overrides:?}"
        );
        assert_eq!(
            args.iter().any(|a| a == "-C"),
            !resumed,
            "`exec resume` takes no -C: {args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some("the task"));
        if resumed {
            assert_eq!(args[args.len() - 2], "t-1", "{args:?}");
        }
        assert!(
            !cmd.get_envs()
                .any(|(k, _)| k == std::ffi::OsStr::new(crate::team_mcp::IDENTITY_ENV)),
            "the team identity is the toolbox's on this route"
        );
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
            "issue #3 on this route too"
        );
    }
    // The mode and the roots went to the toolbox, not to codex.
    assert_eq!(backend.sandbox, None);
}

/// Bridged, the backend says so to every caller that asks: its calls run
/// here, it enforces both halves, and it offers the toolbox's tools.
#[test]
fn bridged_codex_runs_its_tools_here() {
    let backend = CodexBackend::new(PathBuf::from(".")).with_tools(CodexTools::Incode);
    assert!(backend.runs_tools_here());
    assert_eq!(backend.enforces(), crate::guards::Enforcement::EVERYTHING);
    let tools = backend.offered_tools();
    for tool in ["Read", "Write", "Edit", "Bash", "team_ask"] {
        assert!(tools.iter().any(|t| t == tool), "{tool}: {tools:?}");
    }
    assert!(
        backend
            .mcp_config()
            .is_some_and(|c| c.contains("--tool-server")),
        "{:?}",
        backend.mcp_config()
    );
    // And setting it back to codex's own is the route as it was.
    let own = CodexBackend::new(PathBuf::from("."))
        .with_tools(CodexTools::Incode)
        .with_tools(CodexTools::Own);
    assert!(!own.runs_tools_here());
    assert_eq!(own.mcp_config(), None);
}

#[test]
fn the_setting_spells_own_and_incode() {
    assert_eq!(CodexTools::parse("own"), Some(CodexTools::Own));
    assert_eq!(CodexTools::parse(" INCODE "), Some(CodexTools::Incode));
    assert_eq!(CodexTools::parse("bridge"), None);
    assert_eq!(CodexTools::parse(""), None);
    assert_eq!(CodexTools::default(), CodexTools::Own);
    for tools in [CodexTools::Own, CodexTools::Incode] {
        assert_eq!(CodexTools::parse(tools.as_str()), Some(tools));
    }
}

// -----------------------------------------------------------------------
// The flow, end to end, with a stand-in codex.
// -----------------------------------------------------------------------

/// Set only in the stand-in codex the flow test starts: the directory it
/// reads its argv from.
const FAKE_CODEX: &str = "INCODE_FAKE_CODEX";
/// Set only in the stand-in tool server the stand-in codex starts.
const FAKE_TOOL_SERVER: &str = "INCODE_FAKE_TOOL_SERVER";

/// The value of the `-c` override for `key`, as codex's TOML reads it.
fn override_value<'a>(overrides: &'a [String], key: &str) -> &'a str {
    overrides
        .iter()
        .find_map(|o| o.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} among {overrides:?}"))
}

/// The tool server half, as `incode --tool-server` runs it.
#[test]
fn tool_server_child() {
    if std::env::var_os(FAKE_TOOL_SERVER).is_none() {
        return;
    }
    let _ = crate::tool_bridge::run_stdio();
    std::process::exit(0);
}

/// A stand-in codex: reads the argv it was started with, starts the MCP
/// server it was told to — with ONLY the variables `env_vars` names, as
/// codex does — and, over MCP stdio, lists the tools and calls `Write`
/// twice. It prints what it did as `codex exec --json` lines.
#[test]
fn fake_codex_child() {
    use std::io::Write as _;
    let Some(dir) = std::env::var_os(FAKE_CODEX) else {
        return;
    };
    let dir = PathBuf::from(dir);
    // The registration codex was handed: off its real argv where the
    // stand-in's wrapper could record it (`printf '%s\0' "$@"`, Unix), else
    // as the backend builds it (`argv.rs` `bridge_registration`), written by
    // the test — `cmd` cannot hand on an argv carrying quotes intact.
    let overrides = match std::fs::read(dir.join("argv")) {
        Ok(argv) => {
            let args: Vec<String> = argv
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            overrides_in(&args)
        }
        Err(_) => serde_json::from_slice(
            &std::fs::read(dir.join("registration")).expect("the registration"),
        )
        .expect("a list of overrides"),
    };
    let command = override_value(&overrides, "mcp_servers.incode.command")
        .trim_matches('"')
        .replace("\\\\", "\\");
    let server_args: Vec<String> =
        serde_json::from_str(override_value(&overrides, "mcp_servers.incode.args")).expect("args");
    let passed: Vec<String> =
        serde_json::from_str(override_value(&overrides, "mcp_servers.incode.env_vars"))
            .expect("env_vars");
    // Codex's own short list beside the named ones (`rmcp-client`
    // `create_env_for_mcp_server`); on Windows that includes what a process
    // needs to open a socket or run a `.cmd` at all.
    #[cfg(windows)]
    let defaults = ["PATH", "SystemRoot", "ComSpec", "PATHEXT", "TEMP", "TMP"];
    #[cfg(not(windows))]
    let defaults = ["PATH"];
    let mut server = std::process::Command::new(&command)
        .args(&server_args)
        .env_clear()
        .envs(
            defaults
                .iter()
                .copied()
                .chain(passed.iter().map(String::as_str))
                .filter_map(|name| Some((name, std::env::var_os(name)?))),
        )
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
    let say = |line: serde_json::Value| println!("{line}");
    say(serde_json::json!({"type":"thread.started","thread_id":"fake-thread"}));
    say(serde_json::json!({"type":"turn.started"}));
    ask(
        0,
        "initialize",
        serde_json::json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake-codex","version":"0"}}),
    );
    let listed = ask(1, "tools/list", serde_json::json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    say(
        serde_json::json!({"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":format!("tools: {}", names.join(","))}}),
    );
    for (n, path) in [(2, "notes.txt"), (3, "secret.txt")] {
        let arguments = serde_json::json!({"path": path, "content": "written"});
        let item = format!("item_{n}");
        say(
            serde_json::json!({"type":"item.started","item":{"id":item,"type":"mcp_tool_call","server":"incode","tool":"Write","arguments":arguments,"result":null,"error":null,"status":"in_progress"}}),
        );
        let reply = ask(
            n,
            "tools/call",
            serde_json::json!({"name":"Write","arguments":arguments}),
        );
        let failed = reply["result"]["isError"] == true;
        say(
            serde_json::json!({"type":"item.completed","item":{"id":item,"type":"mcp_tool_call","server":"incode","tool":"Write","arguments":arguments,
            "result": if failed { serde_json::Value::Null } else { serde_json::json!({"content": reply["result"]["content"]}) },
            "error": if failed { serde_json::json!({"message": reply["result"]["content"][0]["text"]}) } else { serde_json::Value::Null },
            "status": if failed { "failed" } else { "completed" }}}),
        );
    }
    drop(to);
    let _ = server.wait();
    say(
        serde_json::json!({"type":"item.completed","item":{"id":"item_9","type":"agent_message","text":"done"}}),
    );
    say(serde_json::json!({"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}));
    std::process::exit(0);
}

/// A script at `path` that runs this test binary's `test` with `marker` set
/// to `value`: a shell script on Unix, which first records its argv to
/// `records_argv` when given; a `.cmd` on Windows, which records nothing
/// (see [`fake_codex_child`]).
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
        let _ = records_argv;
        let body = format!(
            "@echo off\r\nset \"{marker}={}\"\r\n\"{}\" --exact {test} --nocapture -q --test-threads=1\r\n",
            value.display(),
            exe.display()
        );
        std::fs::write(path, body).expect("write the script");
    }
}

/// A bridged backend in `ws` whose "codex" is the stand-in in `bin` — which
/// records its argv to `bin/argv` on Unix, and on Windows is handed the
/// registration the backend builds, in `bin/registration` — and whose tool
/// server is the stand-in server, returned beside it; with a deny rule on
/// `Write(secret.txt)` in incode's toolbox.
///
/// Called by [`a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one`]
/// and by `bridged_tests.rs`.
pub(super) fn stand_in_bridged_backend(ws: &Path, bin: &Path) -> (CodexBackend, PathBuf) {
    let script = if cfg!(windows) { "cmd" } else { "sh" };
    let codex = bin.join(format!("codex.{script}"));
    let server = bin.join(format!("server.{script}"));
    child_script(
        &codex,
        Some(&bin.join("argv")),
        FAKE_CODEX,
        bin,
        "agent::codex::tests::fake_codex_child",
    );
    child_script(
        &server,
        None,
        FAKE_TOOL_SERVER,
        Path::new("1"),
        "agent::codex::tests::tool_server_child",
    );
    #[cfg(windows)]
    std::fs::write(
        bin.join("registration"),
        serde_json::to_vec(&super::argv::bridge_registration(&server)).expect("json"),
    )
    .expect("write the registration");

    let mut backend = CodexBackend::new(ws.to_path_buf())
        .with_binary(codex.to_string_lossy())
        .with_tools(CodexTools::Incode)
        .with_tool_server(server.clone());
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
    (backend, server)
}

/// Every tool use a turn showed, as `(id, name)`.
///
/// Called by [`a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one`].
fn uses_in(seen: &[AgentEvent]) -> Vec<(&str, &str)> {
    seen.iter()
        .filter_map(|e| match e {
            AgentEvent::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect()
}

/// Every tool result a turn showed, as `(id, is_error, output)`.
///
/// Called by [`a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one`].
fn results_in(seen: &[AgentEvent]) -> Vec<(&str, bool, &str)> {
    seen.iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error,
                output,
            } => Some((id.as_str(), *is_error, output.as_str())),
            _ => None,
        })
        .collect()
}

/// **A BRIDGED CODEX TURN RUNS ITS TOOL CALLS IN incode's TOOLBOX**, end
/// to end: the backend opens the bridge and starts "codex" with the
/// bridged argv; the stand-in reads the registration off that argv,
/// starts the tool server with only the variables it names, and calls
/// `Write` twice over MCP. The first lands — written by incode. The
/// second is refused by a deny rule in incode's toolbox and nothing is
/// written. Each call is shown once, under incode's id; codex's record of
/// it is not shown again.
///
/// The test of `agent/codex/bridged.rs` `run_bridged`, on every platform.
/// The argv codex was started with is checked on Unix only, where the
/// stand-in can record it (see [`child_script`]).
#[test]
fn a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one() {
    let ws = tempfile::tempdir().expect("ws");
    let bin = tempfile::tempdir().expect("bin");
    let (mut backend, server) = stand_in_bridged_backend(ws.path(), bin.path());

    let mut seen = Vec::new();
    let outcome = backend
        .run_turn("write two files", &mut |e| seen.push(e.clone()))
        .expect("the turn completes");
    assert!(!outcome.is_error, "{outcome:?}");
    assert_eq!(outcome.session_id, "fake-thread");
    assert_eq!(backend.session_id(), Some("fake-thread"));

    // The argv codex was really started with: the bridged one.
    #[cfg(unix)]
    {
        let argv = String::from_utf8(std::fs::read(bin.path().join("argv")).expect("argv"))
            .expect("utf-8");
        assert!(argv.contains("--ignore-user-config"), "{argv:?}");
        assert!(argv.contains("features.shell_tool=false"), "{argv:?}");
        assert!(
            argv.contains(&format!(
                "mcp_servers.incode.command=\"{}\"",
                server.display()
            )),
            "{argv:?}"
        );
    }
    #[cfg(windows)]
    let _ = &server;

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
    assert_eq!(
        uses_in(&seen),
        [("incode-1", "Write"), ("incode-2", "Write")],
        "each call once, under incode's id: {seen:?}"
    );
    let results = results_in(&seen);
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
            .any(|e| matches!(e, AgentEvent::Text(t) if t == "done")),
        "{seen:?}"
    );
}
