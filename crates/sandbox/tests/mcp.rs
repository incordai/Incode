//! `sbx mcp` driven the way an agent drives it: over stdio, one JSON-RPC
//! message per line. Create a sandbox, write a script, run it, read what it
//! made, and try to get out.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sbx"))
            .arg("mcp")
            .args(args)
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

    /// A tool call: its parsed JSON result, or Err with the error text.
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

fn landlock() -> bool {
    Command::new(env!("CARGO_BIN_EXE_sbx"))
        .args(["check", "--fs", "workspace-write", "--net", "deny"])
        .output()
        .expect("check")
        .status
        .success()
}

#[test]
fn an_agent_writes_a_script_runs_it_and_reads_the_result() {
    if !landlock() {
        eprintln!("SKIPPED: this kernel cannot enforce the sandbox");
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let mut m = Mcp::start(&["--root", root.path().to_str().expect("utf8")]);

    let created = m.call("sandbox_create", json!({})).expect("create");
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["network"], "deny");
    let folder = std::path::PathBuf::from(created["folder"].as_str().expect("folder"));
    assert!(folder.starts_with(std::fs::canonicalize(root.path()).expect("canon")));

    // A script that does real work and leaves a result file.
    m.call(
        "sandbox_write_file",
        json!({"id": id, "path": "tools/sum.sh", "content": "#!/bin/sh\necho $(( $1 + $2 )) > out/result.txt\necho done\n", "executable": true}),
    )
    .expect("write");
    let ran = m
        .call(
            "sandbox_run",
            json!({"id": id, "script": "mkdir -p out && ./tools/sum.sh 2 40"}),
        )
        .expect("run");
    assert_eq!(ran["exit_code"], 0, "{ran}");
    assert_eq!(ran["stdout"], "done\n");
    let read = m
        .call(
            "sandbox_read_file",
            json!({"id": id, "path": "out/result.txt"}),
        )
        .expect("read");
    assert_eq!(read["content"], "42\n");
    let listed = m.call("sandbox_list", json!({"id": id})).expect("list");
    let names: Vec<&str> = listed["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        names.contains(&"out") && names.contains(&"tools"),
        "{names:?}"
    );

    // A command list, stdin, and a non-zero exit come back as they are.
    let ran = m
        .call(
            "sandbox_run",
            json!({"id": id, "command": ["sh", "-c", "cat; exit 3"], "stdin": "fed in"}),
        )
        .expect("run");
    assert_eq!(
        (ran["exit_code"].as_i64(), ran["stdout"].as_str()),
        (Some(3), Some("fed in"))
    );

    // Getting out: writing beside the sandbox, the network, the host's
    // secrets, a link planted for the unconfined server to follow.
    let outside = root.path().join("escaped");
    let ran = m
        .call(
            "sandbox_run",
            json!({"id": id, "script": format!(
                "touch {}; echo \"[$SECRET_TOKEN]\"; (exec 3<>/dev/tcp/127.0.0.1/22) 2>/dev/null && echo NET; ln -s / root; true",
                outside.display()
            )}),
        )
        .expect("run");
    assert!(!outside.exists(), "wrote outside the sandbox: {ran}");
    assert!(
        ran["stdout"].as_str().expect("stdout").contains("[]"),
        "{ran}"
    );
    assert!(
        !ran["stdout"].as_str().expect("stdout").contains("NET"),
        "{ran}"
    );
    assert!(m
        .call(
            "sandbox_read_file",
            json!({"id": id, "path": "root/etc/hostname"})
        )
        .is_err());
    assert!(m
        .call(
            "sandbox_write_file",
            json!({"id": id, "path": "root/tmp/x", "content": "x"})
        )
        .is_err());
    assert!(m
        .call(
            "sandbox_write_file",
            json!({"id": id, "path": "../x", "content": "x"})
        )
        .is_err());
    assert!(m
        .call(
            "sandbox_read_file",
            json!({"id": id, "path": "/etc/passwd"})
        )
        .is_err());

    // The time limit stops the run and what it left in the background.
    let tag = format!("sbx_mcp_bg_{}", std::process::id());
    let ran = m
        .call(
            "sandbox_run",
            json!({"id": id, "interpreter": "bash", "timeout_secs": 1,
                   "script": format!("(exec -a {tag} sleep 30) & sleep 30")}),
        )
        .expect("run");
    assert_eq!(ran["timed_out"], true, "{ran}");
    m.call(
        "sandbox_run",
        json!({"id": id, "interpreter": "bash", "script": format!("(exec -a {tag} sleep 30) & echo started")}),
    )
    .expect("run");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let left = std::fs::read_dir("/proc")
        .expect("proc")
        .filter_map(Result::ok)
        .any(|e| {
            std::fs::read(e.path().join("cmdline"))
                .ok()
                .is_some_and(|c| c.split(|b| *b == 0).next() == Some(tag.as_bytes()))
        });
    assert!(!left, "a background process outlived its run");

    // Above the ceilings: refused, not lowered.
    assert!(m
        .call("sandbox_create", json!({"network": "allow"}))
        .is_err());
    assert!(m
        .call(
            "sandbox_run",
            json!({"id": id, "command": ["true"], "timeout_secs": 100000})
        )
        .is_err());

    m.call("sandbox_destroy", json!({"id": id}))
        .expect("destroy");
    assert!(!folder.exists());
    assert!(m.call("sandbox_info", json!({"id": id})).is_err());
    m.stop();
}

#[test]
fn sandboxes_are_removed_when_the_server_stops_unless_kept() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut m = Mcp::start(&["--root", root.path().to_str().expect("utf8")]);
    let created = m.call("sandbox_create", json!({})).expect("create");
    let folder = std::path::PathBuf::from(created["folder"].as_str().expect("folder"));
    assert!(folder.is_dir());
    m.stop();
    assert!(!folder.exists());

    let mut m = Mcp::start(&["--root", root.path().to_str().expect("utf8"), "--keep"]);
    let created = m.call("sandbox_create", json!({})).expect("create");
    let folder = std::path::PathBuf::from(created["folder"].as_str().expect("folder"));
    m.stop();
    assert!(folder.is_dir());
}
