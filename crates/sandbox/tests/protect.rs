//! A command confined to a repository cannot leave a git hook or a config
//! change behind for the next unsandboxed `git` to run.

#![cfg(unix)]

use std::process::Command;

#[test]
fn a_planted_hook_and_config_are_put_back_after_the_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = std::fs::canonicalize(dir.path()).expect("canon");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .expect("git")
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    let config_before = std::fs::read_to_string(repo.join(".git/config")).expect("config");

    let out = Command::new(env!("CARGO_BIN_EXE_sbx"))
        .args([
            "run",
            "--best-effort",
            "-w",
            ".",
            "--",
            "sh",
            "-c",
            "printf '#!/bin/sh\\necho pwned\\n' > .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit; \
             git config core.fsmonitor 'echo pwned'; echo ok > work.txt",
        ])
        .current_dir(&repo)
        .output()
        .expect("sbx");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!repo.join(".git/hooks/pre-commit").exists(), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(repo.join(".git/config")).expect("config"),
        config_before,
        "{stderr}"
    );
    assert!(
        repo.join("work.txt").exists(),
        "ordinary work stays: {stderr}"
    );
    // Linux puts the paths back after the run and says so; macOS's profile
    // refuses the writes outright, so there is nothing to put back.
    if cfg!(target_os = "macos") {
        assert!(stderr.contains("Operation not permitted"), "{stderr}");
    } else {
        assert!(stderr.contains("protected path changed"), "{stderr}");
    }

    // Opted out, the change stays.
    Command::new(env!("CARGO_BIN_EXE_sbx"))
        .args([
            "run",
            "--best-effort",
            "--no-protect-git",
            "-w",
            ".",
            "--",
            "sh",
            "-c",
            "echo '#!/bin/sh' > .git/hooks/post-commit",
        ])
        .current_dir(&repo)
        .output()
        .expect("sbx");
    assert!(repo.join(".git/hooks/post-commit").exists());
}
