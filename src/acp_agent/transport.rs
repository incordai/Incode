//! Newline-delimited JSON-RPC 2.0 over a byte stream: the bounded line reader,
//! the classifier for what a line is, and the one writer every outgoing message
//! goes through.
//!
//! The reader and the line cap come from the ACP client this crate once had
//! (reverted: incode must never drive another harness over ACP). The mechanics
//! are the same on this side of the pipe; what changed is who is on the other
//! end — an editor now, sending requests, where it used to be an agent sending
//! replies.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Mutex;

use agent_client_protocol_schema::v1 as acp;
use serde::Serialize;
use serde_json::Value;

/// The longest single message accepted from the client, in bytes.
///
/// One JSON-RPC message is one line. The biggest legitimate one a client sends
/// is a `session/prompt` carrying embedded context: an ACP `Resource` block
/// holds the whole text of a file, so a prompt with a few attached files runs
/// to megabytes. Eight MiB leaves room for that.
///
/// A longer line is DISCARDED as it is read — never buffered whole — and
/// answered with an error, after which the connection carries on. Holding it
/// would let one runaway client grow this process without bound; cutting it
/// would only hand the parser half a JSON value.
pub(crate) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// How much of an over-long line is kept, to find the request id in.
///
/// A request refused without its own id is a request the client waits on
/// forever: a `session/prompt` over the cap would hang the editor's prompt.
/// Encoders write the envelope first — `jsonrpc`, `id`, `method` — and the
/// payload after, so the id sits in the first few dozen bytes. Four KiB holds
/// that with a wide margin and is a fixed cost, not one that grows with the
/// line.
const ID_SEARCH_BYTES: usize = 4 * 1024;

/// What one read produced.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Line {
    /// A whole line, terminator removed.
    Text(String),
    /// A line longer than the cap. Its bytes were consumed and dropped; `id`
    /// is the request id found in its first [`ID_SEARCH_BYTES`], if any.
    TooLong { id: acp::RequestId },
}

/// The next line, or `None` at the end of the stream.
///
/// Never holds more than `max` bytes of one line: the cap is checked before
/// each chunk is kept, not after the line is whole. Past the cap the rest of
/// the line is still CONSUMED, so the next read starts on the next message.
pub(crate) fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<Line>> {
    let mut line: Vec<u8> = Vec::new();
    let mut overflowed = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            if overflowed {
                return Ok(Some(Line::TooLong {
                    id: top_level_id(&line),
                }));
            }
            return Ok((!line.is_empty()).then(|| Line::Text(text_of(line))));
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at, true),
            None => (available.len(), false),
        };
        if !overflowed {
            if line.len() + take > max {
                overflowed = true;
                // Cut back to the head now, not at the end of the line: the
                // point of the cap is that this memory is not held while the
                // rest streams by. The head is where the id is.
                let room = ID_SEARCH_BYTES.saturating_sub(line.len()).min(take);
                line.extend_from_slice(&available[..room]);
                line.truncate(ID_SEARCH_BYTES);
                line.shrink_to_fit();
            } else {
                line.extend_from_slice(&available[..take]);
            }
        }
        reader.consume(if done { take + 1 } else { take });
        if done {
            return Ok(Some(if overflowed {
                Line::TooLong {
                    id: top_level_id(&line),
                }
            } else {
                Line::Text(text_of(line))
            }));
        }
    }
}

