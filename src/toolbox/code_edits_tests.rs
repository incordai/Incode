//! The executor's side of the edit check: what is announced, confirmed,
//! cancelled and recorded around each call, and what a refusal does.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;

/// A ledger that records every call and answers from `answer`.
struct Recorder {
    calls: Mutex<Vec<(Route, Value)>>,
    answer: Box<dyn Fn(Route, &Value) -> Result<Value, LedgerError> + Send + Sync>,
}

impl EditLedger for Recorder {
    fn post(&self, route: Route, body: &Value) -> Result<Value, LedgerError> {
        self.calls.lock().unwrap().push((route, body.clone()));
        (self.answer)(route, body)
    }
}

impl Recorder {
    fn new(answer: impl Fn(Route, &Value) -> Result<Value, LedgerError> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { calls: Mutex::new(Vec::new()), answer: Box::new(answer) })
    }

    /// The engine as it answers a write it allows: no warnings.
    fn allowing() -> Arc<Self> {
        Self::new(|_, body| Ok(json!({ "allowed": true, "file": body["path"], "slots": [] })))
    }

    fn calls(&self) -> Vec<(Route, Value)> {
        self.calls.lock().unwrap().clone()
    }

    /// Each mark call's phase, in order.
    fn phases(&self) -> Vec<String> {
        self.calls()
            .iter()
            .filter(|(r, _)| *r == Route::Mark)
            .map(|(_, b)| b["phase"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

/// A workspace whose specification is complete, so the intake gate lets
/// source be written and only the edit check has a say.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    specified(dir.path());
    dir
}

fn specified(root: &std::path::Path) {
    for f in &crate::spec::SPEC_SET {
        let full = root.join(f.path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, crate::spec::complete_text(f)).unwrap();
    }
}

fn toolbox(root: &std::path::Path, ledger: Arc<Recorder>) -> Toolbox {
    toolbox_as(root, CodeEdits::new(ledger))
}

fn toolbox_as(root: &std::path::Path, edits: CodeEdits) -> Toolbox {
    let mut tools = Toolbox::new(root, true).with_code_edits(Some(edits));
    tools.pin_session("session-1");
    tools
}

const SOURCE: &str = "fn one() {\n    1\n}\n\nfn two() {\n    2\n}\n";

/// **AN EDIT ANNOUNCES EXACTLY THE TEXT IT REPLACES**, as this agent in this
/// session for this call, then confirms it once it landed.
#[test]
fn an_edit_is_announced_by_its_bytes_then_confirmed() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::allowing();
    let tools = toolbox(ws.path(), Arc::clone(&ledger));

    tools
        .run_call("call-7", "Edit", &json!({ "path": "lib.rs", "old_string": "    2", "new_string": "    22" }))
        .expect("the edit runs");

    let calls = ledger.calls();
    assert_eq!(ledger.phases(), ["pending", "confirmed"]);
    let pending = &calls[0].1;
    // The whole line the replaced text is on.
    let at = SOURCE.find("    2").unwrap() as u64;
    assert_eq!(pending["ranges"], json!([[at, at + 6]]));
    assert_eq!(pending["agent_id"], "incode");
    assert_eq!(pending["session_id"], "session-1");
    assert_eq!(pending["tool_use_id"], "call-7");
    let path = std::path::Path::new(pending["path"].as_str().unwrap());
    assert!(path.is_absolute() && path.ends_with("lib.rs"), "{path:?}");
    assert_eq!(pending["cwd"].as_str().unwrap(), ws.path().to_string_lossy());
    // The confirm names the same file and call, and no ranges.
    assert_eq!(calls[1].1["path"], pending["path"]);
    assert_eq!(calls[1].1["tool_use_id"], "call-7");
    assert!(calls[1].1.get("ranges").is_none());
    assert!(std::fs::read_to_string(ws.path().join("lib.rs")).unwrap().contains("    22"));
}

/// **A REFUSED WRITE WRITES NOTHING**: the engine's refusal is the call's
/// result, nothing is confirmed, and the file is as it was.
#[test]
fn a_refused_write_is_not_written_and_says_why() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::new(|_, _| {
        Ok(json!({ "allowed": false, "file": "lib.rs", "refusal": "`two` changed since your read by `incode/sub-1`. Re-read and merge." }))
    });
    let tools = toolbox(ws.path(), Arc::clone(&ledger));

    let refused = tools
        .run_call("c", "Edit", &json!({ "path": "lib.rs", "old_string": "    2", "new_string": "    3" }))
        .expect_err("refused");

    assert!(refused.contains("NOT WRITTEN"), "{refused}");
    assert!(refused.contains("changed since your read by `incode/sub-1`"), "{refused}");
    assert_eq!(ledger.phases(), ["pending"], "nothing to confirm or cancel");
    assert_eq!(std::fs::read_to_string(ws.path().join("lib.rs")).unwrap(), SOURCE);
}

