use super::*;

#[test]
fn a_pub_item_nobody_calls_is_found() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("lonely.rs"),
        "pub fn never_called_anywhere() {}\npub fn called_from_elsewhere() {}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("caller.rs"),
        "fn go() { called_from_elsewhere(); }\n",
    )
    .expect("write");

    let found = unwired_in(dir.path()).expect("scan");
    let names: Vec<&str> = found.iter().map(|u| u.name.as_str()).collect();

    assert!(names.contains(&"never_called_anywhere"), "got: {names:?}");
    assert!(
        !names.contains(&"called_from_elsewhere"),
        "a called item was reported: {names:?}"
    );
}

/// **THE TESTS ARE NOT CALLERS.**
///
/// The whole point. Every one of the six capabilities this was written
/// for had passing tests — that is what made them look used, and what
/// kept them unwired for months.
#[test]
fn a_test_calling_it_does_not_count_as_wired() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("thing.rs"), "pub fn heartbeat() {}\n").expect("write");
    std::fs::write(
        dir.path().join("other.rs"),
        "fn unrelated() {}\n#[cfg(test)]\nmod tests {\n  #[test]\n  fn t() { heartbeat(); }\n}\n",
    )
    .expect("write");

    let names: Vec<String> = unwired_in(dir.path())
        .expect("scan")
        .into_iter()
        .map(|u| u.name)
        .collect();
    assert!(
        names.contains(&"heartbeat".to_string()),
        "a test was counted as a caller: {names:?}"
    );
}

/// **A MENTION IN A DOC COMMENT IS NOT A CALL.**
///
/// This codebase refers to items by name constantly. Counting those would
/// mean the better-documented an unwired item is, the more wired it looks
/// — and nothing would ever be reported.
#[test]
fn being_written_about_is_not_being_called() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("a.rs"), "pub fn orphan() {}\n").expect("write");
    std::fs::write(
        dir.path().join("b.rs"),
        "/// Calls [`orphan`] one day.\n// orphan();\nfn real() {}\n",
    )
    .expect("write");

    let names: Vec<String> = unwired_in(dir.path())
        .expect("scan")
        .into_iter()
        .map(|u| u.name)
        .collect();
    assert!(
        names.contains(&"orphan".to_string()),
        "prose was counted as a caller: {names:?}"
    );
}

/// **A MODULE CALLING ITS OWN HELPER IS NOT A FINDING.**
///
/// The rule this replaced was "no caller outside the defining file", and
/// against the real tree it reported 225 items — almost all of them
/// ordinary helpers used exactly where they were meant to be. A number
/// that size is a check nobody reads.
///
/// What the six real capabilities had in common is narrower: the
/// declaration was the ONLY appearance of the name in production code
/// anywhere.
#[test]
fn a_helper_its_own_module_calls_is_wired() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("only.rs"),
        "pub fn helper() {}\nfn local() { helper(); }\n",
    )
    .expect("write");

    let names: Vec<String> = unwired_in(dir.path())
        .expect("scan")
        .into_iter()
        .map(|u| u.name)
        .collect();
    assert!(
        !names.contains(&"helper".to_string()),
        "an ordinary helper was reported as dead: {names:?}"
    );
}

