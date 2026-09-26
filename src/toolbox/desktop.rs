//! The desktop tools, run through the toolbox's gate.
//!
//! # Why the desktop still runs in its own process
//!
//! The tools are the toolbox's now: offered by it, validated against their
//! schemas, held to the permission mode, asked of the memory engine, drawn by
//! incode as its own calls, and their screenshots carried as pictures. What
//! EXECUTES them is still `incode --desktop-mcp`, started by the toolbox as a
//! child the first time a desktop tool is called, because three things the
//! desktop owns are process-wide and must not be taken on by the session:
//!
//! - **DPI awareness.** Declared once per process and never undone
//!   (`desktop::dpi`): set in the session's process it would change how the
//!   whole terminal session is scaled, for every run, desktop or not.
//! - **The input thread**, which owns the synthetic mouse and keyboard.
//! - **The kill switch.** EOF on the child's stdin halts the desktop, and a
//!   stop file beside its journal is checked before every call. A process
//!   boundary is what makes both absolute: dropping the session's tools
//!   closes the pipe, and the child's tree is killed after it.
//!
//! The child runs under `-p read-only` when the session is read-only and
//! `-p workspace-write` otherwise — the toolbox has already applied the
//! session's mode, including asking the person under `prompt`, so the child's
//! own check is the second line, not the first. The operator's desktop hooks
//! still run in the child, as before.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

use super::image::ToolOutput;

/// The desktop tools that only look.
pub(super) const OBSERVING: &[&str] = &["screen_info", "screenshot", "ui_tree", "cursor_position"];
/// The desktop tools that act on the machine.
pub(super) const ACTING: &[&str] = &[
    "click_element",
    "type_text",
    "paste_text",
    "key",
    "scroll",
    "drag",
    "move_mouse",
    "click",
];
/// How long one desktop call may take. A UI tree of a large window is the
/// slowest thing it does.
const CALL_BOUND: Duration = Duration::from_secs(120);
/// How long the child has to answer `initialize`.
const START_BOUND: Duration = Duration::from_secs(20);
/// The longest line read from the child: a screenshot, base64, inside the
/// image bounds, with room to be refused by those bounds.
const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;
/// How long the child has to exit on its own once its stdin closes, before
/// its tree is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Whether `name` is a desktop tool.
pub(super) fn is_desktop(name: &str) -> bool {
    OBSERVING.contains(&name) || ACTING.contains(&name)
}

/// The desktop tools, in the toolbox's spelling (`input_schema`), from the
/// desktop server's own definitions — one source for both.
pub(super) fn definitions() -> Vec<Value> {
    crate::desktop::mcp::tool_definitions()
        .into_iter()
        .map(|mut def| {
            if let Some(object) = def.as_object_mut() {
                if let Some(schema) = object.remove("inputSchema") {
                    object.insert("input_schema".to_string(), schema);
                }
            }
            def
        })
        .collect()
}

/// Where the session's desktop is started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSetup {
    /// This program, run as `--desktop-mcp`.
    pub exe: PathBuf,
    /// Where the desktop journals what it did; its stop file sits beside it.
    pub journal: PathBuf,
}

/// One line each way to a desktop server.
pub(super) trait Link: Send {
    fn send(&mut self, line: &str) -> Result<(), String>;
    fn recv(&mut self, bound: Duration) -> Result<String, String>;
}

/// The child process, spoken to over its stdin and stdout.
struct ChildLink {
    child: Child,
    tree: crate::agent::process_tree::Tree,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<String, String>>,
}

impl ChildLink {
    fn start(setup: &DesktopSetup, read_only: bool) -> Result<Self, String> {
        let mode = if read_only {
            crate::permissions::PermissionMode::ReadOnly
        } else {
            crate::permissions::PermissionMode::WorkspaceWrite
        };
        let mut command = Command::new(&setup.exe);
        command
            .arg("--desktop-mcp")
            .arg("--desktop-journal")
            .arg(&setup.journal)
            .args(["-p", mode.as_str()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::agent::process_tree::prepare(&mut command);
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start the desktop: {e}"))?;
        let tree = crate::agent::process_tree::Tree::adopt(&child);
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or("the desktop was started without a stdout")?;
        let (tx, lines) = channel();
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
                        let _ = tx.send(Err("the desktop sent a line past the bound".to_string()));
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
                        let _ = tx.send(Err(format!("the desktop's output failed: {e}")));
                        break;
                    }
                }
            }
        });
        crate::trace!("desktop", "started pid {} ({})", child.id(), mode.as_str());
        Ok(Self {
            child,
            tree,
            stdin,
            lines,
        })
    }
}

impl Link for ChildLink {
    fn send(&mut self, line: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("the desktop's input is closed")?;
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("the desktop could not be reached: {e}"))
    }

    fn recv(&mut self, bound: Duration) -> Result<String, String> {
        match self.lines.recv_timeout(bound) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "the desktop did not answer within {}s",
                bound.as_secs()
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err("the desktop exited".to_string())
            }
        }
    }
}

impl Drop for ChildLink {
    /// The kill switch: EOF first, which halts the desktop before the child
    /// returns, then the whole tree.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let began = std::time::Instant::now();
        while began.elapsed() < EXIT_GRACE {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.tree.kill(&mut self.child);
        let _ = self.child.wait();
        crate::trace!("desktop", "stopped");
    }
}

