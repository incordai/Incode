//! The backend's own state — context, sessions, models — and whole turns run
//! through `run_turn` on stand-in CLIs. [`stand_in_cli`] is shared with tests
//! elsewhere in the crate.

use super::command::rejects_thinking_display;
use super::wire::map_line;
use super::*;

/// A turn outcome, spelled out.
pub(super) fn outcome_of(
    session: &str,
    stop: &str,
    is_error: bool,
    detail: Option<&str>,
    served_by: &[&str],
) -> TurnOutcome {
    TurnOutcome {
        session_id: session.to_string(),
        stop_reason: stop.to_string(),
        is_error,
        detail: detail.map(str::to_string),
        served_by: served_by.iter().map(|m| (*m).to_string()).collect(),
    }
}

pub(super) fn usage_of(input: u32, output: u32, creation: u32, read: u32) -> AgentEvent {
    AgentEvent::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: creation,
        cache_read_tokens: read,
    }
}

/// **The harness's project context must actually REACH the agent.** It was
/// assembled and then dropped when the loop was removed: the build stayed
/// green, the tests passed and the live runs worked, while the guard rules,
/// the auditor's findings and the recorder's handoff silently stopped being
/// sent. Nothing catches that except asserting the argument is built.
#[test]
fn the_session_context_is_carried_to_the_agent() {
    let backend = ClaudeCodeBackend::new(std::path::PathBuf::from("."))
        .with_system_context(vec!["<guards>do not edit tests</guards>".to_string()]);
    assert_eq!(
        backend.system_context(),
        &["<guards>do not edit tests</guards>".to_string()]
    );
}

/// Empty blocks are dropped rather than sent as blank lines — a context
/// section with nothing in it is noise the agent pays for.
#[test]
fn empty_context_blocks_are_not_sent() {
    let backend = ClaudeCodeBackend::new(std::path::PathBuf::from(".")).with_system_context(vec![
        String::new(),
        "   ".to_string(),
        "real".to_string(),
    ]);
    assert_eq!(backend.system_context(), &["real".to_string()]);
}

/// A session with no context sends no flag at all.
#[test]
fn no_context_means_no_flag() {
    let backend = ClaudeCodeBackend::new(std::path::PathBuf::from("."));
    assert!(backend.system_context().is_empty());
}

/// **Deltas are opt-in.** A caller collecting a report must not be handed
/// two copies of the same words, so the flag is off unless asked for.
#[test]
fn partial_messages_are_off_unless_asked_for() {
    let plain = ClaudeCodeBackend::new(std::path::PathBuf::from("."));
    assert!(!plain.partial_messages);
    let streaming = ClaudeCodeBackend::new(std::path::PathBuf::from(".")).streaming(true);
    assert!(streaming.partial_messages);
}

/// Session reuse is the single largest cost lever in this backend: measured
/// against CLI 2.1.229, a fresh session costs ~36k cache-creation tokens per
/// turn versus ~1.8k when resuming — roughly 5.8x. That only holds if the
/// backend actually remembers the session and asks to continue it, so this
/// asserts the state transition that makes `--resume` possible.
#[test]
fn backend_remembers_the_session_so_later_turns_can_resume() {
    let mut backend = ClaudeCodeBackend::new(PathBuf::from("."));
    assert!(
        backend.session_id().is_none(),
        "control: a fresh backend has no session to resume"
    );

    // Drive the state transition the way a real turn does: the init line
    // carries the session id.
    let mut outcome = None;
    for event in map_line(
        r#"{"type":"system","subtype":"init","session_id":"sess-xyz","model":"m"}"#,
        &mut outcome,
    ) {
        if let AgentEvent::SessionStarted { session_id, .. } = event {
            backend.session_id = Some(session_id);
        }
    }

    assert_eq!(
        backend.session_id(),
        Some("sess-xyz"),
        "the backend must retain the session id, or every turn starts fresh \
         and re-pays the whole prompt prefix"
    );
}

/// An explicitly supplied session must be carried too, so a caller can
/// continue a task across process restarts.
#[test]
fn explicit_session_is_carried() {
    let backend = ClaudeCodeBackend::new(PathBuf::from(".")).with_session("prior-session");
    assert_eq!(backend.session_id(), Some("prior-session"));
}

