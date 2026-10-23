//! The agy backend: its wire envelope, its own model ladder, the enforcement
//! it does not have, the exact command a turn is started with, and whole
//! turns through `run_turn` on a stand-in CLI.

use super::*;

/// Issue #3: launched from inside another agent session, a child that
/// inherits its variables reports that session as its own.
#[test]
fn the_cli_never_joins_the_session_that_launched_incode() {
    let cmd = AgyBackend::new(PathBuf::from(".")).command("p");
    assert!(
        cmd.get_envs()
            .any(|(k, v)| k == "CLAUDECODE" && v.is_none()),
        "CLAUDECODE is inherited, so the child joins the parent's session"
    );
}

/// **AN AGENT THAT CANNOT TAKE A PICTURE HEADLESS SAYS SO, BY NAME.**
/// `agy --help` (1.2.11) has no image flag, and its stream-json input
/// shape is not measured, so pictures are refused rather than dropped.
#[test]
fn pictures_are_refused_by_name() {
    let picture = crate::toolbox::Image::from_base64(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
    )
    .expect("a 1x1 png");
    let mut backend = AgyBackend::new(PathBuf::from("."));
    let why = backend
        .attach_images(vec![picture])
        .expect_err("agy takes no pictures");
    assert!(why.starts_with("agy cannot take pictures"), "{why}");
}

fn parse(line: &str) -> Wire {
    serde_json::from_str(line).expect("a wire line")
}

/// **A REPLY STREAMED IN FRAGMENTS IS SAID ONCE, WHOLE.** Lines shaped as
/// a real long reply (measured on agy, Insi's lesson writer): fragments of
/// one step, `ACTIVE` then `DONE`, each a few words and split inside words
/// and inside a JSON key. The step is one `Text`, the fragments joined byte
/// for byte, so its JSON parses; every fragment is still shown live as a
/// `TextDelta`; a later step is a `Text` of its own.
#[test]
fn a_reply_streamed_in_fragments_is_said_once_whole() {
    let line = |index: u64, state: &str, kind: &str, text: &str| {
        format!(
            r#"{{"event":"step_update","step_update":{{"step_index":{index},"state":"{state}","step_type":"{kind}","text_delta":{}}}}}"#,
            serde_json::to_string(text).expect("json")
        )
    };
    let stream = [
        line(0, "DONE", "user_input", ""),
        line(2, "ACTIVE", "agent_response", "{\n  \"issue"),
        line(2, "ACTIVE", "agent_response", "\": \"The agent added anim"),
        line(
            2,
            "ACTIVE",
            "agent_response",
            "ation math\",\n  \"use\": \"Wire it",
        ),
        line(2, "DONE", "agent_response", " end to end.\"\n}"),
        line(4, "DONE", "agent_response", "Done."),
    ];
    let mut said = Vec::new();
    let mut deltas = 0;
    {
        let mut on_event = |event: &AgentEvent| match event {
            AgentEvent::Text(text) => said.push(text.clone()),
            AgentEvent::TextDelta(_) => deltas += 1,
            _ => {}
        };
        let mut reply = StepText::default();
        for wire in stream.iter().map(|l| parse(l)) {
            if let Wire::StepUpdate { step_update } = wire {
                reply.feed(&step_update, &mut on_event);
            }
        }
        reply.finish(&mut on_event);
    }
    assert_eq!(said.len(), 2, "{said:?}");
    let answer: serde_json::Value = serde_json::from_str(&said[0]).expect("the whole reply parses");
    assert_eq!(answer["issue"], "The agent added animation math");
    assert_eq!(answer["use"], "Wire it end to end.");
    assert_eq!(said[1], "Done.");
    assert_eq!(deltas, 5, "every fragment shown as it came");
}

/// The envelope is keyed on `event`, not `type`.
///
/// Copied from a real v1.1.23 run. If this ever parses as `Other` the CLI
/// changed its envelope, and every other test here would still pass while
/// the backend silently read nothing.
#[test]
fn the_init_line_from_a_real_run_is_understood() {
    let wire = parse(
        r#"{"event":"init","conversation_id":"1b343cea","init":{"cwd":"C:\\tmp","tools":["view_file","invoke_subagent"],"permission_mode":"request-review"}}"#,
    );
    match wire {
        Wire::Init(init) => {
            assert_eq!(init.conversation_id, "1b343cea");
            assert!(init.init.tools.iter().any(|t| t == "invoke_subagent"));
        }
        other => panic!("the init envelope was not recognised: {other:?}"),
    }
}