/// **A WRITE THAT FAILED IS CANCELLED**: an Edit whose text is not in the
/// file is announced for the whole file (its effect cannot be worked out)
/// and withdrawn when the tool fails.
#[test]
fn a_write_that_failed_is_cancelled() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::allowing();
    let tools = toolbox(ws.path(), Arc::clone(&ledger));

    tools
        .run_call("c", "Edit", &json!({ "path": "lib.rs", "old_string": "not there", "new_string": "x" }))
        .expect_err("the tool fails");

    assert_eq!(ledger.phases(), ["pending", "cancelled"]);
    assert!(ledger.calls()[0].1.get("ranges").is_none(), "the whole file");
}

/// **A PATCH IS ALL OR NOTHING AT THE CHECK TOO**: the second file refused,
/// the first file's announcement is withdrawn and neither is written.
#[test]
fn a_patch_refused_on_one_file_withdraws_the_others() {
    let ws = workspace();
    std::fs::write(ws.path().join("a.rs"), "fn a() {\n    1\n}\n").unwrap();
    std::fs::write(ws.path().join("b.rs"), "fn b() {\n    1\n}\n").unwrap();
    let ledger = Recorder::new(|_, body| {
        let refused = body["phase"] == "pending" && body["path"].as_str().unwrap().ends_with("b.rs");
        Ok(if refused {
            json!({ "allowed": false, "file": "b.rs", "refusal": "changed by a human since your read" })
        } else {
            json!({ "allowed": true, "file": body["path"], "slots": [] })
        })
    });
    let tools = toolbox(ws.path(), Arc::clone(&ledger));
    let patch = "--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n fn a() {\n-    1\n+    2\n }\n\
                 --- a/b.rs\n+++ b/b.rs\n@@ -1,3 +1,3 @@\n fn b() {\n-    1\n+    2\n }\n";

    let refused = tools.run_call("c", "ApplyPatch", &json!({ "patch": patch })).expect_err("refused");

    assert!(refused.contains("`b.rs`") && refused.contains("changed by a human"), "{refused}");
    assert_eq!(ledger.phases(), ["pending", "pending", "cancelled"]);
    let cancelled = &ledger.calls()[2].1;
    assert!(cancelled["path"].as_str().unwrap().ends_with("a.rs"), "{cancelled}");
    // Each file announced by the line it changes, not all of it.
    let first = &ledger.calls()[0].1;
    assert_eq!(first["ranges"], json!([[9, 15]]), "{first}");
    assert_eq!(std::fs::read_to_string(ws.path().join("a.rs")).unwrap(), "fn a() {\n    1\n}\n");
}

/// **WHAT THE ENGINE WARNS, AND A LOST UPDATE, TRAVEL WITH THE RESULT.**
#[test]
fn warnings_and_lost_updates_are_added_to_the_result() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::new(|_, body| {
        Ok(match body["phase"].as_str() {
            Some("pending") => json!({ "allowed": true, "warnings": ["`two` has no read on record; read it first"] }),
            _ => json!({ "lost_update_suspected": [{ "message": "`two` moved from v3 to v5 since your check" }] }),
        })
    });
    let tools = toolbox(ws.path(), Arc::clone(&ledger));

    let said = tools
        .run_call("c", "Write", &json!({ "path": "lib.rs", "content": SOURCE.replace("    2", "    9") }))
        .expect("written")
        .text;

    assert!(said.contains("[edit check] `two` has no read on record"), "{said}");
    assert!(said.contains("[edit check] LOST UPDATE SUSPECTED: `two` moved from v3 to v5"), "{said}");
}

