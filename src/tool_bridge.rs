//! incode's tools, offered to an agent CLI that is used only as a model.
//!
//! # The shape
//!
//! ```text
//! incode (parent) ──spawns──▶ agent CLI (the model, its own tools switched off)
//!      ▲                           │ spawns, over its MCP config
//!      │ loopback + token          ▼
//!      └──────────────────── incode --tool-server (this module's child half)
//! ```
//!
//! The CLI can only reach tools over MCP, and an MCP stdio server is a process
//! the CLI starts — so the server cannot BE the parent. Only the parent holds
//! what a tool call must pass through: the operator's guards, the memory
//! engine's hook, the person to ask (the terminal, or the ACP client), the
//! turn's cancel token and the observer that draws the call. So the server
//! runs nothing. It forwards each request to the parent over a loopback socket
//! and hands back whatever the parent's [`Toolbox`] said.
//!
//! # The channel
//!
//! - Bound on `127.0.0.1`, port chosen by the OS, for ONE turn.
//! - Every request carries a token of 256 random bits, made per turn and
//!   given only to the CLI's environment ([`TOKEN_ENV`]); it is compared in
//!   constant time, and a request without it runs nothing.
//! - A request that arrives after the turn was stopped or ended is refused
//!   without reaching the toolbox.
//! - One JSON line each way per connection. A request is bounded in size and
//!   in how long it may take to arrive; the number of requests in flight is
//!   bounded too.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::agent::AgentEvent;
use crate::runtime::cancel::CancelToken;
use crate::toolbox::{ToolOutput, Toolbox};

/// How long, in whole seconds, the server half waits for a tool to finish —
/// what a CLI that bounds its own MCP calls must be told, or it gives up on
/// a call incode is still running. See `REPLY_BOUND`.
pub const REPLY_BOUND_SECS: u64 = 60 * 60;

/// Where the tool server finds the parent: `127.0.0.1:<port>`.
pub const ADDR_ENV: &str = "INCODE_TOOL_BRIDGE";
/// The turn's token, which every request must carry.
pub const TOKEN_ENV: &str = "INCODE_TOOL_TOKEN";
/// The name the agent CLI knows this server by; its tools appear to the model
/// as `mcp__incode__<Tool>`.
pub const CLIENT_KEY: &str = "incode";
/// The flag that starts the server half.
pub const SERVER_FLAG: &str = "--tool-server";

/// The largest request accepted. A `Write` carries a whole file, so this is
/// generous; past it the request is refused rather than read.
const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
/// Requests being answered at once. The model asks for tools a few at a time;
/// more than this is not a model.
const MAX_IN_FLIGHT: usize = 8;
/// How long a connected client has to send its request line.
const REQUEST_BOUND: Duration = Duration::from_secs(10);
/// How often the listener looks to see whether the turn is over.
const ACCEPT_POLL: Duration = Duration::from_millis(25);
/// How long the server half waits to reach the parent.
const CONNECT_BOUND: Duration = Duration::from_secs(5);
/// How long the server half waits for a tool to finish. A person may be asked
/// (bounded at ten minutes by the engine) and a command may run long; past an
/// hour nothing is still coming.
const REPLY_BOUND: Duration = Duration::from_secs(REPLY_BOUND_SECS);

/// What reached the parent from the tool server.
pub enum Request {
    /// "What tools are there?"
    List {
        reply: Sender<Result<Vec<Value>, String>>,
    },
    /// "Run this one." Answered with the text and any pictures.
    Call {
        name: String,
        input: Value,
        reply: Sender<Result<ToolOutput, String>>,
    },
}

/// The parent's end of the channel, open for one turn.
///
/// Dropping it closes the channel: the listener stops, and any request still
/// waiting is answered as refused.
pub struct Bridge {
    addr: SocketAddr,
    token: String,
    closed: Arc<AtomicBool>,
    /// Set when the tool server asked, with the turn's token, for the tools.
    listed: Arc<AtomicBool>,
    listener: Option<std::thread::JoinHandle<()>>,
}

impl Bridge {
    /// Listen on loopback, handing every authenticated request to `to` as
    /// `wrap(request)`. Requests are refused once `cancel` trips.
    ///
    /// # Errors
    ///
    /// The socket could not be bound or configured.
    pub fn open<M: Send + 'static>(
        to: Sender<M>,
        wrap: fn(Request) -> M,
        cancel: Option<CancelToken>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let closed = Arc::new(AtomicBool::new(false));
        let listed = Arc::new(AtomicBool::new(false));
        let gate = Gate {
            token: token.clone(),
            closed: Arc::clone(&closed),
            listed: Arc::clone(&listed),
            cancel,
            in_flight: Arc::new(AtomicUsize::new(0)),
        };
        let handle = std::thread::spawn(move || accept_until_closed(&listener, &gate, &to, wrap));
        crate::trace!("bridge", "open on {addr}");
        Ok(Self {
            addr,
            token,
            closed,
            listed,
            listener: Some(handle),
        })
    }

    /// Whether the tool server reached this bridge and asked for the tools.
    ///
    /// An agent CLI lists its MCP servers' tools when it starts, before the
    /// model runs. A turn that ends without that request ran with none of
    /// incode's tools, whatever its reply says: the server never started or
    /// never got through (see [`require_tools`]).
    #[must_use]
    pub fn tools_listed(&self) -> bool {
        self.listed.load(Ordering::SeqCst)
    }

    /// What the agent CLI's environment must carry for its tool server to
    /// reach this bridge.
    #[must_use]
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            (ADDR_ENV, self.addr.to_string()),
            (TOKEN_ENV, self.token.clone()),
        ]
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
        crate::trace!("bridge", "closed {}", self.addr);
    }
}