/// **ONE TEST-ONLY METHOD MUST NOT BLIND THE REST OF ITS FILE.**
///
/// The check used to cut a file at the first `#[cfg(test)]` anywhere in
/// it. `cli/agent_turn.rs` has one test-only method 177 lines in, so the
/// remaining 1,028 lines — every caller in the file — were invisible, and
/// four working capabilities sat on the dead list for months:
/// `checkpoint::begin`, `has_undo`, `pending_label` and `undo_last`, all
/// of them reached every time anyone typed `/undo`.
///
/// A false positive is worse than a miss here. It makes the list
/// something a reader has to verify by hand before believing any of it,
/// which is how a check stops being used.
#[test]
fn a_test_only_method_does_not_hide_the_callers_below_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn used_later() {}\n\
         impl Thing {\n\
         \x20   #[cfg(test)]\n\
         \x20   pub fn only_for_tests(&self) {}\n\
         }\n\
         fn real_caller() { used_later(); }\n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20   fn t() { whatever(); }\n\
         }\n",
    )
    .expect("write");

    let names: Vec<String> = unwired_in(dir.path())
        .expect("scan")
        .into_iter()
        .map(|u| u.name)
        .collect();

    assert!(
        !names.contains(&"used_later".to_string()),
        "a caller below an item-level `#[cfg(test)]` was not seen: {names:?}"
    );
    // AND THE TEST-ONLY ITEM IS NOT A CAPABILITY. Keeping the rest of the
    // file in scope must not put every test helper onto the list instead.
    assert!(
        !names.contains(&"only_for_tests".to_string()),
        "a test-only method was reported as a dead capability: {names:?}"
    );
}

fn unwired_names(dir: &Path) -> Vec<String> {
    unwired_in(dir)
        .expect("scan")
        .into_iter()
        .map(|u| u.name)
        .collect()
}

/// **A `#[cfg(test)]` ON A `use` HIDES THAT `use`, NOT THE FILE.**
///
/// `agent/claude_code.rs` opened with `#[cfg(test)]\nuse std::io::BufRead;`
/// at column zero, which the old rule read as "the tests start here" — so
/// ~1,600 production lines were never read. Its callers were invisible and
/// its own functions were never declared, in both directions at once.
#[test]
fn a_test_only_use_does_not_hide_the_rest_of_the_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("turn.rs"),
        "use std::fmt;\n\
         #[cfg(test)]\n\
         use std::io::BufRead;\n\
         \n\
         pub fn spawn_never_called() {}\n\
         fn run_once() { called_from_turn(); }\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("other.rs"),
        "pub fn called_from_turn() {}\n",
    )
    .expect("write");

    let names = unwired_names(dir.path());
    assert!(
        !names.contains(&"called_from_turn".to_string()),
        "a caller below a test-only `use` was not seen: {names:?}"
    );
    assert!(
        names.contains(&"spawn_never_called".to_string()),
        "a function below a test-only `use` was never declared: {names:?}"
    );
}

/// **AN INLINE TEST MODULE HIDES ITS OWN BODY AND NOTHING AFTER IT.**
///
/// Its closing brace is found by matching, with strings, chars and
/// comments skipped — the body here holds a `'{'`, a `"}"` and a `// }`
/// that would each end it in the wrong place if they were counted.
#[test]
fn an_inline_test_module_hides_only_its_own_body() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn only_tests_call_me() {}\n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20   fn t() {\n\
         \x20       let _ = ('{', \"}\", r#\"}}\"#); // }\n\
         \x20       only_tests_call_me();\n\
         \x20       wired_below();\n\
         \x20   }\n\
         }\n\
         fn production_after_the_tests() { wired_below(); }\n\
         pub fn dead_after_the_tests() {}\n",
    )
    .expect("write");
    std::fs::write(dir.path().join("b.rs"), "pub fn wired_below() {}\n").expect("write");

    let names = unwired_names(dir.path());
    assert!(
        !names.contains(&"wired_below".to_string()),
        "production code after an inline test module was not read: {names:?}"
    );
    assert!(
        names.contains(&"dead_after_the_tests".to_string()),
        "a function after an inline test module was never declared: {names:?}"
    );
    assert!(
        names.contains(&"only_tests_call_me".to_string()),
        "a call inside the test module counted as production: {names:?}"
    );
}