/// **AN ENGINE THAT DECLINES CHECKS NOTHING**: the write runs as it did
/// before, and nothing is confirmed for a mark that was never stored.
#[test]
fn a_declined_announcement_leaves_the_write_unchecked() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::new(|_, _| Err(LedgerError::Declined("the engine answered 400: not_in_project".into())));
    let tools = toolbox(ws.path(), Arc::clone(&ledger));

    tools
        .run_call("c", "Edit", &json!({ "path": "lib.rs", "old_string": "    1", "new_string": "    5" }))
        .expect("written unchecked");

    assert_eq!(ledger.phases(), ["pending"]);
    assert!(std::fs::read_to_string(ws.path().join("lib.rs")).unwrap().contains("    5"));
}

/// **SILENCE IS ASKED ONCE PER CALL**: a patch over two files whose first
/// announcement gets no answer does not wait on the engine again for the
/// second, and is written unchecked.
#[test]
fn no_answer_stops_asking_for_the_rest_of_the_call() {
    let ws = workspace();
    std::fs::write(ws.path().join("a.rs"), "fn a() {\n    1\n}\n").unwrap();
    std::fs::write(ws.path().join("b.rs"), "fn b() {\n    1\n}\n").unwrap();
    let ledger = Recorder::new(|_, _| Err(LedgerError::NoAnswer("connection refused".into())));
    let tools = toolbox(ws.path(), Arc::clone(&ledger));
    let patch = "--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n fn a() {\n-    1\n+    2\n }\n\
                 --- a/b.rs\n+++ b/b.rs\n@@ -1,3 +1,3 @@\n fn b() {\n-    1\n+    2\n }\n";

    tools.run_call("c", "ApplyPatch", &json!({ "patch": patch })).expect("written unchecked");

    assert_eq!(ledger.phases(), ["pending"]);
    assert!(std::fs::read_to_string(ws.path().join("b.rs")).unwrap().contains("    2"));
}

/// **A FAILED CALL THAT CHANGED A FILE IS CONFIRMED, NOT CANCELLED**, so its
/// change is this agent's and not a human's; an untouched file is withdrawn.
#[test]
fn a_failed_call_confirms_what_it_changed_and_cancels_the_rest() {
    let ws = workspace();
    let (changed, untouched) = (ws.path().join("a.rs"), ws.path().join("b.rs"));
    std::fs::write(&changed, "fn a() {}\n").unwrap();
    std::fs::write(&untouched, "fn b() {}\n").unwrap();
    let ledger = Recorder::allowing();
    let tools = toolbox(ws.path(), Arc::clone(&ledger));
    let target = |p: &std::path::Path| {
        let path = p.to_string_lossy().into_owned();
        Target { cwd: ws.path().to_string_lossy().into_owned(), before: fingerprint(&path), path }
    };
    let announced = Announced { targets: vec![target(&changed), target(&untouched)], warnings: Vec::new() };
    // What a patch whose put-back failed leaves: one file written.
    std::fs::write(&changed, "fn a() { 1 }\n").unwrap();

    tools.settle_writes("c", announced, Err("put back FAILED".into())).expect_err("still failed");

    let calls = ledger.calls();
    let phase_of = |name: &str| {
        calls.iter().find(|(_, b)| b["path"].as_str().unwrap().ends_with(name)).map(|(_, b)| b["phase"].clone())
    };
    assert_eq!(phase_of("a.rs"), Some(json!("confirmed")));
    assert_eq!(phase_of("b.rs"), Some(json!("cancelled")));
}

/// **A WRITE THAT CHANGES NOTHING, A NEW FILE, AND NO SESSION.** Identical
/// content is not announced; a new file is announced whole; without a
/// session id there is nobody to announce it as.
#[test]
fn what_is_announced_for_new_unchanged_and_sessionless_writes() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::allowing();
    let tools = toolbox(ws.path(), Arc::clone(&ledger));

    tools.run_call("c", "Write", &json!({ "path": "lib.rs", "content": SOURCE })).expect("same");
    assert!(ledger.calls().is_empty(), "unchanged: {:?}", ledger.calls());

    tools.run_call("c", "Write", &json!({ "path": "new.rs", "content": "fn n() {}\n" })).expect("new");
    assert_eq!(ledger.phases(), ["pending", "confirmed"]);
    assert!(ledger.calls()[0].1.get("ranges").is_none(), "a new file is all of it");

    let quiet = Recorder::allowing();
    let sessionless = Toolbox::new(ws.path(), true).with_code_edits(Some(CodeEdits::new(quiet.clone())));
    sessionless.run_call("c", "Write", &json!({ "path": "lib.rs", "content": "x" })).expect("written");
    assert!(quiet.calls().is_empty());
}

