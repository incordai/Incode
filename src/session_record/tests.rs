use super::*;

/// **LIVE, AGAINST A REAL ENGINE** — the probe that found the break, run
/// through the code that fixes it: a note written to a session's record comes
/// back with an id that resolves, and a lesson saves (it answered 404 before).
///
/// It WRITES to that engine's memory. The note is withdrawn (`done: false`)
/// before the test ends; the lesson stays, so give it something true to keep.
/// Run with `INCODE_LIVE_MEMORY_URL`, `INCODE_LIVE_REPO` (a folder of a project
/// the engine knows), `INCODE_LIVE_SESSION` (a session to note in) and
/// `INCODE_LIVE_LESSON` (the fact to save).
#[test]
#[ignore = "writes to a running engine: INCODE_LIVE_MEMORY_URL, _REPO, _SESSION, _LESSON"]
fn live_a_note_resolves_and_a_lesson_saves() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let (url, repo, session, lesson) = (
        var("INCODE_LIVE_MEMORY_URL"),
        var("INCODE_LIVE_REPO"),
        var("INCODE_LIVE_SESSION"),
        var("INCODE_LIVE_LESSON"),
    );
    let _guard = crate::brief::ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::env::set_var(crate::brief::MEMORY_URL_ENV, &url);
    let record = Record {
        session: &session,
        workspace: Path::new(&repo),
    };
    let line = format!(
        "live probe of session_note, withdrawn by its own test ({})",
        std::process::id()
    );

    let noted = handle_tool(
        NOTE_TOOL,
        &serde_json::json!({ "category": "state", "line": line }),
        &record,
    )
    .expect("handled");
    let saved = crate::knowledge::remember(&crate::knowledge::Lesson::new(
        "fact",
        &lesson,
        Path::new(&repo),
    ));
    let id = noted
        .as_deref()
        .ok()
        .and_then(|said| said.split('`').nth(1))
        .map(str::to_string);
    let withdrawn = id.as_deref().map(|id| {
        handle_tool(
            RESOLVE_TOOL,
            &serde_json::json!({ "id": id, "done": false }),
            &record,
        )
        .expect("handled")
    });
    std::env::remove_var(crate::brief::MEMORY_URL_ENV);

    let noted = noted.expect("the note is written");
    assert!(
        noted.starts_with("Recorded as `"),
        "its id was read back: {noted}"
    );
    let withdrawn = withdrawn.expect("an id to resolve");
    assert!(withdrawn.expect("the id resolves").contains("withdrawn"));
    let saved = saved.expect("the lesson saves where it answered 404");
    assert!(
        !saved.is_empty() && saved != "saved",
        "the store's id: {saved}"
    );
}

fn here() -> Record<'static> {
    Record {
        session: "0b1c2d3e-0000-4000-8000-000000000001",
        workspace: Path::new("D:/work/app"),
    }
}

/// The entry is filed under the session's own id and the folder the work is
/// in — the same two the engine's `put_entry` resolves the project from.
#[test]
fn a_note_is_sent_under_the_session_and_its_folder() {
    let body =
        note_body(&here(), " Pending ", "  run the gate, then open the PR  ").expect("sendable");
    assert_eq!(body["session_id"], "0b1c2d3e-0000-4000-8000-000000000001");
    assert_eq!(body["workspace"], "D:/work/app");
    assert_eq!(
        body["category"], "pending",
        "categories are the engine's lower-case words"
    );
    assert_eq!(body["line"], "run the gate, then open the PR");
}

/// Refused here, in words that name the fix, rather than as a bare 400.
#[test]
fn a_note_the_record_cannot_keep_is_refused_before_it_is_sent() {
    let why = note_body(&here(), "done", "x").expect_err("done is a resolve, not a note");
    assert!(why.contains("state, pending, decision"), "{why}");
    assert!(note_body(&here(), "state", "   ").is_err(), "an empty line");
    assert!(
        note_body(&here(), "state", "one\ntwo").is_err(),
        "two lines are two entries"
    );

    let nobody = Record {
        session: " ",
        workspace: Path::new("."),
    };
    let why = note_body(&nobody, "state", "x").expect_err("no session");
    assert!(why.contains("no id"), "{why}");
}

