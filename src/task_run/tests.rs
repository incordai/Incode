//! Running one instruction: the operator's hooks, the prompt, the token
//! line, and the JSON output. The fallback between candidate agents is
//! tested beside it, in `drive.rs`.

use super::*;

/// **A REFUSED LINE STILL ENDS THE SESSION IT STARTED.** The operator's
/// `user_prompt_submit` hook refuses the instruction: the run stops with
/// the reason, and `session_end` runs after `session_start`, in the
/// workspace, so no start is left without its end.
#[test]
fn a_refused_instruction_still_runs_the_session_end_hook() {
    let ws = tempfile::tempdir().expect("ws");
    let (refuse, mark) = if cfg!(windows) {
        (
            "echo no secrets 1>&2& exit /b 2",
            ["echo started> started.txt", "echo ended> ended.txt"],
        )
    } else {
        (
            "echo 'no secrets' >&2; exit 2",
            ["echo started > started.txt", "echo ended > ended.txt"],
        )
    };
    let hooks = crate::hooks::HookRunner::new(crate::hooks::HookConfig {
        session_start: vec![mark[0].to_string().into()],
        user_prompt_submit: vec![refuse.to_string().into()],
        session_end: vec![mark[1].to_string().into()],
        ..crate::hooks::HookConfig::default()
    });
    let ran = run_task_hooked(
        ws.path(),
        "fix it",
        None,
        &[],
        None,
        &mut Report::text(),
        Some(hooks),
    );
    let why = ran.expect_err("exit 2 did not refuse the instruction");
    assert!(why.contains("no secrets"), "{why}");
    assert!(
        ws.path().join("started.txt").exists(),
        "session_start did not run"
    );
    assert!(
        ws.path().join("ended.txt").exists(),
        "a refused run left its session_start without a session_end"
    );
}

#[test]
fn the_allowed_tools_are_a_comma_separated_list_or_all() {
    assert_eq!(parse_allowed_tools(""), None);
    assert_eq!(parse_allowed_tools(" , "), None);
    assert_eq!(
        parse_allowed_tools("Read, Grep,,Glob "),
        Some(vec!["Read".into(), "Grep".into(), "Glob".into()])
    );
}

/// **AN UNANSWERED DECISION MUST NOT READ AS PROSE.**
///
/// `--task` is unattended by definition: one instruction, nobody at the
/// prompt. It never looked at what the agent said for an `<ask>` block,
/// so an agent that reached a fork, asked, and carried on with a guess
/// produced a run reporting plain success — with the question sitting in
/// the middle of the output as though it were commentary.
///
/// `ask::unattended_note` was written for this and had no caller. This
/// asserts the pieces the wiring depends on: that the block is recognised
/// in ordinary agent text, and that the note names the question rather
/// than merely saying one was asked.
#[test]
fn a_question_asked_with_nobody_to_answer_it_is_named() {
    let said = "I started on the endpoint.\n\
                <ask>\n\
                Which store should this use?\n\
                1. Postgres — the document implies joins\n\
                2. SQLite — one file, nothing to operate\n\
                </ask>\n\
                I went with Postgres for now.";

    let questions = crate::ask::parse(said);
    assert_eq!(questions.len(), 1, "the block was not recognised: {said}");

    let note = crate::ask::unattended_note(&questions).expect("a question was asked");
    assert!(
        note.contains("Which store should this use?"),
        "the note must carry the question, not just its existence: {note}"
    );
    assert!(
        note.contains("Postgres") && note.contains("SQLite"),
        "and the options, or the operator cannot answer it: {note}"
    );
}

/// And ordinary output stays ordinary. A note on every run would train an
/// operator to ignore the one that matters.
#[test]
fn a_task_that_asked_nothing_gets_no_note() {
    let plain = "I added the endpoint and its test. Both pass.";
    assert!(crate::ask::parse(plain).is_empty());
    assert_eq!(crate::ask::unattended_note(&crate::ask::parse(plain)), None);
}

/// **The instruction goes last.** Context after an instruction reads as an
/// afterthought, and the agent acts on what it read most recently.
#[test]
fn the_instruction_is_the_last_thing_the_agent_reads() {
    let _guard = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Pointed at nothing, so the context blocks are all absent and the test
    // is about ORDER rather than about what any one service returned.
    std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
    let dir = tempfile::tempdir().expect("temp dir");
    let prompt = build_prompt(
        dir.path(),
        "write the PRD",
        false,
        None,
        crate::spec::SpecGate::On,
    );
    std::env::remove_var(crate::brief::MEMORY_URL_ENV);

    assert!(prompt.ends_with("write the PRD"), "got: {prompt}");
    assert!(prompt.contains("Now do this:"), "got: {prompt}");
}

/// An unreachable body costs the context blocks, never the run — this door
/// has to work on a machine where the service is not running at all.
#[test]
fn the_task_still_runs_when_the_body_is_unreachable() {
    let _guard = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
    let dir = tempfile::tempdir().expect("temp dir");
    let prompt = build_prompt(
        dir.path(),
        "do the thing",
        false,
        None,
        crate::spec::SpecGate::On,
    );
    std::env::remove_var(crate::brief::MEMORY_URL_ENV);

    assert!(prompt.contains("do the thing"));
}