/// **A READ IS RECORDED FOR THE LINES IT SHOWED**; the whole file names no
/// range. A failed read records nothing.
#[test]
fn a_read_is_recorded_for_what_it_showed() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::new(|_, _| Ok(json!({ "recorded": 1 })));
    let tools = toolbox(ws.path(), Arc::clone(&ledger));

    tools.run_call("c", "Read", &json!({ "path": "lib.rs" })).expect("read");
    tools.run_call("c", "Read", &json!({ "path": "lib.rs", "offset": 4, "limit": 3 })).expect("read");
    tools.run_call("c", "Read", &json!({ "path": "missing.rs" })).expect_err("no such file");

    let calls = ledger.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|(r, b)| *r == Route::Read && b["session_id"] == "session-1" && b["via"] == "file_read"));
    assert!(calls[0].1.get("ranges").is_none());
    let two = SOURCE.find("fn two").unwrap() as u64;
    assert_eq!(calls[1].1["ranges"], json!([[two, SOURCE.len() as u64]]));
}

/// **EVERY SUB-AGENT IS A WRITER AND A CONVERSATION OF ITS OWN**: named
/// under its parent, never twice the same, and filed under a session of its
/// own — under its parent's, its write would read as its parent's read.
#[test]
fn a_sub_agent_announces_under_its_own_name_and_session() {
    let ws = workspace();
    std::fs::write(ws.path().join("lib.rs"), SOURCE).unwrap();
    let ledger = Recorder::allowing();
    let root = CodeEdits::new(ledger.clone());
    let (first, second) = (root.child(), root.child());
    assert_ne!(first.agent_id(), second.agent_id());
    assert!(first.agent_id().starts_with("incode/sub-"), "{}", first.agent_id());
    let grandchild = first.child();
    assert!(grandchild.agent_id().starts_with(&format!("{}/sub-", first.agent_id())));
    // Another root — another backend of the same session — never reuses a name.
    assert_ne!(CodeEdits::new(ledger.clone()).child().agent_id(), first.agent_id());

    let tools = toolbox_as(ws.path(), first.clone());
    tools.run_call("c", "Read", &json!({ "path": "lib.rs" })).expect("read");
    tools
        .run_call("c", "Edit", &json!({ "path": "lib.rs", "old_string": "    2", "new_string": "    3" }))
        .expect("written");

    let scope = first.agent_id().trim_start_matches(ROOT_AGENT).to_string();
    for (_, body) in ledger.calls() {
        assert_eq!(body["session_id"], format!("session-1{scope}"), "{body}");
        if body.get("agent_id").is_some() {
            assert_eq!(body["agent_id"], first.agent_id(), "{body}");
        }
    }
}

#[test]
fn changed_ranges_are_whole_lines_of_the_file_as_it_stands() {
    let before = "a\nb\nc\nd\n";
    assert_eq!(changed_ranges(before, "a\nB\nc\nd\n"), vec![[2, 4]]);
    assert_eq!(changed_ranges(before, "a\nc\nd\n"), vec![[2, 4]], "a removed line");
    assert_eq!(changed_ranges(before, "A\nb\nc\nD\n"), vec![[0, 2], [6, 8]]);
    assert_eq!(changed_ranges("a\r\nb\r\n", "a\r\nB\r\n"), vec![[3, 6]]);
    assert!(changed_ranges(before, before).is_empty());
}

/// **AN INSERTION IS ITS NEIGHBOURS**: lines only added are announced as the
/// line before and the line after, since what they change is at an edge —
/// at the start of the file, at its end, and in between.
#[test]
fn an_insertion_is_announced_as_the_lines_around_it() {
    let before = "a\nb\nc\nd\n";
    assert_eq!(changed_ranges(before, "a\nb\nx\nc\nd\n"), vec![[2, 6]], "b and c");
    assert_eq!(changed_ranges(before, "x\na\nb\nc\nd\n"), vec![[0, 2]], "the first line");
    assert_eq!(changed_ranges(before, "a\nb\nc\nd\nx\n"), vec![[6, 8]], "the last line");
    assert_eq!(changed_ranges("", "x\n"), vec![[0, 0]], "an empty file has nothing to cover");
}