/// What every connection is checked against.
struct Gate {
    token: String,
    closed: Arc<AtomicBool>,
    listed: Arc<AtomicBool>,
    cancel: Option<CancelToken>,
    in_flight: Arc<AtomicUsize>,
}

impl Gate {
    /// Why a request may not run now, if it may not.
    fn shut(&self) -> Option<&'static str> {
        if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Some("the turn was stopped, so this call was not run");
        }
        if self.closed.load(Ordering::SeqCst) {
            return Some("the turn this call belongs to has ended, so it was not run");
        }
        None
    }
}

fn accept_until_closed<M: Send + 'static>(
    listener: &TcpListener,
    gate: &Gate,
    to: &Sender<M>,
    wrap: fn(Request) -> M,
) {
    while !gate.closed.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if gate.in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
                    gate.in_flight.fetch_sub(1, Ordering::SeqCst);
                    let _ = refuse(stream, "too many tool calls at once; try again");
                    continue;
                }
                let conn = Gate {
                    token: gate.token.clone(),
                    closed: Arc::clone(&gate.closed),
                    listed: Arc::clone(&gate.listed),
                    cancel: gate.cancel.clone(),
                    in_flight: Arc::clone(&gate.in_flight),
                };
                let to = to.clone();
                std::thread::spawn(move || {
                    if let Err(e) = answer_connection(stream, &conn, &to, wrap) {
                        crate::trace!("bridge", "a connection failed: {e}");
                    }
                    conn.in_flight.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(e) => {
                crate::trace!("bridge", "accept failed: {e}");
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    }
}

fn refuse(mut stream: TcpStream, why: &str) -> std::io::Result<()> {
    writeln!(stream, "{}", json!({"ok": false, "error": why}))?;
    stream.flush()
}

/// Read one request, check it, hand it to the parent's loop, write the answer.
fn answer_connection<M>(
    stream: TcpStream,
    gate: &Gate,
    to: &Sender<M>,
    wrap: fn(Request) -> M,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(REQUEST_BOUND))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?.take(MAX_REQUEST_BYTES)).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return refuse(stream, "the request was too large or was cut off");
    }
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return refuse(stream, "the request was not JSON");
    };
    let given = request.get("token").and_then(Value::as_str).unwrap_or("");
    if !same_secret(given, &gate.token) {
        crate::trace!("bridge", "a request with a wrong token was refused");
        return refuse(
            stream,
            "refused: this request does not carry the turn's token",
        );
    }
    if let Some(why) = gate.shut() {
        return refuse(stream, why);
    }

    let answer = match request.get("op").and_then(Value::as_str) {
        Some("list") => {
            gate.listed.store(true, Ordering::SeqCst);
            let (reply, back) = std::sync::mpsc::channel();
            forward(to, wrap(Request::List { reply }));
            match back.recv() {
                Ok(Ok(tools)) => json!({"ok": true, "tools": tools}),
                Ok(Err(why)) => json!({"ok": false, "error": why}),
                Err(_) => json!({"ok": false, "error": ENDED}),
            }
        }
        Some("call") => {
            let name = request
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
            let (reply, back) = std::sync::mpsc::channel();
            forward(to, wrap(Request::Call { name, input, reply }));
            match back.recv() {
                // The text, and the whole answer as MCP content: the server
                // half hands the content on as it is, pictures included.
                Ok(Ok(output)) => {
                    json!({"ok": true, "output": output.text, "content": output.mcp_content()})
                }
                Ok(Err(why)) => json!({"ok": false, "error": why}),
                Err(_) => json!({"ok": false, "error": ENDED}),
            }
        }
        _ => json!({"ok": false, "error": "unknown request"}),
    };
    let mut stream = stream;
    writeln!(stream, "{answer}")?;
    stream.flush()
}

/// What a request is told when the loop it was handed to went away first.
const ENDED: &str = "the turn ended before this call ran, so it was not run";

/// Hand a request to the parent's loop. A loop that has already gone drops
/// the request, and with it the reply channel — which the waiting connection
/// reads as [`ENDED`].
fn forward<M>(to: &Sender<M>, message: M) {
    let _ = to.send(message);
}