/// **WITH THE SPEC GATE OFF, THE PROMPT DOES NOT ASK FOR THE SPEC SET.**
/// The prompt is what made a one-line fix write five documents first;
/// relaxing only the refusal at the write would have changed nothing.
/// The same workspace, gated, is still told to write them.
#[test]
fn with_the_spec_gate_off_the_prompt_does_not_ask_for_the_spec_set() {
    let _guard = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
    let dir = tempfile::tempdir().expect("temp dir");
    let on = build_prompt(dir.path(), "fix it", false, None, crate::spec::SpecGate::On);
    let off = build_prompt(
        dir.path(),
        "fix it",
        false,
        None,
        crate::spec::SpecGate::Off,
    );
    std::env::remove_var(crate::brief::MEMORY_URL_ENV);

    assert!(on.contains("spec document(s) are missing"), "{on}");
    assert!(on.contains("WRITING SOURCE CODE IS REFUSED"), "{on}");
    assert!(on.contains("<intake>"), "{on}");

    for asked in [
        "spec document(s) are missing",
        "WRITING SOURCE CODE IS REFUSED",
        "<intake>",
    ] {
        assert!(!off.contains(asked), "`{asked}` is still asked for: {off}");
    }
    assert!(off.contains(crate::spec::GATE_OFF_NOTE), "{off}");
    assert!(off.ends_with("fix it"), "the instruction stays last: {off}");
}

/// **THE TOKEN LINE COUNTS WHAT THE BUDGET COUNTS.** The numbers from the
/// report: the line read `20 in / 2247 out` for a turn whose request
/// carried 157,878 input tokens, 140,221 of them served from the cache.
#[test]
fn the_token_line_counts_cached_input_as_input() {
    let line = usage_line(20, 2_247, 17_637, 140_221);
    assert_eq!(
        line, "157.9k in (140.2k cached) / 2.2k out",
        "cache writes and reads are input the turn carried"
    );
}

/// A turn with nothing from the cache says nothing about it.
#[test]
fn a_turn_with_no_cache_hits_does_not_mention_the_cache() {
    assert_eq!(usage_line(12, 3, 0, 0), "12 in / 3 out");
    assert_eq!(usage_line(12, 3, 900, 0), "912 in / 3 out");
}

/// An empty instruction is refused before an agent is spawned for nothing.
#[test]
fn an_empty_instruction_is_refused_without_starting_anything() {
    let dir = tempfile::tempdir().expect("temp dir");
    let refused = run_task(dir.path(), "   ").expect_err("refused");
    assert!(refused.contains("nothing to do"), "got: {refused}");
}

/// A turn, as the stream sees it.
fn a_turn(report: &mut Report) {
    report.session = "sess-json".to_string();
    report.init(Path::new("/ws"), "claude-code", Some("sonnet"));
    for event in [
        AgentEvent::SessionStarted {
            session_id: "agent-conv".to_string(),
            model: "sonnet".to_string(),
        },
        AgentEvent::Thinking("look first".to_string()),
        AgentEvent::TextDelta("Rea".to_string()),
        AgentEvent::ToolUse {
            id: "incode-1".to_string(),
            name: "Read".to_string(),
            input: r#"{"path":"a.txt"}"#.to_string(),
        },
        AgentEvent::ToolResult {
            id: "incode-1".to_string(),
            output: "hello".to_string(),
            is_error: false,
        },
        AgentEvent::Text("It says hello.".to_string()),
        AgentEvent::Usage {
            input_tokens: 10,
            output_tokens: 3,
            cache_creation_tokens: 0,
            cache_read_tokens: 7,
        },
    ] {
        report.event(&event);
    }
    report.outcome(&crate::agent::TurnOutcome {
        session_id: "agent-conv".to_string(),
        stop_reason: "end_turn".to_string(),
        is_error: false,
        detail: None,
        served_by: vec!["claude-sonnet".to_string()],
    });
    report.finish(
        &Ok("It says hello.".to_string()),
        std::time::Duration::from_millis(5),
    );
}

fn lines_of(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("written")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

/// **`stream-json` IS ONE OBJECT PER EVENT, IN ORDER, ENDING WITH THE
/// RESULT** — the whole blocks, never the deltas beside them.
#[test]
fn stream_json_writes_each_event_as_one_json_line() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.jsonl");
    let mut report = Report::text();
    report.out = Some((std::fs::File::create(&path).expect("file"), true));
    a_turn(&mut report);
    let lines = lines_of(&path);
    let kinds: Vec<&str> = lines.iter().filter_map(|l| l["type"].as_str()).collect();
    assert_eq!(
        kinds,
        [
            "init",
            "thinking",
            "tool_use",
            "tool_result",
            "text",
            "usage",
            "result"
        ]
    );
    assert_eq!(lines[0]["agent"], "claude-code");
    assert_eq!(lines[0]["session_id"], "sess-json");
    assert_eq!(
        lines[2]["input"]["path"], "a.txt",
        "the input is JSON, not a string"
    );
    let result = &lines[6];
    assert_eq!(result["is_error"], false);
    assert_eq!(result["result"], "It says hello.");
    assert_eq!(result["agent_session_id"], "agent-conv");
    assert_eq!(result["stop_reason"], "end_turn");
    assert_eq!(result["usage"]["cache_read_tokens"], 7);
    // sonnet: 10 in at $3, 3 out at $15, 7 cache reads at $0.30, per million.
    let cost = result["cost_usd"]
        .as_f64()
        .expect("a priced model has a cost");
    assert!((cost - 77.1e-6).abs() < 1e-12, "{cost}");
    assert_eq!(result["spec_gate"], "on", "the default, said: {result}");
}