/// Assistant text arrives on a step update, not a content block.
#[test]
fn the_agents_reply_is_read_from_a_step_update() {
    let wire = parse(
        r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"PONG\n","usage":{"input_tokens":16317,"output_tokens":183,"cache_read_tokens":0}}}"#,
    );
    match wire {
        Wire::StepUpdate { step_update } => {
            assert_eq!(step_update.step_type, "agent_response");
            assert_eq!(step_update.text_delta, "PONG\n");
            let u = to_usage(&step_update.usage.expect("usage"));
            assert_eq!(u.input_tokens, 16317);
            assert_eq!(u.output_tokens, 183);
        }
        other => panic!("a step update was not recognised: {other:?}"),
    }
}

/// **Every `status: ERROR` moves the work, whatever the wording.**
///
/// The three the owner named — a lapsed subscription, a model the account
/// cannot reach, a model that is busy — arrive with three unrelated
/// messages and one identical status. Only the middle one is in the phrase
/// list; the first and third are asserted below to MISS it, which is what
/// makes reading the status the fix rather than a longer list.
#[test]
fn any_error_status_moves_the_work_however_it_is_worded() {
    let backend = AgyBackend::new(std::path::PathBuf::from("."));
    let failed = |detail: &str| crate::agent::TurnOutcome {
        session_id: "c1".to_string(),
        stop_reason: "ERROR".to_string(),
        is_error: true,
        detail: Some(detail.to_string()),
        served_by: Vec::new(),
    };

    // VERBATIM from the live run.
    let busy = "Our servers are experiencing high traffic right now, please try again in a minute.";
    let lapsed = "Your plan does not include this model.";
    let missing = "model sonnet is not recognized";

    assert!(
        !crate::agent::moves_the_work(busy) && !crate::agent::moves_the_work(lapsed),
        "the control holds only while the phrase list really does miss these"
    );
    for detail in [busy, lapsed, missing] {
        assert!(
            backend.agent_failed(&failed(detail)),
            "an ERROR status is the agent's own failure: {detail}"
        );
    }

    // A turn the agent COMPLETED is never moved, however it reads — that is
    // the harness's own check to judge, and walking a wrong task through
    // every subscription spends them all to learn one thing.
    let finished = crate::agent::TurnOutcome {
        session_id: "c1".to_string(),
        stop_reason: "SUCCESS".to_string(),
        is_error: false,
        detail: None,
        served_by: Vec::new(),
    };
    assert!(!backend.agent_failed(&finished));
}