/// **Changing the model abandons the session, and that is deliberate.** A
/// resumed session carries earlier turns; asking a different model to
/// continue them means paying to resume a conversation it never had.
#[test]
fn switching_model_starts_a_fresh_session() {
    let mut backend = ClaudeCodeBackend::new(PathBuf::from("."))
        .with_session("s-existing")
        .with_models(&["cheap".to_string()]);
    assert_eq!(backend.model(), Some("cheap"));
    assert_eq!(
        backend.session_id(),
        None,
        "a session belonging to another model must not be resumed"
    );

    // Re-asking for the SAME model must not throw away a session — that
    // would re-pay the whole prompt prefix on every turn.
    backend.session_id = Some("s-cheap".to_string());
    backend.set_models(&["cheap".to_string()]);
    assert_eq!(backend.session_id(), Some("s-cheap"));

    // And switching again drops it.
    backend.set_models(&["expensive".to_string()]);
    assert_eq!(backend.session_id(), None);
}

/// An empty candidate list is "no opinion", not "clear the model".
#[test]
fn no_candidates_leaves_the_choice_untouched() {
    let mut backend =
        ClaudeCodeBackend::new(PathBuf::from(".")).with_models(&["cheap".to_string()]);
    backend.set_models(&[]);
    assert_eq!(backend.model(), Some("cheap"));
}

/// The rest of the chain becomes the CLI's own fallback list, which is the
/// shape it takes: one comma-separated argument, tried in order.
#[test]
fn the_candidate_chain_becomes_the_fallback_list() {
    let backend = ClaudeCodeBackend::new(PathBuf::from(".")).with_models(&[
        "first".to_string(),
        "second".to_string(),
        "third".to_string(),
    ]);
    assert_eq!(backend.model(), Some("first"));
    assert_eq!(
        backend.fallbacks,
        vec!["second".to_string(), "third".to_string()]
    );
}

/// Foreign vendor model IDs are dropped so mixed-fleet candidate lists from
/// shared tiers do not cause Claude Code CLI to error on unsupported flags.
#[test]
fn foreign_model_id_is_dropped_from_claude_candidates() {
    let mut backend = ClaudeCodeBackend::new(PathBuf::from("."));
    backend.set_models(&[
        "gemini-3.8-flash-high".to_string(),
        "gpt-5.6-terra".to_string(),
    ]);
    assert_eq!(backend.model(), None);

    backend.set_models(&[
        "gemini-3.8-flash-high".to_string(),
        "sonnet".to_string(),
        "gpt-5.6-terra".to_string(),
    ]);
    assert_eq!(backend.model(), Some("sonnet"));
    assert_eq!(backend.fallbacks, Vec::<String>::new());
}

/// Every role in the native Claude Code ladder can be served by Claude Code.
#[test]
fn claude_code_ladder_for_known_roles_serves_every_model() {
    let backend = ClaudeCodeBackend::new(PathBuf::from("."));
    for role in crate::roles::KNOWN_ROLES {
        for id in backend.ladder_for(role) {
            assert!(
                backend.serves(&id),
                "role {role} drew model {id} which claude-code cannot serve"
            );
        }
    }
    assert_ne!(
        backend.ladder_for("explore"),
        backend.ladder_for("diagnose"),
        "reading and reasoning must not draw the same tier"
    );
}

/// A stand-in agent that writes far more to stderr than a pipe holds, then
/// exits without ever printing a result — the shape of a CLI that fails
/// loudly at startup.
fn a_loud_failing_agent(dir: &std::path::Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path = dir.join("loud-agent.cmd");
        std::fs::write(
            &path,
            "@echo off\r\nfor /L %%i in (1,1,3000) do echo stderr line %%i \
             padding padding padding padding padding padding padding 1>&2\r\n\
             echo token=hunter2-not-a-real-secret 1>&2\r\nexit /b 3\r\n",
        )
        .expect("write the fake agent");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("loud-agent.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 3000 ]; do echo \"stderr line $i padding \
             padding padding padding padding padding padding\" >&2; i=$((i+1)); done\n\
             echo 'token=hunter2-not-a-real-secret' >&2\nexit 3\n",
        )
        .expect("write the fake agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it runnable");
        path
    }
}

