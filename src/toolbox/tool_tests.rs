//! Tests for MultiEdit, ApplyPatch, LS, NotebookEdit, the background shell,
//! TodoWrite, Diagnostics, watchman_lookup, the team tools and the reach
//! table — each through [`Toolbox::run`], the path every route takes.

use super::*;
use crate::permissions::PermissionMode;
use std::time::{Duration, Instant};

fn toolbox(root: &Path) -> Toolbox {
    Toolbox::new(root, true)
}

fn read_only(root: &Path) -> Toolbox {
    let mut tools = toolbox(root);
    tools.set_permission_mode(PermissionMode::ReadOnly);
    tools
}

fn on_disk(path: &Path) -> String {
    std::fs::read_to_string(path).expect("the file is there")
}

// ---------------------------------------------------------------------------
// The reach table
// ---------------------------------------------------------------------------

/// **EVERY ADVERTISED TOOL HAS A REACH, AND EVERY TOOL THE TABLE KNOWS IS
/// ADVERTISED.** A tool with no reach is refused as unknown; a reach with no
/// tool is a rule for nothing.
#[test]
fn the_reach_table_and_the_tool_list_agree() {
    let dir = tempfile::tempdir().expect("tmp");
    let names = toolbox(dir.path()).names();
    for name in &names {
        assert!(reach_of(name).is_some(), "{name} is offered with no reach");
    }
    for name in [
        MULTI_EDIT,
        APPLY_PATCH,
        NOTEBOOK_EDIT,
        LS,
        BASH_OUTPUT,
        KILL_SHELL,
        TODO_WRITE,
        DIAGNOSTICS,
        crate::recorder::LOOKUP_TOOL,
        crate::team_mcp::ASK_TOOL,
        crate::team_mcp::TELL_TOOL,
        crate::team_mcp::ANSWER_TOOL,
        crate::team_mcp::READ_TOOL,
        crate::web::FETCH_TOOL,
        crate::web::SEARCH_TOOL,
    ] {
        assert!(names.iter().any(|n| n == name), "{name} is not offered");
    }
    assert_eq!(reach_of("Teleport"), None);
}

#[test]
fn only_observing_tools_run_in_a_read_only_session() {
    for (name, observes) in [
        (READ, true),
        (LS, true),
        (BASH_OUTPUT, true),
        (TODO_WRITE, true),
        (crate::web::FETCH_TOOL, true),
        (MULTI_EDIT, false),
        (APPLY_PATCH, false),
        (NOTEBOOK_EDIT, false),
        (KILL_SHELL, false),
        (DIAGNOSTICS, false),
        (crate::team_mcp::TELL_TOOL, false),
    ] {
        let reach = reach_of(name).expect("known");
        assert_eq!(reach.only_observes(), observes, "{name}: {reach:?}");
        assert_eq!(reach.fails_closed(), !observes, "{name}");
    }
}

/// The owner's failure policy, for the new tools: an engine that cannot
/// decide stops a MultiEdit and lets an LS through.
#[test]
fn an_engine_that_cannot_decide_stops_a_multi_edit_and_lets_ls_run() {
    use crate::hooks::engine::fake::{self, Reply};
    let (ws, engine) = (
        tempfile::tempdir().expect("ws"),
        tempfile::tempdir().expect("engine"),
    );
    std::fs::write(ws.path().join("f.txt"), "one").expect("seed");
    let bin = fake::write(engine.path(), &[("pre-tool", Reply::exit(1))]);
    let tools =
        toolbox(ws.path()).with_engine(Some(fake::engine(bin).capped(Duration::from_secs(30))));
    let refused = tools
        .run(
            MULTI_EDIT,
            &json!({"path": "f.txt", "edits": [{"old_string": "one", "new_string": "two"}]}),
        )
        .expect_err("fail closed");
    assert!(refused.contains("could not decide"), "{refused}");
    assert_eq!(on_disk(&ws.path().join("f.txt")), "one");
    let listed = tools.run(LS, &json!({})).expect("fail open");
    assert!(listed.contains("f.txt"), "{listed}");
}

// ---------------------------------------------------------------------------
// MultiEdit
// ---------------------------------------------------------------------------

