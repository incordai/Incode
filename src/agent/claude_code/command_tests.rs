//! Models only: the CLI's tools off, incode's toolbox on — the argv and
//! environment a turn is started with, the one server it is handed, and the
//! deny rules rendered in this agent's syntax.

use std::process::Command;

use super::command::{servers_for, user_message, MODEL_ONLY_ENV};
use super::deny::{render_denials, SUBAGENT_TOOL};
use super::profiles::PROFILE_ENV;
use super::*;

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn value_after(args: &[String], flag: &str) -> Option<String> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).cloned()
}

/// **THE CLI IS STARTED AS A MODEL AND NOTHING ELSE**: no tools of its
/// own, only incode's servers, none of its settings — and none of the
/// harness flags it used to be handed, because it has nothing left for
/// them to govern.
#[test]
fn the_cli_is_started_with_its_own_harness_switched_off() {
    let mut backend =
        ClaudeCodeBackend::new(PathBuf::from(".")).with_guards(&crate::guards::Guards::default());
    backend.set_permission_mode("plan");
    assert!(
        !backend.deny_rules().is_empty(),
        "control: there are deny rules that could leak onto the argv"
    );
    let (servers, allowed) = servers_for(std::path::Path::new("incode"));
    let cmd = backend.command(Some("hi"), "", &servers, &allowed);
    let args = args_of(&cmd);

    assert_eq!(
        value_after(&args, "--tools").as_deref(),
        Some(""),
        "{args:?}"
    );
    assert_eq!(
        value_after(&args, "--setting-sources").as_deref(),
        Some(""),
        "{args:?}"
    );
    assert!(args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
    assert!(
        args.iter().any(|a| a == "--disable-slash-commands"),
        "{args:?}"
    );
    assert_eq!(
        value_after(&args, "--allowedTools").as_deref(),
        Some("mcp__incode")
    );
    for gone in [
        "--permission-mode",
        "--disallowedTools",
        "--add-dir",
        "--dangerously-skip-permissions",
        "--bare",
    ] {
        assert!(
            !args.iter().any(|a| a == gone),
            "{gone} reached the CLI: {args:?}"
        );
    }
    let envs: Vec<(String, String)> = cmd
        .get_envs()
        .filter_map(|(k, v)| {
            Some((
                k.to_string_lossy().into_owned(),
                v?.to_string_lossy().into_owned(),
            ))
        })
        .collect();
    for (key, value) in MODEL_ONLY_ENV {
        assert!(
            envs.iter().any(|(k, v)| k == key && v == value),
            "{key} is not set: {envs:?}"
        );
    }
}

/// **A TURN IS ITS OWN SESSION, NEVER THE ONE INCODE WAS LAUNCHED FROM.**
/// Issue #3: run from inside another session, every turn reported that
/// session's id as its own.
#[test]
fn the_cli_never_joins_the_session_that_launched_incode() {
    let backend = ClaudeCodeBackend::new(PathBuf::from("."));
    let (servers, allowed) = servers_for(std::path::Path::new("incode"));
    for prompt in [Some("hi"), None] {
        let cmd = backend.command(prompt, "", &servers, &allowed);
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
            "CLAUDECODE is inherited, so the turn joins the parent's session"
        );
    }
}

/// **THE DESKTOP IS THE TOOLBOX'S, NOT A SERVER BESIDE THE BRIDGE.** With
/// the desktop switched on, the model's tool list (the toolbox's) holds
/// the desktop tools, and the CLI is handed the bridge and nothing else.
#[test]
fn the_bridge_is_the_only_server_and_the_desktop_is_the_toolboxs() {
    let off = ClaudeCodeBackend::new(PathBuf::from("."));
    assert!(
        !off.tools.names().iter().any(|n| n == "screenshot"),
        "a desktop nobody switched on"
    );
    let on =
        ClaudeCodeBackend::new(PathBuf::from(".")).with_desktop(crate::toolbox::DesktopSetup {
            exe: PathBuf::from("incode"),
            journal: PathBuf::from(".incode/desktop.jsonl"),
        });
    let names = on.tools.names();
    for tool in ["screenshot", "screen_info", "ui_tree", "click", "type_text"] {
        assert!(names.iter().any(|n| n == tool), "{tool} missing: {names:?}");
    }
    let (servers, allowed) = servers_for(std::path::Path::new("incode"));
    let parsed: serde_json::Value = serde_json::from_str(&servers).expect("json");
    let keys: Vec<&String> = parsed["mcpServers"]
        .as_object()
        .expect("servers")
        .keys()
        .collect();
    assert_eq!(keys, [crate::tool_bridge::CLIENT_KEY], "{servers}");
    assert_eq!(
        parsed["mcpServers"][crate::tool_bridge::CLIENT_KEY]["args"][0],
        crate::tool_bridge::SERVER_FLAG
    );
    assert_eq!(allowed, ["mcp__incode".to_string()]);
}

