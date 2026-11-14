//! Watchman's records, read from the local memory engine.
//!
//! # The endpoint, as it was measured
//!
//! `GET /v1/memory/watchman/record/{id}` on the engine (default
//! `http://127.0.0.1:9180`), read-only by the engine's own design. Checked
//! against a running engine on 2026-11-13. It answers `200` with
//! `content-type: application/json` and a `content-length`, and a record is:
//!
//! ```text
//! { "record_id": "w-<24 hex>", "started_at_ms": <int>, "task": ..., "spans": [
//!     { "span_id": 1, "seq": 1, "kind": "ToolCall", "name": "Bash",
//!       "input": { ... }, "output": <string | null | array | object>,
//!       "error": <string | null>, "started_at_ms": <int>, "attrs": {} } ],
//!   ... }
//! ```
//!
//! An id the engine does not hold answers `200` with `{"error": "no record
//! \"...\""}`, so the body, not the status, tells. `output` is NOT always text
//! (a search tool's answer was an array), and `error` is a string that is
//! itself JSON-quoted. A tool result flagged `is_error` becomes the span's
//! `error`; the capture keeps no images and cuts text at 4 KiB.
//!
//! Only `spans` with `kind` `ToolCall` are read, in `seq` order; every other
//! field is ignored, so the engine may add fields without breaking this.
//!
//! # Plain HTTP, to this machine only
//!
//! The crate has no HTTP client and the engine is local, so this speaks
//! `HTTP/1.1` over a [`std::net::TcpStream`], with `Connection: close`. It
//! refuses any host that is not a loopback address: a record becomes steps that
//! are replayed on the operator's desktop, so records may only come from the
//! operator's own machine. The engine's URL is `INCORD_MEMORY_URL` (the variable
//! incode uses), else the default above.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use super::source::{Call, Record, StepSource};

/// The variable that names the engine, the one incode reads.
pub const MEMORY_URL_ENV: &str = "INCORD_MEMORY_URL";

/// Where the engine is when nothing says.
pub const DEFAULT_MEMORY_URL: &str = "http://127.0.0.1:9180";

/// The most of an answer that is read, in bytes.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// How long to wait to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait for each read or write.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Records read from the local memory engine's Watchman endpoints.
#[derive(Debug, Clone)]
pub struct WatchmanSource {
    base: String,
}

impl WatchmanSource {
    /// A source for the engine at `base` (`http://host:port`).
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// A source for the engine `INCORD_MEMORY_URL` names, else the default.
    #[must_use]
    pub fn from_env() -> Self {
        let url = std::env::var(MEMORY_URL_ENV)
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MEMORY_URL.to_string());
        Self::new(url.trim())
    }
}

impl StepSource for WatchmanSource {
    fn record(&self, id: &str) -> Result<Record, String> {
        if !valid_id(id) {
            return Err(format!(
                "{id:?} is not a Watchman record id (letters, digits, `-` and `_`, at most 64)"
            ));
        }
        let (status, body) = get(&self.base, &format!("/v1/memory/watchman/record/{id}"))?;
        if status != 200 {
            return Err(format!(
                "the memory engine answered {status} for record {id}"
            ));
        }
        let text = String::from_utf8(body)
            .map_err(|_| format!("the memory engine's answer for record {id} is not text"))?;
        parse_record(id, &text)
    }
}

/// Whether `id` can be put in a URL path as it is.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A record from the engine's answer for `id`.
///
/// # Errors
///
/// The answer is not JSON, is the engine's `{"error": ...}`, names another
/// record, or has no `spans`.
pub fn parse_record(id: &str, text: &str) -> Result<Record, String> {
    let doc: Value = serde_json::from_str(text)
        .map_err(|e| format!("the memory engine's answer for record {id} is not JSON: {e}"))?;
    let Some(spans) = doc.get("spans").and_then(Value::as_array) else {
        let why = doc
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("it has no spans");
        return Err(format!("record {id}: {why}"));
    };
    if let Some(named) = doc.get("record_id").and_then(Value::as_str) {
        if named != id {
            return Err(format!("asked for record {id} and was given {named}"));
        }
    }
    let mut spans: Vec<&Value> = spans
        .iter()
        .filter(|s| {
            s.get("kind")
                .and_then(Value::as_str)
                .is_none_or(|k| k == "ToolCall")
        })
        .collect();
    spans.sort_by_key(|s| s.get("seq").and_then(Value::as_i64).unwrap_or(0));
    Ok(Record {
        id: id.to_string(),
        started_at_ms: doc
            .get("started_at_ms")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        calls: spans.into_iter().filter_map(call_of).collect(),
    })
}

/// One span as a [`Call`]; `None` for a span with no tool name.
fn call_of(span: &Value) -> Option<Call> {
    let tool = span.get("name").and_then(Value::as_str)?.to_string();
    Some(Call {
        tool,
        args: span.get("input").cloned().unwrap_or(Value::Null),
        error: span.get("error").and_then(error_text),
        output: span.get("output").and_then(output_text),
    })
}