/// **A LOUD FAILURE DOES NOT HANG THE TURN, AND WHAT IT SAID IS BOUNDED
/// AND CLEANED.**
///
/// stderr was piped and only read after `wait()`. An agent that wrote more
/// than the pipe holds blocked on the write, never exited, and `wait()`
/// never returned: the turn hung with no message. The same text, once
/// read, went unbounded and unredacted into the error, the ledger and the
/// transcript.
#[test]
fn an_agent_that_floods_stderr_fails_promptly_with_a_bounded_clean_reason() {
    let dir = tempfile::tempdir().expect("scratch");
    let fake = a_loud_failing_agent(dir.path());
    let workspace = dir.path().to_path_buf();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut backend = ClaudeCodeBackend::new(workspace).with_binary(fake.to_string_lossy());
        let result = backend.run_turn("hi", &mut |_| {});
        let _ = tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
    });

    let result = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the turn hung: the agent's stderr was never drained");
    let reason = result.expect_err("the fake agent produced no result line");

    assert!(
        reason.contains("stderr line"),
        "what the agent said was lost: {reason}"
    );
    assert!(
        reason.len() <= crate::agent::stderr_tail::MAX_BYTES + 1024,
        "the reason was not bounded: {} bytes",
        reason.len()
    );
    assert!(
        !reason.contains("hunter2-not-a-real-secret"),
        "a secret-shaped value reached the error unredacted"
    );
}

/// What a stand-in CLI runs first, as the real one does when it starts: its
/// tool server reaches the bridge with the turn's token and asks for the
/// tools. Without it a turn is refused, as a real one is when the server
/// never started (`tool_bridge::require_tools`).
///
/// One line of the stand-in's script. On Windows it calls a PowerShell file
/// written beside it (quoting JSON through `cmd` is not worth it); on Unix,
/// bash's `/dev/tcp`, so those stand-ins start with `#!/bin/bash`.
pub(crate) fn lists_the_tools(dir: &std::path::Path) -> String {
    #[cfg(windows)]
    {
        let script = dir.join("list-tools.ps1");
        std::fs::write(
            &script,
            "$a = $env:INCODE_TOOL_BRIDGE.Split(':')\r\n\
             $c = New-Object Net.Sockets.TcpClient($a[0], [int]$a[1])\r\n\
             $s = $c.GetStream()\r\n\
             $w = New-Object IO.StreamWriter($s)\r\n\
             $w.WriteLine('{\"op\":\"list\",\"token\":\"' + $env:INCODE_TOOL_TOKEN + '\"}')\r\n\
             $w.Flush()\r\n\
             [void](New-Object IO.StreamReader($s)).ReadLine()\r\n\
             $c.Close()\r\n",
        )
        .expect("write the stand-in's tool server");
        format!(
            "\"%SystemRoot%\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\" -NoProfile \
             -ExecutionPolicy Bypass -File \"{}\" >nul\r\n",
            script.display()
        )
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        "exec 3<>\"/dev/tcp/${INCODE_TOOL_BRIDGE%:*}/${INCODE_TOOL_BRIDGE##*:}\"\n\
         printf '{\"op\":\"list\",\"token\":\"%s\"}\\n' \"$INCODE_TOOL_TOKEN\" >&3\n\
         read -r _ <&3\nexec 3>&-\n"
            .to_string()
    }
}

/// A stand-in CLI whose option parser does not know the thinking flag:
/// named on its argv, it says so on stderr and exits 1 before any
/// output; otherwise it prints a short turn. Every start is counted in
/// `starts.txt` beside it.
fn a_cli_without_the_thinking_flag(dir: &std::path::Path) -> PathBuf {
    const LINES: [&str; 3] = [
        r#"{"type":"system","subtype":"init","session_id":"no-think-1","model":"m"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"still here"}]},"parent_tool_use_id":null}"#,
        r#"{"type":"result","is_error":false,"stop_reason":"end_turn","session_id":"no-think-1","usage":{"input_tokens":1,"output_tokens":1}}"#,
    ];
    #[cfg(windows)]
    {
        let mut body = String::from(
            "@echo off\r\necho x>>\"%~dp0starts.txt\"\r\n\
             echo %* | findstr /C:\"--thinking-display\" >nul\r\n\
             if not errorlevel 1 (\r\n\
             echo error: unknown option '--thinking-display' 1>&2\r\n\
             exit /b 1\r\n)\r\n",
        );
        body.push_str(&lists_the_tools(dir));
        for line in LINES {
            body.push_str(&format!("echo {line}\r\n"));
        }
        let path = dir.join("old-agent.cmd");
        std::fs::write(&path, body).expect("write the stand-in agent");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut body = String::from(
            "#!/bin/bash\necho x >> \"$(dirname \"$0\")/starts.txt\"\n\
             case \"$*\" in *--thinking-display*) \
             echo \"error: unknown option '--thinking-display'\" >&2; exit 1;; esac\n",
        );
        body.push_str(&lists_the_tools(dir));
        for line in LINES {
            body.push_str(&format!("echo '{line}'\n"));
        }
        let path = dir.join("old-agent.sh");
        std::fs::write(&path, body).expect("write the stand-in agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it runnable");
        path
    }
}