/// A non-SUCCESS status is a failed turn, and says which status it was.
#[test]
fn a_failed_turn_is_reported_with_the_status_that_caused_it() {
    let ok = parse(r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS"}}"#);
    let bad = parse(r#"{"event":"result","result":{"conversation_id":"c1","status":"CANCELLED"}}"#);
    let status_of = |w: Wire| match w {
        Wire::Result { result } => (
            result.status.clone(),
            !result.status.eq_ignore_ascii_case("SUCCESS"),
        ),
        other => panic!("a result line was not recognised: {other:?}"),
    };
    assert_eq!(status_of(ok), ("SUCCESS".to_string(), false));
    assert_eq!(status_of(bad), ("CANCELLED".to_string(), true));
}

/// An unknown event is ignored, never fatal — the CLI may add events.
#[test]
fn an_unrecognised_event_does_not_break_the_turn() {
    assert!(matches!(
        parse(r#"{"event":"something_new_in_a_later_release"}"#),
        Wire::Other
    ));
}

/// **This backend enforces nothing, and says so.**
///
/// The measured reason: no per-tool deny flag exists on this CLI, and its
/// own init line advertises four sub-agent tools. Claiming enforcement it
/// does not have would be the exact failure the guard docs warn about.
#[test]
fn this_backend_claims_no_enforcement_it_does_not_have() {
    let backend = AgyBackend::new(PathBuf::from("."));
    assert_eq!(
        backend.enforces(),
        crate::guards::Enforcement::NOTHING,
        "claiming enforcement this CLI has no flag for would report a guard \
         that is not there"
    );
}

/// The harness's policy becomes a REPORTED GAP here, not a silent drop.
#[test]
fn the_whole_policy_is_reported_as_unenforced_on_this_agent() {
    let policy = crate::guards::restrictions(&crate::guards::Guards::default());
    let gaps = policy.unenforced_by(&AgyBackend::new(PathBuf::from(".")).enforces());
    assert_eq!(gaps.len(), 2, "both halves must be reported: {gaps:?}");
    assert!(gaps.iter().any(|g| g.contains("protected path")));
    assert!(gaps.iter().any(|g| g.contains("sub-agents")));
}

/// Only the first SERVEABLE candidate survives: no fallback chain here.
#[test]
fn a_fallback_chain_collapses_to_the_first_choice() {
    let mut backend = AgyBackend::new(PathBuf::from("."));
    backend.set_models(&[
        "gemini-3.8-flash-high".to_string(),
        "gemini-3.1-pro-high".to_string(),
    ]);
    assert_eq!(backend.model(), Some("gemini-3.8-flash-high"));
}

/// **Another vendor's model id is NOT forwarded.**
///
/// The live failure: the role ladder holds `sonnet`/`opus`, this agent
/// answered `status: ERROR, error: model sonnet is not recognized`, and
/// every task in the run died before any work began. Leaving the agent's
/// own default standing loses routing; forwarding the name loses the run.
#[test]
fn a_foreign_model_id_is_dropped_rather_than_sent() {
    let mut backend = AgyBackend::new(PathBuf::from("."));
    backend.set_models(&["sonnet".to_string(), "opus".to_string()]);
    assert_eq!(
        backend.model(),
        None,
        "a model this agent cannot serve must not reach its command line"
    );

    // Control: a ladder it CAN serve is still honoured, so the assertion
    // above is not passing because nothing is ever accepted.
    backend.set_models(&["sonnet".to_string(), "gemini-3.1-pro-high".to_string()]);
    assert_eq!(backend.model(), Some("gemini-3.1-pro-high"));
}

/// **This agent routes on its OWN ladder, not the operator's foreign one.**
///
/// Before this, a ladder of `sonnet, opus` left the agent un-routed and
/// every role ran on its default — the expensive one. The ids below come
/// from `agy models`, and a cheap role must not draw the strong model.
#[test]
fn each_role_draws_a_model_this_agent_actually_has() {
    let backend = AgyBackend::new(PathBuf::from("."));
    for role in crate::roles::KNOWN_ROLES {
        for id in backend.ladder_for(role) {
            assert!(
                backend.serves(&id),
                "role {role} was given `{id}`, which this agent cannot serve"
            );
        }
    }
    assert_ne!(
        backend.ladder_for("explore"),
        backend.ladder_for("implement"),
        "reading and writing must not draw the same tier, or routing saves nothing"
    );
    assert!(
        backend.ladder_for("no-such-role").is_empty(),
        "an unknown role gets the agent's default, never a guess"
    );
}

/// A failed turn carries the agent's OWN reason, not a shrug.
#[test]
fn a_failure_reports_the_reason_the_agent_gave() {
    let line = r#"{"event":"result","result":{"conversation_id":"c1","status":"ERROR","error":"model sonnet is not recognized"}}"#;
    match parse(line) {
        Wire::Result { result } => {
            let detail = result.error.clone().filter(|e| !e.trim().is_empty());
            assert_eq!(
                detail.as_deref(),
                Some("model sonnet is not recognized"),
                "the reason the agent gave was discarded"
            );
        }
        other => panic!("not a result: {other:?}"),
    }
}

/// **The harness's context reaches this agent even with no flag for it.**
///
/// The pinned failure on the other backend was a run where every task was
/// worked by an agent told none of the guards, findings or roles. This CLI
/// has no `--append-system-prompt`, so the same loss would happen silently
/// unless the context is carried in the prompt instead.
#[test]
fn the_harness_context_is_carried_in_the_prompt_when_no_flag_exists() {
    let bare = AgyBackend::new(PathBuf::from("."));
    assert_eq!(
        bare.full_prompt("do the work").expect("sent"),
        "do the work"
    );

    let briefed = AgyBackend::new(PathBuf::from("."))
        .with_system_context(vec!["the guards in force: tests/ is protected".to_string()]);
    let sent = briefed.full_prompt("do the work").expect("sent");
    assert!(
        sent.contains("tests/ is protected"),
        "the harness context never reached the agent: {sent}"
    );
    assert!(
        sent.ends_with("do the work"),
        "the task must come last, after its context: {sent}"
    );
}

/// An unknown permission mode leaves the agent's default standing.
#[test]
fn an_unknown_permission_mode_is_not_passed_through() {
    let mut backend = AgyBackend::new(PathBuf::from("."));
    backend.set_permission_mode("danger-full-access");
    assert!(
        backend.mode.is_none(),
        "passing a word this CLI rejects would fail the turn over a setting"
    );
    backend.set_permission_mode("plan");
    assert_eq!(backend.mode.as_deref(), Some("plan"));
}

// ---------------------------------------------------------------------------
// Pinned behaviour: the exact command a turn is started with, and what a
// whole turn through `run_turn` reports.
// ---------------------------------------------------------------------------

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// What the command SETS in the environment. Removals are the parent
/// session's variables, guarded by the test above.
fn set_envs(cmd: &Command) -> Vec<(String, String)> {
    cmd.get_envs()
        .filter_map(|(k, v)| {
            Some((
                k.to_string_lossy().into_owned(),
                v?.to_string_lossy().into_owned(),
            ))
        })
        .collect()
}

/// **THE EXACT COMMAND FOR A DEFAULT TURN**: accept-edits, the long print
/// timeout, the workspace as the working directory.
#[test]
fn a_default_turn_is_started_with_exactly_this_command() {
    let cmd = AgyBackend::new(PathBuf::from("ws")).command("p");
    assert_eq!(cmd.get_program(), "agy");
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
    assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("ws")));
    assert!(set_envs(&cmd).is_empty(), "{:?}", set_envs(&cmd));
}

/// **THE EXACT COMMAND WITH EVERYTHING SET**: a model, plan mode, two
/// extra roots and a conversation to resume.
#[test]
fn a_configured_turn_is_started_with_exactly_this_command() {
    let mut backend = AgyBackend::new(PathBuf::from(".")).with_binary("/opt/agy");
    backend.set_models(&strings(&["sonnet", "gemini-3.1-pro-high"]));
    backend.set_permission_mode("read-only");
    backend.set_extra_dirs(vec![PathBuf::from("a"), PathBuf::from("b")]);
    backend.set_session_id("c-1");
    let cmd = backend.command("p");
    assert_eq!(cmd.get_program(), "/opt/agy");
    assert_eq!(
        args_of(&cmd),
        strings(&[
            "--print",
            "p",
            "--output-format",
            "stream-json",
            "--print-timeout",
            "30m",
            "--model",
            "gemini-3.1-pro-high",
            "--mode",
            "plan",
            "--add-dir",
            "a",
            "--add-dir",
            "b",
            "--conversation",
            "c-1",
        ])
    );

    backend.set_permission_mode("danger-full-access");
    backend.clear_session();
    assert!(
        !args_of(&backend.command("p"))
            .iter()
            .any(|a| a == "--mode" || a == "--conversation"),
        "an unknown mode and a cleared session leave no flag"
    );
}

/// A stand-in agy: prints `lines` on stdout, `stderr` on stderr, and
/// exits with `code`.
fn stand_in_agy(dir: &std::path::Path, lines: &[&str], stderr: &str, code: i32) -> PathBuf {
    #[cfg(windows)]
    {
        let mut body = String::from("@echo off\r\n");
        for line in lines {
            body.push_str(&format!("echo {line}\r\n"));
        }
        if !stderr.is_empty() {
            body.push_str(&format!("echo {stderr} 1>&2\r\n"));
        }
        body.push_str(&format!("exit /b {code}\r\n"));
        let path = dir.join("agy.cmd");
        std::fs::write(&path, body).expect("write the stand-in agy");
        path
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut body = String::from("#!/bin/sh\n");
        for line in lines {
            body.push_str(&format!("echo '{line}'\n"));
        }
        if !stderr.is_empty() {
            body.push_str(&format!("echo '{stderr}' >&2\n"));
        }
        body.push_str(&format!("exit {code}\n"));
        let path = dir.join("agy.sh");
        std::fs::write(&path, body).expect("write the stand-in agy");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it runnable");
        path
    }
}

/// One turn through `run_turn` on a stand-in, with its events.
fn turn_on(
    lines: &[&str],
    stderr: &str,
    code: i32,
    set_up: impl FnOnce(&mut AgyBackend),
) -> (Result<TurnOutcome, String>, Vec<AgentEvent>, AgyBackend) {
    let dir = tempfile::tempdir().expect("scratch");
    let fake = stand_in_agy(dir.path(), lines, stderr, code);
    let mut backend = AgyBackend::new(dir.path().to_path_buf()).with_binary(fake.to_string_lossy());
    set_up(&mut backend);
    let mut seen = Vec::new();
    let result = backend
        .run_turn("hi", &mut |e| seen.push(e.clone()))
        .map_err(|e| e.to_string());
    (result, seen, backend)
}

const INIT: &str = r#"{"event":"init","conversation_id":"conv-1","init":{"tools":["view_file","invoke_subagent"]}}"#;

/// **A WHOLE TURN**: the session, each fragment live, the step's text
/// once, the last usage reported, a garbage line skipped, and the
/// conversation the result names kept for the next turn.
#[test]
fn a_whole_turn_reports_these_events_and_this_outcome() {
    let (result, seen, backend) = turn_on(
        &[
            INIT,
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"agent_response","text_delta":"PO"}}"#,
            "not json",
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"agent_response","text_delta":"NG","usage":{"input_tokens":7,"output_tokens":3,"cache_read_tokens":2}}}"#,
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool_call","text_delta":"ignored"}}"#,
            r#"{"event":"something_new"}"#,
            r#"{"event":"result","result":{"conversation_id":"conv-2","status":"SUCCESS","usage":{"input_tokens":9,"output_tokens":4,"cache_read_tokens":5}}}"#,
        ],
        "",
        0,
        |b| b.set_models(&strings(&["gemini-3.8-flash-high"])),
    );
    assert_eq!(
        seen,
        vec![
            AgentEvent::SessionStarted {
                session_id: "conv-1".to_string(),
                model: "gemini-3.8-flash-high".to_string(),
            },
            AgentEvent::TextDelta("PO".to_string()),
            AgentEvent::TextDelta("NG".to_string()),
            AgentEvent::Text("PONG".to_string()),
            AgentEvent::Usage {
                input_tokens: 9,
                output_tokens: 4,
                cache_creation_tokens: 0,
                cache_read_tokens: 5,
            },
        ]
    );
    assert_eq!(
        result.expect("a result line"),
        TurnOutcome {
            session_id: "conv-2".to_string(),
            stop_reason: "SUCCESS".to_string(),
            is_error: false,
            detail: None,
            served_by: strings(&["gemini-3.8-flash-high"]),
        }
    );
    assert_eq!(backend.session_id(), Some("conv-2"));
}

