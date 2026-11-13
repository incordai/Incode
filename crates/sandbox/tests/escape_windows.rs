//! Escape tests against the native Windows sandbox (write-restricted token,
//! ACL grant, job object): the Linux escape tests that apply on Windows,
//! through the real `insbx.exe`.
//!
//! They use only what a stock Windows (and the `windows-latest` CI runner)
//! has: `cmd.exe` and Windows PowerShell. Scripts are written to files
//! rather than passed inline, so no test depends on how `cmd` re-parses
//! quotes. Every run is native (`INSBX_BACKEND=native`) unless the test is
//! about choosing the backend.
//!
//! The network tests (layer 2: the offline account and its WFP filters) run
//! only where `insbx setup-windows` has been run (CI runs it before the
//! tests); elsewhere they print SKIPPED. They reach the network with
//! `curl.exe` (in System32 since Windows 10 1803) and PowerShell.

#![cfg(windows)]

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const INSBX: &str = env!("CARGO_BIN_EXE_insbx");
/// Every run SID starts with this (`incord_sandbox::windows::SID_PREFIX`).
const SID_MARK: &str = "4242424242";
/// Every root's kept SID has this (`incord_sandbox::windows::ROOT_SID_PREFIX`).
const KEEP_MARK: &str = "4141414141";
/// The offline account (`incord_sandbox::windows::offline::ACCOUNT`).
const OFFLINE: &str = "insbx-offline";

fn base(cwd: &Path) -> Command {
    let mut c = Command::new(INSBX);
    c.current_dir(cwd)
        .env("INSBX_BACKEND", "native")
        .env_remove("INSBX_NATIVE")
        .stdin(Stdio::null());
    c
}

fn insbx(args: &[&str], cwd: &Path) -> Output {
    base(cwd).args(args).output().expect("insbx runs")
}

/// `insbx run <flags> -- cmd /d /s /c <line>`.
fn cmd(cwd: &Path, flags: &[&str], line: &str) -> Output {
    let mut args = vec!["run"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&["--", "cmd", "/d", "/s", "/c", line]);
    insbx(&args, cwd)
}

/// `insbx run <flags> -- powershell ... -File <script>`.
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
    insbx(&args, cwd)
}

/// `insbx run <flags> -- powershell ... -Command <command>`.
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
    insbx(&args, cwd)
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

/// Whether `insbx setup-windows` has run here: `check --net deny` passes.
fn offline_ready() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| {
        let d = std::env::temp_dir();
        insbx(&["check", "--net", "deny"], &d).status.code() == Some(0)
    })
}