#[test]
fn multi_edit_applies_every_edit_in_order() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("m.txt"), "alpha beta gamma beta\n").expect("seed");
    let said = toolbox(dir.path())
        .run(
            MULTI_EDIT,
            &json!({"path": "m.txt", "edits": [
                {"old_string": "alpha", "new_string": "ALPHA"},
                {"old_string": "ALPHA beta", "new_string": "ALPHA BETA"},
                {"old_string": "beta", "new_string": "b", "replace_all": true}
            ]}),
        )
        .expect("applies");
    assert_eq!(on_disk(&dir.path().join("m.txt")), "ALPHA BETA gamma b\n");
    assert!(said.contains("Applied 3 edit(s)"), "{said}");
}

/// **ALL OR NOTHING.** The third edit does not apply, so the first two are
/// not on disk either.
#[test]
fn multi_edit_changes_nothing_when_one_edit_fails() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("m.txt"), "one two three\n").expect("seed");
    let refused = toolbox(dir.path())
        .run(
            MULTI_EDIT,
            &json!({"path": "m.txt", "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "two", "new_string": "2"},
                {"old_string": "absent", "new_string": "x"}
            ]}),
        )
        .expect_err("the third edit does not apply");
    assert!(refused.contains("edit 3"), "{refused}");
    assert_eq!(
        on_disk(&dir.path().join("m.txt")),
        "one two three\n",
        "an earlier edit landed although a later one failed"
    );
}

#[test]
fn multi_edit_refuses_an_ambiguous_match_and_too_many_edits() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("m.txt"), "x x\n").expect("seed");
    let tools = toolbox(dir.path());
    let refused = tools
        .run(
            MULTI_EDIT,
            &json!({"path": "m.txt", "edits": [{"old_string": "x", "new_string": "y"}]}),
        )
        .expect_err("ambiguous");
    assert!(refused.contains("matches 2 times"), "{refused}");
    let many: Vec<Value> = (0..=edits::MAX_EDITS)
        .map(|_| json!({"old_string": "x", "new_string": "y"}))
        .collect();
    let refused = tools
        .run(MULTI_EDIT, &json!({"path": "m.txt", "edits": many}))
        .expect_err("over the bound");
    assert!(refused.contains("more than"), "{refused}");
    assert_eq!(on_disk(&dir.path().join("m.txt")), "x x\n");
}

#[test]
fn multi_edit_is_refused_on_a_protected_path_and_in_read_only() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
    std::fs::write(dir.path().join("tests/t.txt"), "a").expect("seed");
    std::fs::write(dir.path().join("free.txt"), "a").expect("seed");
    let input =
        |path: &str| json!({"path": path, "edits": [{"old_string": "a", "new_string": "b"}]});

    let refused = toolbox(dir.path())
        .run(MULTI_EDIT, &input("tests/t.txt"))
        .expect_err("protected");
    assert!(refused.contains("protected"), "{refused}");
    assert_eq!(on_disk(&dir.path().join("tests/t.txt")), "a");

    let refused = read_only(dir.path())
        .run(MULTI_EDIT, &input("free.txt"))
        .expect_err("read-only");
    assert!(refused.contains("read-only"), "{refused}");
    assert_eq!(on_disk(&dir.path().join("free.txt")), "a");
}

// ---------------------------------------------------------------------------
// ApplyPatch
// ---------------------------------------------------------------------------

fn patch_of(parts: &[&str]) -> Value {
    json!({ "patch": parts.join("") })
}

const MODIFY_A: &str = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n";
const CREATE_C: &str = "--- /dev/null\n+++ b/c.txt\n@@ -0,0 +1 @@\n+created\n";
const DELETE_D: &str = "--- a/d.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-doomed\n";

fn patch_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(dir.path().join("a.txt"), "keep\nold\n").expect("seed");
    std::fs::write(dir.path().join("b.txt"), "left\nalone\n").expect("seed");
    std::fs::write(dir.path().join("d.txt"), "doomed\n").expect("seed");
    dir
}

#[test]
fn a_patch_modifies_creates_and_deletes() {
    let dir = patch_workspace();
    let said = toolbox(dir.path())
        .run(APPLY_PATCH, &patch_of(&[MODIFY_A, CREATE_C, DELETE_D]))
        .expect("applies");
    assert_eq!(on_disk(&dir.path().join("a.txt")), "keep\nnew\n");
    assert_eq!(on_disk(&dir.path().join("c.txt")), "created\n");
    assert!(!dir.path().join("d.txt").exists(), "deleted");
    assert!(said.contains("modified a.txt"), "{said}");
    assert!(said.contains("created c.txt"), "{said}");
    assert!(said.contains("deleted d.txt"), "{said}");
}