/// Whether two secrets are equal, in time that does not depend on where they
/// first differ. An empty secret never matches.
fn same_secret(given: &str, expected: &str) -> bool {
    let (given, expected) = (given.as_bytes(), expected.as_bytes());
    if expected.is_empty() || given.len() != expected.len() {
        return false;
    }
    given
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// The toolbox's tools in MCP's spelling: `inputSchema` where the toolbox
/// writes `input_schema`. Nothing else changes, so the model is told exactly
/// what [`Toolbox::definitions`] says.
fn mcp_definitions(tools: &Toolbox) -> Vec<Value> {
    tools
        .definitions()
        .into_iter()
        .map(|mut def| {
            if let Some(object) = def.as_object_mut() {
                if let Some(schema) = object.remove("input_schema") {
                    object.insert("inputSchema".to_string(), schema);
                }
            }
            def
        })
        .collect()
}

/// Answer one request in the parent, on the thread that owns the turn.
///
/// A call is shown as incode's OWN tool call and result — `on_event` sees
/// them exactly as it sees a call on the API route — and is run through
/// [`Toolbox::run_call`], so the guards, the engine's hook and the person to
/// ask all apply. `call_id` names the call to the observer and to the engine.
pub fn answer(
    tools: &Toolbox,
    request: Request,
    call_id: &str,
    stopped: bool,
    on_event: &mut dyn FnMut(&AgentEvent),
) {
    match request {
        Request::List { reply } => {
            let definitions = mcp_definitions(tools);
            crate::trace!(
                "bridge",
                "the tool server asked for the tools: {} offered",
                definitions.len()
            );
            let _ = reply.send(Ok(definitions));
        }
        Request::Call { reply, .. } if stopped => {
            let _ = reply.send(Err(
                "the turn was stopped, so this call was not run".to_string()
            ));
        }
        Request::Call { name, input, reply } => {
            on_event(&AgentEvent::ToolUse {
                id: call_id.to_string(),
                name: name.clone(),
                input: input.to_string(),
            });
            let outcome = tools.run_call(call_id, &name, &input);
            // What incode shows and records: the text, with a placeholder
            // for each picture — never the pixels.
            let (output, is_error) = match &outcome {
                Ok(said) => (said.shown(), false),
                Err(why) => (why.clone(), true),
            };
            on_event(&AgentEvent::ToolResult {
                id: call_id.to_string(),
                output,
                is_error,
            });
            let _ = reply.send(outcome);
        }
    }
}

// ---------------------------------------------------------------------------
// The parent's turn: one loop for every CLI driven over the bridge.
// ---------------------------------------------------------------------------

/// What a cancelled turn reports, so the caller can tell it apart from a crash.
pub const CANCELLED: &str = "the turn was cancelled";

/// What a bridged turn's loop hears: a line the CLI printed, or a tool call
/// the model made, forwarded by the bridge.
pub enum Incoming {
    Line(String),
    ReadFailed(String),
    Ended,
    Tool(Request),
}

/// How long the loop waits for something to arrive before looking at the
/// cancel flag again. A turn that has gone quiet — the model thinking, a long
/// command running — still stops within this of being asked to.
pub const POLL: Duration = Duration::from_millis(100);

/// Read the CLI's output on its own thread, one line at a time, into the loop.
///
/// Its own thread because the loop must also answer tool calls, and the CLI
/// is BLOCKED on each of those until it is answered: a loop sitting in a read
/// of the CLI's output would never see the call the CLI is waiting on.
pub fn pump<R: BufRead + Send + 'static>(reader: R, to: Sender<Incoming>) {
    std::thread::spawn(move || {
        for line in reader.lines() {
            let message = match line {
                Ok(line) => Incoming::Line(line),
                Err(e) => {
                    let _ = to.send(Incoming::ReadFailed(e.to_string()));
                    return;
                }
            };
            if to.send(message).is_err() {
                return;
            }
        }
        let _ = to.send(Incoming::Ended);
    });
}

/// One turn's channel, opened before the CLI is started: the bridge, the
/// loop's two ends, and the binary the CLI must start as the tool server.
pub struct Turn {
    pub exe: std::path::PathBuf,
    pub bridge: Bridge,
    pub to: Sender<Incoming>,
    pub from: std::sync::mpsc::Receiver<Incoming>,
}

impl Turn {
    /// Open the channel for one turn, refused once `cancel` trips.
    ///
    /// # Errors
    ///
    /// This binary cannot be named, or the socket cannot be bound. Either
    /// way the turn is not started: without the bridge the model would have
    /// no tools but the CLI's, and falling back to those is the thing this
    /// route exists to avoid.
    pub fn open(cancel: Option<CancelToken>) -> Result<Self, crate::agent::AgentError> {
        let exe = std::env::current_exe().map_err(|e| {
            crate::agent::AgentError::new(format!(
                "incode could not name its own binary, so it cannot offer its tools; \
                 the turn was not started: {e}"
            ))
        })?;
        let (to, from) = std::sync::mpsc::channel();
        let bridge = Bridge::open(to.clone(), Incoming::Tool, cancel).map_err(|e| {
            crate::agent::AgentError::new(format!(
                "incode could not open the channel its tools run over, so the turn \
                 was not started: {e}"
            ))
        })?;
        Ok(Self {
            exe,
            bridge,
            to,
            from,
        })
    }
}

/// Refuse a finished turn whose tool server never asked for the tools.
///
/// Such a turn ran with none of incode's tools: the model had nothing to act
/// with, and a model with no tools may write tool calls out as plain text
/// that nothing ran. Its reply is not a result, so the turn fails and says
/// why rather than reporting success. Measured: under a Windows
/// write-restricted token (sbx, native), Node cannot give a child piped
/// stdio, so Claude Code starts no MCP server at all (#68).
///
/// Called by the Claude Code route (`agent/claude_code/turn.rs` `run_once`)
/// once its turn produced a result. Not by the codex route
/// (`agent/codex/bridged.rs`): whether codex always lists its servers' tools
/// before the model runs is not yet measured (#19).
///
/// # Errors
///
/// `listed` is false.
pub fn require_tools(listed: bool, agent: &str) -> Result<(), crate::agent::AgentError> {
    if listed {
        return Ok(());
    }
    Err(no_tools(agent))
}

/// Why a turn without incode's tools was stopped: the one wording for the
/// end-of-turn check ([`require_tools`]) and for a CLI that says at once
/// that the server failed (the Claude route's init line,
/// `agent/claude_code/wire.rs` `incode_server_failed`).
#[must_use]
pub fn no_tools(agent: &str) -> crate::agent::AgentError {
    crate::trace!("bridge", "{agent} never started incode's tool server");
    crate::agent::AgentError::new(format!(
        "{agent} never started incode's tool server, so the model had none of incode's tools \
         and its reply was not used. If it runs inside a sandbox, the sandbox may stop it \
         starting child processes; INCODE_TRACE=<file> shows the bridge's side"
    ))
}