/// A step's usage stands when the result carries none; an `ERROR` status
/// is a failed outcome carrying the agent's reason.
#[test]
fn an_error_result_is_an_outcome_with_the_agents_reason() {
    let (result, seen, _) = turn_on(
        &[
            INIT,
            r#"{"event":"step_update","step_update":{"step_index":0,"step_type":"agent_response","text_delta":"","usage":{"input_tokens":2,"output_tokens":1,"cache_read_tokens":0}}}"#,
            r#"{"event":"result","result":{"conversation_id":"conv-1","status":"ERROR","error":"model x is not recognized"}}"#,
        ],
        "",
        1,
        |_| {},
    );
    assert_eq!(
        seen,
        vec![
            AgentEvent::SessionStarted {
                session_id: "conv-1".to_string(),
                model: String::new(),
            },
            AgentEvent::Usage {
                input_tokens: 2,
                output_tokens: 1,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        ]
    );
    assert_eq!(
        result.expect("a result line"),
        TurnOutcome {
            session_id: "conv-1".to_string(),
            stop_reason: "ERROR".to_string(),
            is_error: true,
            detail: Some("model x is not recognized".to_string()),
            served_by: Vec::new(),
        }
    );
}

/// No result line: the exact error, with and without stderr, after the
/// usage is still reported.
#[test]
fn a_turn_without_a_result_fails_with_this_error() {
    let (result, seen, backend) = turn_on(&[INIT], "boom", 3, |_| {});
    assert_eq!(
        result.expect_err("no result line"),
        "the agent produced no result line (exit 3): boom"
    );
    assert_eq!(
        seen.last(),
        Some(&AgentEvent::Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
        })
    );
    assert_eq!(backend.session_id(), Some("conv-1"));

    let (quiet, _, _) = turn_on(&[INIT], "", 0, |_| {});
    assert_eq!(
        quiet.expect_err("no result line"),
        "the agent produced no result line (exit 0)"
    );
}

/// A stopped turn says so, before handling any output.
#[test]
fn a_cancelled_turn_stops_with_this_error() {
    let (result, seen, _) = turn_on(&[INIT], "", 0, |b| {
        let token = crate::runtime::cancel::CancelToken::new();
        token.cancel();
        b.set_cancel(token);
    });
    assert_eq!(result.expect_err("cancelled"), "the turn was cancelled");
    assert!(seen.is_empty(), "{seen:?}");
}

/// A CLI that cannot be started names itself in the error.
#[test]
fn a_cli_that_cannot_start_is_named_in_the_error() {
    let dir = tempfile::tempdir().expect("scratch");
    let missing = dir.path().join("no-such-agy-binary");
    let mut backend =
        AgyBackend::new(dir.path().to_path_buf()).with_binary(missing.to_string_lossy());
    let err = backend
        .run_turn("hi", &mut |_| {})
        .expect_err("nothing to start");
    assert!(
        err.to_string()
            .starts_with(&format!("could not start `{}`: ", missing.display())),
        "{err}"
    );
}