/// **ALL OR NOTHING WHEN ONE HUNK FAILS.** The second file's hunk does not
/// match, so the first file's change, the creation and the deletion are not
/// made either.
#[test]
fn a_patch_with_one_bad_hunk_changes_no_file() {
    let dir = patch_workspace();
    let bad_b = "--- a/b.txt\n+++ b/b.txt\n@@ -1,2 +1,2 @@\n left\n-NOT THERE\n+x\n";
    let refused = toolbox(dir.path())
        .run(
            APPLY_PATCH,
            &patch_of(&[MODIFY_A, CREATE_C, bad_b, DELETE_D]),
        )
        .expect_err("one hunk does not fit");
    assert!(refused.contains("b.txt"), "{refused}");
    assert!(refused.contains("No file was changed"), "{refused}");
    assert_eq!(
        on_disk(&dir.path().join("a.txt")),
        "keep\nold\n",
        "a.txt changed"
    );
    assert!(!dir.path().join("c.txt").exists(), "c.txt was created");
    assert!(dir.path().join("d.txt").exists(), "d.txt was deleted");
}

#[test]
fn a_patch_touching_a_protected_file_is_refused_whole() {
    let dir = patch_workspace();
    std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
    std::fs::write(dir.path().join("tests/t.txt"), "x\n").expect("seed");
    let protected = "--- a/tests/t.txt\n+++ b/tests/t.txt\n@@ -1 +1 @@\n-x\n+y\n";
    let refused = toolbox(dir.path())
        .run(APPLY_PATCH, &patch_of(&[MODIFY_A, protected]))
        .expect_err("protected");
    assert!(refused.contains("protected"), "{refused}");
    assert_eq!(on_disk(&dir.path().join("a.txt")), "keep\nold\n");
    assert_eq!(on_disk(&dir.path().join("tests/t.txt")), "x\n");
}

#[test]
fn a_patch_cannot_reach_outside_the_workspace() {
    let root = tempfile::tempdir().expect("tmp");
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    let escape = "--- /dev/null\n+++ b/../escaped.txt\n@@ -0,0 +1 @@\n+out\n";
    let refused = toolbox(&ws)
        .run(APPLY_PATCH, &patch_of(&[escape]))
        .expect_err("outside the workspace");
    assert!(refused.contains("escapes the workspace"), "{refused}");
    assert!(!root.path().join("escaped.txt").exists());
}

#[test]
fn a_patch_is_refused_in_read_only_and_past_its_file_bound() {
    let dir = patch_workspace();
    let refused = read_only(dir.path())
        .run(APPLY_PATCH, &patch_of(&[MODIFY_A]))
        .expect_err("read-only");
    assert!(refused.contains("read-only"), "{refused}");
    assert_eq!(on_disk(&dir.path().join("a.txt")), "keep\nold\n");

    let many: Vec<String> = (0..=edits::MAX_PATCH_FILES)
        .map(|n| format!("--- /dev/null\n+++ b/n{n}.txt\n@@ -0,0 +1 @@\n+x\n"))
        .collect();
    let refused = toolbox(dir.path())
        .run(APPLY_PATCH, &json!({"patch": many.join("")}))
        .expect_err("over the bound");
    assert!(refused.contains("more than"), "{refused}");
    assert!(!dir.path().join("n0.txt").exists());
}

/// A patch is snapshotted file by file, so `/undo` puts every file back.
#[test]
fn a_patch_is_undoable() {
    let dir = patch_workspace();
    let store = Arc::new(Mutex::new(crate::tools::checkpoint::CheckpointStore::new()));
    let tools = toolbox(dir.path()).with_checkpoints(store.clone());
    store.lock().expect("store").begin("patch");
    tools
        .run(APPLY_PATCH, &patch_of(&[MODIFY_A, CREATE_C, DELETE_D]))
        .expect("applies");
    store
        .lock()
        .expect("store")
        .undo_last(dir.path())
        .expect("undo");
    assert_eq!(on_disk(&dir.path().join("a.txt")), "keep\nold\n");
    assert!(!dir.path().join("c.txt").exists());
    assert_eq!(on_disk(&dir.path().join("d.txt")), "doomed\n");
}

