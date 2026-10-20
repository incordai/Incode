//! Escape tests against the native Windows sandbox (write-restricted token,
//! ACL grant, job object): the Linux escape tests that apply on Windows,
//! through the real `sbx.exe`.
//!
//! They use only what a stock Windows (and the `windows-latest` CI runner)
//! has: `cmd.exe` and Windows PowerShell. Scripts are written to files
//! rather than passed inline, so no test depends on how `cmd` re-parses
//! quotes. Every run is native (`SBX_BACKEND=native`) unless the test is
//! about choosing the backend.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SBX: &str = env!("CARGO_BIN_EXE_sbx");
/// Every run SID starts with this (`incord_sandbox::windows::SID_PREFIX`).
const SID_MARK: &str = "4242424242";

fn base(cwd: &Path) -> Command {
    let mut c = Command::new(SBX);
    c.current_dir(cwd)
        .env("SBX_BACKEND", "native")
        .env_remove("SBX_NATIVE")
        .stdin(Stdio::null());
    c
}

fn sbx(args: &[&str], cwd: &Path) -> Output {
    base(cwd).args(args).output().expect("sbx runs")
}

/// `sbx run <flags> -- cmd /d /s /c <line>`.
fn cmd(cwd: &Path, flags: &[&str], line: &str) -> Output {
    let mut args = vec!["run"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&["--", "cmd", "/d", "/s", "/c", line]);
    sbx(&args, cwd)
}

/// `sbx run <flags> -- powershell ... -File <script>`.
fn ps_file(cwd: &Path, flags: &[&str], script: &Path) -> Output {
    let script = script.to_str().expect("utf8");
    let mut args = vec!["run"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&[
        "--",
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        script,
    ]);
    sbx(&args, cwd)
}

