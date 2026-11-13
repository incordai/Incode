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
pub fn run_stdio(mut server: Server) -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            // EOF: the client is gone. Halt the desktop before returning, so a
            // slow in-flight action cannot outlive the thing that asked for it.
            Ok(0) => {
                server.safety().stop();
                return Ok(());
            }
            Ok(_) => {
                // Every request received is evidence the client is alive.
                server.safety().heartbeat();

                if let Some(reply) = server.handle_line(&line) {
                    writeln!(stdout, "{reply}")?;
                    stdout.flush()?;
                }
            }
            Err(e) => {
                server.safety().stop();
                return Err(e);
            }
        }
    }
}