// ---------------------------------------------------------------------------
// LS
// ---------------------------------------------------------------------------

#[test]
fn ls_lists_directories_first_and_honours_ignore() {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::create_dir_all(dir.path().join("zdir")).expect("dir");
    std::fs::write(dir.path().join("a.txt"), "12345").expect("seed");
    std::fs::write(dir.path().join("skip.log"), "x").expect("seed");
    let listed = read_only(dir.path())
        .run(LS, &json!({"ignore": ["*.log"]}))
        .expect("LS observes, so read-only runs it");
    let dir_at = listed.find("zdir/").expect("the directory");
    let file_at = listed.find("a.txt  (5 bytes)").expect("the file and size");
    assert!(dir_at < file_at, "directories first: {listed}");
    assert!(!listed.contains("skip.log"), "{listed}");
}

#[test]
fn ls_is_bounded_and_counts_the_rest() {
    let dir = tempfile::tempdir().expect("tmp");
    for n in 0..listing::MAX_LS_ENTRIES + 5 {
        std::fs::write(dir.path().join(format!("f{n:04}.txt")), "").expect("seed");
    }
    let listed = toolbox(dir.path()).run(LS, &json!({})).expect("lists");
    assert!(
        listed.contains("5 more entries"),
        "{}",
        &listed[listed.len() - 200..]
    );
}

#[test]
fn ls_cannot_list_outside_the_workspace() {
    let root = tempfile::tempdir().expect("tmp");
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    let refused = toolbox(&ws)
        .run(LS, &json!({"path": ".."}))
        .expect_err("outside");
    assert!(refused.contains("escapes the workspace"), "{refused}");
}

// ---------------------------------------------------------------------------
// NotebookEdit
// ---------------------------------------------------------------------------

fn notebook() -> Value {
    json!({
        "cells": [
            {"cell_type": "markdown", "id": "intro", "metadata": {}, "source": ["# Title\n"]},
            {"cell_type": "code", "id": "calc", "metadata": {}, "execution_count": 3,
             "outputs": [{"output_type": "stream", "name": "stdout", "text": ["4\n"]}],
             "source": ["2 + 2\n"]}
        ],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    })
}

fn read_notebook(path: &Path) -> Value {
    serde_json::from_str(&on_disk(path)).expect("still valid JSON")
}

#[test]
fn a_notebook_cell_is_replaced_inserted_and_deleted() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("n.ipynb");
    std::fs::write(&path, notebook().to_string()).expect("seed");
    let tools = toolbox(dir.path());

    tools
        .run(
            NOTEBOOK_EDIT,
            &json!({"path": "n.ipynb", "cell_id": "calc", "new_source": "3 + 3\nprint(6)"}),
        )
        .expect("replace");
    let nb = read_notebook(&path);
    assert_eq!(nb["cells"][1]["source"], json!(["3 + 3\n", "print(6)"]));
    assert_eq!(
        nb["cells"][1]["outputs"],
        json!([]),
        "stale outputs cleared"
    );

    tools
        .run(
            NOTEBOOK_EDIT,
            &json!({"path": "n.ipynb", "cell_id": "intro", "edit_mode": "insert",
                    "cell_type": "markdown", "new_source": "More text"}),
        )
        .expect("insert");
    let nb = read_notebook(&path);
    assert_eq!(nb["cells"].as_array().map(Vec::len), Some(3));
    assert_eq!(nb["cells"][1]["source"], json!(["More text"]));
    assert!(
        nb["cells"][1]["id"].is_string(),
        "nbformat 4.5 cells carry ids"
    );
    assert!(
        nb["cells"][1].get("outputs").is_none(),
        "markdown has no outputs"
    );

    tools
        .run(
            NOTEBOOK_EDIT,
            &json!({"path": "n.ipynb", "cell_number": 0, "edit_mode": "delete"}),
        )
        .expect("delete");
    let nb = read_notebook(&path);
    assert_eq!(nb["cells"].as_array().map(Vec::len), Some(2));
    assert_eq!(nb["cells"][0]["source"], json!(["More text"]));
    assert_eq!(nb["nbformat"], 4, "the rest of the notebook is kept");
}