/// The `id` of the top-level JSON object `head` begins, read from a prefix.
///
/// A small scanner rather than a parser, because the rest of the object is
/// gone: it tracks string and nesting state so that only a key named `id` at
/// depth one counts — an `id` inside `params` is not the request's. `null`
/// when the id is not in the prefix or is not a number or string.
fn top_level_id(head: &[u8]) -> acp::RequestId {
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < head.len() {
        match head[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'"' => {
                let start = i + 1;
                let Some(end) = string_end(head, start) else {
                    return acp::RequestId::Null;
                };
                i = end;
                if depth == 1 && &head[start..end] == b"id" {
                    let mut j = end + 1;
                    while head.get(j).is_some_and(u8::is_ascii_whitespace) {
                        j += 1;
                    }
                    if head.get(j) == Some(&b':') {
                        return value_at(head, j + 1);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    acp::RequestId::Null
}

/// Where the string starting at `start` (just after its quote) ends — the
/// index of its closing quote — or `None` when the prefix cuts it off.
fn string_end(head: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < head.len() {
        match head[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The number or string value starting at `from`, as a request id.
fn value_at(head: &[u8], from: usize) -> acp::RequestId {
    let rest = &head[from.min(head.len())..];
    let rest = std::str::from_utf8(rest)
        .or_else(|e| std::str::from_utf8(&rest[..e.valid_up_to()]))
        .unwrap_or("");
    let rest = rest.trim_start();
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    match stream.next() {
        Some(Ok(value)) => id_of(&value),
        _ => acp::RequestId::Null,
    }
}

fn text_of(mut bytes: Vec<u8>) -> String {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What a line from the client is.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// Something to answer.
    Request {
        id: acp::RequestId,
        method: String,
        params: Option<Value>,
    },
    /// Something to act on, with no answer.
    Notification {
        method: String,
        params: Option<Value>,
    },
    /// The client's answer to a request this agent sent it — a
    /// `session/request_permission`. Handed to [`Outbox::resolve`], which
    /// passes it to whoever is waiting on that id.
    Response {
        id: acp::RequestId,
        outcome: Result<Value, acp::Error>,
    },
}

/// A line that is not a message, and the error it is owed.
///
/// `id` is the request's own when it could be read, `null` otherwise — which is
/// what JSON-RPC 2.0 prescribes for a message whose id cannot be determined.
#[derive(Debug)]
pub(crate) struct Refusal {
    pub(crate) id: acp::RequestId,
    pub(crate) error: acp::Error,
}

fn refuse(id: acp::RequestId, error: acp::Error) -> Box<Refusal> {
    Box::new(Refusal { id, error })
}

/// Classify one line, or say why it is not a message.
pub(crate) fn classify(line: &str) -> Result<Incoming, Box<Refusal>> {
    let value: Value = serde_json::from_str(line).map_err(|e| {
        refuse(
            acp::RequestId::Null,
            acp::Error::parse_error().data(Value::String(e.to_string())),
        )
    })?;
    let Value::Object(fields) = &value else {
        // An array is a JSON-RPC batch. ACP clients send one message per line,
        // and answering a batch needs one reply array per batch, which this
        // agent does not assemble — so it is refused rather than half-handled.
        return Err(refuse(
            acp::RequestId::Null,
            acp::Error::invalid_request().data(Value::String(
                "expected one JSON-RPC message object per line; batches are not accepted"
                    .to_string(),
            )),
        ));
    };
    let id = fields.get("id").map(id_of);
    if !fields.contains_key("method") {
        if let Some(error) = fields.get("error") {
            let error = serde_json::from_value::<acp::Error>(error.clone()).unwrap_or_else(|e| {
                acp::Error::internal_error().data(Value::String(format!(
                    "the client's error could not be read: {e}"
                )))
            });
            return Ok(Incoming::Response {
                id: id.unwrap_or(acp::RequestId::Null),
                outcome: Err(error),
            });
        }
        if let Some(result) = fields.get("result") {
            return Ok(Incoming::Response {
                id: id.unwrap_or(acp::RequestId::Null),
                outcome: Ok(result.clone()),
            });
        }
        return Err(refuse(
            id.unwrap_or(acp::RequestId::Null),
            acp::Error::invalid_request(),
        ));
    }
    match id {
        Some(id) => {
            let request = serde_json::from_value::<acp::JsonRpcMessage<acp::Request<Value>>>(value)
                .map_err(|e| {
                    refuse(
                        id.clone(),
                        acp::Error::invalid_request().data(Value::String(e.to_string())),
                    )
                })?
                .into_inner();
            Ok(Incoming::Request {
                id: request.id,
                method: request.method.to_string(),
                params: request.params,
            })
        }
        None => {
            let notification =
                serde_json::from_value::<acp::JsonRpcMessage<acp::Notification<Value>>>(value)
                    .map_err(|e| {
                        refuse(
                            acp::RequestId::Null,
                            acp::Error::invalid_request().data(Value::String(e.to_string())),
                        )
                    })?
                    .into_inner();
            Ok(Incoming::Notification {
                method: notification.method.to_string(),
                params: notification.params,
            })
        }
    }
}

/// A request id as the client wrote it, or `null` when it is not a legal id.
fn id_of(raw: &Value) -> acp::RequestId {
    serde_json::from_value(raw.clone()).unwrap_or(acp::RequestId::Null)
}

/// The one writer for the protocol channel.
///
/// Every response and notification goes through here, whole, under one lock,
/// so two threads — the reader answering a request while a session streams
/// its turn — can never interleave bytes inside a line. Writes are synchronous:
/// a client that stops reading slows this process down rather than making it
/// buffer without bound.
pub(crate) struct Outbox {
    writer: Mutex<Box<dyn Write + Send>>,
    /// Set once a write fails. The client is gone; every later message is
    /// dropped instead of failing the same way again.
    broken: AtomicBool,
    /// Requests this agent sent the client, waiting for its answer, by id.
    waiting: Mutex<HashMap<acp::RequestId, SyncSender<Reply>>>,
    /// The number in the next request id. Ids are `incode-<n>`, their own
    /// namespace: the client's ids name ITS requests, and a response is told
    /// apart from a request by having no method, not by its id.
    next_id: AtomicU64,
}

/// The client's answer to one request: its `result`, or its `error`.
pub(crate) type Reply = Result<Value, acp::Error>;

impl Outbox {
    pub(crate) fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(writer),
            broken: AtomicBool::new(false),
            waiting: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, HashMap<acp::RequestId, SyncSender<Reply>>> {
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Send the client a request. Its answer arrives on the receiver, once the
    /// reader hands it to [`Self::resolve`].
    ///
    /// The receiver disconnects — never hangs — when the request could not be
    /// sent, when [`Self::forget`] withdraws it, and when the client goes
    /// ([`Self::abandon_all`]).
    pub(crate) fn request<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> (acp::RequestId, Receiver<Reply>) {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        let id = acp::RequestId::Str(format!("incode-{n}"));
        let (answer, answered) = std::sync::mpsc::sync_channel(1);
        // Registered BEFORE it is written: a quick client can answer before
        // the write call has even returned.
        self.waiting().insert(id.clone(), answer);
        let message = acp::JsonRpcMessage::wrap(acp::Request {
            id: id.clone(),
            method: method.into(),
            params: Some(params),
        });
        match serde_json::to_string(&message) {
            Ok(line) => self.send_line(line),
            Err(e) => eprintln!("incode acp: a {method} request could not be encoded: {e}"),
        }
        if self.broken.load(Ordering::SeqCst) {
            self.forget(&id);
        }
        (id, answered)
    }

    /// The client answered request `id`. An answer nobody waits for — the
    /// request was withdrawn, or never ours — is dropped.
    pub(crate) fn resolve(&self, id: &acp::RequestId, reply: Reply) {
        if let Some(waiter) = self.waiting().remove(id) {
            let _ = waiter.try_send(reply);
        }
    }

    /// Stop waiting for request `id`: the answer came from somewhere else.
    pub(crate) fn forget(&self, id: &acp::RequestId) {
        self.waiting().remove(id);
    }

    /// The client has gone: nothing it was asked will be answered.
    pub(crate) fn abandon_all(&self) {
        self.waiting().clear();
    }

    /// Answer one request.
    pub(crate) fn respond<R: Serialize>(&self, id: acp::RequestId, result: Result<R, acp::Error>) {
        let message = acp::JsonRpcMessage::wrap(acp::Response::new(id.clone(), result));
        match serde_json::to_string(&message) {
            Ok(line) => self.send_line(line),
            // The id is still owed an answer, or the client waits forever.
            Err(e) => {
                let fallback = acp::JsonRpcMessage::wrap(acp::Response::<Value>::new(
                    id,
                    Err(acp::Error::internal_error()
                        .data(Value::String(format!("could not encode the reply: {e}")))),
                ));
                if let Ok(line) = serde_json::to_string(&fallback) {
                    self.send_line(line);
                }
            }
        }
    }

    /// Send one notification.
    pub(crate) fn notify<P: Serialize>(&self, method: &str, params: P) {
        let message = acp::JsonRpcMessage::wrap(acp::Notification {
            method: method.into(),
            params: Some(params),
        });
        match serde_json::to_string(&message) {
            Ok(line) => self.send_line(line),
            Err(e) => eprintln!("incode acp: a {method} notification could not be encoded: {e}"),
        }
    }

    fn send_line(&self, mut line: String) {
        if self.broken.load(Ordering::SeqCst) {
            return;
        }
        // serde_json escapes every control character inside a string, so an
        // encoded message never contains a raw newline and one line is exactly
        // one message.
        line.push('\n');
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let written = writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.flush());
        if let Err(e) = written {
            self.broken.store(true, Ordering::SeqCst);
            eprintln!("incode acp: the client stopped reading ({e}); further output is dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &[u8], max: usize) -> Vec<Line> {
        let mut reader = std::io::BufReader::with_capacity(4, input);
        let mut out = Vec::new();
        while let Some(line) = read_bounded_line(&mut reader, max).expect("in-memory read") {
            out.push(line);
        }
        out
    }

    /// **AN OVER-LONG LINE IS DROPPED, AND THE NEXT ONE STILL ARRIVES.**
    ///
    /// The cap is only useful if the connection survives it: a reader that
    /// stopped at the first long line would turn one bad message into a dead
    /// session. The tiny buffer makes the long line arrive in many chunks, the
    /// way a real pipe delivers it.
    #[test]
    fn a_line_over_the_cap_is_dropped_and_reading_resumes() {
        let input = b"short\nthis line is far too long\nnext\r\n";
        assert_eq!(
            read_all(input, 10),
            vec![
                Line::Text("short".to_string()),
                Line::TooLong {
                    id: acp::RequestId::Null
                },
                Line::Text("next".to_string()),
            ]
        );
    }

    /// **AN OVER-LONG REQUEST IS STILL ANSWERED UNDER ITS OWN ID.** Refused
    /// under `null`, the client's prompt would wait forever.
    #[test]
    fn an_over_long_request_keeps_its_id() {
        let long = format!(
            "{{\"jsonrpc\":\"2.0\",\"params\":{{\"id\":\"inner\",\"x\":{{\"id\":9}}}},\"id\":42,\"method\":\"session/prompt\",\"pad\":\"{}\"}}\n",
            "y".repeat(20_000)
        );
        assert_eq!(
            read_all(long.as_bytes(), 1024),
            vec![Line::TooLong {
                id: acp::RequestId::Number(42)
            }],
            "the TOP-LEVEL id, not one nested in params"
        );
        let string_id = format!(
            "{{\"id\": \"abc\\\"d\", \"pad\":\"{}\"}}\n",
            "y".repeat(5_000)
        );
        assert_eq!(
            read_all(string_id.as_bytes(), 1024),
            vec![Line::TooLong {
                id: acp::RequestId::Str("abc\"d".to_string())
            }]
        );
    }

    #[test]
    fn a_line_exactly_at_the_cap_is_kept() {
        assert_eq!(
            read_all(b"0123456789\n", 10),
            vec![Line::Text("0123456789".to_string())]
        );
    }

    #[test]
    fn the_last_line_needs_no_terminator() {
        assert_eq!(
            read_all(b"a\nb", 10),
            vec![Line::Text("a".into()), Line::Text("b".into())]
        );
    }

    #[test]
    fn not_json_is_a_parse_error_with_a_null_id() {
        let refused = classify("{not json").expect_err("must be refused");
        assert_eq!(refused.id, acp::RequestId::Null);
        assert_eq!(refused.error.code, acp::ErrorCode::ParseError);
    }

    #[test]
    fn a_request_keeps_its_id() {
        match classify(r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#) {
            Ok(Incoming::Request { id, method, .. }) => {
                assert_eq!(id, acp::RequestId::Number(7));
                assert_eq!(method, "initialize");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_message_without_the_version_is_an_invalid_request() {
        let refused =
            classify(r#"{"id":3,"method":"initialize"}"#).expect_err("jsonrpc is required");
        assert_eq!(
            refused.id,
            acp::RequestId::Number(3),
            "answered with its own id"
        );
        assert_eq!(refused.error.code, acp::ErrorCode::InvalidRequest);
    }

    /// A response from the client is routed by its id to the request that
    /// asked, and an answer to a request nobody waits for is dropped.
    #[test]
    fn a_response_reaches_the_request_that_asked() {
        let outbox = Outbox::new(Box::new(std::io::sink()));
        let (id, answered) = outbox.request("session/request_permission", Value::Null);
        let line = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"ok": 1}}).to_string();
        match classify(&line) {
            Ok(Incoming::Response { id: got, outcome }) => {
                assert_eq!(got, id);
                outbox.resolve(&got, outcome);
            }
            other => panic!("got {other:?}"),
        }
        assert_eq!(
            answered.try_recv().expect("answered"),
            Ok(serde_json::json!({"ok": 1}))
        );

        // Withdrawn: the waiter is told nothing more is coming.
        let (gone, waiting) = outbox.request("session/request_permission", Value::Null);
        outbox.forget(&gone);
        assert!(matches!(
            waiting.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn a_batch_is_refused_not_half_handled() {
        let refused = classify("[]").expect_err("batches are refused");
        assert_eq!(refused.error.code, acp::ErrorCode::InvalidRequest);
    }
}