/// What [`drive`] hands each line the CLI prints: the events it means, or
/// the error that ends the turn.
pub type LineHandler<'a> =
    dyn FnMut(String, &mut Toolbox) -> Result<Vec<AgentEvent>, crate::agent::AgentError> + 'a;

/// The turn itself: every tool call the model makes run by `tools`, and
/// every line the CLI prints handed to `on_line`, whose events are shown —
/// until the CLI finishes or the turn is stopped.
///
/// `on_line` is given the toolbox too, so a CLI that names its conversation
/// can tell the engine which one the calls belong to. It may end the turn
/// with an error: a line that says the turn cannot go on (the CLI reporting
/// that incode's tool server failed to start).
///
/// # Errors
///
/// [`CANCELLED`] when the turn was stopped; the CLI's output could not be
/// read; `on_line` refused a line.
pub fn drive(
    from: &std::sync::mpsc::Receiver<Incoming>,
    cancel: Option<&CancelToken>,
    tools: &mut Toolbox,
    calls: &mut u64,
    on_event: &mut dyn FnMut(&AgentEvent),
    on_line: &mut LineHandler<'_>,
) -> Result<(), crate::agent::AgentError> {
    let stopped = || cancel.is_some_and(CancelToken::is_cancelled);
    loop {
        if stopped() {
            return Err(crate::agent::AgentError::new(CANCELLED));
        }
        let line = match from.recv_timeout(POLL) {
            Ok(Incoming::Line(line)) => line,
            Ok(Incoming::Tool(request)) => {
                // Numbered per CALL: the list the server asks for first is
                // not a call, and must not use up an id.
                if matches!(request, Request::Call { .. }) {
                    *calls += 1;
                }
                let id = format!("incode-{calls}");
                answer(tools, request, &id, stopped(), on_event);
                continue;
            }
            Ok(Incoming::ReadFailed(e)) => {
                return Err(crate::agent::AgentError::new(format!(
                    "reading agent output: {e}"
                )))
            }
            Ok(Incoming::Ended) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        };
        for event in on_line(line, tools)? {
            on_event(&event);
        }
    }
    Ok(())
}

/// The MCP registration entry for the server half, for an `mcpServers` map.
///
/// Carries no token and no address: those reach the server through the
/// environment of the CLI that starts it, never through an argv that any
/// process on the machine can read.
#[must_use]
pub fn client_entry(exe: &std::path::Path) -> Value {
    json!({
        "command": exe.to_string_lossy(),
        "args": [SERVER_FLAG],
    })
}

// ---------------------------------------------------------------------------
// The server half: `incode --tool-server`, started by the agent CLI.
// ---------------------------------------------------------------------------

/// The tool server. Runs nothing itself: every request goes to the parent.
pub struct ToolServer {
    addr: SocketAddr,
    token: String,
}

impl ToolServer {
    /// The parent this server answers to, from the environment the agent CLI
    /// passed on.
    ///
    /// # Errors
    ///
    /// Either variable is missing, or the address is not a loopback one — a
    /// server told to send its token elsewhere sends nothing.
    fn from_env() -> Result<Self, String> {
        let addr = std::env::var(ADDR_ENV).map_err(|_| format!("{ADDR_ENV} is not set"))?;
        let token = std::env::var(TOKEN_ENV).map_err(|_| format!("{TOKEN_ENV} is not set"))?;
        Self::at(&addr, token)
    }

    /// A server answering to the parent at `addr`.
    fn at(addr: &str, token: String) -> Result<Self, String> {
        let addr: SocketAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("{ADDR_ENV} is not an address: `{addr}`"))?;
        if !addr.ip().is_loopback() {
            return Err(format!("{ADDR_ENV} must be a loopback address, got {addr}"));
        }
        if token.trim().is_empty() {
            return Err(format!("{TOKEN_ENV} is empty"));
        }
        Ok(Self { addr, token })
    }

    /// One request to the parent, and its answer.
    fn ask(&self, mut body: Value) -> Result<Value, String> {
        body["token"] = json!(self.token);
        let addr = self.addr;
        let reach = |e: std::io::Error| {
            crate::trace!(
                "bridge",
                "tool server could not reach incode at {addr}: {e}"
            );
            format!("incode could not be reached: {e}")
        };
        let mut stream = TcpStream::connect_timeout(&self.addr, CONNECT_BOUND).map_err(reach)?;
        stream.set_read_timeout(Some(REPLY_BOUND)).map_err(reach)?;
        writeln!(stream, "{body}").map_err(reach)?;
        stream.flush().map_err(reach)?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).map_err(reach)?;
        let answer: Value = serde_json::from_str(&line)
            .map_err(|_| "incode answered with something that is not JSON".to_string())?;
        if answer.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(answer);
        }
        Err(answer
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("incode refused the request and gave no reason")
            .to_string())
    }
}

impl crate::mcp_stdio::Handler for ToolServer {
    fn name(&self) -> &str {
        "incode-tools"
    }

    fn tools(&mut self) -> Result<Vec<Value>, String> {
        let answer = self.ask(json!({"op": "list"}))?;
        answer
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| "incode sent no tool list".to_string())
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<String, String> {
        self.call_content(name, args)
            .map(|blocks| ToolOutput::from_mcp_content(&blocks).text)
    }