/// A desktop server this session has started, and has said hello to.
pub(super) struct DesktopClient {
    link: Box<dyn Link>,
    next: u64,
    read_only: bool,
    /// Set when the link failed; the next call starts a fresh one.
    broken: bool,
}

impl DesktopClient {
    /// Start the desktop child and open the session with it.
    pub(super) fn start(setup: &DesktopSetup, read_only: bool) -> Result<Self, String> {
        Self::over(Box::new(ChildLink::start(setup, read_only)?), read_only)
    }

    /// A client over an already-open link.
    pub(super) fn over(link: Box<dyn Link>, read_only: bool) -> Result<Self, String> {
        let mut client = Self {
            link,
            next: 0,
            read_only,
            broken: false,
        };
        client.request(
            "initialize",
            json!({
                "protocolVersion": crate::mcp_stdio::PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "incode", "version": env!("CARGO_PKG_VERSION")}
            }),
            START_BOUND,
        )?;
        client
            .link
            .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())?;
        Ok(client)
    }

    /// Whether this client can serve a session that is (or is not)
    /// read-only.
    pub(super) fn serves(&self, read_only: bool) -> bool {
        !self.broken && self.read_only == read_only
    }

    fn request(&mut self, method: &str, params: Value, bound: Duration) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let sent = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let outcome = self.link.send(&sent.to_string()).and_then(|()| loop {
            let line = self.link.recv(bound)?;
            let Ok(reply) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                break Err(format!("the desktop refused {method}: {error}"));
            }
            break Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        });
        if outcome.is_err() {
            self.broken = true;
        }
        outcome
    }

    /// Run one desktop tool. A tool's own refusal is an `Err` with its words.
    pub(super) fn call(&mut self, name: &str, args: &Value) -> Result<ToolOutput, String> {
        let result = self.request(
            "tools/call",
            json!({"name": name, "arguments": args}),
            CALL_BOUND,
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::mpsc::Sender;

    /// A desktop server in this process, on its own thread, behind a link —
    /// the child's protocol without the child. Counts the tool calls that
    /// reach it.
    pub(in crate::toolbox) struct InProcess {
        to: Sender<String>,
        from: Receiver<String>,
    }

    impl InProcess {
        /// Over `server`, answering on a thread. `calls` counts every
        /// `tools/call` that arrives.
        pub(in crate::toolbox) fn new(
            mut handle: impl FnMut(&str) -> Option<String> + Send + 'static,
            calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            let (to, inbox) = channel::<String>();
            let (outbox, from) = channel::<String>();
            std::thread::spawn(move || {
                while let Ok(line) = inbox.recv() {
                    if line.contains("\"tools/call\"") {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    if let Some(reply) = handle(&line) {
                        if outbox.send(reply).is_err() {
                            break;
                        }
                    }
                }
            });
            Self { to, from }
        }
    }

    impl Link for InProcess {
        fn send(&mut self, line: &str) -> Result<(), String> {
            self.to.send(line.to_string()).map_err(|e| e.to_string())
        }
        fn recv(&mut self, bound: Duration) -> Result<String, String> {
            self.from.recv_timeout(bound).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn every_desktop_tool_is_classified_once() {
        let names: Vec<String> = definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(names.len(), OBSERVING.len() + ACTING.len(), "{names:?}");
        for name in &names {
            assert!(is_desktop(name), "{name} is not classified");
        }
        for def in definitions() {
            assert!(def.get("input_schema").is_some(), "{def}");
            assert!(def.get("inputSchema").is_none(), "{def}");
        }
    }

    /// A desktop that answers a tool with an image: read back as a picture,
    /// its refusal as an error in its own words.
    #[test]
    fn a_desktop_reply_is_read_as_text_and_pictures() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let png = {
            let canvas = image::RgbaImage::from_pixel(4, 3, image::Rgba([1, 2, 3, 255]));
            crate::desktop::capture::encode(&canvas, crate::desktop::capture::ShotFormat::Png, 0)
                .expect("png")
        };
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        let link = InProcess::new(
            move |line| {
                let request: Value = serde_json::from_str(line).ok()?;
                let id = request.get("id")?.clone();
                let result = match request["params"]["name"].as_str() {
                    Some("screenshot") => json!({"content": [
                        {"type": "text", "text": "{\"width\":4}"},
                        {"type": "image", "data": data, "mimeType": "image/png"}
                    ], "isError": false}),
                    Some(_) => {
                        json!({"content": [{"type": "text", "text": "refused: stopped"}], "isError": true})
                    }
                    None => json!({}),
                };
                Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
            },
            std::sync::Arc::clone(&calls),
        );
        let mut client = DesktopClient::over(Box::new(link), false).expect("hello");
        let shot = client.call("screenshot", &json!({})).expect("a picture");
        assert_eq!(shot.images.len(), 1);
        assert_eq!(shot.shown(), "{\"width\":4}\n[image 4x3 png]");
        let refused = client
            .call("click", &json!({"x": 1, "y": 1}))
            .expect_err("refused");
        assert_eq!(refused, "refused: stopped");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