#[test]
fn a_notebook_edit_refuses_what_it_cannot_do_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("n.ipynb");
    let original = notebook().to_string();
    std::fs::write(&path, &original).expect("seed");
    std::fs::write(dir.path().join("plain.txt"), "x").expect("seed");
    let tools = toolbox(dir.path());

    for (input, says) in [
        (
            json!({"path": "plain.txt", "cell_number": 0, "new_source": "x"}),
            "not a notebook",
        ),
        (
            json!({"path": "n.ipynb", "cell_number": 9, "new_source": "x"}),
            "past the end",
        ),
        (
            json!({"path": "n.ipynb", "cell_id": "nope", "new_source": "x"}),
            "no cell has id",
        ),
        (json!({"path": "n.ipynb", "cell_number": 0}), "new_source"),
        (
            json!({"path": "n.ipynb", "cell_number": 0, "new_source": "x", "edit_mode": "swap"}),
            "edit_mode",
        ),
    ] {
        let refused = tools.run(NOTEBOOK_EDIT, &input).expect_err("refused");
        assert!(refused.contains(says), "{input}: {refused}");
    }
    assert_eq!(on_disk(&path), original, "nothing was written");

    let refused = read_only(dir.path())
        .run(
            NOTEBOOK_EDIT,
            &json!({"path": "n.ipynb", "cell_number": 0, "new_source": "x"}),
        )
        .expect_err("read-only");
    assert!(refused.contains("read-only"), "{refused}");

    std::fs::create_dir_all(dir.path().join("tests")).expect("dir");
    std::fs::write(dir.path().join("tests/t.ipynb"), &original).expect("seed");
    let refused = tools
        .run(
            NOTEBOOK_EDIT,
            &json!({"path": "tests/t.ipynb", "cell_number": 0, "new_source": "x"}),
        )
        .expect_err("protected");
    assert!(refused.contains("protected"), "{refused}");
    assert_eq!(on_disk(&dir.path().join("tests/t.ipynb")), original);
}

// ---------------------------------------------------------------------------
// Background shell
// ---------------------------------------------------------------------------

fn long_running() -> &'static str {
    if cfg!(windows) {
        "ping -n 31 127.0.0.1 > nul"
    } else {
        "sleep 30"
    }
}

fn job_id_in(said: &str) -> String {
    said.split_whitespace()
        .nth(3)
        .map(|w| w.trim_end_matches('.').to_string())
        .expect("`Started background job N.`")
}

#[test]
fn a_background_job_is_started_read_and_its_end_reported() {
    let dir = tempfile::tempdir().expect("tmp");
    let tools = toolbox(dir.path());
    let said = tools
        .run(
            BASH,
            &json!({"command": "echo background_probe", "run_in_background": true}),
        )
        .expect("starts");
    let id = job_id_in(&said);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = String::new();
    while Instant::now() < deadline {
        let read = tools
            .run(BASH_OUTPUT, &json!({"bash_id": id}))
            .expect("the job exists");
        seen.push_str(&read);
        if read.contains("exited with code 0") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen.contains("background_probe"), "{seen}");
    assert!(seen.contains("exited with code 0"), "{seen}");
    let again = tools
        .run(BASH_OUTPUT, &json!({"bash_id": id}))
        .expect("read again");
    assert!(again.contains("no new output"), "{again}");
}

#[test]
fn kill_shell_stops_a_running_job_and_the_count_falls() {
    let dir = tempfile::tempdir().expect("tmp");
    let tools = toolbox(dir.path());
    let said = tools
        .run(
            BASH,
            &json!({"command": long_running(), "run_in_background": true}),
        )
        .expect("starts");
    let id = job_id_in(&said);
    assert_eq!(tools.state.running_jobs(), 1);
    tools
        .run(KILL_SHELL, &json!({"shell_id": id}))
        .expect("stopped");
    assert_eq!(tools.state.running_jobs(), 0);
    let refused = tools
        .run(KILL_SHELL, &json!({"shell_id": id}))
        .expect_err("already stopped");
    assert!(refused.contains("not running"), "{refused}");
    assert!(tools
        .run(BASH_OUTPUT, &json!({"bash_id": "999"}))
        .expect_err("no such job")
        .contains("no background job"));
}

