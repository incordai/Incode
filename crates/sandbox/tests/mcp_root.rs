//! `insbx mcp`'s default root (`<temp>/insbx-mcp-<pid>`) is the server's own:
//! it goes when the server stops, on every platform (`fn mcp` in
//! `src/main.rs`). A `--root` that was given is the caller's and stays; the
//! sandboxes in either are removed unless `--keep` (`tests/mcp.rs`).

use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// Send request `id` and return its reply.
fn request(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    writeln!(stdin, "{msg}").expect("write");
    stdin.flush().expect("flush");
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read");
    let reply: Value = serde_json::from_str(&line).expect("json");
    assert_eq!(reply["id"], id, "{reply}");
    reply
}

#[test]
fn the_default_root_goes_with_the_server() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_insbx"))
        .args(["mcp", "--net", "allow"])
        .env("TMPDIR", temp.path())
        .env("TMP", temp.path())
        .env("TEMP", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("insbx mcp starts");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
    );
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .expect("write");
    let reply = request(
        &mut stdin,
        &mut stdout,
        2,
        "tools/call",
        json!({"name": "sandbox_create", "arguments": {"network": "allow"}}),
    );
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    let created: Value = serde_json::from_str(text).expect("tool result is JSON");
    let folder = std::path::PathBuf::from(created["folder"].as_str().expect("folder"));
    let root = folder.parent().expect("root").to_path_buf();
    assert!(folder.is_dir());
    assert!(root
        .file_name()
        .expect("name")
        .to_string_lossy()
        .starts_with("insbx-mcp-"));

    drop(stdin);
    child.wait().expect("insbx mcp exits");
    assert!(!root.exists(), "{} left behind", root.display());
}