    /// The parent's answer as MCP content, pictures included. Each picture
    /// is checked against the image bounds again here, on the way out.
    fn call_content(&mut self, name: &str, args: &Value) -> Result<Vec<Value>, String> {
        let answer = self.ask(json!({"op": "call", "name": name, "input": args}))?;
        match answer.get("content").and_then(Value::as_array) {
            Some(blocks) => Ok(ToolOutput::from_mcp_content(blocks).mcp_content()),
            None => Ok(ToolOutput::from(
                answer
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
            .mcp_content()),
        }
    }
}

/// Serve the tool bridge over stdin/stdout until the agent CLI closes it.
///
/// stdout carries protocol lines and nothing else; anything this process
/// has to say goes to stderr.
///
/// # Errors
///
/// The parent could not be named from the environment, or the pipe failed.
pub fn run_stdio() -> std::io::Result<()> {
    let mut server = ToolServer::from_env().map_err(|why| {
        crate::trace!("bridge", "tool server could not start: {why}");
        std::io::Error::new(std::io::ErrorKind::InvalidInput, why)
    })?;
    crate::trace!(
        "bridge",
        "tool server started (pid {}), answering to {}",
        std::process::id(),
        server.addr
    );
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stdout();
    crate::mcp_stdio::serve(&mut server, &mut input, &mut output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_stdio::Handler;
    use std::sync::mpsc::{channel, Receiver};

    /// What the fake agent loop receives: a request, or nothing more.
    fn open(cancel: Option<CancelToken>) -> (Bridge, Receiver<Request>) {
        let (tx, rx) = channel();
        let bridge = Bridge::open(tx, |r| r, cancel).expect("bind loopback");
        (bridge, rx)
    }

    /// A tool server pointed at `bridge`, as the agent CLI would start it.
    fn server_for(bridge: &Bridge) -> ToolServer {
        let [(_, addr), (_, token)] = bridge.env();
        ToolServer::at(&addr, token).expect("loopback")
    }

    /// The parent's loop, run on a thread over a real toolbox: answers
    /// requests until the bridge side hangs up, recording what it showed.
    fn parent_loop(
        tools: Toolbox,
        rx: Receiver<Request>,
    ) -> std::thread::JoinHandle<Vec<AgentEvent>> {
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut n = 0;
            while let Ok(request) = rx.recv_timeout(Duration::from_secs(20)) {
                n += 1;
                answer(&tools, request, &format!("incode-{n}"), false, &mut |e| {
                    seen.push(e.clone());
                });
            }
            seen
        })
    }

    /// Run `work` on a thread, so a test can look at the parent's side while
    /// the request is in flight — and is not hung by one that never returns.
    fn in_the_background<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Receiver<T> {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx
    }

    fn mcp(server: &mut ToolServer, line: &str) -> Value {
        let said = crate::mcp_stdio::handle_line(server, line).expect("answered");
        serde_json::from_str(&said).expect("json")
    }

