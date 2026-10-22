//! The operator's own hooks around every toolbox tool, and a sub-agent's
//! allow-list, both at [`super::Toolbox::run_call`]'s gate.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;

/// A pre-tool hook that writes every tool it is shown to `calls.txt` beside
/// it and refuses `Write` with exit 2; the command that runs it.
fn logging_hook(dir: &Path) -> (String, PathBuf) {
    let calls = dir.join("calls.txt");
    #[cfg(windows)]
    {
        let script = dir.join("gate.cmd");
        std::fs::write(
            &script,
            "@echo off\r\necho %HOOK_TOOL_NAME%>>\"%~dp0calls.txt\"\r\n\
             if \"%HOOK_TOOL_NAME%\"==\"Write\" (\r\n\
             echo writes are refused by the operator 1>&2\r\nexit /b 2\r\n)\r\nexit /b 0\r\n",
        )
        .expect("hook");
        (format!("\"{}\"", script.display()), calls)
    }
    #[cfg(not(windows))]
    {
        let script = dir.join("gate.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"$HOOK_TOOL_NAME\" >> \"$(dirname \"$0\")/calls.txt\"\n\
             if [ \"$HOOK_TOOL_NAME\" = \"Write\" ]; then \
             echo 'writes are refused by the operator' >&2; exit 2; fi\nexit 0\n",
        )
        .expect("hook");
        (format!("sh '{}'", script.display()), calls)
    }
}

fn hooked(ws: &Path, pre: String, post: Vec<String>) -> Toolbox {
    Toolbox::new(ws, true).with_operator_hooks(Some(crate::hooks::HookRunner::new(
        crate::hooks::HookConfig {
            pre_tool_use: vec![pre],
            post_tool_use: post,
            user_prompt_submit: Vec::new(),
        },
    )))
}

/// **THE OPERATOR'S HOOK RUNS BEFORE EVERY TOOL, AND EXIT 2 REFUSES.** Not
/// only the desktop's: a read, a search and a write each reach it, and the
/// refused write never happens — the model is told the hook's reason.
#[test]
fn the_operators_pre_hook_runs_for_every_tool_and_exit_two_refuses() {
    let ws = tempfile::tempdir().expect("ws");
    let hooks = tempfile::tempdir().expect("hooks");
    std::fs::write(ws.path().join("a.txt"), "hello").expect("seed");
    let (gate, calls) = logging_hook(hooks.path());
    let tools = hooked(ws.path(), gate, Vec::new());

    tools
        .run(READ, &json!({"path": "a.txt"}))
        .expect("a read runs");
    tools
        .run(GLOB, &json!({"pattern": "*.txt"}))
        .expect("a search runs");
    let refused = tools
        .run(WRITE, &json!({"path": "b.txt", "content": "x"}))
        .expect_err("the hook refuses writes");

    assert!(
        refused.contains("writes are refused by the operator"),
        "{refused}"
    );
    assert!(
        !ws.path().join("b.txt").exists(),
        "the refused write happened"
    );
    let seen = std::fs::read_to_string(&calls).expect("the hook ran");
    let seen: Vec<&str> = seen.lines().map(str::trim).collect();
    assert_eq!(seen, ["Read", "Glob", "Write"], "{seen:?}");
}

/// A post hook is told each call that ran; what it prints reaches the model
/// beside the result, and it cannot turn a result into a refusal.
#[test]
fn the_operators_post_hook_speaks_after_the_call_and_cannot_refuse() {
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("a.txt"), "hello").expect("seed");
    let (allow, say) = if cfg!(windows) {
        (
            "exit /b 0".to_string(),
            "echo checked by the operator& exit /b 2",
        )
    } else {
        (
            "exit 0".to_string(),
            "echo 'checked by the operator'; exit 2",
        )
    };
    let tools = hooked(ws.path(), allow, vec![say.to_string()]);
    let said = tools
        .run(READ, &json!({"path": "a.txt"}))
        .expect("still a result");
    assert!(said.contains("hello"), "{said}");
    assert!(
        said.contains("[operator hook] checked by the operator"),
        "{said}"
    );
}

/// **A SUB-AGENT'S ALLOW-LIST IS ENFORCED AT THE GATE**: only its tools are
/// offered, and a call to any other is refused before anything runs.
#[test]
fn a_restricted_toolbox_offers_and_runs_only_its_tools() {
    let ws = tempfile::tempdir().expect("ws");
    std::fs::write(ws.path().join("a.txt"), "hello").expect("seed");
    let mut tools = Toolbox::new(ws.path(), true);
    tools.restrict_tools(vec![READ.to_string(), GREP.to_string()]);

    let mut names = tools.names();
    names.sort();
    assert_eq!(names, [GREP, READ]);
    tools.run(READ, &json!({"path": "a.txt"})).expect("allowed");
    let refused = tools
        .run(WRITE, &json!({"path": "b.txt", "content": "x"}))
        .expect_err("not in the list");
    assert!(
        refused.contains("not one of the tools this agent may use"),
        "{refused}"
    );
    assert!(!ws.path().join("b.txt").exists());

    // Narrowed further, never widened.
    tools.restrict_tools(vec![READ.to_string(), WRITE.to_string()]);
    assert_eq!(tools.names(), [READ]);
}
