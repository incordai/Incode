//! The MCP framing incode's own stdio servers share.
//!
//! # Why this is its own module
//!
//! Every stdio server this crate runs answers the same few methods the same
//! way: `initialize`, `ping`, `tools/list`, `tools/call`, one JSON-RPC message
//! per line, notifications left unanswered. The team channel carried its own
//! copy of that, and the tool bridge needs it too — a second copy is one that
//! drifts, and the copy that is called is not always the one being read.
//!
//! What differs per server is what its tools ARE, which is the [`Handler`].
//! The desktop server keeps its own loop for now: it answers with images and
//! trips a kill switch when its client goes away, neither of which the others
//! do.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

/// The protocol version these servers answer with. Negotiated down to this
/// by the live clients when they ask for a newer one — measured for the
/// desktop server and relied on by each server since.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC error codes, as the specification numbers them.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// One server's tools, behind the shared framing.
pub trait Handler {
    /// The name the client is told this server has.
    fn name(&self) -> &str;

    /// What the client is told before any tool is listed, if anything.
    fn instructions(&self) -> Option<&str> {
        None
    }

    /// Every tool, in MCP's shape (`inputSchema`). An `Err` is a server that
    /// cannot say what it offers, and is answered as a protocol error.
    fn tools(&mut self) -> Result<Vec<Value>, String>;

    /// Run one tool. A refusal or a failure is `Err`, and reaches the model
    /// as a result marked `isError` — never as a transport error, which a
    /// client answers by retrying the connection instead of reading why.
    fn call(&mut self, name: &str, args: &Value) -> Result<String, String>;
}

/// Answer one line of JSON-RPC. `None` for a notification, or a blank line.
pub fn handle_line(handler: &mut dyn Handler, line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let request: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                code::PARSE_ERROR,
                &format!("invalid JSON: {e}"),
            ))
        }
    };

    let id = request.get("id").cloned();
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Some(error_response(
            id.unwrap_or(Value::Null),
            code::INVALID_REQUEST,
            "no method",
        ));
    };
    // A notification has no id and must not be answered.
    let id = id?;
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    Some(match dispatch(handler, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        Err((code, message)) => error_response(id, code, &message),
    })
}

fn dispatch(
    handler: &mut dyn Handler,
    method: &str,
    params: &Value,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let mut result = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {"name": handler.name(), "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"tools": {"listChanged": false}},
            });
            if let Some(said) = handler.instructions() {
                result["instructions"] = json!(said);
            }
            Ok(result)
        }
        "ping" => Ok(json!({})),
        "tools/list" => handler
            .tools()
            .map(|tools| json!({ "tools": tools }))
            .map_err(|why| (code::INTERNAL_ERROR, why)),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).ok_or((
                code::INVALID_PARAMS,
                "tools/call needs a tool name".to_string(),
            ))?;
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Ok(match handler.call(name, &args) {
                Ok(body) => json!({"content": [text(body)], "isError": false}),
                Err(why) => json!({"content": [text(why)], "isError": true}),
            })
        }
        other => Err((
            code::METHOD_NOT_FOUND,
            format!("method '{other}' not found"),
        )),
    }
}

/// A text content block.
fn text(body: String) -> Value {
    json!({"type": "text", "text": body})
}

/// A JSON-RPC error, as one line.
fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
    .to_string()
}

/// Serve `handler` over a line pipe until the client closes it.
///
/// Nothing but protocol lines is ever written to `output`: it is the client's
/// channel, and anything else there is a parse error at the other end.
///
/// # Errors
///
/// An I/O failure on either side of the pipe.
pub fn serve(
    handler: &mut dyn Handler,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> std::io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if let Some(reply) = handle_line(handler, &line) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server with one tool that echoes, and one that always refuses.
    struct Echo;

    impl Handler for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn instructions(&self) -> Option<&str> {
            Some("say it back")
        }
        fn tools(&mut self) -> Result<Vec<Value>, String> {
            Ok(vec![
                json!({"name": "echo", "inputSchema": {"type": "object"}}),
            ])
        }
        fn call(&mut self, name: &str, args: &Value) -> Result<String, String> {
            match name {
                "echo" => Ok(args["text"].as_str().unwrap_or_default().to_string()),
                other => Err(format!("no such tool: {other}")),
            }
        }
    }

    fn reply(line: &str) -> Value {
        let said = handle_line(&mut Echo, line).expect("a request is answered");
        serde_json::from_str(&said).expect("the answer is JSON")
    }

    #[test]
    fn initialize_names_the_server_and_carries_its_instructions() {
        let got = reply(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        assert_eq!(got["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(got["result"]["serverInfo"]["name"], "echo");
        assert_eq!(got["result"]["instructions"], "say it back");
    }

    #[test]
    fn a_notification_is_never_answered() {
        assert!(handle_line(
            &mut Echo,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .is_none());
        assert!(handle_line(&mut Echo, "   ").is_none());
    }

    #[test]
    fn a_refused_tool_is_a_result_not_a_protocol_error() {
        let got =
            reply(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope"}}"#);
        assert!(got.get("error").is_none(), "{got}");
        assert_eq!(got["result"]["isError"], true);
        let ok = reply(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#,
        );
        assert_eq!(ok["result"]["isError"], false);
        assert_eq!(ok["result"]["content"][0]["text"], "hi");
    }

    #[test]
    fn malformed_and_unknown_requests_get_their_codes() {
        assert_eq!(reply("{not json")["error"]["code"], code::PARSE_ERROR);
        assert_eq!(reply(r#"{"id":4}"#)["error"]["code"], code::INVALID_REQUEST);
        assert_eq!(
            reply(r#"{"id":5,"method":"resources/list"}"#)["error"]["code"],
            code::METHOD_NOT_FOUND
        );
        assert_eq!(
            reply(r#"{"id":6,"method":"tools/call","params":{}}"#)["error"]["code"],
            code::INVALID_PARAMS
        );
    }

    #[test]
    fn serve_writes_only_protocol_lines() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            "\n"
        );
        let mut out = Vec::new();
        serve(&mut Echo, &mut input.as_bytes(), &mut out).expect("serves");
        let text = String::from_utf8(out).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "one request, one line: {text}");
        let only: Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(only["id"], 1);
    }
}