/// **A TURN WITH PICTURES GIVES ITS PROMPT ON STDIN**, as the stream-json
/// message the CLI was measured reading: no prompt on the argv, the input
/// format named, and the message holding the text then each picture as a
/// base64 image block. Thinking is asked for as a readable summary when
/// partial messages are on.
#[test]
fn a_turn_with_pictures_sends_its_prompt_as_a_stream_json_message() {
    let backend = ClaudeCodeBackend::new(PathBuf::from(".")).streaming(true);
    let (servers, allowed) = servers_for(std::path::Path::new("incode"));
    let args = args_of(&backend.command(None, "", &servers, &allowed));
    assert_eq!(value_after(&args, "-p").as_deref(), Some("--input-format"));
    assert_eq!(
        value_after(&args, "--input-format").as_deref(),
        Some("stream-json")
    );
    assert_eq!(
        value_after(&args, "--thinking-display").as_deref(),
        Some("summarized")
    );
    let plain = args_of(&backend.command(Some("hi"), "", &servers, &allowed));
    assert_eq!(value_after(&plain, "-p").as_deref(), Some("hi"));
    assert!(!plain.iter().any(|a| a == "--input-format"), "{plain:?}");

    let picture = crate::toolbox::Image::from_base64(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
    )
    .expect("a 1x1 png");
    let line = user_message("what is this", std::slice::from_ref(&picture));
    assert!(line.ends_with('\n'));
    let message: serde_json::Value = serde_json::from_str(line.trim()).expect("json");
    assert_eq!(message["type"], "user");
    let content = &message["message"]["content"];
    assert_eq!(
        content[0],
        serde_json::json!({"type": "text", "text": "what is this"})
    );
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[1]["source"]["data"], picture.base64());
}

/// **`Edit`, never `Write`.** Measured: a rule denying only `Write(tests/**)`
/// let the file through, and this CLI reports that only `Edit(path)` rules
/// are matched by file permission checks. A guard that parses and does
/// nothing is worse than no guard, so the tool name is pinned.
///
/// This test lives HERE, beside the renderer, because it is a fact about one
/// agent's permission layer rather than about the harness's policy.
#[test]
fn the_rendered_rules_use_the_tool_name_that_actually_binds() {
    let rules = render_denials(&crate::guards::restrictions(
        &crate::guards::Guards::default(),
    ));
    assert!(!rules.is_empty(), "the defaults must produce rules");
    for rule in rules.iter().filter(|r| *r != SUBAGENT_TOOL) {
        assert!(
            rule.starts_with("Edit("),
            "`Write(...)` is silently ignored by the permission layer: {rule}"
        );
    }
}

/// The policy's "no sub-agents" becomes THIS agent's word for it.
#[test]
fn refusing_sub_agents_renders_as_this_agents_tool_name() {
    let with = crate::guards::Restrictions {
        no_write: Vec::new(),
        no_subagents: true,
    };
    let without = crate::guards::Restrictions {
        no_write: Vec::new(),
        no_subagents: false,
    };
    assert_eq!(render_denials(&with), vec![SUBAGENT_TOOL.to_string()]);
    assert!(
        render_denials(&without).is_empty(),
        "a policy that permits delegation must not deny it anyway"
    );
}

/// A bare glob from the policy is wrapped, not passed through raw.
///
/// The policy deliberately carries `**/tests/**` with no syntax around it;
/// handing that to the CLI unwrapped would be accepted and enforce nothing.
#[test]
fn a_bare_policy_glob_is_wrapped_in_this_agents_syntax() {
    let rules = render_denials(&crate::guards::Restrictions {
        no_write: vec!["**/tests/**".to_string()],
        no_subagents: false,
    });
    assert_eq!(rules, vec!["Edit(**/tests/**)".to_string()]);
}

// ---------------------------------------------------------------------------
// Pinned behaviour: the exact argv and environment a turn is started with.
// ---------------------------------------------------------------------------

