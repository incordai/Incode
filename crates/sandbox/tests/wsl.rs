//! Inside WSL2 a sandboxed command must not be able to start a Windows
//! program, which would run outside the sandbox altogether.
//!
//! The first test checks the mechanism on any Linux: a program hidden with
//! `deny_read` (as WSL's interop interpreter is) can't be executed, directly
//! or as the interpreter of a script. The second runs only inside WSL2 and
//! tries the real ways out.

#![cfg(target_os = "linux")]

use std::process::{Command, Stdio};

const SBX: &str = env!("CARGO_BIN_EXE_sbx");

fn landlock() -> bool {
    Command::new(SBX)
        .args(["check", "--fs", "read-only"])
        .output()
        .expect("check")
        .status
        .success()
}

#[test]
fn a_hidden_interpreter_cannot_be_executed() {
    if !landlock() {
        eprintln!("SKIPPED: this kernel cannot enforce the filesystem");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(dir.path()).expect("canon");
    let hidden = base.join("hidden");
    std::fs::create_dir(&hidden).expect("mkdir");
    // An interpreter, like WSL's /init for .exe files, and a script that
    // names it.
    std::fs::copy("/bin/sh", hidden.join("interp")).expect("copy sh");
    let script = base.join("job");
    std::fs::write(
        &script,
        format!("#!{}\necho escaped\n", hidden.join("interp").display()),
    )
    .expect("script");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let run = |argv: &[&str]| {
        let mut args = vec!["run", "--fs", "read-only", "--deny-read"];
        let h = hidden.to_str().expect("utf8");
        args.push(h);
        args.push("--");
        args.extend_from_slice(argv);
        Command::new(SBX)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .expect("sbx")
    };
    let direct = run(&[
        hidden.join("interp").to_str().expect("utf8"),
        "-c",
        "echo escaped",
    ]);
    assert!(
        !String::from_utf8_lossy(&direct.stdout).contains("escaped"),
        "ran the hidden program directly"
    );
    let via_script = run(&[script.to_str().expect("utf8")]);
    assert!(
        !String::from_utf8_lossy(&via_script.stdout).contains("escaped"),
        "ran the hidden program as a script's interpreter"
    );
    // Control: without hiding it, both run.
    let open = Command::new(SBX)
        .args([
            "run",
            "--fs",
            "read-only",
            "--",
            script.to_str().expect("utf8"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("sbx");
    assert!(String::from_utf8_lossy(&open.stdout).contains("escaped"));
}

#[test]
fn inside_wsl_no_windows_program_can_be_started() {
    if !incord_sandbox::wsl::detected() {
        eprintln!("SKIPPED: not running inside WSL");
        return;
    }
    if !landlock() {
        eprintln!("SKIPPED: this WSL kernel cannot enforce the filesystem");
        return;
    }
    let tries = "cmd.exe /c echo ESCAPED 2>/dev/null; \
                 /mnt/c/Windows/System32/cmd.exe /c echo ESCAPED 2>/dev/null; \
                 powershell.exe -c 'echo ESCAPED' 2>/dev/null; \
                 echo \"[$WSL_INTEROP]\"; ls /run/WSL 2>/dev/null && echo LISTED; true";
    let open = Command::new("sh").args(["-c", tries]).output().expect("sh");
    let confined = Command::new(SBX)
        .args(["run", "--fs", "read-only", "--", "sh", "-c", tries])
        .stdin(Stdio::null())
        .output()
        .expect("sbx");
    let out = String::from_utf8_lossy(&confined.stdout);
    eprintln!(
        "unconfined: {}",
        String::from_utf8_lossy(&open.stdout).replace('\n', " | ")
    );
    assert!(!out.contains("ESCAPED"), "a Windows program ran: {out}");
    assert!(out.contains("[]"), "WSL_INTEROP reached the command: {out}");
    assert!(
        !out.contains("LISTED"),
        "the interop sockets were visible: {out}"
    );
}
