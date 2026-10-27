//! The steps that build a session, each on its own: opening the workspace,
//! the recorder's scope, and where the late lookups land in the prompt.

use super::*;

/// A lookup that has already answered `block`.
fn answered(block: Option<&str>) -> JoinHandle<Option<String>> {
    let block = block.map(str::to_string);
    std::thread::spawn(move || block)
}

/// Owned blocks from string literals.
fn blocks(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|t| t.to_string()).collect()
}

/// **OPENING A WORKSPACE MOVES incode's STATE OUT OF `.incord/` FIRST.**
/// incode's own item moves to `.incode/`, the memory engine's stays where
/// it is, the new folder gets its `.gitignore`, and the path every later
/// step uses is the canonical one however the workspace was named.
#[test]
fn opening_a_workspace_moves_incodes_state_and_names_it_canonically() {
    let ws = tempfile::tempdir().expect("ws");
    let old = ws.path().join(crate::migrate::OLD_PROJECT_DIR);
    std::fs::create_dir(&old).expect("old dir");
    std::fs::write(old.join("ledger.json"), "{}").expect("ledger");
    std::fs::write(old.join("RULE_ACK"), "the engine's").expect("engine file");

    let opened = open_workspace(&ws.path().join(".")).expect("opens");

    assert_eq!(
        opened,
        crate::tools::file_ops::workspace_root(ws.path()).expect("root")
    );
    let new = opened.join(crate::migrate::PROJECT_DIR);
    assert!(new.join("ledger.json").is_file(), "incode's ledger moved");
    assert!(!old.join("ledger.json").exists(), "and is not left behind");
    assert!(old.join("RULE_ACK").is_file(), "the engine's file stays");
    assert!(
        new.join(crate::folders::GITIGNORE).is_file(),
        "the new folder is kept out of commits"
    );
}

/// A workspace that is not there is refused, with its name in the reason.
#[test]
fn a_missing_workspace_is_refused_by_name() {
    let ws = tempfile::tempdir().expect("ws");
    let missing = ws.path().join("not-here");
    let err = open_workspace(&missing).expect_err("no such directory");
    assert!(err.to_string().contains("not-here"), "{err}");
}

/// The recorder files under the brief's own scope; without one, this
/// repository's known id; otherwise nothing, and the recorder keys on the
/// path.
#[test]
fn the_recorder_scope_is_the_briefs_else_this_repositorys_else_none() {
    let scoped = crate::brief::Gate::Ready(Box::new(crate::brief::ProjectBrief {
        scope: "proj-x".to_string(),
        ..Default::default()
    }));
    assert_eq!(
        recorder_scope(&scoped, Path::new("/w/incord-code")),
        "proj-x"
    );

    let unscoped = crate::brief::Gate::Ready(Box::default());
    let no_brief = crate::brief::Gate::NoBrief {
        workspace: PathBuf::from("w"),
    };
    for gate in [&unscoped, &no_brief] {
        for path in [
            r"C:\work\incord-code",
            "/home/me/incord-code",
            "incord-code",
        ] {
            assert_eq!(
                recorder_scope(gate, Path::new(path)),
                "proj-incordcode",
                "{path}"
            );
        }
        assert_eq!(
            recorder_scope(gate, Path::new("/home/me/incord-code-fork")),
            ""
        );
        assert_eq!(recorder_scope(gate, Path::new("/home/me/other")), "");
    }
}

/// **THE LATE LOOKUPS LAND WHERE THEY ALWAYS STOOD.** The intake task goes
/// after the project's instructions and the guards after the skills index,
/// though both are collected after the role table and the style were added.
#[test]
fn the_guards_and_the_intake_land_where_they_always_stood() {
    let mut prompt = Prompt {
        blocks: blocks(&[
            "ask",
            "instructions",
            "rules",
            "skills",
            "auditor",
            "roles",
            "style",
        ]),
        native: Vec::new(),
        intake_at: 2,
        guards_at: 4,
    };

    prompt.place_guards(Some(answered(Some("guards"))));
    prompt.place_intake(
        blocks(&["the brief is missing"]),
        answered(Some("the spec is missing")),
        std::time::Instant::now(),
    );

    assert_eq!(prompt.blocks[..2], blocks(&["ask", "instructions"]));
    let intake = &prompt.blocks[2];
    assert!(intake.starts_with("<intake>\n"), "{intake}");
    assert!(
        intake.contains("the brief is missing\n\nthe spec is missing\n</intake>"),
        "both notes, in order: {intake}"
    );
    assert_eq!(
        prompt.blocks[3..],
        blocks(&["rules", "skills", "guards", "auditor", "roles", "style"])
    );
}

/// Nothing to say leaves the prompt as it was — and a lookup whose thread
/// panicked counts as nothing found, not as a failed session.
#[test]
fn nothing_to_place_leaves_the_prompt_as_it_was() {
    let before = blocks(&["ask", "rules", "skills"]);
    let mut prompt = Prompt {
        blocks: before.clone(),
        native: Vec::new(),
        intake_at: 1,
        guards_at: 3,
    };

    prompt.place_guards(None);
    prompt.place_guards(Some(std::thread::spawn(|| -> Option<String> {
        panic!("the lookup failed")
    })));
    prompt.place_intake(Vec::new(), answered(None), std::time::Instant::now());

    assert_eq!(prompt.blocks, before);
}

/// The auditor's findings, then the recorder's handoff, join the prompt AND
/// the list of blocks the memory engine's own context may stand in for.
#[test]
fn the_findings_join_the_prompt_and_the_engines_overlap() {
    let mut prompt = Prompt {
        blocks: blocks(&["ask"]),
        native: blocks(&["rules"]),
        intake_at: 1,
        guards_at: 1,
    };
    let auditor = "Open findings\none";
    let recorder = "Already solved in this project\nbuild";

    prompt.add_findings(
        Some(answered(Some(auditor))),
        Some(answered(Some(recorder))),
    );

    assert_eq!(prompt.blocks, blocks(&["ask", auditor, recorder]));
    assert_eq!(prompt.native, blocks(&["rules", auditor, recorder]));
}

/// The intake notes become one task, blank-line separated, or none at all.
#[test]
fn the_intake_notes_are_one_task_or_none() {
    assert_eq!(intake_task(&[]), None);
    assert_eq!(
        intake_task(&blocks(&["first", "second"])).as_deref(),
        Some("first\n\nsecond")
    );
}