/// The environment a command sets or removes, minus the entries that
/// depend on the machine: the profile choice, and the session variables
/// removed only when this process inherited them.
fn pinned_envs(cmd: &Command) -> Vec<(String, Option<String>)> {
    let mut envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .filter(|(k, _)| {
            let upper = k.to_ascii_uppercase();
            upper != PROFILE_ENV
                && upper != "SESSION_INGRESS_URL"
                && !upper.starts_with("CLAUDE_CODE_SESSION")
                && !upper.starts_with("CLAUDE_CODE_REMOTE")
        })
        .collect();
    envs.sort();
    envs
}

/// The environment every turn's command carries, sorted.
fn turn_envs() -> Vec<(String, Option<String>)> {
    let mut envs: Vec<(String, Option<String>)> = [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_SESSION_INGRESS_TOKEN_FILE",
    ]
    .into_iter()
    .map(|k| (k.to_string(), None))
    .chain([
        (
            "CLAUDE_CODE_DISABLE_CLAUDE_MDS".to_string(),
            Some("1".to_string()),
        ),
        (
            "CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(),
            Some("1".to_string()),
        ),
    ])
    .collect();
    envs.sort();
    envs
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// **THE EXACT ARGV AND ENVIRONMENT, FOR A PLAIN TURN.** Guards, a
/// permission mode and extra roots change nothing on the command line:
/// the toolbox enforces them.
#[test]
fn a_plain_turn_is_started_with_exactly_this_command() {
    let mut backend = ClaudeCodeBackend::new(PathBuf::from("."))
        .with_guards(&crate::guards::Guards::default())
        .with_extra_dirs(vec![PathBuf::from("other-root")]);
    backend.set_permission_mode("acceptEdits");
    let allowed = strings(&["mcp__incode"]);
    let cmd = backend.command(Some("hi"), "", "SERVERS", &allowed);
    assert_eq!(cmd.get_program(), "claude");
    assert_eq!(
        args_of(&cmd),
        strings(&[
            "-p",
            "hi",
            "--output-format",
            "stream-json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--disable-slash-commands",
            "--verbose",
            "--mcp-config",
            "SERVERS",
            "--allowedTools",
            "mcp__incode",
        ])
    );
    assert_eq!(pinned_envs(&cmd), turn_envs());
    assert_eq!(cmd.get_current_dir(), None);
}

/// **THE EXACT ARGV FOR A TURN WITH EVERYTHING SET**: pictures (prompt on
/// stdin), a model chain, streaming, context and a session to resume.
#[test]
fn a_fully_configured_turn_is_started_with_exactly_this_command() {
    let backend = ClaudeCodeBackend::new(PathBuf::from("."))
        .with_binary("/opt/agent/claude")
        .with_models(&strings(&["opus", "sonnet", "haiku"]))
        .with_session("s-9")
        .streaming(true);
    let allowed = strings(&["a", "b"]);
    let cmd = backend.command(None, "CTX", "SERVERS", &allowed);
    assert_eq!(cmd.get_program(), "/opt/agent/claude");
    assert_eq!(
        args_of(&cmd),
        strings(&[
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--disable-slash-commands",
            "--verbose",
            "--mcp-config",
            "SERVERS",
            "--allowedTools",
            "a",
            "b",
            "--model",
            "opus",
            "--include-partial-messages",
            "--thinking-display",
            "summarized",
            "--append-system-prompt",
            "CTX",
            "--fallback-model",
            "sonnet,haiku",
            "--resume",
            "s-9",
        ])
    );
    assert_eq!(pinned_envs(&cmd), turn_envs());
}

/// Once the CLI has refused the thinking flag, a streaming turn is
/// started without it — and nothing else changes.
#[test]
fn a_turn_after_the_thinking_flag_was_refused_omits_only_that_flag() {
    let mut backend = ClaudeCodeBackend::new(PathBuf::from(".")).streaming(true);
    backend.thinking_display = false;
    let cmd = backend.command(Some("q"), "", "S", &[]);
    assert_eq!(
        args_of(&cmd),
        strings(&[
            "-p",
            "q",
            "--output-format",
            "stream-json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--disable-slash-commands",
            "--verbose",
            "--mcp-config",
            "S",
            "--allowedTools",
            "--include-partial-messages",
        ])
    );
}

/// The stdin message for a turn with no pictures, byte for byte.
#[test]
fn the_stdin_message_is_pinned() {
    assert_eq!(
        user_message("hi", &[]),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n"
    );
}