/// The layer-2 tests' gate: true to go on, else says it skipped.
fn needs_offline(test: &str) -> bool {
    if offline_ready() {
        return true;
    }
    eprintln!("SKIPPED {test}: `insbx setup-windows` has not been run on this machine");
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

/// `insbx run <flags> -- curl.exe <args>`: what it printed and its exit code.
fn curl_in(cwd: &Path, flags: &[&str], args: &[&str]) -> (String, Option<i32>) {
    let curl = curl().expect("curl.exe");
    let mut all = vec!["run"];
    all.extend_from_slice(flags);
    all.push("--");
    all.push(curl.to_str().expect("utf8"));
    all.extend_from_slice(&["-s", "--max-time", "8"]);
    all.extend_from_slice(args);
    let o = insbx(&all, cwd);
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

/// The root's grant is its own SID's, kept after the run (later runs only
/// check it); the temp directory's is the run's SID, gone with the run.
/// `insbx grants` lists the root and `insbx revoke` takes the grant back.
#[test]
fn the_roots_grant_is_kept_the_temp_grant_is_not_and_revoke_takes_it_back() {
    let d = dirs();
    let o = cmd(&d.ws, &["-w", "."], "icacls . & icacls %TEMP%");
    let during = text(&o);
    assert!(during.contains(KEEP_MARK), "the root's own ACE: {during}");
    assert!(during.contains(SID_MARK), "the temp's run ACE: {during}");
    let after = icacls(&d.ws);
    assert!(after.contains(KEEP_MARK), "kept after the run: {after}");
    assert!(!after.contains(SID_MARK), "no run SID on the root: {after}");

    let ws = d.ws.display().to_string().to_ascii_lowercase();
    let listed = text(&insbx(&["grants"], &d.ws)).to_ascii_lowercase();
    assert!(
        listed
            .lines()
            .any(|l| l.starts_with("granted") && l.ends_with(&ws)),
        "{listed}"
    );
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let revoked = icacls(&d.ws);
    assert!(!revoked.contains(KEEP_MARK), "{revoked}");
    let listed = text(&insbx(&["grants"], &d.ws)).to_ascii_lowercase();
    assert!(!listed.contains(&ws), "{listed}");
}

/// A kept grant lets no other run write there: a run whose roots do not
/// include the folder holds no SID its ACE names.
#[test]
fn a_kept_grant_is_writable_only_to_runs_that_name_the_folder() {
    let d = dirs();
    let o = cmd(&d.out, &["-w", "."], "echo a> first.txt");
    assert!(d.out.join("first.txt").exists(), "{}", text(&o));
    assert!(icacls(&d.out).contains(KEEP_MARK), "kept on out");
    let target = d.out.join("from-ws.txt");
    let o = cmd(
        &d.ws,
        &["-w", "."],
        &format!("echo bad> {} & echo ok> mine.txt", target.display()),
    );
    assert!(d.ws.join("mine.txt").exists(), "{}", text(&o));
    assert!(
        !target.exists(),
        "a run for ws wrote in out's kept grant: {}",
        text(&o)
    );
    // Control: naming out again, it writes.
    let o = cmd(&d.out, &["-w", "."], "echo b> second.txt");
    assert!(d.out.join("second.txt").exists(), "{}", text(&o));
    for dir in [&d.ws, &d.out] {
        insbx(&["revoke", dir.to_str().expect("utf8")], dir);
    }
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
    assert!(out.contains(r"\insbx-"), "TEMP is the private one: {out}");
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
    let o = insbx(&["run", "-w", ".", "--", r".\parent.cmd"], &d.ws);
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
    let o = insbx(&["run", "--", "planted"], &d.ws);
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
fn killing_insbx_takes_the_command_with_it() {
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
        .expect("insbx");
    let deadline = Instant::now() + Duration::from_secs(40);
    while !started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(started.exists(), "the command never started");
    child.kill().expect("TerminateProcess");
    let _ = child.wait();
    std::thread::sleep(Duration::from_secs(16));
    assert!(!survived.exists(), "the command outlived insbx");
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
        .expect("insbx");
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
    let o = insbx(&["run", "--", "no-such-command-insbx"], &d.ws);
    assert_eq!(o.status.code(), Some(127), "{}", text(&o));
    let o = insbx(
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
    let o = insbx(
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
        let o = insbx(&["check", "--net", "deny"], &d.ws);
        assert!(text(&o).contains(OFFLINE), "{}", text(&o));
    } else {
        refused.push(vec!["--net", "deny"]);
        refused.push(vec!["--allow-host", "example.com"]);
        let o = insbx(&["check", "--net", "deny"], &d.ws);
        assert!(text(&o).contains("insbx setup-windows"), "{}", text(&o));
    }
    for flags in refused {
        let mut args = vec!["run"];
        args.extend(flags.iter().copied());
        args.extend(["--", "cmd", "/c", "exit 0"]);
        let o = insbx(&args, &d.ws);
        assert_eq!(o.status.code(), Some(125), "{flags:?}: {}", text(&o));
        assert!(text(&o).contains("NOT ENFORCED"), "{flags:?}: {}", text(&o));
    }
}

#[test]
fn auto_refuses_a_network_policy_when_wsl_cannot_take_it() {
    let d = dirs();
    if offline_ready() {
        // With layer 2 set up, auto runs network deny natively.
        let o = Command::new(INSBX)
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
            .env_remove("INSBX_BACKEND")
            .env_remove("INSBX_NATIVE")
            .stdin(Stdio::null())
            .output()
            .expect("insbx");
        assert_eq!(o.status.code(), Some(0), "{}", text(&o));
        assert!(d.ws.join("auto-deny.txt").exists(), "{}", text(&o));
        return;
    }
    let o = Command::new(INSBX)
        .args(["run", "--net", "deny", "--", "cmd", "/c", "exit 0"])
        .current_dir(&d.ws)
        .env_remove("INSBX_BACKEND")
        .env_remove("INSBX_NATIVE")
        .stdin(Stdio::null())
        .output()
        .expect("insbx");
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
    let o = Command::new(INSBX)
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
        .env_remove("INSBX_BACKEND")
        .env_remove("INSBX_NATIVE")
        .stdin(Stdio::null())
        .output()
        .expect("insbx");
    assert!(d.ws.join("auto.txt").exists(), "{}", text(&o));
}

#[test]
fn doctor_reports_native_and_wsl() {
    let d = dirs();
    let o = insbx(&["doctor"], &d.ws);
    let out = text(&o);
    assert!(out.contains("write-restricted token"), "{out}");
    assert!(out.contains("WSL2"), "{out}");
    assert!(out.contains("deny_read"), "{out}");
    assert!(out.contains("offline user"), "{out}");
    if offline_ready() {
        assert!(out.contains("[ok  ] offline user"), "{out}");
        assert!(out.contains("Windows Filtering Platform"), "{out}");
    } else {
        assert!(out.contains("insbx setup-windows"), "{out}");
    }
}

// ---- insbx mcp, natively ----

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(args: &[&str]) -> Self {
        // No INSBX_BACKEND: with `--net allow`, auto must pick native.
        let mut child = Command::new(INSBX)
            .arg("mcp")
            .args(args)
            .env_remove("INSBX_BACKEND")
            .env_remove("INSBX_NATIVE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("insbx mcp starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut m = Self {
            child,
            stdin,
            stdout,
            next: 0,
        };
        let init = m.request("initialize", json!({"protocolVersion": "2025-06-18"}));
        assert_eq!(init["serverInfo"]["name"], "insbx");
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

    // The default (deny) is enforced natively only after `insbx
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

// ---- Layer 2: network deny natively (after `insbx setup-windows`) ----

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
        assert_ne!(code, Some(125), "{flags:?}: insbx refused instead: {out}");
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
    // None of the user's own: an elevated process (a CI runner) gives a new
    // folder an explicit full-control ACE for its user, which
    // `/inheritance:r` does not take off and which would hide the case.
    let me = Command::new("whoami").output().expect("whoami");
    let me = String::from_utf8_lossy(&me.stdout).trim().to_string();
    let removed = Command::new("icacls")
        .arg(&d.ws)
        .args(["/remove:g", &me])
        .output()
        .expect("icacls");
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stdout)
    );
    let flags = ["--net", "deny", "-w", "."];
    let file = d.ws.join(r"cache\f.txt");
    let first = cmd(&d.ws, &flags, r"mkdir cache & echo one> cache\f.txt");
    let read = || std::fs::read_to_string(&file).unwrap_or_default();
    assert_eq!(read().trim(), "one", "{}", text(&first));
    // The case that failed: the file is the account's, not the user's.
    // Windows PowerShell started from a PowerShell 7 shell (a CI step)
    // inherits 7's module path, cannot load `Get-Acl`'s module and prints
    // nothing; without the variable it uses its own.
    let owner = Command::new("powershell")
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("(Get-Acl -LiteralPath '{}').Owner", file.display()))
        .output()
        .expect("powershell");
    assert!(
        String::from_utf8_lossy(&owner.stdout).contains(OFFLINE),
        "the first run's file should be the offline account's: {} {}",
        String::from_utf8_lossy(&owner.stdout),
        String::from_utf8_lossy(&owner.stderr)
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
    let o = insbx(
        &["run", "--net", "deny", "--", "no-such-command-insbx"],
        &d.ws,
    );
    assert_eq!(o.status.code(), Some(127), "{}", text(&o));
}

/// Under network deny the account's ACE on a root is kept with the root's
/// own, and `insbx revoke` takes both back.
#[test]
fn the_offline_accounts_grant_on_a_root_is_kept_until_revoked() {
    if !needs_offline("the_offline_accounts_grant_on_a_root_is_kept") {
        return;
    }
    let d = dirs();
    let o = cmd(&d.ws, &["--net", "deny", "-w", "."], "icacls . & whoami");
    let during = text(&o);
    assert!(during.contains(KEEP_MARK), "{during}");
    assert!(during.contains(OFFLINE), "the account's ACE: {during}");
    assert!(
        during
            .to_ascii_lowercase()
            .contains(&format!("\\{OFFLINE}")),
        "whoami names the offline account: {during}"
    );
    let after = icacls(&d.ws);
    assert!(after.contains(KEEP_MARK), "{after}");
    assert!(after.contains(OFFLINE), "kept: {after}");
    assert!(!after.contains(SID_MARK), "{after}");
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let revoked = icacls(&d.ws);
    assert!(!revoked.contains(KEEP_MARK), "{revoked}");
    assert!(!revoked.contains(OFFLINE), "{revoked}");
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
        .expect("insbx");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !d.ws.join("started.txt").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    // B: the same workspace only. `out` carries the account's ACE and out's
    // own kept SID, which B's token does not hold: B cannot write there.
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
    for dir in [&d.ws, &other] {
        let o = insbx(&["revoke", dir.to_str().expect("utf8")], &d.ws);
        assert_eq!(o.status.code(), Some(0), "{}", text(&o));
        assert!(!icacls(dir).contains(OFFLINE), "{}", icacls(dir));
    }
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
        .expect("insbx");
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
    let o = child.wait_with_output().expect("insbx ends");
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
    // The workspace is the run's root: its grant, the account's included,
    // is kept until revoked.
    assert!(!offline_aces(&icacls(&d.ws)).is_empty(), "kept on the root");
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
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
    let o = insbx(
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

/// **A PLAIN `--net deny` RUN STILL STARTS** after the second helper locks its
/// own process and threads: the first helper locks that helper's main thread
/// before it runs, and the helper then locks every thread of its own, the
/// current one through the pseudo-handle (a locked thread denies even its
/// owner `WRITE_DAC`, which once refused every account run in strict mode).
/// Both the confined and the unconfined-filesystem kinds, strict (the
/// default).
#[test]
fn a_plain_network_deny_command_runs_with_the_second_helper_locked() {
    if !needs_offline("a_plain_network_deny_command_runs_with_the_second_helper_locked") {
        return;
    }
    let d = dirs();
    let o = cmd(
        &d.ws,
        &["--net", "deny", "-w", "."],
        "echo hello> out.txt & echo ran",
    );
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("ran"), "{out}");
    assert!(d.ws.join("out.txt").exists(), "{out}");
    assert!(
        !out.contains("could not protect itself"),
        "the helper locked itself: {out}"
    );

    let o = cmd(&d.ws, &["--net", "deny", "--fs", "off"], "echo ran");
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("ran"), "{out}");
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}

// ---- `--msys`: Git Bash as the account insbx-msys (#38). ----

/// Git for Windows' bash, where its installer puts it.
const GIT_BASH: &str = r"C:\Program Files\Git\bin\bash.exe";
/// The msys account (`incord_sandbox::windows::offline::MSYS_ACCOUNT`).
const MSYS: &str = "insbx-msys";

/// The `--msys` tests' gate: the layer-2 setup, the msys account that
/// `insbx setup-windows` makes beside the offline one, and Git Bash. Else
/// says it skipped. (Run `insbx setup-windows` as administrator again on a
/// machine set up before `--msys` existed.)
fn needs_msys(test: &str) -> bool {
    if !needs_offline(test) {
        return false;
    }
    let account = Command::new("net")
        .args(["user", MSYS])
        .output()
        .is_ok_and(|o| o.status.success());
    if !account {
        eprintln!("SKIPPED {test}: the account {MSYS} does not exist; run `insbx setup-windows`");
        return false;
    }
    if !Path::new(GIT_BASH).is_file() {
        eprintln!("SKIPPED {test}: Git for Windows is not at {GIT_BASH}");
        return false;
    }
    // The msys tests share the msys account and its records: a crash test
    // leaves a record that refuses every other msys run, so they go one at a
    // time. The guard lives until the test's thread ends.
    MSYS_TEST_GUARD.with(|guard| {
        if guard.borrow().is_none() {
            let held = MSYS_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
            *guard.borrow_mut() = Some(held);
        }
    });
    true
}

/// Held by every running msys test ([`needs_msys`]).
static MSYS_TESTS: Mutex<()> = Mutex::new(());

thread_local! {
    static MSYS_TEST_GUARD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
}

/// `insbx run --msys --net deny -w <roots> -- bash -c <script>`.
fn bash_msys(cwd: &Path, extra: &[&str], script: &str) -> Output {
    let mut args = vec!["run", "--msys", "--net", "deny", "-w", "."];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", GIT_BASH, "-c", script]);
    insbx(&args, cwd)
}

/// A path as Git Bash takes it: forward slashes.
fn msys_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[test]
fn git_bash_runs_under_msys_and_not_without_it() {
    if !needs_msys("git_bash_runs_under_msys") {
        return;
    }
    let d = dirs();
    let o = bash_msys(&d.ws, &[], "echo ok > x && cat x && whoami");
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("ok"), "{out}");
    assert!(d.ws.join("x").exists(), "{out}");
    // (msys's own `whoami` prints the bare name.)
    assert!(
        out.to_ascii_lowercase().contains(MSYS),
        "runs as the msys account: {out}"
    );
    // The control: without `--msys` the same command dies where msys builds
    // its pipes (measured: "couldn't create signal pipe, Win32 error 5").
    let o = insbx(
        &[
            "run",
            "--net",
            "deny",
            "-w",
            ".",
            "--",
            GIT_BASH,
            "-c",
            "echo ok > y",
        ],
        &d.ws,
    );
    assert!(
        !o.status.success() && !d.ws.join("y").exists(),
        "bash without --msys: {}",
        text(&o)
    );
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}

/// The reason for a second account: the offline account's grant on a folder
/// is KEPT, so trusting its SID would let a run write into every folder an
/// earlier deny run used (measured). An msys run must not.
#[test]
fn an_msys_run_cannot_write_where_another_run_kept_a_grant() {
    if !needs_msys("an_msys_run_cannot_write_where_another_run_kept_a_grant") {
        return;
    }
    let d = dirs();
    // B: an earlier deny run leaves insbx-offline's kept grant on `out`.
    let o = cmd(&d.out, &["--net", "deny", "-w", "."], "echo a> f");
    assert!(d.out.join("f").exists(), "{}", text(&o));
    assert!(icacls(&d.out).contains(OFFLINE), "{}", icacls(&d.out));

    // A msys run in `ws` writes in `ws` and not in `out`.
    let target = d.out.join("from-msys");
    let o = bash_msys(
        &d.ws,
        &[],
        &format!(
            "echo ok > here; echo bad > '{}'; echo bad2 > '{}/g'; true",
            msys_path(&target),
            msys_path(&d.out)
        ),
    );
    let out = text(&o);
    assert!(d.ws.join("here").exists(), "the run's own root: {out}");
    assert!(
        !target.exists(),
        "an msys run wrote into a kept grant: {out}"
    );
    assert!(!d.out.join("g").exists(), "{out}");
    for dir in [&d.ws, &d.out] {
        let o = insbx(&["revoke", dir.to_str().expect("utf8")], &d.ws);
        assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    }
}

/// The msys account's ACEs are the run's alone: gone from the root and what
/// is beneath it afterwards, while the root's own SID and the OWNER RIGHTS
/// entry stay.
#[test]
fn an_msys_runs_account_is_gone_from_the_root_afterwards() {
    if !needs_msys("an_msys_runs_account_is_gone_from_the_root_afterwards") {
        return;
    }
    let d = dirs();
    let o = bash_msys(&d.ws, &[], "icacls . && echo ok > made");
    let during = text(&o);
    assert_eq!(o.status.code(), Some(0), "{during}");
    assert!(
        during.contains(MSYS),
        "the account's ACE during the run: {during}"
    );
    assert!(d.ws.join("made").exists(), "{during}");
    for path in [d.ws.clone(), d.ws.join("made")] {
        let after = icacls(&path);
        assert!(!after.contains(MSYS), "{}: {after}", path.display());
        assert!(!after.contains(SID_MARK), "{}: {after}", path.display());
    }
    let after = icacls(&d.ws);
    assert!(
        after.contains(KEEP_MARK),
        "the root's own SID is kept: {after}"
    );
    assert!(
        after.contains("OWNER RIGHTS"),
        "and the OWNER RIGHTS entry: {after}"
    );
    // Nothing of the offline account either: this run was not its.
    assert!(!after.contains(OFFLINE), "{after}");
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}

/// What the thread probe prints, one line per thread of every `insbx`
/// process, then the control.
const THREAD_PROBE: &str = r#"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Probe {
  [DllImport("kernel32.dll", SetLastError=true)]
  static extern IntPtr OpenThread(uint access, bool inherit, uint tid);
  [DllImport("kernel32.dll")]
  static extern bool CloseHandle(IntPtr h);
  public static string Open(uint tid, uint right) {
    IntPtr h = OpenThread(right, false, tid);
    if (h == IntPtr.Zero) { return "denied:" + Marshal.GetLastWin32Error(); }
    CloseHandle(h);
    return "ok";
  }
}
"@
# Each write right a command could take a thread over with, ON ITS OWN:
# THREAD_SET_CONTEXT, THREAD_SUSPEND_RESUME, THREAD_SET_INFORMATION, WRITE_DAC, WRITE_OWNER.
$rights = @(0x10, 0x2, 0x20, 0x40000, 0x80000)
$n = 0
foreach ($p in (Get-Process -Name insbx -ErrorAction SilentlyContinue)) {
  $n++
  foreach ($t in $p.Threads) {
    foreach ($r in $rights) { Write-Output ("THREAD " + $p.Id + " " + $t.Id + " " + $r + " " + [Probe]::Open([uint32]$t.Id, [uint32]$r)) }
  }
}
Write-Output ("INSBX-PROCESSES " + $n)
$c = Start-Process -FilePath cmd.exe -ArgumentList '/d','/c','ping -n 8 127.0.0.1 > nul' -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds 500
foreach ($t in $c.Threads) {
  foreach ($r in $rights) { Write-Output ("CONTROL " + $r + " " + [Probe]::Open([uint32]$t.Id, [uint32]$r)) }
}
Stop-Process -Id $c.Id -Force
"#;

/// The second helper runs as `insbx-msys`, unrestricted, and the confined
/// command is `insbx-msys` too, with that SID among its restricting SIDs: so
/// every part of the helper must be locked, not only its process object. The
/// helper's main thread came from the logon service with a default DACL
/// granting the account `GENERIC_ALL`; the command could have opened it for
/// `THREAD_SET_CONTEXT | THREAD_SUSPEND_RESUME` and hijacked it, unrestricted.
/// Every thread of every `insbx` process the command can see must refuse that
/// (the first helper and `insbx run` are the user's; the second helper's are
/// hardened); the control, a child the command started itself, must open.
#[test]
fn the_msys_helpers_threads_cannot_be_opened_by_the_command() {
    if !needs_msys("the_msys_helpers_threads_cannot_be_opened_by_the_command") {
        return;
    }
    let d = dirs();
    let script = d.ws.join("probe-threads.ps1");
    std::fs::write(&script, THREAD_PROBE).expect("script");
    let o = ps_file(&d.ws, &["--msys", "--net", "deny", "-w", "."], &script);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let processes: usize = out
        .lines()
        .find_map(|l| l.strip_prefix("INSBX-PROCESSES "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("the probe did not finish: {out}"));
    assert!(
        processes >= 2,
        "the helpers should be visible (the first and the second): {out}"
    );
    let threads: Vec<&str> = out.lines().filter(|l| l.starts_with("THREAD ")).collect();
    assert!(!threads.is_empty(), "{out}");
    // Every right, on its own, on every thread: denied (error 5), or the
    // thread has just exited (87).
    for line in &threads {
        assert!(
            line.ends_with("denied:5") || line.ends_with("denied:87"),
            "the command opened a helper's thread for a write right: {line}\n{out}"
        );
    }
    for right in ["16", "2", "32", "262144", "524288"] {
        assert!(
            threads
                .iter()
                .any(|l| l.contains(&format!(" {right} denied:5"))),
            "right {right} was probed and denied: {out}"
        );
        assert!(
            out.lines()
                .any(|l| l.trim() == format!("CONTROL {right} ok")),
            "the control (a thread of its own child) must open for right {right}, or the \
             probe proves nothing: {out}"
        );
    }
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}

/// The msys account's SID, as Windows has it.
fn msys_sid() -> String {
    let o = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "([System.Security.Principal.NTAccount]'{MSYS}').Translate(\
                 [System.Security.Principal.SecurityIdentifier]).Value"
            ),
        ])
        .output()
        .expect("powershell");
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A crashed msys run whose root cannot be found any more leaves a record the
/// sweep cannot finish. The msys account is trusted by every msys run, so the
/// next one is REFUSED, naming the path, until `insbx revoke --msys-leftovers`.
#[test]
fn a_crashed_msys_runs_leftovers_refuse_the_next_run_until_cleared() {
    if !needs_msys("a_crashed_msys_runs_leftovers_refuse_the_next_run") {
        return;
    }
    let d = dirs();
    let sid = msys_sid();
    assert!(sid.starts_with("S-1-5-21-"), "the msys SID: {sid}");
    let grants = PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"))
        .join("insbx")
        .join("grants");
    std::fs::create_dir_all(&grants).expect("grants dir");
    // A dead run (nothing holds its liveness mutex) whose msys root was moved.
    let rid = 2_000_000_001u32;
    let record = grants.join(format!("{rid}.grant"));
    let moved = d.out.join("moved-away");
    std::fs::write(
        &record,
        format!(
            "sid S-1-5-21-1111111111-2222222222-4242424242-{rid}\nuser {sid}\nmodify {}\n",
            moved.display()
        ),
    )
    .expect("record");

    let refused = bash_msys(&d.ws, &[], "echo ok > x");
    let said = text(&refused);
    assert_eq!(refused.status.code(), Some(125), "{said}");
    assert!(said.contains("moved-away"), "names the path: {said}");
    assert!(said.contains("--msys-leftovers"), "and the way out: {said}");
    assert!(!d.ws.join("x").exists(), "nothing ran: {said}");

    // A root that is gone is not taken on the user's word: without
    // `--forget-gone` the record stays, the exit code says so, and msys runs
    // stay refused.
    let kept = insbx(&["revoke", "--msys-leftovers"], &d.ws);
    let said = text(&kept);
    assert_eq!(kept.status.code(), Some(1), "{said}");
    assert!(said.contains("--forget-gone"), "says how: {said}");
    assert!(record.exists(), "the record is kept");
    let still = bash_msys(&d.ws, &[], "echo ok > x");
    assert_eq!(still.status.code(), Some(125), "{}", text(&still));

    let cleared = insbx(&["revoke", "--msys-leftovers", "--forget-gone"], &d.ws);
    assert_eq!(cleared.status.code(), Some(0), "{}", text(&cleared));
    assert!(!record.exists(), "the record is dropped");
    let again = bash_msys(&d.ws, &[], "echo ok > x");
    assert_eq!(again.status.code(), Some(0), "{}", text(&again));
    assert!(d.ws.join("x").exists());
    let _ = std::fs::remove_file(&record);
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}