#[test]
fn a_background_job_is_guarded_and_refused_read_only() {
    let dir = tempfile::tempdir().expect("tmp");
    let refused = toolbox(dir.path())
        .run(
            BASH,
            &json!({"command": "echo x > tests/x.txt", "run_in_background": true}),
        )
        .expect_err("writes a protected path");
    assert!(refused.contains("protected"), "{refused}");

    let tools = read_only(dir.path());
    let refused = tools
        .run(
            BASH,
            &json!({"command": "echo hi", "run_in_background": true}),
        )
        .expect_err("read-only");
    assert!(refused.contains("read-only"), "{refused}");
    let refused = tools
        .run(KILL_SHELL, &json!({"shell_id": "1"}))
        .expect_err("read-only");
    assert!(refused.contains("read-only"), "{refused}");
    assert_eq!(tools.state.running_jobs(), 0, "nothing was started");
}

/// Two toolboxes given one session's state see the same jobs — a standby
/// taking over a turn can read and stop what the first backend started.
#[test]
fn a_shared_session_shares_its_jobs() {
    let dir = tempfile::tempdir().expect("tmp");
    let state = SessionTools::new(true);
    let mut first = toolbox(dir.path());
    let mut second = toolbox(dir.path());
    first.share_session(Arc::clone(&state));
    second.share_session(Arc::clone(&state));
    let said = first
        .run(
            BASH,
            &json!({"command": long_running(), "run_in_background": true}),
        )
        .expect("starts");
    assert_eq!(state.running_jobs(), 1);
    second
        .run(KILL_SHELL, &json!({"shell_id": job_id_in(&said)}))
        .expect("the other backend stops it");
    assert_eq!(state.running_jobs(), 0);
}

// ---------------------------------------------------------------------------
// TodoWrite
// ---------------------------------------------------------------------------

#[test]
fn the_task_list_is_kept_whole_and_replaced() {
    let dir = tempfile::tempdir().expect("tmp");
    let tools = read_only(dir.path());
    let said = tools
        .run(
            TODO_WRITE,
            &json!({"todos": [
                {"content": "read the code", "status": "completed"},
                {"content": "fix the bug", "status": "in_progress", "priority": "high"}
            ]}),
        )
        .expect("a task list is session state, so read-only keeps it");
    assert!(said.contains("2 task(s)"), "{said}");
    assert!(said.contains("[>] fix the bug (high)"), "{said}");
    assert_eq!(tools.state.todos().len(), 2);

    tools
        .run(
            TODO_WRITE,
            &json!({"todos": [{"content": "ship", "status": "pending", "priority": "low"}]}),
        )
        .expect("replaced");
    let kept = tools.state.todos().clone();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].priority, Priority::Low);
}

