//! Escape tests against the native Windows sandbox (write-restricted token,
//! ACL grant, job object): the Linux escape tests that apply on Windows,
//! through the real `sbx.exe`.
//!
//! They use only what a stock Windows (and the `windows-latest` CI runner)
//! has: `cmd.exe` and Windows PowerShell. Scripts are written to files
//! rather than passed inline, so no test depends on how `cmd` re-parses
//! quotes. Every run is native (`SBX_BACKEND=native`) unless the test is
//! about choosing the backend.
//!
//! The network tests (layer 2: the offline account and its WFP filters) run
//! only where `sbx setup-windows` has been run (CI runs it before the
//! tests); elsewhere they print SKIPPED. They reach the network with
//! `curl.exe` (in System32 since Windows 10 1803) and PowerShell.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SBX: &str = env!("CARGO_BIN_EXE_sbx");
/// Every run SID starts with this (`incord_sandbox::windows::SID_PREFIX`).
const SID_MARK: &str = "4242424242";
/// The offline account (`incord_sandbox::windows::offline::ACCOUNT`).
const OFFLINE: &str = "sbx-offline";

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

/// Whether `sbx setup-windows` has run here: `check --net deny` passes.
fn offline_ready() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        let d = std::env::temp_dir();
        sbx(&["check", "--net", "deny"], &d).status.code() == Some(0)
    })
}

/// The layer-2 tests' gate: true to go on, else says it skipped.
fn needs_offline(test: &str) -> bool {
    if offline_ready() {
        return true;
    }
    eprintln!("SKIPPED {test}: `sbx setup-windows` has not been run on this machine");
    false
}

fn curl() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Some(PathBuf::from(root).join(r"System32\curl.exe")).filter(|p| p.is_file())
}