/// **A CLI THAT DROPS THE UNDOCUMENTED THINKING FLAG DOES NOT BREAK EVERY
/// TURN.** The first refusal is recognised, the turn runs again without
/// the flag, and the backend remembers: the next turn starts once.
#[test]
fn a_refused_thinking_flag_is_dropped_once_and_remembered() {
    let dir = tempfile::tempdir().expect("scratch");
    let fake = a_cli_without_the_thinking_flag(dir.path());
    let starts = dir.path().join("starts.txt");
    let count = || {
        std::fs::read_to_string(&starts)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    let mut backend = ClaudeCodeBackend::new(dir.path().to_path_buf())
        .with_binary(fake.to_string_lossy())
        .streaming(true);

    let mut said = Vec::new();
    let first = backend
        .run_turn("hi", &mut |e| {
            if let AgentEvent::Text(t) = e {
                said.push(t.clone());
            }
        })
        .expect("the turn is run again without the flag");
    assert!(!first.is_error, "{first:?}");
    assert_eq!(said, ["still here"], "{said:?}");
    assert_eq!(count(), 2, "one refused start, one without the flag");

    backend.run_turn("again", &mut |_| {}).expect("second turn");
    assert_eq!(count(), 3, "the refusal is remembered: one start this turn");
}

/// Only a refusal that names the flag counts; any other failure is left
/// as it was.
#[test]
fn only_a_refusal_naming_the_thinking_flag_is_recognised() {
    assert!(rejects_thinking_display(
        "agent exited with 1 before producing a result: error: unknown option \
         '--thinking-display'"
    ));
    assert!(!rejects_thinking_display(
        "error: unknown option '--fallback-model'"
    ));
    assert!(!rejects_thinking_display(
        "the --thinking-display summary was empty"
    ));
}

/// A stand-in for the agent CLI, for tests elsewhere in the crate: it prints
/// a short stream-json turn — a summarized thinking delta, the reply
/// `three files`, the result — and ignores its argv and stdin. With `waits`
/// it pauses half a minute after the session starts, so a test can stop it.
pub(crate) fn stand_in_cli(dir: &std::path::Path, waits: bool) -> PathBuf {
    const LINES: [&str; 5] = [
        r#"{"type":"system","subtype":"init","session_id":"sub-conv-1","model":"fake-sub-model"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"looking around"}},"parent_tool_use_id":null}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"three files"}},"parent_tool_use_id":null}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"three files"}]},"parent_tool_use_id":null}"#,
        r#"{"type":"result","is_error":false,"stop_reason":"end_turn","session_id":"sub-conv-1","usage":{"input_tokens":1,"output_tokens":1}}"#,
    ];
    #[cfg(windows)]
    {
        let mut body = format!("@echo off\r\n{}echo {}\r\n", lists_the_tools(dir), LINES[0]);
        if waits {
            body.push_str("ping -n 30 127.0.0.1 >nul\r\n");
        }
        for line in &LINES[1..] {
            body.push_str(&format!("echo {line}\r\n"));
        }
        let path = dir.join(if waits { "slow-agent.cmd" } else { "agent.cmd" });
        std::fs::write(&path, body).expect("write the stand-in agent");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut body = format!("#!/bin/bash\n{}echo '{}'\n", lists_the_tools(dir), LINES[0]);
        if waits {
            body.push_str("sleep 30\n");
        }
        for line in &LINES[1..] {
            body.push_str(&format!("echo '{line}'\n"));
        }
        let path = dir.join(if waits { "slow-agent.sh" } else { "agent.sh" });
        std::fs::write(&path, body).expect("write the stand-in agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it runnable");
        path
    }
}

// ---------------------------------------------------------------------------
// Pinned behaviour: what a whole turn through `run_turn` reports.
// ---------------------------------------------------------------------------

/// **A WHOLE TURN THROUGH `run_turn`**: the events in order, the outcome,
/// and the session kept for the next turn.
#[test]
fn a_whole_turn_reports_these_events_and_this_outcome() {
    let dir = tempfile::tempdir().expect("scratch");
    let fake = stand_in_cli(dir.path(), false);
    let mut backend =
        ClaudeCodeBackend::new(dir.path().to_path_buf()).with_binary(fake.to_string_lossy());
    let mut seen = Vec::new();
    let outcome = backend
        .run_turn("hi", &mut |e| seen.push(e.clone()))
        .expect("the stand-in finishes its turn");
    assert_eq!(
        seen,
        vec![
            AgentEvent::SessionStarted {
                session_id: "sub-conv-1".to_string(),
                model: "fake-sub-model".to_string(),
            },
            AgentEvent::Thinking("looking around".to_string()),
            AgentEvent::TextDelta("three files".to_string()),
            AgentEvent::Text("three files".to_string()),
            usage_of(1, 1, 0, 0),
        ]
    );
    assert_eq!(
        outcome,
        outcome_of("sub-conv-1", "end_turn", false, None, &[])
    );
    assert_eq!(backend.session_id(), Some("sub-conv-1"));
}

/// A stand-in that prints a whole, successful-looking turn but never starts
/// incode's tool server: the shape measured under sbx on Windows, where the
/// CLI could not start any child with piped stdio (#68).
fn a_cli_whose_tool_server_never_starts(dir: &std::path::Path) -> PathBuf {
    const LINES: [&str; 3] = [
        r#"{"type":"system","subtype":"init","session_id":"no-tools-1","model":"m"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I read the file: all good"}]},"parent_tool_use_id":null}"#,
        r#"{"type":"result","is_error":false,"stop_reason":"end_turn","session_id":"no-tools-1","usage":{"input_tokens":1,"output_tokens":1}}"#,
    ];
    #[cfg(windows)]
    {
        let mut body = String::from("@echo off\r\n");
        for line in LINES {
            body.push_str(&format!("echo {line}\r\n"));
        }
        let path = dir.join("toolless-agent.cmd");
        std::fs::write(&path, body).expect("write the stand-in agent");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut body = String::from("#!/bin/sh\n");
        for line in LINES {
            body.push_str(&format!("echo '{line}'\n"));
        }
        let path = dir.join("toolless-agent.sh");
        std::fs::write(&path, body).expect("write the stand-in agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it runnable");
        path
    }
}

/// **A TURN THAT NEVER REACHED incode's TOOLS IS NOT A RESULT.** The model
/// had nothing to act with, so a reply like "I read the file" is made up;
/// it was reported as `is_error: false`. Now the turn fails and says why.
#[test]
fn a_turn_whose_tool_server_never_started_fails_and_says_so() {
    let dir = tempfile::tempdir().expect("scratch");
    let fake = a_cli_whose_tool_server_never_starts(dir.path());
    let mut backend =
        ClaudeCodeBackend::new(dir.path().to_path_buf()).with_binary(fake.to_string_lossy());
    let err = backend
        .run_turn("read hello.txt", &mut |_| {})
        .expect_err("a turn with none of incode's tools is not a finished one");
    assert!(
        err.to_string()
            .starts_with("Claude Code never started incode's tool server"),
        "{err}"
    );
}

/// A stand-in whose `init` line says incode's server failed, as Claude Code
/// says it under sbx on Windows (#68), and which then goes on, as the
/// toolless model did until the wall clock: about five seconds later it
/// writes `still-running.txt` beside itself, then a result.
fn a_cli_reporting_the_tool_server_failed(dir: &std::path::Path) -> PathBuf {
    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"failed-1","model":"m","mcp_servers":[{"name":"incode","status":"failed","source":"dynamic"}]}"#;
    const RESULT: &str = r#"{"type":"result","is_error":false,"stop_reason":"end_turn","session_id":"failed-1","usage":{"input_tokens":1,"output_tokens":1}}"#;
    #[cfg(windows)]
    {
        let path = dir.join("failed-server-agent.cmd");
        std::fs::write(
            &path,
            format!(
                "@echo off\r\necho {INIT}\r\nping -n 6 127.0.0.1 >nul\r\n\
                 echo x>\"%~dp0still-running.txt\"\r\necho {RESULT}\r\n"
            ),
        )
        .expect("write the stand-in agent");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("failed-server-agent.sh");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho '{INIT}'\nsleep 5\n\
                 touch \"$(dirname \"$0\")/still-running.txt\"\necho '{RESULT}'\n"
            ),
        )
        .expect("write the stand-in agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it runnable");
        path
    }
}

