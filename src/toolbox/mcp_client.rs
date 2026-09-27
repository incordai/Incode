//! The client half of MCP, for a server incode started itself as a child: the
//! desktop executor, and the stdio servers an ACP client hands a session. An
//! HTTP server an ACP client names is the same client over another [`Link`]
//! ([`super::mcp_http`]).
//!
//! One line of JSON-RPC each way ([`crate::mcp_stdio`] is the server half).
//! Every wait is bounded, a line is bounded, and a call can be abandoned when
//! the turn is stopped. Dropping the link is the kill switch: EOF on the
//! child's stdin, a short grace, then its whole process tree.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::image::ToolOutput;

/// The longest line read from a server: a screenshot, base64, inside the
/// image bounds, with room to be refused by those bounds.
pub(super) const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;
/// How long a child has to exit on its own once its stdin closes, before its
/// tree is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How often a wait looks at whether the turn was stopped.
const STOP_POLL: Duration = Duration::from_millis(100);
/// The most pages of `tools/list` read from one server. A server that keeps
/// handing out cursors is not listed forever.
const MAX_LIST_PAGES: usize = 20;

/// One line each way to a server.
pub(super) trait Link: Send {
    fn send(&mut self, line: &str) -> Result<(), String>;
    /// The next line, `Ok(None)` when none came within `bound`, or why the
    /// server can no longer be heard.
    fn recv(&mut self, bound: Duration) -> Result<Option<String>, String>;
}

/// A child process, spoken to over its stdin and stdout.
pub(super) struct ChildLink {
    child: Child,
    tree: crate::agent::process_tree::Tree,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<String, String>>,
    /// Who this is, for the messages it causes: `the desktop`.
    what: String,
}

impl ChildLink {
    /// Start `command` with its stdin and stdout piped to this process and
    /// its stderr discarded, contained in a process tree of its own.
    pub(super) fn spawn(mut command: Command, what: &str) -> Result<Self, String> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::agent::process_tree::prepare(&mut command);
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start {what}: {e}"))?;
        let tree = crate::agent::process_tree::Tree::adopt(&child);
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{what} was started without a stdout"))?;
        let (tx, lines) = channel();
        let named = what.to_string();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match (&mut reader)
                    .take(MAX_LINE_BYTES)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => break,
                    Ok(_) if !line.ends_with(b"\n") => {
                        let _ = tx.send(Err(format!("{named} sent a line past the bound")));
                        break;
                    }
                    Ok(_) => {
                        if tx
                            .send(Ok(String::from_utf8_lossy(&line).into_owned()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("{named}'s output failed: {e}")));
                        break;
                    }
                }
            }
        });
        crate::trace!("mcp", "started {what}, pid {}", child.id());
        Ok(Self {
            child,
            tree,
            stdin,
            lines,
            what: what.to_string(),
        })
    }
}

impl Link for ChildLink {
    fn send(&mut self, line: &str) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| format!("{}'s input is closed", self.what))?;
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("{} could not be reached: {e}", self.what))
    }

    fn recv(&mut self, bound: Duration) -> Result<Option<String>, String> {
        match self.lines.recv_timeout(bound) {
            Ok(line) => line.map(Some),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(format!("{} exited", self.what)),
        }
    }
}

impl Drop for ChildLink {
    /// The kill switch: EOF first, which lets a server stop cleanly, then the
    /// whole tree.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let began = Instant::now();
        while began.elapsed() < EXIT_GRACE {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.tree.kill(&mut self.child);
        let _ = self.child.wait();
        crate::trace!("mcp", "stopped {}", self.what);
    }
}

/// A server this session has started and said hello to.
pub(super) struct McpClient {
    link: Box<dyn Link>,
    next: u64,
    /// Set when the link failed; the owner decides whether to start another.
    broken: bool,
    what: String,
}

impl McpClient {
    /// Open the session with the server behind `link`: `initialize`, answered
    /// within `bound`, then `notifications/initialized`.
    pub(super) fn open(link: Box<dyn Link>, what: &str, bound: Duration) -> Result<Self, String> {
        let mut client = Self {
            link,
            next: 0,
            broken: false,
            what: what.to_string(),
        };
        client.request(
            "initialize",
            json!({
                "protocolVersion": crate::mcp_stdio::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "incode", "version": env!("CARGO_PKG_VERSION")}
            }),
            bound,
            &|| false,
        )?;
        client
            .link
            .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())?;
        Ok(client)
    }

    /// Whether the link has failed.
    pub(super) fn is_broken(&self) -> bool {
        self.broken
    }

    /// One request, answered within `bound` or abandoned when `stopped` says
    /// so. A reply to an earlier, abandoned request is skipped by its id.
    fn request(
        &mut self,
        method: &str,
        params: Value,
        bound: Duration,
        stopped: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let sent = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(why) = self.link.send(&sent.to_string()) {
            self.broken = true;
            return Err(why);
        }
        let deadline = Instant::now() + bound;
        loop {
            if stopped() {
                return Err(format!(
                    "the turn was stopped while {} was answering {method}",
                    self.what
                ));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.broken = true;
                return Err(format!(
                    "{} did not answer {method} within {}s",
                    self.what,
                    bound.as_secs()
                ));
            }
            let line = match self.link.recv(left.min(STOP_POLL)) {
                Ok(Some(line)) => line,
                Ok(None) => continue,
                Err(why) => {
                    self.broken = true;
                    return Err(why);
                }
            };
            let Ok(reply) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(format!("{} refused {method}: {error}", self.what));
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Every tool the server offers, in its own spelling (`inputSchema`),
    /// following `nextCursor` for at most [`MAX_LIST_PAGES`] pages.
    pub(super) fn list_tools(&mut self, bound: Duration) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(at) => json!({ "cursor": at }),
                None => json!({}),
            };
            let page = self.request("tools/list", params, bound, &|| false)?;
            if let Some(listed) = page.get("tools").and_then(Value::as_array) {
                tools.extend(listed.iter().cloned());
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Ok(tools)
    }

    /// Run one tool. A tool's own refusal (`isError`) is an `Err` with its
    /// words; pictures in the answer are checked like any other process's.
    pub(super) fn call(
        &mut self,
        name: &str,
        args: &Value,
        bound: Duration,
        stopped: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, String> {
        let result = self.request(
            "tools/call",
            json!({"name": name, "arguments": args}),
            bound,
            stopped,
        )?;
        let blocks = result
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let output = ToolOutput::from_mcp_content(&blocks);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(output.text);
        }
        Ok(output)
    }
}
