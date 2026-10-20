//! Escape tests: each one tries to get out of the sandbox the way a confused
//! or hostile command would, through the real `sbx` binary.
//!
//! Linux only, and each test skips (with a note) when the kernel cannot
//! enforce what it checks, so a machine without Landlock reports a skip
//! rather than a false pass.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SBX: &str = env!("CARGO_BIN_EXE_sbx");

fn sbx(args: &[&str], cwd: &Path) -> Output {
    Command::new(SBX)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("sbx runs")
}

fn sh(cwd: &Path, flags: &[&str], script: &str) -> Output {
    let mut args = vec!["run"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&["--", "sh", "-c", script]);
    sbx(&args, cwd)
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn landlock_fs() -> bool {
    let o = Command::new(SBX)
        .args(["check", "--fs", "workspace-write"])
        .output()
        .expect("check");
    o.status.success()
}

fn landlock_net() -> bool {
    Command::new(SBX)
        .args(["check", "--net", "deny"])
        .output()
        .expect("check")
        .status
        .success()
}

macro_rules! require {
    ($cond:expr, $what:expr) => {
        if !$cond {
            eprintln!("SKIPPED: this kernel cannot enforce {}", $what);
            return;
        }
    };
}

struct Dirs {
    _root: tempfile::TempDir,
    ws: std::path::PathBuf,
    out: std::path::PathBuf,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("ws");
    let out = root.path().join("out");
    std::fs::create_dir(&ws).expect("ws");
    std::fs::create_dir(&out).expect("out");
    Dirs {
        ws,
        out,
        _root: root,
    }
}

#[test]
fn writes_land_only_inside_the_writable_root() {
    require!(landlock_fs(), "the filesystem");
    let d = dirs();
    let outside = d.out.join("x");
    let o = sh(
        &d.ws,
        &["-w", "."],
        &format!("echo ok > inside && echo bad > {}", outside.display()),
    );
    assert!(d.ws.join("inside").exists(), "{}", text(&o));
    assert!(!outside.exists(), "wrote outside the root: {}", text(&o));
    assert!(!o.status.success());
}

#[test]
fn symlinks_hardlinks_renames_and_proc_root_do_not_escape() {
    require!(landlock_fs(), "the filesystem");
    let d = dirs();
    std::fs::write(d.out.join("f"), "secret").expect("seed");
    std::os::unix::fs::symlink(&d.out, d.ws.join("link")).expect("symlink");
    let out = d.out.display();
    for script in [
        "echo bad > link/via-symlink".to_string(),
        format!("ln {out}/f hl && echo pwned > hl"),
        format!("mv {out}/f ./moved"),
        format!("echo bad > /proc/self/root{out}/via-proc"),
        format!("sh -c 'touch {out}/via-child'"),
    ] {
        let o = sh(&d.ws, &["-w", "."], &script);
        assert!(!o.status.success(), "`{script}` succeeded: {}", text(&o));
    }
    let mut left: Vec<String> = std::fs::read_dir(&d.out)
        .expect("read")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["f"], "something escaped into {out}");
    assert_eq!(
        std::fs::read_to_string(d.out.join("f")).expect("f"),
        "secret"
    );
}

#[test]
fn temp_is_private_and_shared_tmp_is_not_writable() {
    require!(landlock_fs(), "the filesystem");
    let d = dirs();
    let marker = std::env::temp_dir().join(format!("sbx-escape-{}", std::process::id()));
    let o = sh(
        &d.ws,
        &["-w", "."],
        &format!(
            "touch \"$TMPDIR/ok\" && echo \"$TMPDIR\"; touch {}",
            marker.display()
        ),
    );
    let out = text(&o);
    assert!(out.contains("sbx-"), "TMPDIR is the private one: {out}");
    assert!(!marker.exists(), "wrote to the shared temp dir");
}

#[test]
fn deny_read_hides_a_path_through_every_route() {
    require!(landlock_fs(), "the filesystem");
    let d = dirs();
    std::fs::write(d.out.join("answer"), "42").expect("seed");
    std::os::unix::fs::symlink(&d.out, d.ws.join("peek")).expect("symlink");
    let out = d.out.display();
    for script in [
        format!("cat {out}/answer"),
        format!("ls {out}"),
        "cat peek/answer".to_string(),
        format!("cat /proc/self/root{out}/answer"),
        format!("cd {out}/.. && cat out/answer"),
        format!("sh -c 'cat {out}/answer'"),
    ] {
        let o = sh(
            &d.ws,
            &["-w", ".", "--deny-read", &out.to_string()],
            &script,
        );
        assert!(
            !String::from_utf8_lossy(&o.stdout).contains("42"),
            "`{script}` read the hidden file: {}",
            text(&o)
        );
    }
    // Everything else is still readable, the workspace included. Listing
    // a directory ABOVE the hidden path is refused (Landlock grants whole
    // subtrees, so `/` itself is not granted); reading beneath it is not.
    std::fs::write(d.ws.join("mine"), "ok").expect("seed");
    let o = sh(
        &d.ws,
        &["-w", ".", "--deny-read", &out.to_string()],
        "cat mine && cat /etc/hostname >/dev/null && ls /usr >/dev/null && echo fine",
    );
    assert!(text(&o).contains("okfine"), "{}", text(&o));
}

#[test]
fn deny_read_inside_a_writable_root_is_refused() {
    let d = dirs();
    std::fs::create_dir(d.ws.join("secret")).expect("mkdir");
    let o = sh(&d.ws, &["-w", ".", "--deny-read", "secret"], "true");
    assert_eq!(o.status.code(), Some(125), "{}", text(&o));
    assert!(
        text(&o).contains("inside the writable root"),
        "{}",
        text(&o)
    );
}