#[test]
fn a_bad_task_list_is_refused_and_the_old_one_kept() {
    let dir = tempfile::tempdir().expect("tmp");
    let tools = toolbox(dir.path());
    tools
        .run(
            TODO_WRITE,
            &json!({"todos": [{"content": "keep me", "status": "pending"}]}),
        )
        .expect("saved");
    for bad in [
        json!({"todos": [{"content": "x", "status": "done"}]}),
        json!({"todos": [{"content": "", "status": "pending"}]}),
        json!({"todos": [{"content": "x", "status": "pending", "priority": "urgent"}]}),
        json!({"todos": (0..=todo::MAX_TODOS).map(|_| json!({"content": "x", "status": "pending"})).collect::<Vec<_>>()}),
        json!({}),
    ] {
        assert!(tools.run(TODO_WRITE, &bad).is_err(), "{bad}");
    }
    assert_eq!(tools.state.todos()[0].content, "keep me");
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// A one-file crate whose library holds `body`.
fn crate_with(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tmp");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("manifest");
    std::fs::create_dir_all(dir.path().join("src")).expect("src");
    std::fs::write(dir.path().join("src/lib.rs"), body).expect("lib");
    dir
}

/// **THE CHECKER'S ERROR REACHES THE MODEL**, cut to the file asked about;
/// the control is the same crate compiling cleanly.
#[test]
fn diagnostics_reports_the_projects_own_errors() {
    let broken = crate_with("pub fn f() -> u32 { \"not a number\" }\n");
    let said = toolbox(broken.path())
        .run(DIAGNOSTICS, &json!({"files": ["src/lib.rs"]}))
        .expect("the checker ran");
    assert!(said.contains("failed"), "{said}");
    assert!(said.contains("lib.rs"), "{said}");
    assert!(said.contains("error"), "{said}");

    let clean = crate_with("pub fn f() -> u32 { 4 }\n");
    let said = toolbox(clean.path())
        .run(DIAGNOSTICS, &json!({}))
        .expect("the checker ran");
    assert!(said.contains("passed"), "{said}");
}

#[test]
fn diagnostics_says_when_no_checker_is_known_and_is_refused_read_only() {
    let dir = tempfile::tempdir().expect("tmp");
    let refused = toolbox(dir.path())
        .run(DIAGNOSTICS, &json!({}))
        .expect_err("nothing to run");
    assert!(refused.contains("no checker is known"), "{refused}");

    let refused = read_only(dir.path())
        .run(DIAGNOSTICS, &json!({}))
        .expect_err("it runs code");
    assert!(refused.contains("read-only"), "{refused}");

    let many: Vec<String> = (0..=checks::MAX_DIAGNOSTIC_FILES)
        .map(|n| format!("f{n}.rs"))
        .collect();
    let refused = toolbox(dir.path())
        .run(DIAGNOSTICS, &json!({"files": many}))
        .expect_err("over the bound");
    assert!(refused.contains("more than"), "{refused}");
}

// ---------------------------------------------------------------------------
// watchman_lookup and the team channel
// ---------------------------------------------------------------------------

#[test]
fn a_lookup_needs_a_tool_and_its_input() {
    let dir = tempfile::tempdir().expect("tmp");
    let tools = toolbox(dir.path());
    // Refused by the tool's own schema, before the lookup is reached.
    let refused = tools
        .run(crate::recorder::LOOKUP_TOOL, &json!({"input": {}}))
        .expect_err("no tool");
    assert!(
        refused.contains("\"tool\" is a required property"),
        "{refused}"
    );
    let refused = tools
        .run(crate::recorder::LOOKUP_TOOL, &json!({"tool": "Read"}))
        .expect_err("no input");
    assert!(
        refused.contains("\"input\" is a required property"),
        "{refused}"
    );
    // A blank tool name passes the schema and is caught by the tool itself.
    let refused = tools
        .run(
            crate::recorder::LOOKUP_TOOL,
            &json!({"tool": " ", "input": {}}),
        )
        .expect_err("blank tool");
    assert!(refused.contains("`tool` is required"), "{refused}");
}

/// **THE TEAM TOOLS ARE THE TOOLBOX'S**: a message lands on the channel
/// under the identity the harness set, whatever the model claims; telling is
/// a record and is refused read-only, reading is not.
#[test]
fn the_team_tools_speak_as_the_harness_says() {
    let dir = tempfile::tempdir().expect("tmp");
    let mut tools = toolbox(dir.path());
    tools.set_team_identity("implement");
    tools
        .run(
            crate::team_mcp::TELL_TOOL,
            &json!({"from": "review", "text": "the column is user_id"}),
        )
        .expect("told");
    let said = crate::team::Channel::at(crate::team::path_in(dir.path())).all();
    assert_eq!(said.len(), 1);
    assert_eq!(said[0].from, "implement", "the model's `from` was believed");

    let mut quiet = read_only(dir.path());
    quiet.set_team_identity("review");
    let refused = quiet
        .run(crate::team_mcp::TELL_TOOL, &json!({"text": "x"}))
        .expect_err("a record, refused read-only");
    assert!(refused.contains("read-only"), "{refused}");
    let heard = quiet
        .run(crate::team_mcp::READ_TOOL, &json!({}))
        .expect("reading is observing");
    assert!(
        heard.contains("user_id") || heard.contains("Nothing"),
        "{heard}"
    );
}

// ---------------------------------------------------------------------------
// WebFetch through the toolbox
// ---------------------------------------------------------------------------

#[test]
fn web_fetch_through_the_toolbox_refuses_this_machine() {
    let dir = tempfile::tempdir().expect("tmp");
    let refused = read_only(dir.path())
        .run(
            crate::web::FETCH_TOOL,
            &json!({"url": "http://127.0.0.1:9180/v1/memory/recall"}),
        )
        .expect_err("a private address");
    assert!(refused.contains("private network"), "{refused}");
}