/// **A TEST MODULE IN ITS OWN FILE IS STILL A TEST.**
///
/// `#[cfg(test)] mod wire_tests;` puts the marker in the PARENT; the file
/// itself carries none. Read alone, its calls looked like production
/// callers and its helpers like capabilities. Covered: the plain sibling
/// file, a `#[path]` one, a module tree below a test file, and the two
/// `cfg`s that look like tests and are not.
#[test]
fn a_split_out_test_file_is_neither_a_caller_nor_a_capability() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("runner").join("tests")).expect("mkdir");
    std::fs::write(
        root.join("runner.rs"),
        "pub fn only_tests_call_me() {}\n\
         pub fn wired_by_any_test_cfg() {}\n\
         #[cfg(test)]\n\
         mod lane_tests;\n\
         #[cfg(test)]\n\
         #[path = \"runner_pinned.rs\"]\n\
         mod pinned;\n\
         #[cfg(all(test, unix))]\n\
         pub(crate) mod tests;\n\
         #[cfg(any(test, feature = \"x\"))]\n\
         mod shipped;\n",
    )
    .expect("write");
    // Every test file calls it, so ANY one of them misread as production
    // wires it. The helper is declared once: declared twice, each copy
    // would count as a mention of the other.
    let calls = "fn t() { only_tests_call_me(); }\n";
    std::fs::write(
        root.join("runner").join("lane_tests.rs"),
        format!("{calls}pub fn helper_in_a_test_file() {{}}\n"),
    )
    .expect("write");
    std::fs::write(root.join("runner_pinned.rs"), calls).expect("write");
    std::fs::write(
        root.join("runner").join("tests.rs"),
        format!("mod deep;\n{calls}"),
    )
    .expect("write");
    std::fs::write(root.join("runner").join("tests").join("deep.rs"), calls).expect("write");
    // `any(test, …)` ships: this file is production, and its call counts.
    std::fs::write(
        root.join("runner").join("shipped.rs"),
        "fn go() { wired_by_any_test_cfg(); }\n",
    )
    .expect("write");

    let names = unwired_names(root);
    assert!(
        names.contains(&"only_tests_call_me".to_string()),
        "a call in a split-out test file counted as production: {names:?}"
    );
    assert!(
        !names.contains(&"helper_in_a_test_file".to_string()),
        "a helper in a split-out test file was reported as a capability: {names:?}"
    );
    assert!(
        !names.contains(&"wired_by_any_test_cfg".to_string()),
        "a `cfg(any(test, …))` module was treated as test-only: {names:?}"
    );
}

/// Every allowlist entry carries a reason somebody had to write.
#[test]
fn nothing_is_excused_without_saying_why() {
    for (name, why) in DELIBERATELY_UNWIRED {
        assert!(!name.trim().is_empty(), "an entry with no name");
        assert!(
            why.trim().len() > 30,
            "`{name}` is excused without a real reason: {why:?}"
        );
    }
}

/// **THIS CRATE, CHECKED AGAINST ITSELF — AS A RATCHET.**
///
/// The first run over the real tree found 68. Demanding zero would mean
/// wiring or excusing all of them in one unfocused change, and a gate that
/// cannot be made green on the day it lands is a gate somebody deletes.
///
/// So the number may go DOWN and never up. That is the property worth
/// enforcing: it does not clear the debt, it stops a seventh
/// `PermissionPolicy` being added without anyone noticing — which is the
/// failure that actually happened, six times.
///
/// When it goes down, lower [`MAX_UNWIRED`]. The test says so on failure.
#[test]
fn no_new_dead_capability_is_added() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let found = unwired_in(&src).expect("the source tree must be readable");

    assert!(
        found.len() <= MAX_UNWIRED,
        "{} capabilities have no caller, up from {MAX_UNWIRED}. Wire the new one \
         up, or add it to DELIBERATELY_UNWIRED with the reason:\n{}",
        found.len(),
        found
            .iter()
            .map(Unwired::line_report)
            .collect::<Vec<_>>()
            .join("\n")
    );

    assert!(
        found.len() >= MAX_UNWIRED,
        "good news: {} capabilities are unwired, down from {MAX_UNWIRED}. \
         Lower MAX_UNWIRED to {} so the ratchet holds the ground you just took.",
        found.len(),
        found.len()
    );
}