/// **THE RESULT SAYS WHEN THE SPEC GATE WAS OFF**, so a caller that
/// relaxed it — Insi, a script — can report that it did.
#[test]
fn the_result_line_carries_the_spec_gate() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.json");
    let mut report = Report::text();
    report.spec_gate = crate::spec::SpecGate::Off;
    report.out = Some((std::fs::File::create(&path).expect("file"), false));
    a_turn(&mut report);
    let lines = lines_of(&path);
    assert_eq!(lines[0]["type"], "result");
    assert_eq!(lines[0]["spec_gate"], "off", "{:?}", lines[0]);
}

/// **WHAT A RUN WROTE OUTSIDE THE WORKSPACE IS IN ITS FIRST `init`.**
/// A first `-t` run adds definitions to the operator's home and said so on
/// stderr only, which a JSON consumer never reads. Present and empty when
/// nothing was written; absent from a fallback's `init`, which would
/// otherwise report the same files twice.
#[test]
fn the_first_init_names_the_files_written_outside_the_workspace() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.jsonl");
    let mut report = Report::text();
    report.out = Some((std::fs::File::create(&path).expect("file"), true));
    report.created = vec!["/home/op/.incode/agents/researcher.md".to_string()];
    report.init(Path::new("/ws"), "claude-code", None);
    report.init(Path::new("/ws"), "other-agent", None);
    let lines = lines_of(&path);
    assert_eq!(
        lines[0]["created"],
        json!(["/home/op/.incode/agents/researcher.md"]),
        "the file written outside the workspace was not named"
    );
    assert_eq!(lines[1]["fallback_from"], "claude-code");
    assert!(lines[1].get("created").is_none(), "{}", lines[1]);

    let quiet = dir.path().join("quiet.jsonl");
    let mut report = Report::text();
    report.out = Some((std::fs::File::create(&quiet).expect("file"), true));
    report.init(Path::new("/ws"), "claude-code", None);
    assert_eq!(lines_of(&quiet)[0]["created"], json!([]));
}

/// `json` is the result alone, once; a failed run's says why.
#[test]
fn json_writes_only_the_result() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("out.json");
    let mut report = Report::text();
    report.out = Some((std::fs::File::create(&path).expect("file"), false));
    a_turn(&mut report);
    let lines = lines_of(&path);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["type"], "result");

    let failed = dir.path().join("failed.json");
    let mut report = Report::text();
    report.out = Some((std::fs::File::create(&failed).expect("file"), false));
    report.finish(&Err("no agent".to_string()), std::time::Duration::ZERO);
    let lines = lines_of(&failed);
    assert_eq!(lines[0]["is_error"], true);
    assert_eq!(lines[0]["error"], "no agent");
}

/// Set only in the child process the purity test starts.
const JSON_CHILD: &str = "INCODE_TASK_JSON_CHILD";

/// The child half: stdout claimed as `run_task_as` claims it, then every
/// kind of stray print, then a turn reported as `stream-json`.
#[test]
fn json_output_child() {
    if std::env::var_os(JSON_CHILD).is_none() {
        return;
    }
    let claimed = crate::acp_agent::stdio::claim().expect("claim stdout");
    println!("stray println");
    crate::outln!("stray outln");
    let _ = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
        .args(if cfg!(windows) {
            ["/C", "echo stray child"]
        } else {
            ["-c", "echo stray child"]
        })
        .status();
    let mut report = Report::text();
    report.out = Some((claimed.output, true));
    a_turn(&mut report);
    std::process::exit(0);
}

/// **STDOUT CARRIES THE JSON AND NOTHING ELSE.** Every stray print — a
/// `println!`, the console, a child that inherits stdout — lands on
/// stderr; stdout is JSON lines only, apart from the test harness's own
/// banner, printed before the child's code runs.
#[test]
fn the_json_output_carries_nothing_else() {
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "task_run::tests::json_output_child",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ])
        .env(JSON_CHILD, "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut json_lines = 0;
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line == "running 1 test" {
            continue;
        }
        assert!(
            serde_json::from_str::<Value>(line).is_ok(),
            "a stray line reached stdout: {line:?}\nstdout:\n{stdout}"
        );
        json_lines += 1;
    }
    assert_eq!(json_lines, 7, "stdout:\n{stdout}\nstderr:\n{stderr}");
    for stray in ["stray println", "stray outln", "stray child"] {
        assert!(stderr.contains(stray), "{stray} went nowhere: {stderr}");
    }
}