/// A local HTTP server answering every request with `body`, until the test
/// ends; its port.
fn http_server(body: &'static str) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("binds");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

/// `sbx run <flags> -- curl.exe <args>`: what it printed and its exit code.
fn curl_in(cwd: &Path, flags: &[&str], args: &[&str]) -> (String, Option<i32>) {
    let curl = curl().expect("curl.exe");
    let mut all = vec!["run"];
    all.extend_from_slice(flags);
    all.push("--");
    all.push(curl.to_str().expect("utf8"));
    all.extend_from_slice(&["-s", "--max-time", "8"]);
    all.extend_from_slice(args);
    let o = sbx(&all, cwd);
    (text(&o), o.status.code())
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
    let mut refused = vec![
        vec!["-w", ".", "--deny-read", secret.to_str().expect("utf8")],
        vec!["--fsize", "1"],
    ];
    if offline_ready() {
        // Layer 2 is set up: the network is enforced, and says how.
        let o = sbx(&["check", "--net", "deny"], &d.ws);
        assert!(text(&o).contains(OFFLINE), "{}", text(&o));
    } else {
        refused.push(vec!["--net", "deny"]);
        refused.push(vec!["--allow-host", "example.com"]);
        let o = sbx(&["check", "--net", "deny"], &d.ws);
        assert!(text(&o).contains("sbx setup-windows"), "{}", text(&o));
    }
    for flags in refused {
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
    if offline_ready() {
        // With layer 2 set up, auto runs network deny natively.
        let o = Command::new(SBX)
            .args([
                "run",
                "--net",
                "deny",
                "-w",
                ".",
                "--",
                "cmd",
                "/d",
                "/c",
                "echo ok> auto-deny.txt",
            ])
            .current_dir(&d.ws)
            .env_remove("SBX_BACKEND")
            .env_remove("SBX_NATIVE")
            .stdin(Stdio::null())
            .output()
            .expect("sbx");
        assert_eq!(o.status.code(), Some(0), "{}", text(&o));
        assert!(d.ws.join("auto-deny.txt").exists(), "{}", text(&o));
        return;
    }
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
    assert!(out.contains("offline user"), "{out}");
    if offline_ready() {
        assert!(out.contains("[ok  ] offline user"), "{out}");
        assert!(out.contains("Windows Filtering Platform"), "{out}");
    } else {
        assert!(out.contains("sbx setup-windows"), "{out}");
    }
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

    // The default (deny) is enforced natively only after `sbx
    // setup-windows`; without it that is said at once.
    let deny = m.call("sandbox_create", json!({}));
    if offline_ready() {
        assert!(deny.is_ok(), "{deny:?}");
    } else {
        let err = deny.expect_err("deny refused");
        assert!(err.contains("cannot be enforced"), "{err}");
    }
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

    // With layer 2, a deny sandbox's script runs (as the offline account)
    // and cannot reach a local server.
    if offline_ready() {
        let port = http_server("mcp-reached");
        let created = m.call("sandbox_create", json!({})).expect("create");
        let id = created["id"].as_str().expect("id").to_string();
        let ran = m
            .call(
                "sandbox_run",
                json!({"id": id, "script": format!(
                    "Set-Content -Path mine.txt -Value x\r\n\
                     $c = New-Object Net.Sockets.TcpClient\r\n\
                     try {{ $c.Connect('127.0.0.1', {port}); 'CONNECTED' }} catch {{ 'REFUSED' }}\r\n\
                     Get-Content mine.txt\r\n"
                )}),
            )
            .expect("run");
        let out = ran["stdout"].as_str().unwrap_or("");
        assert!(out.contains("REFUSED"), "{ran}");
        assert!(!out.contains("CONNECTED"), "{ran}");
        assert!(out.contains('x'), "it wrote in its own folder: {ran}");
    }
    m.stop();
}

// ---- Layer 2: network deny natively (after `sbx setup-windows`) ----

#[test]
fn network_deny_refuses_tcp_to_a_local_server_and_to_the_internet() {
    if !needs_offline("network_deny_refuses_tcp") || curl().is_none() {
        return;
    }
    let d = dirs();
    let port = http_server("listener-ok");
    let local = format!("http://127.0.0.1:{port}/");
    // The control: without deny, the same command reaches it.
    let (out, code) = curl_in(&d.ws, &["-w", "."], &["--noproxy", "*", &local]);
    assert!(out.contains("listener-ok"), "control: {code:?} {out}");
    // Under deny, with the filesystem confined (the write-restricted token
    // under the offline account), read-only, and not confined at all.
    for flags in [
        &["--net", "deny", "-w", "."][..],
        &["--net", "deny", "--fs", "read-only"][..],
        &["--net", "deny"][..],
    ] {
        let (out, code) = curl_in(&d.ws, flags, &["--noproxy", "*", &local]);
        assert!(!out.contains("listener-ok"), "{flags:?} reached it: {out}");
        assert_ne!(code, Some(0), "{flags:?}: {out}");
        assert_ne!(code, Some(125), "{flags:?}: sbx refused instead: {out}");
    }
    // Outside, by address (no DNS needed): only if the control gets there.
    let outside = "http://1.1.1.1/";
    let (_, code) = curl_in(
        &d.ws,
        &["-w", "."],
        &["--noproxy", "*", "-o", "NUL", outside],
    );
    if code == Some(0) {
        let (out, code) = curl_in(
            &d.ws,
            &["--net", "deny", "-w", "."],
            &["--noproxy", "*", "-o", "NUL", outside],
        );
        assert_ne!(code, Some(0), "reached {outside} under deny: {out}");
    } else {
        eprintln!("SKIPPED the outside half: {outside} is not reachable from here ({code:?})");
    }
}

#[test]
fn network_deny_refuses_udp() {
    if !needs_offline("network_deny_refuses_udp") {
        return;
    }
    let d = dirs();
    let socket = UdpSocket::bind(("127.0.0.1", 0)).expect("binds");
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let port = socket.local_addr().expect("addr").port();
    let script = d.ws.join("udp.ps1");
    std::fs::write(
        &script,
        format!(
            "$u = New-Object Net.Sockets.UdpClient\r\n\
             $b = [Text.Encoding]::ASCII.GetBytes('ping')\r\n\
             try {{ [void]$u.Send($b, $b.Length, '127.0.0.1', {port}); 'SENT' }} catch {{ 'REFUSED' }}\r\n"
        ),
    )
    .expect("script");
    let received = |socket: &UdpSocket| {
        let mut buf = [0u8; 16];
        socket.recv(&mut buf).ok().map(|n| buf[..n].to_vec())
    };
    let o = ps_file(&d.ws, &["-w", "."], &script);
    assert_eq!(
        received(&socket).as_deref(),
        Some(&b"ping"[..]),
        "control: {}",
        text(&o)
    );
    let o = ps_file(&d.ws, &["--net", "deny", "-w", "."], &script);
    assert_eq!(received(&socket), None, "a datagram got out: {}", text(&o));
    assert_eq!(o.status.code(), Some(0), "PowerShell ran: {}", text(&o));
}

/// A file one network-deny run creates is owned by the offline account; the
/// next run must still write it, as a second `cargo test` rewrites the build
/// cache the first one made. It failed with "Access is denied" before the
/// user kept `READ_CONTROL | WRITE_DAC` beneath the root, because the second
/// run's grant could not be propagated onto a file the user does not own.
#[test]
fn a_second_network_deny_run_writes_what_the_first_created() {
    if !needs_offline("a_second_network_deny_run_writes_what_the_first_created") {
        return;
    }
    let d = dirs();
    // The ACL a folder on a data drive (`D:\Projects\...`) inherits, not the
    // profile's: there the user's access is `Authenticated Users`' Modify,
    // with no `WRITE_DAC` on a file someone else owns. Under the profile
    // (the temp folder) an inherited full-control ACE for the user hid the
    // failure.
    let set = Command::new("icacls")
        .arg(&d.ws)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-11:(OI)(CI)M",
            "*S-1-5-32-545:(OI)(CI)RX",
        ])
        .output()
        .expect("icacls");
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stdout)
    );
    let flags = ["--net", "deny", "-w", "."];
    let file = d.ws.join(r"cache\f.txt");
    let first = cmd(&d.ws, &flags, r"mkdir cache & echo one> cache\f.txt");
    let read = || std::fs::read_to_string(&file).unwrap_or_default();
    assert_eq!(read().trim(), "one", "{}", text(&first));
    // The case that failed: the file is the account's, not the user's.
    let owner = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("(Get-Acl -LiteralPath '{}').Owner", file.display()))
        .output()
        .expect("powershell");
    assert!(
        String::from_utf8_lossy(&owner.stdout).contains(OFFLINE),
        "the first run's file should be the offline account's: {}",
        String::from_utf8_lossy(&owner.stdout)
    );

    let second = cmd(&d.ws, &flags, r"echo two> cache\f.txt");
    assert_eq!(
        read().trim(),
        "two",
        "the second run could not write the first run's file: {}\n{}",
        text(&second),
        icacls(&file)
    );
}