    /// **THE LIST THE MODEL SEES IS THE PARENT'S TOOLBOX**, in MCP's spelling.
    #[test]
    fn the_tool_list_comes_from_the_parents_toolbox() {
        let ws = tempfile::tempdir().expect("ws");
        let (bridge, rx) = open(None);
        let expected = Toolbox::new(ws.path(), true).names();
        let parent = parent_loop(Toolbox::new(ws.path(), true), rx);

        let mut server = server_for(&bridge);
        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        let tools = got["result"]["tools"].as_array().expect("a list");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, expected, "the list must be the toolbox's, in order");
        for tool in tools {
            assert!(tool.get("inputSchema").is_some(), "MCP spelling: {tool}");
            assert!(tool.get("input_schema").is_none(), "{tool}");
        }
        drop(bridge);
        drop(server);
        let _ = parent.join();
    }

    /// **THE NEW TOOLS REACH THE MODEL THROUGH THE BRIDGE**, named as models
    /// know them — and one of them runs in the parent when called.
    #[test]
    fn the_phase_five_tools_are_listed_over_the_bridge() {
        let ws = tempfile::tempdir().expect("ws");
        let (bridge, rx) = open(None);
        let parent = parent_loop(Toolbox::new(ws.path(), true), rx);
        let mut server = server_for(&bridge);
        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        let names: Vec<&str> = got["result"]["tools"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in [
            "MultiEdit",
            "ApplyPatch",
            "LS",
            "NotebookEdit",
            "BashOutput",
            "KillShell",
            "TodoWrite",
            "Diagnostics",
            "WebFetch",
            "WebSearch",
            "watchman_lookup",
            "team_ask",
            "team_read",
        ] {
            assert!(names.contains(&expected), "{expected} missing: {names:?}");
        }
        let listed = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"TodoWrite","arguments":{"todos":[{"content":"one","status":"pending"}]}}}"#,
        );
        assert_eq!(listed["result"]["isError"], false, "{listed}");
        drop(bridge);
        drop(server);
        let _ = parent.join();
    }

    /// The names in the model's tool list, for a parent toolbox of `tools`.
    fn listed_names(tools: Toolbox) -> Vec<String> {
        let (bridge, rx) = open(None);
        let parent = parent_loop(tools, rx);
        let mut server = server_for(&bridge);
        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        drop(bridge);
        drop(server);
        let _ = parent.join();
        got["result"]["tools"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect()
    }

    /// **OUTLINE REACHES THE MODEL; CONVERTDOCUMENT ONLY WITH LIBREOFFICE.**
    /// And a document Read and an Outline run in the parent when called, with
    /// a malformed call refused by the schema before it runs.
    #[test]
    fn the_phase_six_tools_are_listed_over_the_bridge_and_run_in_the_parent() {
        let ws = tempfile::tempdir().expect("ws");
        let without = listed_names(Toolbox::new(ws.path(), true).with_soffice(None));
        assert!(without.iter().any(|n| n == "Outline"), "{without:?}");
        assert!(
            !without.iter().any(|n| n == "ConvertDocument"),
            "{without:?}"
        );
        let with = listed_names(
            Toolbox::new(ws.path(), true).with_soffice(Some(ws.path().join("soffice"))),
        );
        assert!(with.iter().any(|n| n == "ConvertDocument"), "{with:?}");

        std::fs::write(ws.path().join("m.rs"), "pub fn marker() {}\n").expect("seed");
        std::fs::write(
            ws.path().join("d.docx"),
            crate::tools::documents::fixtures::docx(crate::tools::documents::fixtures::DOCX_BODY),
        )
        .expect("seed");
        let (bridge, rx) = open(None);
        let parent = parent_loop(Toolbox::new(ws.path(), true).with_soffice(None), rx);
        let mut server = server_for(&bridge);
        let outline = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"Outline","arguments":{"path":"m.rs"}}}"#,
        );
        assert_eq!(outline["result"]["isError"], false, "{outline}");
        assert!(outline.to_string().contains("fn marker [L1]"), "{outline}");
        let read = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"Read","arguments":{"path":"d.docx"}}}"#,
        );
        assert!(read.to_string().contains("# Quarterly Report"), "{read}");
        let bad = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"Outline","arguments":{}}}"#,
        );
        assert_eq!(bad["result"]["isError"], true, "{bad}");
        assert!(
            bad.to_string().contains("does not match its schema"),
            "{bad}"
        );
        drop(bridge);
        drop(server);
        let _ = parent.join();
    }

    /// **THE BROWSER TOOLS AND THE DESKTOP REACH THE MODEL ONLY WHERE THEY
    /// CAN RUN.** Screenshot and RenderPdf with a headless browser, the
    /// desktop's tools with the desktop switched on — and none of them
    /// otherwise.
    #[test]
    fn the_phase_six_b_tools_are_listed_only_where_they_can_run() {
        let ws = tempfile::tempdir().expect("ws");
        let bare = listed_names(Toolbox::new(ws.path(), true).with_browser(None));
        for absent in ["Screenshot", "RenderPdf", "screenshot", "click", "ui_tree"] {
            assert!(!bare.iter().any(|n| n == absent), "{absent}: {bare:?}");
        }
        let mut full = Toolbox::new(ws.path(), true).with_browser(Some(ws.path().join("obscura")));
        full.enable_desktop(crate::toolbox::DesktopSetup {
            exe: ws.path().join("incode"),
            journal: ws.path().join("desktop.jsonl"),
        });
        let listed = listed_names(full);
        for present in [
            "Screenshot",
            "RenderPdf",
            "screen_info",
            "screenshot",
            "ui_tree",
            "cursor_position",
            "click_element",
            "type_text",
            "paste_text",
            "key",
            "scroll",
            "drag",
            "move_mouse",
            "click",
        ] {
            assert!(listed.iter().any(|n| n == present), "{present}: {listed:?}");
        }
    }

    /// **A CALL IS RUN BY THE PARENT'S TOOLBOX**, and the parent shows it as
    /// its own call and result.
    #[test]
    fn a_call_is_executed_by_the_parents_toolbox_and_shown_as_incodes_own() {
        let ws = tempfile::tempdir().expect("ws");
        let (bridge, rx) = open(None);
        let parent = parent_loop(Toolbox::new(ws.path(), true), rx);
        let mut server = server_for(&bridge);

        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"Write","arguments":{"path":"hello.txt","content":"hi"}}}"#,
        );
        assert_eq!(got["result"]["isError"], false, "{got}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("hello.txt")).expect("written by the parent"),
            "hi"
        );
        drop(bridge);
        drop(server);
        let seen = parent.join().expect("the parent loop");
        assert!(
            matches!(&seen[0], AgentEvent::ToolUse { name, .. } if name == "Write"),
            "{seen:?}"
        );
        assert!(
            matches!(
                &seen[1],
                AgentEvent::ToolResult {
                    is_error: false,
                    ..
                }
            ),
            "{seen:?}"
        );
    }

    /// Set only in the child process the spec-gate test starts.
    const SPEC_GATE_CHILD: &str = "INCODE_SPEC_GATE_BRIDGE_CHILD";

    /// The child half: a process whose run said `--spec-gate=off`, as
    /// `task_run` says it, then a source write from the agent CLI's side of
    /// the bridge into a workspace with no spec set. Prints what came back.
    #[test]
    fn spec_gate_off_bridge_child() {
        let Some(ws) = std::env::var_os(SPEC_GATE_CHILD) else {
            return;
        };
        let ws = std::path::PathBuf::from(ws);
        crate::spec::set_for_this_run(crate::spec::SpecGate::Off);
        let (bridge, rx) = open(None);
        // Built the way a backend builds it: nothing passes the gate in.
        let parent = parent_loop(Toolbox::new(&ws, true), rx);
        let mut server = server_for(&bridge);
        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"Write","arguments":{"path":"src/fix.rs","content":"fn fixed() {}"}}}"#,
        );
        println!("BRIDGE-ANSWER {got}");
        drop(bridge);
        drop(server);
        let _ = parent.join();
        std::process::exit(0);
    }

    /// **A `-t --spec-gate=off` RUN'S AGENT CLI WRITES SOURCE THROUGH THE
    /// TOOL SERVER.** The tool server is a separate process the CLI starts,
    /// and it runs nothing: the write is decided by the parent's toolbox. So
    /// the off state must reach the toolboxes the run builds — not the tool
    /// server's environment — and this proves it does, end to end, in a
    /// process of its own because the run's gate is process state.
    #[test]
    fn a_run_with_the_spec_gate_off_writes_source_through_the_tool_server() {
        let ws = tempfile::tempdir().expect("ws");
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tool_bridge::tests::spec_gate_off_bridge_child",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .env(SPEC_GATE_CHILD, ws.path())
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains("BRIDGE-ANSWER"),
            "the child did not run: {stdout}\n{stderr}"
        );
        assert!(
            !stdout.contains("not specified yet"),
            "the write was refused by the spec gate: {stdout}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path().join("src/fix.rs")).unwrap_or_default(),
            "fn fixed() {}",
            "{stdout}\n{stderr}"
        );
        // And in this process, which no run set, the same write is refused.
        let (bridge, rx) = open(None);
        let parent = parent_loop(Toolbox::new(ws.path(), true), rx);
        let mut server = server_for(&bridge);
        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"Write","arguments":{"path":"src/other.rs","content":"x"}}}"#,
        );
        assert_eq!(got["result"]["isError"], true, "{got}");
        assert!(got.to_string().contains("not specified yet"), "{got}");
        drop(bridge);
        drop(server);
        let _ = parent.join();
    }

    /// **THE MODEL GETS THE PICTURE; INCODE SHOWS A PLACEHOLDER.** A tool
    /// that answers with an image, through the bridge: the MCP result the CLI
    /// receives carries an `image` block with the PNG, and the result event
    /// incode draws and records carries `[image WxH png]` and no pixels.
    #[test]
    fn the_bridge_hands_the_model_a_picture_and_incode_a_placeholder() {
        use std::sync::atomic::AtomicUsize;
        let ws = tempfile::tempdir().expect("ws");
        let png = crate::desktop::capture::encode(
            &image::RgbaImage::from_pixel(32, 24, image::Rgba([10, 20, 30, 255])),
            crate::desktop::capture::ShotFormat::Png,
            0,
        )
        .expect("png");
        let tools = Toolbox::new(ws.path(), true)
            .with_desktop_picture(png.clone(), Arc::new(AtomicUsize::new(0)));
        let (bridge, rx) = open(None);
        let parent = parent_loop(tools, rx);
        let mut server = server_for(&bridge);

        let listed = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        assert!(
            listed.to_string().contains("\"screenshot\""),
            "the desktop's screenshot is listed: {listed}"
        );
        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"screenshot","arguments":{}}}"#,
        );
        assert_eq!(got["result"]["isError"], false, "{got}");
        let blocks = got["result"]["content"].as_array().expect("content");
        assert_eq!(blocks[0]["type"], "text", "{got}");
        assert_eq!(blocks[1]["type"], "image", "{got}");
        assert_eq!(blocks[1]["mimeType"], "image/png");
        use base64::Engine as _;
        let sent = base64::engine::general_purpose::STANDARD
            .decode(blocks[1]["data"].as_str().expect("data"))
            .expect("base64");
        assert_eq!(sent, png, "the model got the picture the tool made");

        drop(bridge);
        drop(server);
        let seen = parent.join().expect("the parent loop");
        let recorded = seen
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .expect("a result event");
        assert!(recorded.contains("[image 32x24 png]"), "{recorded}");
        assert!(
            !recorded.contains(blocks[1]["data"].as_str().unwrap_or("")),
            "the event carries the pixels"
        );
    }

    /// **A GUARD REFUSAL COMES BACK AS AN ERROR RESULT**, not a transport
    /// failure, and nothing is written.
    #[test]
    fn a_guard_refusal_reaches_the_model_as_an_error_result() {
        let ws = tempfile::tempdir().expect("ws");
        std::fs::create_dir_all(ws.path().join("tests")).expect("dir");
        let (bridge, rx) = open(None);
        let parent = parent_loop(Toolbox::new(ws.path(), true), rx);
        let mut server = server_for(&bridge);

        let got = mcp(
            &mut server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"Write","arguments":{"path":"tests/it.rs","content":"x"}}}"#,
        );
        assert!(got.get("error").is_none(), "a refusal is a result: {got}");
        assert_eq!(got["result"]["isError"], true, "{got}");
        let said = got["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(said.contains("protected"), "{said}");
        assert!(
            !ws.path().join("tests/it.rs").exists(),
            "nothing was written"
        );
        drop(bridge);
        drop(server);
        let seen = parent.join().expect("the parent loop");
        assert!(
            matches!(&seen[1], AgentEvent::ToolResult { is_error: true, .. }),
            "{seen:?}"
        );
    }

    /// **A WRONG TOKEN RUNS NOTHING.** The request never reaches the loop.
    #[test]
    fn a_bad_token_is_refused_before_anything_runs() {
        let ws = tempfile::tempdir().expect("ws");
        let (bridge, rx) = open(None);
        let [(_, addr), (_, token)] = bridge.env();
        let mut forged = ToolServer::at(&addr, "0".repeat(token.len())).expect("loopback");

        let answered = in_the_background(move || {
            forged.call("Write", &json!({"path": "forged.txt", "content": "x"}))
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "the request reached the parent's loop"
        );
        let refused = answered
            .recv_timeout(Duration::from_secs(5))
            .expect("answered")
            .expect_err("a wrong token must be refused");
        assert!(refused.contains("token"), "{refused}");
        assert!(!ws.path().join("forged.txt").exists());

        // The control: the SAME call with the real token does reach it.
        let mut real = server_for(&bridge);
        std::thread::spawn(move || {
            let _ = real.call("Write", &json!({"path": "real.txt", "content": "x"}));
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "with the right token the request must arrive"
        );
    }

    /// **A CALL AFTER THE TURN WAS STOPPED IS REFUSED**, at the bridge, and
    /// never reaches the toolbox.
    #[test]
    fn a_call_after_cancel_is_refused() {
        let ws = tempfile::tempdir().expect("ws");
        let token = CancelToken::new();
        let (bridge, rx) = open(Some(token.clone()));
        let mut server = server_for(&bridge);
        token.cancel();

        let answered = in_the_background(move || {
            server.call("Write", &json!({"path": "late.txt", "content": "x"}))
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "the request reached the parent's loop"
        );
        let refused = answered
            .recv_timeout(Duration::from_secs(5))
            .expect("answered")
            .expect_err("a stopped turn runs nothing");
        assert!(refused.contains("stopped"), "{refused}");
        assert!(!ws.path().join("late.txt").exists());
    }

    /// The parent's own check: a call handed over as the turn stops is
    /// answered as refused, with no event and nothing run.
    #[test]
    fn the_parent_refuses_a_call_it_receives_once_stopped() {
        let ws = tempfile::tempdir().expect("ws");
        let tools = Toolbox::new(ws.path(), true);
        let (reply, back) = channel();
        let mut seen = Vec::new();
        answer(
            &tools,
            Request::Call {
                name: "Write".to_string(),
                input: json!({"path": "late.txt", "content": "x"}),
                reply,
            },
            "incode-1",
            true,
            &mut |e| seen.push(e.clone()),
        );
        let said = back.recv().expect("answered").expect_err("refused");
        assert!(said.contains("stopped"), "{said}");
        assert!(
            seen.is_empty(),
            "nothing is shown for a call that did not run"
        );
        assert!(!ws.path().join("late.txt").exists());
    }

    /// A call that reaches a bridge whose turn has ended is refused, rather
    /// than left waiting on a loop that is gone.
    #[test]
    fn a_call_after_the_turn_ended_is_refused() {
        let (bridge, rx) = open(None);
        let mut server = server_for(&bridge);
        drop(rx);
        let refused = server
            .call("Read", &json!({"path": "x"}))
            .expect_err("nobody is there to run it");
        assert!(refused.contains("not run"), "{refused}");
        drop(bridge);
        let gone = server
            .call("Read", &json!({"path": "x"}))
            .expect_err("the bridge is closed");
        assert!(
            gone.contains("could not be reached") || gone.contains("not run"),
            "{gone}"
        );
    }

    /// **THE ENGINE HOOK AND THE PERSON ARE REACHED THROUGH THE BRIDGE.** An
    /// engine that asks, and a person at the session who allows: the write
    /// lands, the engine was asked once and the person once.
    #[test]
    fn the_engine_hook_and_the_prompter_are_reached_through_the_bridge() {
        use crate::hooks::engine::fake::{self, Reply};
        use crate::hooks::engine::{Ask, Prompter, Verdict};

        struct Yes(Arc<AtomicUsize>);
        impl Prompter for Yes {
            fn confirm(&self, _: &Ask<'_>, _: &dyn Fn() -> bool) -> Option<Verdict> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Some(Verdict::Allow)
            }
            fn choose(&self, _: &[crate::ask::Question]) -> Option<Vec<crate::ask::Answer>> {
                None
            }
        }

        let (ws, engine) = (
            tempfile::tempdir().expect("ws"),
            tempfile::tempdir().expect("e"),
        );
        let ask = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"confirm this write"}}"#;
        let bin = fake::write(
            engine.path(),
            &[
                ("pre-tool", Reply::exit(0).stdout(ask)),
                ("permission-request", Reply::exit(0).after(30)),
            ],
        );
        let asked = Arc::new(AtomicUsize::new(0));
        let mut tools = Toolbox::new(ws.path(), true)
            .with_engine(Some(fake::engine(bin).capped(Duration::from_secs(60))));
        tools.set_prompter(Arc::new(Yes(Arc::clone(&asked))));

        let (bridge, rx) = open(None);
        let parent = parent_loop(tools, rx);
        let mut server = server_for(&bridge);
        let got = server
            .call("Write", &json!({"path": "asked.txt", "content": "yes"}))
            .expect("the person allowed it");
        assert!(!got.is_empty(), "{got}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("asked.txt")).expect("written"),
            "yes"
        );
        assert_eq!(
            fake::calls(engine.path(), "pre-tool"),
            1,
            "the engine hook ran"
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1, "the person was asked");
        drop(bridge);
        drop(server);
        let _ = parent.join();
    }

    #[test]
    fn a_server_will_not_send_its_token_off_the_machine() {
        let refused = ToolServer::at("203.0.113.9:4000", "t".to_string())
            .err()
            .expect("not loopback");
        assert!(refused.contains("loopback"), "{refused}");
        assert!(ToolServer::at("127.0.0.1:4000", "t".to_string()).is_ok());
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret("abcd", "abcd"));
        assert!(!same_secret("abce", "abcd"));
        assert!(!same_secret("abc", "abcd"));
        assert!(!same_secret("", ""));
    }

    #[test]
    fn the_registration_carries_no_secret() {
        let entry = client_entry(std::path::Path::new("incode"));
        let text = entry.to_string();
        assert!(text.contains(SERVER_FLAG), "{text}");
        assert!(
            !text.contains(TOKEN_ENV) && !text.contains(ADDR_ENV),
            "{text}"
        );
    }
}