/// A span's `error` as text: absent for null or empty, and unquoted when the
/// engine stored it JSON-quoted.
fn error_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::Null => return None,
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::String(inner)) => inner,
            _ => s.clone(),
        },
        other => other.to_string(),
    };
    (!text.trim().is_empty()).then_some(text)
}

/// A span's `output` as text, whatever shape the engine kept it in.
fn output_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// `GET path` from `base` (`http://host:port`), returning the status and body.
fn get(base: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let (host, port) = split_base(base)?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .find(|a| a.ip().is_loopback())
        .ok_or_else(|| {
            format!(
                "{host} is not a loopback address; records are only read from the memory \
                 engine on this machine"
            )
        })?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("the memory engine at {host}:{port} is not answering: {e}"))?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\n\
         Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("cannot ask the memory engine: {e}"))?;
    let mut raw = Vec::new();
    // One byte over the cap, to tell "exactly the cap" from "more".
    stream
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("the memory engine stopped answering: {e}"))?;
    if raw.len() > MAX_RESPONSE_BYTES {
        return Err("the memory engine's answer is over 4 MiB".to_string());
    }
    parse_http(&raw)
}

/// `(host, port)` of an `http://host[:port]` URL, with nothing after it.
fn split_base(base: &str) -> Result<(String, u16), String> {
    let rest = base.strip_prefix("http://").ok_or_else(|| {
        format!("the memory engine's URL must start with http:// (got {base:?}); it is local")
    })?;
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() || authority.contains('/') || authority.contains('@') {
        return Err(format!("{base:?} is not a plain http://host:port URL"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (
            h,
            p.parse::<u16>()
                .map_err(|_| format!("{p:?} is not a port"))?,
        ),
        _ => (authority, 80),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(format!("{base:?} has no host"));
    }
    Ok((host.to_string(), port))
}

/// The status and the body of an HTTP/1.x response read to the end.
fn parse_http(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("the memory engine's answer has no header end")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "the answer's header is not text")?;
    let body = &raw[split + 4..];
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("the memory engine's answer has no status")?;
    let header = |name: &str| {
        lines
            .clone()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_ascii_lowercase())
    };
    if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        return Ok((status, dechunk(body)?));
    }
    let length = header("content-length").and_then(|v| v.parse::<usize>().ok());
    let body = match length {
        Some(n) if n <= body.len() => &body[..n],
        Some(n) => {
            return Err(format!(
                "the answer ended after {} of {n} bytes",
                body.len()
            ))
        }
        None => body,
    };
    Ok((status, body.to_vec()))
}