#[test]
fn under_network_deny_writes_stay_confined_and_exit_codes_pass_through() {
    if !needs_offline("under_network_deny_writes_stay_confined") {
        return;
    }
    let d = dirs();
    let outside = d.out.join("x");
    let o = cmd(
        &d.ws,
        &["--net", "deny", "-w", "."],
        &format!(
            "echo ok> inside & echo bad> {} & echo tmp> %TEMP%\\t.txt & type %TEMP%\\t.txt",
            outside.display()
        ),
    );
    assert!(d.ws.join("inside").exists(), "{}", text(&o));
    assert!(!outside.exists(), "wrote outside the root: {}", text(&o));
    assert!(
        text(&o).contains("tmp"),
        "its temp is writable: {}",
        text(&o)
    );
    // Read-only: nothing in the folder, even as another account.
    let o = cmd(
        &d.ws,
        &["--net", "deny", "--fs", "read-only"],
        "echo x> ro.txt",
    );
    assert!(!d.ws.join("ro.txt").exists(), "{}", text(&o));
    // Exit codes, whole, through both helpers.
    let o = cmd(&d.ws, &["--net", "deny", "--wall", "60"], "exit 7");
    assert_eq!(o.status.code(), Some(7), "{}", text(&o));
    let o = cmd(&d.ws, &["--net", "deny"], "exit 300");
    assert_eq!(o.status.code(), Some(300), "{}", text(&o));
    let o = sbx(
        &["run", "--net", "deny", "--", "no-such-command-sbx"],
        &d.ws,
    );
    assert_eq!(o.status.code(), Some(127), "{}", text(&o));
}

#[test]
fn the_offline_accounts_grant_lasts_the_run_and_no_longer() {
    if !needs_offline("the_offline_accounts_grant_lasts_the_run") {
        return;
    }
    let d = dirs();
    let o = cmd(&d.ws, &["--net", "deny", "-w", "."], "icacls . & whoami");
    let during = text(&o);
    assert!(during.contains(SID_MARK), "{during}");
    assert!(during.contains(OFFLINE), "the account's ACE: {during}");
    assert!(
        during
            .to_ascii_lowercase()
            .contains(&format!("\\{OFFLINE}")),
        "whoami names the offline account: {during}"
    );
    let after = icacls(&d.ws);
    assert!(!after.contains(SID_MARK), "{after}");
    assert!(
        !after.contains(OFFLINE),
        "the account's ACE outlived the run: {after}"
    );
}