/// An open handle to the liveness mutex of the live run whose grant record
/// names `root` as an msys root, closed on drop.
struct Liveness(windows_sys::Win32::Foundation::HANDLE);

impl Drop for Liveness {
    fn drop(&mut self) {
        // SAFETY: a handle this test opened, closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

fn hold_liveness_of_run_rooted_at(root: &Path) -> Liveness {
    use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let grants = PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"))
        .join("insbx")
        .join("grants");
    let root = root.to_string_lossy().into_owned();
    let rid = std::fs::read_dir(&grants)
        .expect("grants dir")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "grant"))
        .find(|e| {
            std::fs::read_to_string(e.path()).is_ok_and(|t| {
                t.lines()
                    .any(|l| l.starts_with("modify ") && l.contains(&root))
            })
        })
        .and_then(|e| e.path().file_stem()?.to_str().map(str::to_string))
        .expect("the live run's record");
    let name: Vec<u16> = format!("Global\\insbx-grant-{rid}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: a NUL-terminated name; the handle is closed by `Liveness`.
    let handle = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    assert!(!handle.is_null(), "the run's liveness mutex");
    Liveness(handle)
}

/// A REAL crash: an msys run is started and its `insbx run` killed, so its
/// lease never drops and its record stays. Then its root is moved aside and a
/// new folder made under the old name. The next msys run is refused; clearing
/// without saying the folder is dealt with keeps the record (exit 1); only
/// after the old folder is revoked where it went and `--forget-gone` is given
/// does an msys run go again.
#[test]
fn a_root_moved_aside_and_replaced_after_a_crash_stays_refused_until_dealt_with() {
    if !needs_msys("a_root_moved_aside_and_replaced_after_a_crash") {
        return;
    }
    let d = dirs();
    let mut crashing = base(&d.ws)
        .args([
            "run",
            "--msys",
            "--net",
            "deny",
            "-w",
            ".",
            "--",
            GIT_BASH,
            "-c",
            "echo go > started; sleep 120",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("insbx");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !d.ws.join("started").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(d.ws.join("started").exists(), "the run never started");
    // Any other run's sweep (the offline tests run concurrently) would take
    // the dead record's grant back, rightly, while the root still sits where
    // it was. To test the window AFTER the move, this test keeps the run's
    // liveness mutex open (`Global\insbx-grant-<rid>`, named in the record's
    // file name) until the root is replaced: nobody sweeps a run that is
    // "alive". Closing the handle is the moment the run becomes dead.
    let alive = hold_liveness_of_run_rooted_at(&d.ws);
    // The crash: the process that holds the lease dies; nothing revokes.
    crashing.kill().expect("kill insbx run");
    let _ = crashing.wait();

    // Its helper notices and ends the command; then the root can be moved.
    let aside = d.ws.with_file_name("ws.bak");
    let moved = (0..150).any(|_| {
        std::thread::sleep(Duration::from_millis(200));
        std::fs::rename(&d.ws, &aside).is_ok()
    });
    assert!(moved, "the crashed run's command never let go of the root");
    std::fs::create_dir(&d.ws).expect("a new root under the old name");
    drop(alive);

    let refused = bash_msys(&d.ws, &[], "echo ok > x");
    let said = text(&refused);
    assert_eq!(refused.status.code(), Some(125), "{said}");
    assert!(said.contains("--msys-leftovers"), "{said}");
    assert!(!d.ws.join("x").exists(), "{said}");

    let kept = insbx(&["revoke", "--msys-leftovers"], &d.out);
    let said = text(&kept);
    assert_eq!(kept.status.code(), Some(1), "{said}");
    assert!(said.contains("replaced") && said.contains("KEPT"), "{said}");
    let still = bash_msys(&d.ws, &[], "echo ok > x");
    assert_eq!(still.status.code(), Some(125), "{}", text(&still));

    // The user takes the grant back where the folder went, then says so.
    let o = insbx(&["revoke", aside.to_str().expect("utf8")], &d.out);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(
        !icacls(&aside).contains(MSYS),
        "the moved folder no longer names the account: {}",
        icacls(&aside)
    );
    let o = insbx(&["revoke", "--msys-leftovers", "--forget-gone"], &d.out);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let again = bash_msys(&d.ws, &[], "echo ok > x");
    assert_eq!(again.status.code(), Some(0), "{}", text(&again));
    assert!(d.ws.join("x").exists());
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}

/// A subfolder that carries its own inheritable CREATOR OWNER entry (the root
/// does not, so the root check passes): what the msys account creates there
/// gets an entry naming it, which a revoke cannot clear (re-propagation
/// resolves CREATOR OWNER to the owner, still the account). The next run is
/// refused naming the object; `--msys-leftovers` re-reads the tree, finds it
/// comes back, keeps the record and SAYS why (exit 1), msys runs stay refused;
/// once the CREATOR OWNER entry is removed it clears and runs go again.
#[test]
fn a_subfolder_with_its_own_creator_owner_entry_stays_refused_until_it_is_removed() {
    if !needs_msys("a_subfolder_with_its_own_creator_owner_entry") {
        return;
    }
    let d = dirs();
    let sub = d.ws.join("sub");
    std::fs::create_dir(&sub).expect("sub");
    let set = Command::new("icacls")
        .arg(&sub)
        .args(["/grant", "*S-1-3-0:(OI)(CI)(IO)F"])
        .output()
        .expect("icacls");
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stdout)
    );

    let first = bash_msys(&d.ws, &[], "echo x > sub/f; echo ok");
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    assert!(sub.join("f").exists());

    // What was left is found by its effect, and the next run is refused.
    let refused = bash_msys(&d.ws, &[], "echo y > y");
    let said = text(&refused);
    assert_eq!(refused.status.code(), Some(125), "{said}");
    assert!(said.contains("sub"), "names the object: {said}");

    // A revoke cannot clear it: the record is kept, with the reason.
    let kept = insbx(&["revoke", "--msys-leftovers"], &d.ws);
    let said = text(&kept);
    assert_eq!(kept.status.code(), Some(1), "{said}");
    assert!(said.contains("CREATOR OWNER"), "says why: {said}");
    assert!(said.contains("KEPT"), "{said}");
    let still = bash_msys(&d.ws, &[], "echo y > y");
    assert_eq!(
        still.status.code(),
        Some(125),
        "must stay refused: {}",
        text(&still)
    );

    // Removing the CREATOR OWNER entry (and what came of it) lets it clear.
    let o = Command::new("icacls")
        .arg(&sub)
        .args(["/remove:g", "*S-1-3-0"])
        .output()
        .expect("icacls");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let cleared = insbx(&["revoke", "--msys-leftovers"], &d.ws);
    assert_eq!(cleared.status.code(), Some(0), "{}", text(&cleared));
    let again = bash_msys(&d.ws, &[], "echo y > y");
    assert_eq!(again.status.code(), Some(0), "{}", text(&again));
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}

/// One msys run at a time: the second waits for the first to end.
#[test]
fn two_msys_runs_serialize() {
    if !needs_msys("two_msys_runs_serialize") {
        return;
    }
    let d = dirs();
    let first = base(&d.ws)
        .args([
            "run",
            "--msys",
            "--net",
            "deny",
            "-w",
            ".",
            "--",
            GIT_BASH,
            "-c",
            "echo go > a-start; sleep 4; echo done > a-end",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("insbx");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !d.ws.join("a-start").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(d.ws.join("a-start").exists(), "the first run never started");
    let second = bash_msys(&d.ws, &[], "echo go > b-start");
    let second_out = text(&second);
    let first = first.wait_with_output().expect("first run");
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    assert_eq!(second.status.code(), Some(0), "{second_out}");
    let modified = |name: &str| {
        std::fs::metadata(d.ws.join(name))
            .and_then(|m| m.modified())
            .unwrap_or_else(|e| panic!("{name}: {e}"))
    };
    assert!(
        modified("b-start") >= modified("a-end"),
        "the second run started before the first ended: {second_out}"
    );
    assert!(
        second_out.contains("another msys run"),
        "the second says it waited: {second_out}"
    );
    let o = insbx(&["revoke", "."], &d.ws);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
}