/// A `Transfer-Encoding: chunked` body, joined.
fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "the memory engine's chunked answer is malformed".to_string();
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").ok_or_else(bad)?;
        let size_text = std::str::from_utf8(&body[..line_end]).map_err(|_| bad())?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| bad())?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err(bad());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Real answers of `GET /v1/memory/watchman/record/{id}` from a running
    /// engine, captured 2026-11-13. The project, user, memory-link and the text
    /// of the commands are replaced; every key, type and nesting is as captured.
    const SHELL: &str = include_str!("fixtures/record_shell_call.json");
    const FAILED: &str = include_str!("fixtures/record_failed_mcp_call.json");
    const STRUCTURED: &str = include_str!("fixtures/record_structured_output.json");

    #[test]
    fn a_real_record_is_read_into_its_calls() {
        let record = parse_record("w-078c536885c8d141ef6c1ec7", SHELL).unwrap();
        assert_eq!(record.id, "w-078c536885c8d141ef6c1ec7");
        assert_eq!(record.started_at_ms, 1_789_518_445_212);
        assert_eq!(record.calls.len(), 1);
        let call = &record.calls[0];
        assert_eq!(call.tool, "Bash");
        let command = call.args["command"].as_str().expect("the command");
        assert!(command.starts_with("grep -c \"EXAMPLE_FLAG\""), "{command}");
        assert_eq!(call.output.as_deref(), Some("occurrences: 2"));
        assert_eq!(call.error, None);
    }

    #[test]
    fn a_failed_call_carries_its_error_unquoted() {
        let record = parse_record("w-cca0d1763ffa35991f50a9b9", FAILED).unwrap();
        let call = &record.calls[0];
        assert_eq!(call.tool, "mcp__example_memory__recall_index");
        let why = call.error.as_deref().expect("the span's error");
        assert!(why.starts_with("unknown scope"), "{why}");
        assert!(!why.starts_with('"'), "the JSON quoting is removed: {why}");
        assert_eq!(call.output, None);
    }

    #[test]
    fn an_output_that_is_not_text_is_kept_as_its_json() {
        let record = parse_record("w-0503383fb5fdf726a39b6f01", STRUCTURED).unwrap();
        let out = record.calls[0].output.as_deref().expect("output");
        assert!(out.contains("tool_reference"), "{out}");
        assert!(serde_json::from_str::<Value>(out).unwrap().is_array());
    }

    #[test]
    fn spans_are_read_in_seq_order_and_only_tool_calls() {
        let doc = r#"{"record_id":"w-1","started_at_ms":9,"spans":[
            {"seq":3,"kind":"ToolCall","name":"key","input":{"keys":["b"]},"output":null,"error":null},
            {"seq":2,"kind":"ModelCall","name":"claude","input":{},"output":null,"error":null},
            {"seq":1,"kind":"ToolCall","name":"key","input":{"keys":["a"]},"output":null,"error":null}]}"#;
        let record = parse_record("w-1", doc).unwrap();
        let keys: Vec<_> = record
            .calls
            .iter()
            .map(|c| c.args["keys"][0].clone())
            .collect();
        assert_eq!(keys, [serde_json::json!("a"), serde_json::json!("b")]);
    }

    #[test]
    fn the_engines_own_error_and_a_mismatch_are_refusals() {
        let none = parse_record("w-x", r#"{"error":"no record \"w-x\""}"#).unwrap_err();
        assert!(none.contains("no record"), "{none}");
        let wrong = parse_record("w-2", r#"{"record_id":"w-1","spans":[]}"#).unwrap_err();
        assert!(wrong.contains("w-1"), "{wrong}");
        assert!(parse_record("w-3", "not json")
            .unwrap_err()
            .contains("not JSON"));
        assert!(parse_record("w-3", "{}").unwrap_err().contains("no spans"));
    }

    #[test]
    fn an_empty_error_string_is_not_a_failure() {
        let doc = r#"{"spans":[{"seq":1,"kind":"ToolCall","name":"key","input":{},"error":"  "}]}"#;
        assert_eq!(parse_record("w-1", doc).unwrap().calls[0].error, None);
    }

    #[test]
    fn only_a_plain_loopback_http_url_is_spoken_to() {
        assert_eq!(
            split_base("http://127.0.0.1:9180").unwrap(),
            ("127.0.0.1".to_string(), 9180)
        );
        assert_eq!(
            split_base("http://localhost/").unwrap(),
            ("localhost".to_string(), 80)
        );
        assert_eq!(
            split_base("http://[::1]:9180").unwrap(),
            ("::1".to_string(), 9180)
        );
        assert!(split_base("https://127.0.0.1:9180").is_err());
        assert!(split_base("http://user@127.0.0.1").is_err());
        assert!(split_base("http://127.0.0.1:9180/v1").is_err());
        // An address literal, so no name is looked up: not this machine, not asked.
        let err = get("http://192.0.2.1:9180", "/x").unwrap_err();
        assert!(err.contains("not a loopback"), "{err}");
    }

    #[test]
    fn an_id_that_could_escape_the_path_is_refused_before_any_connection() {
        let source = WatchmanSource::new("http://127.0.0.1:9");
        let too_long = "a".repeat(65);
        for bad in [
            "",
            "../x",
            "w-1/../../y",
            "w 1",
            "w-1?x=1",
            too_long.as_str(),
        ] {
            assert!(
                source
                    .record(bad)
                    .unwrap_err()
                    .contains("not a Watchman record id"),
                "{bad}"
            );
        }
    }

    #[test]
    fn http_answers_are_read_by_length_by_chunks_and_to_the_end() {
        let by_length = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhelloEXTRA";
        assert_eq!(parse_http(by_length).unwrap(), (200, b"hello".to_vec()));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;x=1\r\n world\r\n0\r\n\r\n";
        assert_eq!(parse_http(chunked).unwrap(), (200, b"hello world".to_vec()));
        let to_end = b"HTTP/1.0 404 Not Found\r\n\r\nnope";
        assert_eq!(parse_http(to_end).unwrap(), (404, b"nope".to_vec()));
        assert!(parse_http(b"HTTP/1.1 200 OK\r\ncontent-length: 50\r\n\r\nshort").is_err());
        assert!(parse_http(b"garbage").is_err());
        assert!(dechunk(b"zz\r\nhello").is_err());
    }

    /// The whole path, over a real socket: the request is what the engine
    /// expects and the answer is parsed as the engine's real one is.
    #[test]
    fn a_record_is_fetched_over_a_loopback_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = socket.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = SHELL;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).unwrap();
            request
        });
        let source = WatchmanSource::new(&format!("http://127.0.0.1:{port}"));
        let record = source.record("w-078c536885c8d141ef6c1ec7").unwrap();
        assert_eq!(record.calls[0].tool, "Bash");
        let request = served.join().unwrap();
        assert!(
            request.starts_with(
                "GET /v1/memory/watchman/record/w-078c536885c8d141ef6c1ec7 HTTP/1.1\r\n"
            ),
            "{request}"
        );
        assert!(request.contains("Connection: close"), "{request}");
    }

    #[test]
    fn an_engine_that_is_not_there_is_an_error_naming_it_not_a_hang() {
        // Port 9 (discard) is closed on a test machine; the refusal is immediate.
        let source = WatchmanSource::new("http://127.0.0.1:9");
        let why = source.record("w-1").unwrap_err();
        assert!(why.contains("not answering"), "{why}");
    }
}