#[test]
fn concurrent_deny_runs_share_a_root_and_cannot_write_each_others() {
    if !needs_offline("concurrent_deny_runs") {
        return;
    }
    let d = dirs();
    let other = d.out.clone();
    // A: the workspace and `out`, writes once more after B has ended.
    let script = d.ws.join("slow.ps1");
    std::fs::write(
        &script,
        "Set-Content -Path started.txt -Value x\r\n\
         $deadline = (Get-Date).AddSeconds(60)\r\n\
         while (-not (Test-Path b-done.txt) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }\r\n\
         Start-Sleep 2\r\n\
         Set-Content -Path a-late.txt -Value x\r\n",
    )
    .expect("script");
    let a = base(&d.ws)
        .args([
            "run",
            "--net",
            "deny",
            "-w",
            ".",
            "-w",
            other.to_str().expect("utf8"),
            "--",
            "powershell",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script.to_str().expect("utf8"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sbx");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !d.ws.join("started.txt").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // B: the same workspace only. `out` carries the account's ACE (A's),
    // but not B's run SID: B cannot write there.
    let o = cmd(
        &d.ws,
        &["--net", "deny", "-w", "."],
        &format!(
            "echo b> b-done.txt & echo bad> {}",
            other.join("from-b").display()
        ),
    );
    assert!(d.ws.join("b-done.txt").exists(), "{}", text(&o));
    assert!(
        !other.join("from-b").exists(),
        "B wrote in A's root: {}",
        text(&o)
    );
    // A, after B ended (and revoked what B alone needed), still writes.
    let a = a.wait_with_output().expect("A");
    assert!(
        d.ws.join("a-late.txt").exists(),
        "A lost its grant when B ended: {}",
        text(&a)
    );
    assert!(!icacls(&d.ws).contains(OFFLINE), "{}", icacls(&d.ws));
}

/// The account's ACE lines in `icacls` output.
fn offline_aces(icacls: &str) -> Vec<String> {
    icacls
        .lines()
        .filter(|l| l.contains(OFFLINE))
        .map(str::to_string)
        .collect()
}

#[test]
fn powershell_under_network_deny_starts_in_the_current_folder() {
    // Windows PowerShell sets its first location by looking every part of
    // the current folder up in the folder above; as the offline account,
    // with no right to list folders under the user's profile, it was left at
    // `C:\` and relative paths went there. The run lets the account list
    // each folder above (up to `C:\Users`), by one ACE per folder that
    // reaches nothing beneath it, for the run only.
    if !needs_offline("powershell_under_network_deny_starts_in_the_current_folder") {
        return;
    }
    let d = dirs();
    let parent = d.ws.parent().expect("parent").to_path_buf();
    let sibling = parent.join("sibling.txt");
    std::fs::write(&sibling, "x").expect("sibling");
    let child = base(&d.ws)
        .args([
            "run",
            "--net",
            "deny",
            "-w",
            ".",
            "--",
            "powershell",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-Location).Path; [Environment]::CurrentDirectory; \
             try { Set-Location -LiteralPath ([Environment]::CurrentDirectory) -ErrorAction Stop; \
             Write-Error ('SETLOC-OK ' + (Get-Location).Path) } catch { Write-Error ('SETLOC-ERR ' + \
             $_.Exception.GetType().FullName + ': ' + $_.Exception.Message + ' | ' + \
             $_.Exception.InnerException) }; \
             Set-Location -LiteralPath ([Environment]::CurrentDirectory) -ErrorAction SilentlyContinue; \
             Set-Content -Path rel.txt -Value x; \
             $deadline = (Get-Date).AddSeconds(60); \
             while (-not (Test-Path go.txt) -and (Get-Date) -lt $deadline) \
             { Start-Sleep -Milliseconds 200 }",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sbx");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !d.ws.join("rel.txt").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // During the run: the parent carries the account's ACE, explicit and
    // inherited by nothing; nothing beside the workspace got one.
    let during = icacls(&parent);
    let aces = offline_aces(&during);
    let on_parent = aces.len();
    let not_inherited = aces
        .iter()
        .all(|l| !l.contains("(OI)") && !l.contains("(CI)") && !l.contains("(I)"));
    let sibling_during = icacls(&sibling);
    let out_during = icacls(&d.out);
    std::fs::write(d.ws.join("go.txt"), "go").expect("go");
    let o = child.wait_with_output().expect("sbx ends");
    let printed = text(&o);

    let ws = d.ws.to_string_lossy().to_lowercase();
    let lines: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect();
    // What stands in PowerShell's way, when it does: each folder above the
    // workspace as the ACLs say, beside the error Set-Location itself gives.
    let mut chain = String::new();
    let mut up = d.ws.parent();
    while let Some(dir) = up {
        chain.push_str(&icacls(dir));
        chain.push('\n');
        up = dir.parent();
    }
    assert_eq!(
        lines.first(),
        Some(&ws),
        "Get-Location is the workspace: {printed}\n-- ACEs during the run on the parent:\n{during}\n-- the chain after:\n{chain}"
    );
    assert_eq!(
        lines.get(1),
        Some(&ws),
        "the process's current directory is the workspace: {printed}"
    );
    assert!(
        d.ws.join("rel.txt").exists(),
        "a relative path lands in the workspace: {printed}"
    );
    assert_eq!(on_parent, 1, "one ACE on the parent: {during}");
    assert!(not_inherited, "the parent's ACE inherits: {during}");
    assert!(
        offline_aces(&sibling_during).is_empty(),
        "a file beside the workspace got the account's ACE: {sibling_during}"
    );
    assert!(
        offline_aces(&out_during).is_empty(),
        "a folder beside the workspace got the account's ACE: {out_during}"
    );
    // After the run, the parent's ACE is gone, and nothing was left beside.
    let after = icacls(&parent);
    assert!(
        offline_aces(&after).is_empty(),
        "the parent's ACE outlived the run: {after}"
    );
    assert!(offline_aces(&icacls(&sibling)).is_empty());
    assert!(offline_aces(&icacls(&d.out)).is_empty());
    assert!(offline_aces(&icacls(&d.ws)).is_empty());
}

#[test]
fn proxy_lets_only_the_allowlist_through_natively() {
    if !needs_offline("proxy_lets_only_the_allowlist_through") || curl().is_none() {
        return;
    }
    let d = dirs();
    let allowed = http_server("origin-ok");
    let other = http_server("other-ok");
    let script = d.ws.join("proxy.cmd");
    let curl = curl().expect("curl");
    let curl = curl.display();
    std::fs::write(
        &script,
        format!(
            "@echo PROXY=%HTTP_PROXY%\r\n\
             @echo VIA:\r\n\
             @\"{curl}\" -s --max-time 8 -x %HTTP_PROXY% http://127.0.0.1:{allowed}/\r\n\
             @echo.\r\n\
             @echo NOT-ALLOWED:\r\n\
             @\"{curl}\" -s --max-time 8 -x %HTTP_PROXY% http://127.0.0.1:{other}/\r\n\
             @echo.\r\n\
             @echo DIRECT:\r\n\
             @\"{curl}\" -s --max-time 8 --noproxy * http://127.0.0.1:{allowed}/\r\n\
             @echo DIRECT-EXIT=%ERRORLEVEL%\r\n"
        ),
    )
    .expect("script");
    let host = format!("127.0.0.1:{allowed}");
    let o = sbx(
        &[
            "run",
            "-w",
            ".",
            "--allow-host",
            &host,
            "--allow-private",
            "--",
            script.to_str().expect("utf8"),
        ],
        &d.ws,
    );
    let out = text(&o);
    assert!(out.contains("PROXY=http://127.0.0.1:"), "{out}");
    assert!(
        out.contains("DIRECT-EXIT=") && !out.contains("DIRECT-EXIT=0"),
        "a direct connection got through: {out}"
    );
    // So the one answer from the allowed server came through the proxy.
    assert_eq!(out.matches("origin-ok").count(), 1, "{out}");
    assert!(
        !out.contains("other-ok"),
        "a host not on the allowlist was reached: {out}"
    );
    assert!(
        out.contains("not allowed") || out.contains("403") || out.contains("NOT-ALLOWED:"),
        "{out}"
    );
}
