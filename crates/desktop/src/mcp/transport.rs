//! The JSON-RPC side of the desktop server: one line in, at most one line
//! out ([`Server::handle_line`]), the method table behind it, the error
//! envelope, and the stdio loop that feeds it ([`run_stdio`]).
//!
//! # Why this is a file of its own
//!
//! Root cause: `desktop/mcp.rs` was 2,375 lines, over the code auditor's
//! 1,000-line file threshold, because it mixed this transport with the MCP
//! protocol, the per-call gates, the tools and the screen guards. Framing a
//! request and answering it is a different job from deciding what a tool
//! call may do, and the module doc of [`super`] already promised the split
//! ("Splitting the protocol from the pipe"); this file is where it now
//! lives. Moved unchanged: every reply shape and error code is as it was.
//!
//! # Wired to
//!
//! - **Called by** `main.rs` (`Parsed::DesktopMcp`) through [`run_stdio`],
//!   which [`super`] re-exports, and by `toolbox`'s picture tests and this
//!   module's own tests through [`Server::handle_line`].
//! - **Calls** `Server::call_tool` (in [`super`]) for `tools/call`, and
//!   [`super::tool_definitions`] for `tools/list`.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use super::{tool_definitions, Server, UNTRUSTED};

/// Negotiated down to this by the live server when asked for `2025-06-18`.
pub(super) const PROTOCOL_VERSION: &str = "2024-11-05";

pub(super) const SERVER_NAME: &str = "incode-desktop";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Standard JSON-RPC 2.0 codes, confirmed by the live server's -32601.
pub(super) mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
}

/// A request the server could not answer as JSON-RPC: the code and message
/// of the `error` member [`error_response`] writes.
pub(super) struct Failure {
    pub(super) code: i64,
    pub(super) message: String,
}

impl Server {
    /// Handle one line of JSON-RPC.
    ///
    /// `None` means "no reply" — correct for a notification, which by
    /// specification has no `id` and must not be answered. Answering one is a
    /// protocol error that some clients treat as fatal.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            // No id is recoverable from unparseable text, so the error carries a
            // null id, which is what JSON-RPC prescribes for exactly this case.
            Err(e) => {
                return Some(error_response(
                    Value::Null,
                    code::PARSE_ERROR,
                    &format!("invalid JSON: {e}"),
                ))
            }
        };

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);

        let Some(method) = method else {
            return Some(error_response(
                id.unwrap_or(Value::Null),
                code::INVALID_REQUEST,
                "no method",
            ));
        };

        let params = request.get("params").cloned().unwrap_or(Value::Null);

        // A notification has no id. Dispatch for the side effect, answer nothing.
        let Some(id) = id else {
            self.notify(method);
            return None;
        };

        Some(match self.dispatch(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
            Err(failure) => error_response(id, failure.code, &failure.message),
        })
    }

    /// A notification's side effect. Called by [`Server::handle_line`] for a
    /// message with no `id`.
    fn notify(&mut self, method: &str) {
        if method == "notifications/initialized" {
            self.initialised = true;
        }
    }

    /// The method table. Called by [`Server::handle_line`] for a request.
    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, Failure> {
        match method {
            "initialize" => {
                self.initialised = true;
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                    "capabilities": {"tools": {"listChanged": false}},
                    "instructions": UNTRUSTED,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_definitions()})),
            "tools/call" => self.call_tool(params),
            other => Err(Failure {
                code: code::METHOD_NOT_FOUND,
                message: format!("method '{other}' not found"),
            }),
        }
    }
}

/// A JSON-RPC error reply. Called by [`Server::handle_line`].
pub(super) fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
    .to_string()
}

/// Serve MCP over stdin/stdout until the client goes away.
///
/// # The EOF is the dead-man's switch
///
/// This is the liveness signal `super::safety::Supervisor` was built to take.
/// When the agent process dies its end of the pipe closes, `read_line` returns
/// zero bytes, and no timer can fake that. So the loop stops the desktop on EOF
/// rather than relying on a heartbeat that could keep beating into the void.
pub fn run_stdio(server: Server) -> std::io::Result<()> {
    run_lines(server, std::io::stdin(), std::io::stdout())
}

/// [`run_stdio`] over any reader and writer.
///
/// # Why input is read on a thread of its own
///
/// The server handles one call at a time on this thread, and a `watch` holds the
/// call for up to a day. With the read inline, a client that went away in the
/// middle of one would not be noticed until the call returned: the pipe's EOF
/// sat unread behind it. So a reader thread takes the lines and the EOF, passes
/// the lines on, and raises the server's `client_gone` flag at EOF, which the
/// waiting calls look at as often as they look at the stop file. The EOF is
/// still only ACTED on here, after every line before it has been answered,
/// exactly as it always was.
///
/// # Errors
///
/// Reading or writing failed.
pub(crate) fn run_lines<R, W>(server: Server, input: R, mut out: W) -> std::io::Result<()>
where
    R: std::io::Read + Send + 'static,
    W: Write,
{
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    let gone = Arc::new(AtomicBool::new(false));
    let server = server.with_client_gone(gone.clone());
    let (lines, queued) = mpsc::channel::<std::io::Result<String>>();
    std::thread::Builder::new()
        .name("incode-desktop-stdin".to_string())
        .spawn(move || {
            let mut reader = std::io::BufReader::new(input);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        gone.store(true, Ordering::SeqCst);
                        return;
                    }
                    Ok(_) => {
                        if lines.send(Ok(line)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        gone.store(true, Ordering::SeqCst);
                        let _ = lines.send(Err(e));
                        return;
                    }
                }
            }
        })?;
    let mut server = server;
    loop {
        match queued.recv() {
            Ok(Ok(line)) => {
                // Every request received is evidence the client is alive.
                server.safety().heartbeat();

                if let Some(reply) = server.handle_line(&line) {
                    writeln!(out, "{reply}")?;
                    out.flush()?;
                }
            }
            // The reader failed: halt the desktop before returning.
            Ok(Err(e)) => {
                server.safety().stop();
                return Err(e);
            }
            // EOF, every line before it answered: the client is gone. Halt the
            // desktop before returning, so a slow in-flight action cannot outlive
            // the thing that asked for it.
            Err(_) => {
                server.safety().stop();
                return Ok(());
            }
        }
    }
}