/// **WHEN THE CLI SAYS AT ONCE THAT incode's SERVER FAILED, THE TURN STOPS
/// AT ONCE**, with the reason, and the CLI is not left running. Before, the
/// toolless model ran on until the wall clock (281 s measured).
#[test]
fn a_reported_server_failure_stops_the_turn_at_once() {
    let dir = tempfile::tempdir().expect("scratch");
    let fake = a_cli_reporting_the_tool_server_failed(dir.path());
    let mut backend =
        ClaudeCodeBackend::new(dir.path().to_path_buf()).with_binary(fake.to_string_lossy());
    let started = std::time::Instant::now();
    let err = backend
        .run_turn("read hello.txt", &mut |_| {})
        .expect_err("a turn whose tool server failed is not a finished one");
    assert!(
        err.to_string()
            .starts_with("Claude Code never started incode's tool server"),
        "{err}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "stopped only when the stand-in finished: {:?}",
        started.elapsed()
    );
    // Past the moment it would have written its marker: a CLI left running
    // would have. This catches a missing kill on Linux and macOS, where
    // nothing else ends the process group; on Windows the turn's job object
    // ends the CLI on drop as well (`agent/process_tree.rs`), so there the
    // marker stays absent either way.
    std::thread::sleep(std::time::Duration::from_secs(9));
    assert!(
        !dir.path().join("still-running.txt").exists(),
        "the CLI was left running after its turn was stopped"
    );
}