/// The shape `GET /v1/memory/session` answers in (measured 2026-10-26; names
/// made neutral), with the id as the engine stores it: a u64 too large for a
/// JSON double.
const REAL_RECORD: &str = r#"{"session":{"agent_session":"0b1c2d3e-0000-4000-8000-000000000001",
    "entries":[
      {"as_of":1793029000,"category":"State","file":null,"id":13663989768380469857,"key":null,"line":"an older line"},
      {"as_of":1793029096,"category":"Pending","file":null,"id":5641332108904172636,"key":null,"line":"run the gate"}
    ],"project":"app-0a1b2c","workspace":"D:/work/app"}}"#;

/// The id comes back in the ONE form resolving accepts: hex, as the engine's
/// injected block prints it. Decimal would be read as hex by the engine and
/// close some other entry, or none.
#[test]
fn the_entry_id_is_read_back_as_hex() {
    assert_eq!(
        entry_id(REAL_RECORD, "pending", "run the gate").as_deref(),
        Some("4e4a09a9fb3d0c5c")
    );
    // Above 2^53: read as a u64, not through a float that would round it.
    assert_eq!(
        entry_id(REAL_RECORD, "state", "an older line").as_deref(),
        Some("bda03e4ca6e3d261")
    );
    assert_eq!(
        entry_id(REAL_RECORD, "state", "run the gate"),
        None,
        "the category must match too"
    );
    assert_eq!(
        entry_id("{\"session\":null}", "state", "x"),
        None,
        "no record yet"
    );
}

/// Only an id that can be one is sent: the engine answers anything else
/// with a parse error, and a decimal id would resolve the wrong entry.
#[test]
fn an_id_that_is_not_hex_is_refused_before_it_is_sent() {
    for bad in ["", "  ", "0x", "entry-1", "4e4a 09a9"] {
        let why = resolve(&here(), bad, true).expect_err(bad);
        assert!(why.contains("not an entry id"), "{bad:?}: {why}");
    }
}

/// A fact carries the folder that names its project — without it the engine
/// files it under `personal`, where the project never reads it.
#[test]
fn a_fact_is_sent_with_its_project_folder_and_its_evidence() {
    let body = fact_body(
        &here(),
        "Test",
        "cargo test needs PATH de-duplicated first",
        " src/hooks/engine.rs:751 ",
        "echo $PATH | tr ';' '\\n' | sort | uniq -d",
    )
    .expect("sendable");
    assert_eq!(body["cwd"], "D:/work/app");
    assert_eq!(body["kind"], "test");
    assert_eq!(body["cites"], "src/hooks/engine.rs:751");
    assert!(body["verify"]
        .as_str()
        .is_some_and(|v| v.starts_with("echo")));

    let why = fact_body(&here(), "mistake", "x", "", "").expect_err("not a fact kind");
    assert!(why.contains("invariant"), "{why}");
    assert!(
        fact_body(&here(), "build", "a\nb", "", "").is_err(),
        "one fact, one line"
    );
}

/// Each answer the facts store gives is said as what it was — a duplicate is
/// not reported as something new.
#[test]
fn a_fact_answer_says_what_happened_to_it() {
    let stored = r#"{"ok":true,"project":"p","id":"f-1","edited":false,"duplicate":false,"updated_existing":null,"similar":[]}"#;
    assert_eq!(
        parse_fact(stored, true).expect("ok"),
        "Recorded as fact `f-1`."
    );
    assert!(parse_fact(stored, false)
        .expect("ok")
        .contains("unverified"));

    let held = r#"{"ok":true,"id":"f-1","duplicate":true}"#;
    assert!(parse_fact(held, true)
        .expect("ok")
        .contains("Already known"));

    let merged = r#"{"ok":true,"id":"f-2","duplicate":false,"updated_existing":"f-1"}"#;
    assert!(parse_fact(merged, true).expect("ok").contains("`f-1`"));

    assert!(parse_fact(r#"{"ok":false}"#, true).is_err());
}

/// Every tool is offered, and each name this module answers to is one it
/// advertises — a tool told about and not handled, or handled and never told
/// about, is a call that fails or a capability nobody uses.
#[test]
fn the_tools_offered_are_the_tools_handled() {
    let names: Vec<String> = tool_definitions()
        .iter()
        .filter_map(|d| d["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(names, [NOTE_TOOL, RESOLVE_TOOL, FACT_TOOL]);
    assert!(handle_tool("no_such_tool", &serde_json::json!({}), &here()).is_none());
    for name in &names {
        // Empty input is refused before any request: proves the name is handled
        // without reaching for an engine.
        let answer = handle_tool(name, &serde_json::json!({}), &here()).expect("handled");
        assert!(
            answer.is_err(),
            "{name} with no input must be refused: {answer:?}"
        );
    }
}