/// **A DOC COMMENT ADDED ABOVE A FUNCTION IS THAT FUNCTION'S CHANGE**: the
/// function's first line is in the announcement (the engine starts a
/// definition at its doc comment, and checks only what a range overlaps).
#[test]
fn a_doc_comment_added_above_a_function_announces_the_function() {
    let after = SOURCE.replace("fn two", "/// The second.\nfn two");
    let ranges = changed_ranges(SOURCE, &after);
    let two = SOURCE.find("fn two").unwrap() as u64;
    assert!(ranges.iter().any(|&[s, e]| s <= two && two < e), "{ranges:?} misses byte {two}");
}

/// **A LONE `\r` IS A LINE END, AS THE DIFF COUNTS IT**: ranges after it are
/// not shifted onto the wrong lines.
#[test]
fn a_lone_carriage_return_does_not_shift_the_ranges() {
    let before = "x\ry\nfn a() {\n    1\n}\nfn b() {\n    2\n}\n";
    let after = before.replace("    2", "    3");
    let at = before.find("    2").unwrap() as u64;
    assert_eq!(changed_ranges(before, &after), vec![[at, at + 6]]);
}

#[test]
fn shown_range_counts_lines_as_read_does() {
    let text = "a\nbb\nccc\n";
    assert_eq!(shown_range(text, 0, None), None);
    assert_eq!(shown_range(text, 0, Some(10)), None);
    assert_eq!(shown_range(text, 1, Some(1)), Some([2, 5]));
    assert_eq!(shown_range(text, 1, None), Some([2, 9]));
    assert_eq!(shown_range(text, 0, Some(1)), Some([0, 2]));
}

#[test]
fn only_an_engine_on_this_machine_is_announced_to() {
    assert!(is_loopback("http://127.0.0.1:9180"));
    assert!(is_loopback("http://localhost:9180/"));
    assert!(is_loopback("http://[::1]:9180"));
    assert!(!is_loopback("https://api.example.com"));
    assert!(!is_loopback("http://10.0.0.5:9180"));
    assert!(!is_loopback("not a url"));
}