/// `sbx run <flags> -- powershell ... -Command <command>`.
fn ps(cwd: &Path, flags: &[&str], command: &str) -> Output {
    let mut args = vec!["run"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&[
        "--",
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        command,
    ]);
    sbx(&args, cwd)
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

struct Dirs {
    _root: tempfile::TempDir,
    ws: PathBuf,
    out: PathBuf,
}

/// A workspace and a sibling folder outside it, by their real names (no
/// 8.3 short names, no `\\?\`), so paths written into scripts are plain.
fn dirs() -> Dirs {
    let root = tempfile::tempdir().expect("tempdir");
    let real = std::fs::canonicalize(root.path()).expect("canon");
    let real = PathBuf::from(
        real.to_string_lossy()
            .strip_prefix(r"\\?\")
            .map(str::to_string)
            .unwrap_or_else(|| real.to_string_lossy().into_owned()),
    );
    let ws = real.join("ws");
    let out = real.join("out");
    std::fs::create_dir(&ws).expect("ws");
    std::fs::create_dir(&out).expect("out");
    Dirs {
        ws,
        out,
        _root: root,
    }
}

fn icacls(path: &Path) -> String {
    let o = Command::new("icacls").arg(path).output().expect("icacls");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn writes_land_only_inside_the_writable_root() {
    let d = dirs();
    let outside = d.out.join("x");
    let o = cmd(
        &d.ws,
        &["-w", "."],
        &format!("echo ok> inside & echo bad> {}", outside.display()),
    );
    assert!(d.ws.join("inside").exists(), "{}", text(&o));
    assert!(!outside.exists(), "wrote outside the root: {}", text(&o));
}

#[test]
fn the_grant_is_held_for_the_run_and_revoked_after() {
    let d = dirs();
    let o = cmd(&d.ws, &["-w", "."], "icacls .");
    assert!(
        text(&o).contains(SID_MARK),
        "the root carries the run's ACE during the run: {}",
        text(&o)
    );
    let after = icacls(&d.ws);
    assert!(
        !after.contains(SID_MARK),
        "the ACE outlived the run: {after}"
    );
}

#[test]
fn read_only_writes_nothing_but_its_private_temp() {
    let d = dirs();
    let o = cmd(
        &d.ws,
        &["--fs", "read-only"],
        r"echo x> x & echo tmp-ok> %TEMP%\t.txt & type %TEMP%\t.txt & echo TEMP=%TEMP%",
    );
    let out = text(&o);
    assert!(!d.ws.join("x").exists(), "{out}");
    assert!(
        out.contains("tmp-ok"),
        "the private temp is writable: {out}"
    );
    assert!(out.contains(r"\sbx-"), "TEMP is the private one: {out}");
    assert!(!out.contains(r"TEMP=\\?\"), "TEMP is a plain path: {out}");
}

#[test]
fn child_processes_are_confined_too() {
    let d = dirs();
    let via_cmd = d.out.join("via-cmd");
    let via_ps = d.out.join("via-ps");
    std::fs::write(
        d.ws.join("child.cmd"),
        format!("@echo bad> {}\r\n", via_cmd.display()),
    )
    .expect("child");
    std::fs::write(
        d.ws.join("parent.cmd"),
        format!(
            "@call child.cmd\r\n@cmd /d /c child.cmd\r\n\
             @powershell -NoProfile -NonInteractive -Command Set-Content -Path '{}' -Value x\r\n\
             @echo inside> inside.txt\r\n",
            via_ps.display()
        ),
    )
    .expect("parent");
    // A batch file by path: started through cmd.exe, under the token.
    let o = sbx(&["run", "-w", ".", "--", r".\parent.cmd"], &d.ws);
    assert!(d.ws.join("inside.txt").exists(), "{}", text(&o));
    assert!(!via_cmd.exists(), "a child cmd wrote outside: {}", text(&o));
    assert!(
        !via_ps.exists(),
        "a child PowerShell wrote outside: {}",
        text(&o)
    );
}

#[test]
fn the_current_folder_is_not_searched_for_the_program() {
    let d = dirs();
    std::fs::write(d.ws.join("planted.cmd"), "@echo planted\r\n").expect("plant");
    let o = sbx(&["run", "--", "planted"], &d.ws);
    assert_eq!(o.status.code(), Some(127), "{}", text(&o));
}

/// A script whose child records that it started, then, `delay` seconds
/// later, that it survived; the parent waits a minute.
fn tree_script(d: &Dirs, delay: u32) -> (PathBuf, PathBuf, PathBuf) {
    let started = d.ws.join("started.txt");
    let survived = d.ws.join("survived.txt");
    let script = d.ws.join("tree.ps1");
    std::fs::write(
        &script,
        format!(
            "Start-Process -NoNewWindow -FilePath powershell -ArgumentList \
             '-NoProfile','-NonInteractive','-Command',\
             'Set-Content -Path {s} -Value x; Start-Sleep {delay}; Set-Content -Path {v} -Value x'\r\n\
             Start-Sleep 60\r\n",
            s = started.display(),
            v = survived.display(),
        ),
    )
    .expect("script");
    (script, started, survived)
}

#[test]
fn the_wall_limit_kills_the_whole_tree() {
    let d = dirs();
    let (script, started, survived) = tree_script(&d, 12);
    let start = Instant::now();
    // No filesystem confinement: if anything survived, its write would land.
    let o = ps_file(&d.ws, &["--wall", "10"], &script);
    assert_eq!(o.status.code(), Some(124), "{}", text(&o));
    assert!(start.elapsed() < Duration::from_secs(40));
    assert!(started.exists(), "the child never started: {}", text(&o));
    std::thread::sleep(Duration::from_secs(15));
    assert!(!survived.exists(), "a process outlived the wall limit");
}

#[test]
fn killing_sbx_takes_the_command_with_it() {
    let d = dirs();
    let (script, started, survived) = tree_script(&d, 12);
    let mut child = base(&d.ws)
        .args([
            "run",
            "--",
            "powershell",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script.to_str().expect("utf8"),
        ])
        .spawn()
        .expect("sbx");
    let deadline = Instant::now() + Duration::from_secs(40);
    while !started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(started.exists(), "the command never started");
    child.kill().expect("TerminateProcess");
    let _ = child.wait();
    std::thread::sleep(Duration::from_secs(16));
    assert!(!survived.exists(), "the command outlived sbx");
}

#[test]
fn the_memory_limit_is_a_job_limit() {
    let d = dirs();
    let alloc =
        "try { $a = New-Object byte[] 1073741824; $a[0] = 1; 'allocated' } catch { 'refused' }";
    let control = ps(&d.ws, &[], alloc);
    if !text(&control).contains("allocated") {
        eprintln!(
            "SKIPPED: this machine cannot allocate 1 GB: {}",
            text(&control)
        );
        return;
    }
    let o = ps(&d.ws, &["--mem", "400"], alloc);
    assert!(
        !text(&o).contains("allocated"),
        "1 GB was allocated under a 400 MB limit: {}",
        text(&o)
    );
}

#[test]
fn the_cpu_limit_ends_a_busy_process() {
    let d = dirs();
    let start = Instant::now();
    let o = ps(&d.ws, &["--cpu", "2", "--wall", "90"], "while ($true) {}");
    assert_ne!(
        o.status.code(),
        Some(124),
        "the wall limit, not the CPU limit, ended it"
    );
    assert_ne!(o.status.code(), Some(0), "{}", text(&o));
    assert!(start.elapsed() < Duration::from_secs(60));
}

#[test]
fn the_agent_session_and_git_redirects_are_stripped_and_credentials_kept() {
    let d = dirs();
    let o = base(&d.ws)
        .args([
            "run",
            "--",
            "powershell",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "'[' + $env:CLAUDECODE + '][' + $env:CLAUDE_CODE_SESSION_ID + '][' + $env:GIT_DIR + '][' + $env:ANTHROPIC_API_KEY + ']'",
        ])
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_SESSION_ID", "parent")
        .env("GIT_DIR", r"C:\elsewhere\.git")
        .env("ANTHROPIC_API_KEY", "key")
        .output()
        .expect("sbx");
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        "[][][][key]",
        "{}",
        text(&o)
    );
}

#[test]
fn exit_codes() {
    let d = dirs();
    let o = cmd(&d.ws, &["--wall", "30"], "exit 7");
    assert_eq!(o.status.code(), Some(7), "{}", text(&o));
    // Windows codes are 32 bits; they pass through whole.
    let o = cmd(&d.ws, &[], "exit 300");
    assert_eq!(o.status.code(), Some(300), "{}", text(&o));
    let o = sbx(&["run", "--", "no-such-command-sbx"], &d.ws);
    assert_eq!(o.status.code(), Some(127), "{}", text(&o));
    let o = sbx(
        &["run", "-w", "missing-root", "--", "cmd", "/c", "exit 0"],
        &d.ws,
    );
    assert_eq!(o.status.code(), Some(125), "{}", text(&o));
}

#[test]
fn what_native_cannot_enforce_is_refused_and_said() {
    let d = dirs();
    std::fs::create_dir(d.out.join("secret")).expect("secret");
    let secret = d.out.join("secret");
    let o = sbx(
        &[
            "check",
            "--fs",
            "workspace-write",
            "--wall",
            "5",
            "--mem",
            "100",
        ],
        &d.ws,
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(text(&o).contains("write-restricted token"), "{}", text(&o));
    for flags in [
        vec!["--net", "deny"],
        vec!["--allow-host", "example.com"],
        vec!["-w", ".", "--deny-read", secret.to_str().expect("utf8")],
        vec!["--fsize", "1"],
    ] {
        let mut args = vec!["run"];
        args.extend(flags.iter().copied());
        args.extend(["--", "cmd", "/c", "exit 0"]);
        let o = sbx(&args, &d.ws);
        assert_eq!(o.status.code(), Some(125), "{flags:?}: {}", text(&o));
        assert!(text(&o).contains("NOT ENFORCED"), "{flags:?}: {}", text(&o));
    }
}

#[test]
fn auto_refuses_a_network_policy_when_wsl_cannot_take_it() {
    let d = dirs();
    let o = Command::new(SBX)
        .args(["run", "--net", "deny", "--", "cmd", "/c", "exit 0"])
        .current_dir(&d.ws)
        .env_remove("SBX_BACKEND")
        .env_remove("SBX_NATIVE")
        .stdin(Stdio::null())
        .output()
        .expect("sbx");
    if o.status.code() != Some(125) {
        eprintln!(
            "SKIPPED: this machine forwarded to WSL2 (code {:?})",
            o.status.code()
        );
        return;
    }
    let out = text(&o);
    assert!(out.contains("install-windows.ps1"), "{out}");
    assert!(out.contains("--best-effort"), "{out}");
    // And a policy native enforces in full runs natively under auto.
    let o = Command::new(SBX)
        .args([
            "run",
            "-w",
            ".",
            "--",
            "cmd",
            "/d",
            "/c",
            "echo ok> auto.txt",
        ])
        .current_dir(&d.ws)
        .env_remove("SBX_BACKEND")
        .env_remove("SBX_NATIVE")
        .stdin(Stdio::null())
        .output()
        .expect("sbx");
    assert!(d.ws.join("auto.txt").exists(), "{}", text(&o));
}

#[test]
fn doctor_reports_native_and_wsl() {
    let d = dirs();
    let o = sbx(&["doctor"], &d.ws);
    let out = text(&o);
    assert!(out.contains("write-restricted token"), "{out}");
    assert!(out.contains("WSL2"), "{out}");
    assert!(out.contains("deny_read"), "{out}");
}

// ---- sbx mcp, natively ----

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(args: &[&str]) -> Self {
        // No SBX_BACKEND: with `--net allow`, auto must pick native.
        let mut child = Command::new(SBX)
            .arg("mcp")
            .args(args)
            .env_remove("SBX_BACKEND")
            .env_remove("SBX_NATIVE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sbx mcp starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut m = Self {
            child,
            stdin,
            stdout,
            next: 0,
        };
        let init = m.request("initialize", json!({"protocolVersion": "2025-06-18"}));
        assert_eq!(init["serverInfo"]["name"], "sbx");
        writeln!(
            m.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .expect("write");
        m
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").expect("write");
        self.stdin.flush().expect("flush");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        let reply: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(reply["id"], self.next, "{reply}");
        reply["result"].clone()
    }

    fn call(&mut self, tool: &str, args: Value) -> Result<Value, String> {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        if r["isError"] == true {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).expect("tool result is JSON"))
        }
    }

    fn stop(mut self) {
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_runs_a_script_natively_and_keeps_it_in_its_folder() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut m = Mcp::start(&[
        "--root",
        root.path().to_str().expect("utf8"),
        "--net",
        "allow",
    ]);

    // The default (deny) cannot be enforced natively: said at once.
    let err = m
        .call("sandbox_create", json!({}))
        .expect_err("deny refused");
    assert!(err.contains("cannot be enforced"), "{err}");
    let created = m
        .call("sandbox_create", json!({"network": "allow"}))
        .expect("create");
    let id = created["id"].as_str().expect("id").to_string();
    let folder = PathBuf::from(created["folder"].as_str().expect("folder"));

    m.call(
        "sandbox_write_file",
        json!({"id": id, "path": "tools/sum.ps1", "content": "param($a, $b)\r\nNew-Item -ItemType Directory -Force out | Out-Null\r\nSet-Content -Path out/result.txt -Value ([int]$a + [int]$b)\r\n'done'\r\n"}),
    )
    .expect("write");
    let ran = m
        .call(
            "sandbox_run",
            json!({"id": id, "command": ["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", "tools/sum.ps1", "2", "40"]}),
        )
        .expect("run");
    assert_eq!(ran["exit_code"], 0, "{ran}");
    assert!(
        ran["stdout"].as_str().unwrap_or("").contains("done"),
        "{ran}"
    );
    let read = m
        .call(
            "sandbox_read_file",
            json!({"id": id, "path": "out/result.txt"}),
        )
        .expect("read");
    assert_eq!(
        read["content"].as_str().map(str::trim),
        Some("42"),
        "{read}"
    );

    // A script (PowerShell by default here) cannot write beside its folder.
    let escaped = folder.parent().expect("root").join("escaped.txt");
    let ran = m
        .call(
            "sandbox_run",
            json!({"id": id, "script": format!("Set-Content -Path '{}' -Value x\r\n'after'", escaped.display())}),
        )
        .expect("run");
    assert!(!escaped.exists(), "wrote outside its sandbox: {ran}");
    assert!(
        !icacls(&folder).contains(SID_MARK),
        "the grant outlived the run"
    );
    m.stop();
}