#[test]
fn read_only_writes_nothing() {
    require!(landlock_fs(), "the filesystem");
    let d = dirs();
    let o = sh(&d.ws, &["--fs", "read-only"], "touch x");
    assert!(!o.status.success());
    assert!(!d.ws.join("x").exists());
}

#[test]
fn network_deny_refuses_connect_and_bind_even_on_loopback() {
    require!(landlock_net(), "TCP (Landlock ABI v4)");
    let d = dirs();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let script = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port} 2>/dev/null && echo connected; \
         python3 -c 'import socket;socket.socket().bind((\"127.0.0.1\",0));print(\"bound\")' 2>/dev/null"
    );
    let o = sbx(
        &["run", "--net", "deny", "--", "bash", "-c", &script],
        &d.ws,
    );
    let out = text(&o);
    assert!(!out.contains("connected"), "{out}");
    assert!(!out.contains("bound"), "{out}");
}

#[test]
fn the_proxy_lets_through_only_the_allowlist_and_cannot_be_bypassed() {
    require!(landlock_net(), "TCP (Landlock ABI v4)");
    let d = dirs();
    let origin = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = origin.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for conn in origin.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = conn.read(&mut buf);
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\norigin");
        }
    });
    let allow = format!("localhost:{port}");
    let script = format!(
        "printf 'allowed='; curl -s --max-time 5 http://localhost:{port}/; echo; \
         printf 'other='; curl -s --max-time 5 http://127.0.0.1:{port}/ | head -c 40; echo; \
         printf 'bypass='; curl -s --max-time 5 --noproxy '*' http://localhost:{port}/; echo"
    );
    // The test origin is on loopback, so private answers must be allowed,
    // and this process's own upstream proxy (if any) must not be used.
    let o = Command::new(SBX)
        .args([
            "run",
            "--allow-host",
            &allow,
            "--allow-private",
            "--",
            "sh",
            "-c",
            &script,
        ])
        .current_dir(&d.ws)
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .stdin(Stdio::null())
        .output()
        .expect("sbx");
    let out = text(&o);
    assert!(out.contains("allowed=origin"), "{out}");
    assert!(out.contains("other=sbx: 127.0.0.1"), "{out}");
    assert!(
        out.contains("bypass=\n"),
        "the direct connection got through: {out}"
    );
    assert!(out.contains("network refused 127.0.0.1"), "{out}");
}

#[test]
fn the_agent_session_and_git_redirects_are_stripped_and_credentials_kept() {
    let d = dirs();
    let o = Command::new(SBX)
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "echo \"[$CLAUDECODE][$CLAUDE_CODE_SESSION_ID][$GIT_DIR][$ANTHROPIC_API_KEY]\"",
        ])
        .current_dir(&d.ws)
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_SESSION_ID", "parent")
        .env("GIT_DIR", "/elsewhere/.git")
        .env("ANTHROPIC_API_KEY", "key")
        .output()
        .expect("sbx");
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "[][][][key]");
}

#[test]
fn the_wall_limit_kills_the_whole_group() {
    let d = dirs();
    let tag = format!("sbx_wall_{}", std::process::id());
    let start = Instant::now();
    let script = format!("(exec -a {tag} sleep 30) & (exec -a {tag} sleep 30)");
    let o = sbx(&["run", "--wall", "1", "--", "bash", "-c", &script], &d.ws);
    assert_eq!(o.status.code(), Some(124), "{}", text(&o));
    assert!(start.elapsed() < Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!running(&tag), "a process outlived the wall limit");
}

#[test]
fn killing_sbx_with_sigkill_takes_the_command_with_it() {
    let d = dirs();
    let tag = format!("sbx_orphan_{}", std::process::id());
    let mut child = Command::new(SBX)
        .args([
            "run",
            "--wall",
            "60",
            "--",
            "bash",
            "-c",
            &format!("(exec -a {tag} sleep 30) & (exec -a {tag} sleep 30)"),
        ])
        .current_dir(&d.ws)
        .stdin(Stdio::null())
        .spawn()
        .expect("sbx");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !running(&tag) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(running(&tag), "the command never started");
    child.kill().expect("SIGKILL");
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(3);
    while running(&tag) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!running(&tag), "the command outlived sbx");
}

#[test]
fn limits_and_exit_codes() {
    let d = dirs();
    let o = sh(&d.ws, &["--wall", "5"], "exit 7");
    assert_eq!(o.status.code(), Some(7));
    let o = sh(&d.ws, &[], "kill -9 $$");
    assert_eq!(o.status.code(), Some(137));
    let o = sh(&d.ws, &["--fsize", "1"], "head -c 3000000 /dev/zero > big");
    assert!(!o.status.success());
    assert_eq!(
        std::fs::metadata(d.ws.join("big")).expect("big").len(),
        1024 * 1024
    );
    let o = sbx(&["run", "--", "no-such-command-sbx"], &d.ws);
    assert_eq!(o.status.code(), Some(127));
    let o = sbx(&["run", "-w", "missing-root", "--", "true"], &d.ws);
    assert_eq!(o.status.code(), Some(125));
}

/// Whether a process whose argv[0] is `tag` is running.
fn running(tag: &str) -> bool {
    std::fs::read_dir("/proc")
        .expect("proc")
        .filter_map(Result::ok)
        .any(|e| {
            std::fs::read(e.path().join("cmdline"))
                .ok()
                .is_some_and(|c| c.split(|b| *b == 0).next() == Some(tag.as_bytes()))
        })
}