/// **LIVE, AGAINST A REAL ENGINE**: two sessions on one checkout. The one
/// that did not see the other's change is refused, writes nothing, and
/// succeeds once it has read again. Run with `INCODE_LIVE_MEMORY_URL` naming
/// a throwaway engine and `INCODE_LIVE_REPO` a checkout registered with it
/// whose `src/cart.rs` has `apply_voucher` returning `total - 5`.
#[test]
#[ignore = "needs a running engine: INCODE_LIVE_MEMORY_URL and INCODE_LIVE_REPO"]
fn live_an_unseen_change_is_refused_by_the_engine() {
    let (Ok(url), Ok(repo)) = (std::env::var("INCODE_LIVE_MEMORY_URL"), std::env::var("INCODE_LIVE_REPO")) else {
        panic!("set INCODE_LIVE_MEMORY_URL and INCODE_LIVE_REPO");
    };
    let repo = std::path::PathBuf::from(repo);
    specified(&repo);
    let ledger: Arc<dyn EditLedger> = Arc::new(EngineLedger::at(&url));
    let session = |id: &str| {
        let mut tools = Toolbox::new(&repo, true).with_code_edits(Some(CodeEdits::new(Arc::clone(&ledger))));
        tools.pin_session(id);
        tools
    };
    let (a, b) = (session("live-a"), session("live-b"));
    let file = "src/cart.rs";
    // The engine's own answer to one read, before anything relies on it: an
    // `Err` here is why every later call would run unchecked.
    let probe = ledger.post(
        Route::Read,
        &json!({ "cwd": repo.to_string_lossy(), "path": repo.join(file).to_string_lossy(), "session_id": "live-probe" }),
    );
    println!("engine probe: {probe:?}");
    probe.expect("the engine answers a read");

    a.run_call("a-1", "Read", &json!({ "path": file })).expect("A reads");
    b.run_call("b-1", "Read", &json!({ "path": file })).expect("B reads");
    let said = b
        .run_call("b-2", "Edit", &json!({ "path": file, "old_string": "total - 5", "new_string": "total - 7" }))
        .expect("B's edit is allowed")
        .text;
    println!("B edits: {said}");

    let refused = a
        .run_call("a-2", "Edit", &json!({ "path": file, "old_string": "total - 5", "new_string": "total - 9" }))
        .expect_err("A never saw B's change");
    println!("A is refused: {refused}");
    assert!(refused.contains("NOT WRITTEN"), "{refused}");
    assert!(refused.contains("apply_voucher"), "{refused}");
    let now = std::fs::read_to_string(repo.join(file)).unwrap();
    assert!(now.contains("total - 7") && !now.contains("total - 9"), "{now}");

    a.run_call("a-3", "Read", &json!({ "path": file })).expect("A reads again");
    let said = a
        .run_call("a-4", "Edit", &json!({ "path": file, "old_string": "total - 7", "new_string": "total - 9" }))
        .expect("A's edit on top of B's is allowed")
        .text;
    println!("A edits after reading: {said}");
    assert!(std::fs::read_to_string(repo.join(file)).unwrap().contains("total - 9"));

    // A parent and its sub-agent, pinned to one session: the sub-agent's
    // edit is not the parent's read, so the parent's write from its old view
    // is refused, naming the sub-agent.
    let parent_edits = CodeEdits::new(Arc::clone(&ledger));
    let child_edits = parent_edits.child();
    let pinned = |edits: CodeEdits| {
        let mut tools = Toolbox::new(&repo, true).with_code_edits(Some(edits));
        tools.pin_session("live-p");
        tools
    };
    let (parent, child) = (pinned(parent_edits), pinned(child_edits.clone()));
    parent.run_call("p-1", "Read", &json!({ "path": file })).expect("the parent reads");
    child.run_call("c-1", "Read", &json!({ "path": file })).expect("the sub-agent reads");
    child
        .run_call("c-2", "Edit", &json!({ "path": file, "old_string": "items.iter().sum()", "new_string": "items.iter().copied().sum()" }))
        .expect("the sub-agent edits");
    let before_child = std::fs::read_to_string(repo.join(file)).unwrap().replace("items.iter().copied().sum()", "items.iter().sum()");
    let refused = parent
        .run_call("p-2", "Write", &json!({ "path": file, "content": before_child.replace("total - 9", "total - 1") }))
        .expect_err("the parent never saw its sub-agent's change");
    println!("the parent is refused: {refused}");
    assert!(refused.contains(&child_edits.agent_id()), "{refused}");
    assert!(std::fs::read_to_string(repo.join(file)).unwrap().contains("copied()"));

    // The code tools answer from the same engine's code index for this
    // checkout (the script points INCORD_MEMORY_URL at it).
    let found = a.run_call("k-1", "find_symbol", &json!({ "symbol": "apply_voucher" })).expect("find_symbol").text;
    println!("find_symbol: {found}");
    assert!(found.contains("src/cart.rs"), "{found}");
    let outline = a.run_call("k-2", "code_skeleton", &json!({ "path": file })).expect("code_skeleton").text;
    println!("code_skeleton: {outline}");
    assert!(outline.contains("cart_total") && outline.contains("apply_voucher"), "{outline}");
    let blast = a.run_call("k-3", "git_blast_radius", &json!({})).expect("git_blast_radius").text;
    println!("git_blast_radius: {blast}");
    assert!(blast.contains("apply_voucher") || blast.contains("cart_total"), "{blast}");
}

#[test]
fn multi_edit_is_worked_out_as_the_tool_applies_it() {
    let edits = vec![
        json!({ "old_string": "1", "new_string": "one" }),
        json!({ "old_string": "one", "new_string": "uno" }),
    ];
    assert_eq!(multi_edited("x 1 y", &edits).as_deref(), Some("x uno y"));
    let ambiguous = vec![json!({ "old_string": "a", "new_string": "b" })];
    assert_eq!(multi_edited("a a", &ambiguous), None);
    let all = vec![json!({ "old_string": "a", "new_string": "b", "replace_all": true })];
    assert_eq!(multi_edited("a a", &all).as_deref(), Some("b b"));
}