/// A CLI that cannot be started names itself in the error.
#[test]
fn a_cli_that_cannot_start_is_named_in_the_error() {
    let dir = tempfile::tempdir().expect("scratch");
    let missing = dir.path().join("no-such-agent-binary");
    let mut backend =
        ClaudeCodeBackend::new(dir.path().to_path_buf()).with_binary(missing.to_string_lossy());
    let err = backend
        .run_turn("hi", &mut |_| {})
        .expect_err("nothing to start");
    assert!(
        err.to_string()
            .starts_with(&format!("failed to spawn `{}`: ", missing.display())),
        "{err}"
    );
}

/// A stand-in that starts a session and exits cleanly, saying nothing
/// else and writing nothing to stderr.
fn a_cli_that_ends_without_a_result(dir: &std::path::Path) -> PathBuf {
    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"cut-1","model":"m"}"#;
    #[cfg(windows)]
    {
        let path = dir.join("cut-agent.cmd");
        std::fs::write(&path, format!("@echo off\r\necho {INIT}\r\nexit /b 0\r\n"))
            .expect("write the stand-in agent");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("cut-agent.sh");
        std::fs::write(&path, format!("#!/bin/sh\necho '{INIT}'\nexit 0\n"))
            .expect("write the stand-in agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it runnable");
        path
    }
}

/// No result line: the exact error, and the session still kept.
#[test]
fn a_turn_without_a_result_fails_with_this_error_and_keeps_its_session() {
    let dir = tempfile::tempdir().expect("scratch");
    let fake = a_cli_that_ends_without_a_result(dir.path());
    let mut backend =
        ClaudeCodeBackend::new(dir.path().to_path_buf()).with_binary(fake.to_string_lossy());
    let err = backend
        .run_turn("hi", &mut |_| {})
        .expect_err("no result line");
    assert_eq!(
        err.to_string(),
        "agent exited with 0 before producing a result: "
    );
    assert_eq!(backend.session_id(), Some("cut-1"));
}
