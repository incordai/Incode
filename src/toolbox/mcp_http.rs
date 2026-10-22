//! The HTTP half of MCP's client: the Streamable HTTP transport, for a server
//! an ACP client names by address (the desktop app's Incord Memory server).
//!
//! Every JSON-RPC message is one POST to the server's one endpoint. The answer
//! is a JSON body (one message, or a batch) or an event stream whose events
//! carry the messages; `202 Accepted` with no body answers a notification. A
//! session id the server hands out (`Mcp-Session-Id`) goes back on every later
//! request, and the session is ended with a DELETE when the link is dropped.
//!
//! It is a [`Link`], like a child's stdio, so [`super::mcp_client::McpClient`]'s
//! framing, bounds and cancel work unchanged: `send` starts the POST and
//! returns, and the messages its answer carries arrive through `recv`.
//!
//! # What it refuses
//!
//! - An address it may not reach: https anywhere, plain http only to this
//!   machine ([`crate::agent::catalogue::key_may_travel_to`]). The headers can
//!   carry a token, which plain http to another machine would send in clear.
//! - A redirect: it follows none ([`crate::runtime::http::no_redirect_client`]).
//! - An answer past [`MAX_ANSWER_BYTES`], whether declared or streamed.
//!
//! The headers are sent as given and written nowhere else: not to the trace,
//! not into a message. Neither is the address, which can carry a key too.
//!
//! # OAuth
//!
//! A server from incode's own configuration may carry a
//! [`crate::mcp_oauth::TokenSource`]: its token goes as `Authorization:
//! Bearer` on every request, refreshed before it is sent once it has expired,
//! and a `401` is answered by renewing it and sending the request once more. A
//! server still refusing — or one with no token at all — fails the request
//! with the command that logs in (`incode mcp login <name>`).
//!
//! # A failure costs one request
//!
//! An unreachable server, an HTTP error, or an answer that is refused or cannot
//! be read fails the request it answers — as a JSON-RPC error with that
//! request's id — and the link stays up, since a server behind an address can
//! come back. A request that is not answered in time still marks the client
//! broken, as it does for a child.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};

use super::mcp_client::{Link, MAX_LINE_BYTES};
use crate::mcp_oauth::TokenSource;

/// The header a server names its session in, and that carries it back.
const SESSION_HEADER: &str = "mcp-session-id";
/// The most bytes one answer may carry, a JSON body or a whole event stream:
/// the bound a child's line has.
const MAX_ANSWER_BYTES: u64 = MAX_LINE_BYTES;
/// How long the DELETE that ends a session may take.
const END_BOUND: Duration = Duration::from_secs(5);
/// JSON-RPC's code for an error of the server's own, used for a request the
/// transport failed.
const TRANSPORT_ERROR: i64 = -32000;

/// The session id, once the server has named one.
type Session = Arc<Mutex<Option<HeaderValue>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One MCP endpoint, spoken to over HTTP.
pub(super) struct HttpLink {
    url: String,
    /// As the client gave them, each marked sensitive so no `Debug` shows it.
    headers: HeaderMap,
    session: Session,
    /// How long one POST may take, its answer included.
    cap: Duration,
    tx: Sender<String>,
    messages: Receiver<String>,
    /// The POSTs still running, stopped when the link is dropped.
    running: Vec<tokio::task::AbortHandle>,
    /// Signalled when the last notification's POST has finished: the next
    /// POST waits for it, so a server never sees `tools/list` before the
    /// `notifications/initialized` sent ahead of it. Requests themselves may
    /// still overlap one another.
    after_notification: Option<tokio::sync::oneshot::Receiver<()>>,
    /// The OAuth token for this server, when it has one to send.
    auth: Option<Arc<TokenSource>>,
    /// Who this is, for the messages it causes: `MCP server `memory``.
    what: String,
}

impl HttpLink {
    /// A link to the MCP endpoint at `url` that sends `headers` with every
    /// request and gives each at most `cap`. Nothing is sent yet; an address
    /// it may not reach, or a header that is not one, is refused here.
    pub(super) fn connect(
        url: &str,
        headers: &[(String, String)],
        cap: Duration,
        what: &str,
    ) -> Result<Self, String> {
        crate::agent::catalogue::key_may_travel_to(url)?;
        crate::runtime::http::no_redirect_client(url)?;
        let mut map = HeaderMap::new();
        for (index, (name, value)) in headers.iter().enumerate() {
            // Neither the name nor the value goes into the message: a value
            // that fails to parse is still a secret.
            let parsed = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            );
            let (Ok(name), Ok(mut value)) = parsed else {
                return Err(format!(
                    "header number {} it was given is not a valid HTTP header",
                    index + 1
                ));
            };
            value.set_sensitive(true);
            map.append(name, value);
        }
        let (tx, messages) = channel();
        crate::trace!("mcp", "{what} is reached over HTTP");
        Ok(Self {
            url: url.to_string(),
            headers: map,
            session: Arc::default(),
            cap,
            tx,
            messages,
            running: Vec::new(),
            after_notification: None,
            auth: None,
            what: what.to_string(),
        })
    }

    /// The same link, sending `auth`'s bearer token on every request.
    pub(super) fn with_auth(mut self, auth: TokenSource) -> Self {
        self.auth = Some(Arc::new(auth));
        self
    }

    /// The headers one request carries: the client's, then the protocol's
    /// own, which replace any the client gave under the same name.
    fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(session) = lock(&self.session).clone() {
            headers.insert(SESSION_HEADER, session);
        }
        headers
    }
}

impl Link for HttpLink {
    fn send(&mut self, line: &str) -> Result<(), String> {
        let client = crate::runtime::http::no_redirect_client(&self.url)?;
        // A request has an id and must be answered; a notification has none.
        let id = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|message| message.get("id").cloned())
            .filter(|id| !id.is_null());
        let (url, headers, cap, body) = (
            self.url.clone(),
            self.request_headers(),
            self.cap,
            line.to_string(),
        );
        let build = move || {
            client
                .post(&url)
                .headers(headers.clone())
                .timeout(cap)
                .body(body.clone())
        };
        let session = Arc::clone(&self.session);
        let auth = self.auth.clone();
        let out = self.tx.clone();
        let what = self.what.clone();
        self.running.retain(|task| !task.is_finished());
        let wait_for = self.after_notification.take();
        let (done, done_rx) = tokio::sync::oneshot::channel::<()>();
        let done = if id.is_none() {
            self.after_notification = Some(done_rx);
            Some(done)
        } else {
            None
        };
        let task = crate::runtime::http::SHARED_RUNTIME.spawn(async move {
            // Dropped when this task ends, however it ends: that is the signal.
            let _done = done;
            if let Some(earlier) = wait_for {
                let _ = earlier.await;
            }
            let had_session = lock(&session).is_some();
            let answered = match send_authorized(&build, auth.as_deref()).await {
                Ok(response) => {
                    read_answer(response, had_session, &session, &out, id.as_ref()).await
                }
                Err(why) => Err(why),
            };
            let Err(why) = answered else {
                return;
            };
            match id {
                Some(id) => {
                    let failed = json!({"jsonrpc": "2.0", "id": id,
                                        "error": {"code": TRANSPORT_ERROR, "message": why}});
                    let _ = out.send(failed.to_string());
                }
                None => crate::trace!("mcp", "{what}: a notification failed: {why}"),
            }
        });
        self.running.push(task.abort_handle());
        Ok(())
    }

    fn recv(&mut self, bound: Duration) -> Result<Option<String>, String> {
        match self.messages.recv_timeout(bound) {
            Ok(message) => Ok(Some(message)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(format!("{} can no longer be heard", self.what))
            }
        }
    }
}

impl Drop for HttpLink {
    /// Stop every POST still running, and end the session the server opened,
    /// if it opened one — without waiting for the answer.
    fn drop(&mut self) {
        for task in &self.running {
            task.abort();
        }
        let Some(session) = lock(&self.session).take() else {
            return;
        };
        let Ok(client) = crate::runtime::http::no_redirect_client(&self.url) else {
            return;
        };
        let mut headers = self.headers.clone();
        headers.insert(SESSION_HEADER, session);
        if let Some(bearer) = self
            .auth
            .as_ref()
            .and_then(|auth| auth.held_now())
            .and_then(|token| crate::mcp_oauth::bearer_header(&token))
        {
            headers.insert(AUTHORIZATION, bearer);
        }
        let request = client.delete(&self.url).headers(headers).timeout(END_BOUND);
        drop(crate::runtime::http::SHARED_RUNTIME.spawn(async move {
            let _ = request.send().await;
        }));
        crate::trace!("mcp", "ended {}'s session", self.what);
    }
}

fn too_big() -> String {
    format!("its answer was larger than {MAX_ANSWER_BYTES} bytes")
}

/// Whether `message` is the reply to the request `id`.
fn replies_to(message: &Value, id: &Value) -> bool {
    message.get("id") == Some(id) && message.get("method").is_none()
}

/// Send every message in `value` (one, or a batch) to `out`. Whether the reply
/// to `id` was among them.
fn deliver(value: Value, out: &Sender<String>, id: Option<&Value>) -> bool {
    let messages = match value {
        Value::Array(batch) => batch,
        message => vec![message],
    };
    let mut replied = false;
    for message in messages {
        replied |= id.is_some_and(|id| replies_to(&message, id));
        let _ = out.send(message.to_string());
    }
    replied
}

/// `request` with `token` as its bearer, when there is one.
fn with_bearer(request: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token.and_then(crate::mcp_oauth::bearer_header) {
        Some(bearer) => request.header(AUTHORIZATION, bearer),
        None => request,
    }
}

/// Send the request `build` makes, with `auth`'s token when there is one. A
/// `401` renews the token and sends it once more; refused again, it fails
/// with the command that logs in.
async fn send_authorized(
    build: &(dyn Fn() -> reqwest::RequestBuilder + Send + Sync),
    auth: Option<&TokenSource>,
) -> Result<reqwest::Response, String> {
    // `without_url`: the address can carry a key of its own.
    let reached = |e: reqwest::Error| format!("it could not be reached: {}", e.without_url());
    let Some(auth) = auth else {
        return build().send().await.map_err(reached);
    };
    let token = auth.current().await;
    let first = with_bearer(build(), token.as_deref())
        .send()
        .await
        .map_err(reached)?;
    if first.status() != StatusCode::UNAUTHORIZED {
        return Ok(first);
    }
    drop(first);
    let renewed = auth.renew(token.as_deref()).await?;
    let second = with_bearer(build(), Some(&renewed))
        .send()
        .await
        .map_err(reached)?;
    if second.status() == StatusCode::UNAUTHORIZED {
        return Err(auth.refused());
    }
    Ok(second)
}

/// One POST's answer, every message it carries sent to `out`. `id` is the
/// request's own when it is one: a request whose reply the answer does not
/// carry has failed. `had_session` is whether the request carried a session.
async fn read_answer(
    mut response: reqwest::Response,
    had_session: bool,
    session: &Mutex<Option<HeaderValue>>,
    out: &Sender<String>,
    id: Option<&Value>,
) -> Result<(), String> {
    let status = response.status();
    if let Some(named) = response.headers().get(SESSION_HEADER) {
        let mut named = named.clone();
        named.set_sensitive(true);
        *lock(session) = Some(named);
    }
    if status == StatusCode::NOT_FOUND && had_session {
        return Err("it no longer knows this session (404)".to_string());
    }
    if !status.is_success() {
        return Err(format!("it answered {status}"));
    }
    if response
        .content_length()
        .is_some_and(|declared| declared > MAX_ANSWER_BYTES)
    {
        return Err(too_big());
    }
    let kind = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if kind.starts_with("text/event-stream") {
        return read_events(&mut response, out, id).await;
    }
    let body = read_bounded(&mut response).await?;
    if body.iter().all(u8::is_ascii_whitespace) {
        return match id {
            Some(_) => Err(format!("it answered {status} with no reply")),
            None => Ok(()),
        };
    }
    if !kind.starts_with("application/json") {
        let shown: String = kind.chars().take(60).collect();
        return Err(format!(
            "it answered `{shown}`, which is neither JSON nor an event stream"
        ));
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|e| format!("its answer is not JSON: {e}"))?;
    match (deliver(value, out, id), id) {
        (false, Some(_)) => Err("its answer did not carry the reply".to_string()),
        _ => Ok(()),
    }
}

/// A body, read in pieces and refused past [`MAX_ANSWER_BYTES`].
async fn read_bounded(response: &mut reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(piece) = response
        .chunk()
        .await
        .map_err(|e| format!("its answer stopped early: {}", e.without_url()))?
    {
        if (body.len() + piece.len()) as u64 > MAX_ANSWER_BYTES {
            return Err(too_big());
        }
        body.extend_from_slice(&piece);
    }
    Ok(body)
}

/// An event stream, read as it arrives: each event's data is one message (or
/// a batch). Reading stops at the reply to `id` — a server may hold the stream
/// open after it — or when the stream ends, or passes the bound.
async fn read_events(
    response: &mut reqwest::Response,
    out: &Sender<String>,
    id: Option<&Value>,
) -> Result<(), String> {
    let mut events = Events::default();
    let mut read: u64 = 0;
    while let Some(piece) = response
        .chunk()
        .await
        .map_err(|e| format!("its event stream stopped early: {}", e.without_url()))?
    {
        read += piece.len() as u64;
        if read > MAX_ANSWER_BYTES {
            return Err(too_big());
        }
        for data in events.feed(&piece) {
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if deliver(value, out, id) {
                return Ok(());
            }
        }
    }
    match id {
        Some(_) => Err("its event stream ended without the reply".to_string()),
        None => Ok(()),
    }
}

/// The `text/event-stream` format, parsed as its bytes arrive: lines of
/// `field: value`, an event ended by a blank line, `:` starting a comment.
#[derive(Default)]
struct Events {
    /// Bytes of a line not yet ended.
    pending: Vec<u8>,
    /// The `data` lines of the event being read.
    data: Vec<String>,
    /// Its `event` type; empty means `message`.
    kind: String,
}

impl Events {
    /// The data of every `message` event that `bytes` completes, its lines
    /// joined by a line feed.
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut done = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                let data = std::mem::take(&mut self.data);
                let kind = std::mem::take(&mut self.kind);
                if !data.is_empty() && (kind.is_empty() || kind == "message") {
                    done.push(data.join("\n"));
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "data" => self.data.push(value.to_string()),
                "event" => self.kind = value.to_string(),
                _ => {}
            }
        }
        done
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::toolbox::client_servers::tests::Connected;
    use crate::toolbox::client_servers::{ServerSpec, Transport};
    use http_body_util::combinators::UnsyncBoxBody;
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Frame, Incoming};
    use hyper::{Request, Response};
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use std::time::Instant;

    /// A body with no declared length, its pieces arriving as they are sent.
    struct Streamed(tokio::sync::mpsc::Receiver<Bytes>);

    impl hyper::body::Body for Streamed {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            self.0
                .poll_recv(cx)
                .map(|piece| piece.map(|bytes| Ok(Frame::data(bytes))))
        }
    }

    /// The session id the fake hands out, and requires back.
    pub(crate) const SESSION: &str = "fake-session-7";
    /// A header the fake records, standing in for a client's token.
    pub(crate) const TOKEN_HEADER: &str = "x-fake-token";
    const TOKEN: &str = "t0ken-value";

    type Body = UnsyncBoxBody<Bytes, Infallible>;

    /// What one request to the fake carried.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct Seen {
        pub(crate) method: String,
        pub(crate) session: Option<String>,
        pub(crate) token: Option<String>,
    }

    /// An MCP server over Streamable HTTP on a loopback port, with four tools:
    /// `echo` (a JSON answer), `streamed` (an event stream held open after
    /// its reply), `large` and `large_stream` (answers past the bound).
    pub(crate) struct FakeHttp {
        pub(crate) url: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        /// Set when a held-open stream found its reader gone.
        closed: Arc<AtomicBool>,
    }

    impl FakeHttp {
        pub(crate) fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.set_nonblocking(true).expect("nonblocking");
            let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let closed = Arc::new(AtomicBool::new(false));
            let (log, gone) = (Arc::clone(&seen), Arc::clone(&closed));
            crate::runtime::http::SHARED_RUNTIME.spawn(async move {
                let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                    return;
                };
                while let Ok((socket, _)) = listener.accept().await {
                    let (log, gone) = (Arc::clone(&log), Arc::clone(&gone));
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| {
                            answer(req, Arc::clone(&log), Arc::clone(&gone))
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                            .await;
                    });
                }
            });
            Self { url, seen, closed }
        }

        /// Every request so far, in order.
        pub(crate) fn seen(&self) -> Vec<Seen> {
            lock(&self.seen).clone()
        }

        /// How many `tools/call` requests reached it.
        pub(crate) fn calls(&self) -> usize {
            self.seen()
                .iter()
                .filter(|s| s.method == "tools/call")
                .count()
        }
    }

    /// The fake, as a client names it: with a token header.
    pub(crate) fn http_spec(name: &str, url: &str) -> ServerSpec {
        ServerSpec {
            name: name.to_string(),
            transport: Transport::Http {
                url: url.to_string(),
                headers: vec![(TOKEN_HEADER.to_string(), TOKEN.to_string())],
                oauth: None,
            },
        }
    }

    fn full(status: u16, content_type: &str, body: String, session: bool) -> Response<Body> {
        let mut reply = Response::builder().status(status);
        if !content_type.is_empty() {
            reply = reply.header("content-type", content_type);
        }
        if session {
            reply = reply.header("Mcp-Session-Id", SESSION);
        }
        reply
            .body(Full::new(Bytes::from(body)).boxed_unsync())
            .expect("response")
    }

    fn result(id: &Value, result: Value) -> String {
        json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
    }

    fn text(said: &str) -> Value {
        json!({"content": [{"type": "text", "text": said}]})
    }

    /// An event stream with no declared length: `pieces` sent in turn, then —
    /// when `hold` — comments until the reader goes, which sets `gone`.
    fn stream(pieces: Vec<Vec<u8>>, hold: bool, gone: Arc<AtomicBool>) -> Response<Body> {
        let (sender, body) = tokio::sync::mpsc::channel::<Bytes>(4);
        tokio::spawn(async move {
            for piece in pieces {
                if sender.send(Bytes::from(piece)).await.is_err() {
                    gone.store(true, Ordering::SeqCst);
                    return;
                }
            }
            if !hold {
                return;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if sender
                    .send(Bytes::from_static(b": still here\n\n"))
                    .await
                    .is_err()
                {
                    gone.store(true, Ordering::SeqCst);
                    return;
                }
            }
        });
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Streamed(body).boxed_unsync())
            .expect("response")
    }

    async fn answer(
        req: Request<Incoming>,
        log: Arc<Mutex<Vec<Seen>>>,
        gone: Arc<AtomicBool>,
    ) -> Result<Response<Body>, Infallible> {
        let header = |name: &str| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let (session, token) = (header(SESSION_HEADER), header(TOKEN_HEADER));
        let bytes = match req.into_body().collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => Bytes::new(),
        };
        let message: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or("").to_string();
        lock(&log).push(Seen {
            method: method.clone(),
            session: session.clone(),
            token,
        });
        let id = message["id"].clone();
        if method == "initialize" {
            let hello = json!({"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                               "serverInfo": {"name": "fake-http", "version": "0"}});
            return Ok(full(200, "application/json", result(&id, hello), true));
        }
        if session.as_deref() != Some(SESSION) {
            return Ok(full(400, "", String::new(), false));
        }
        if id.is_null() {
            return Ok(full(202, "", String::new(), false));
        }
        let over = usize::try_from(MAX_ANSWER_BYTES).expect("bound") + 1;
        let reply = match (method.as_str(), message["params"]["name"].as_str()) {
            ("tools/list", _) => {
                let object = json!({"type": "object"});
                let tools = json!({"tools": [
                    {"name": "echo", "description": "Say it back.",
                     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                                     "required": ["text"]}},
                    {"name": "streamed", "inputSchema": object},
                    {"name": "large", "inputSchema": object},
                    {"name": "large_stream", "inputSchema": object},
                ]});
                full(200, "application/json", result(&id, tools), false)
            }
            ("tools/call", Some("echo")) => {
                let said = message["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap_or("");
                let body = result(&id, text(&format!("echo: {said}")));
                full(200, "application/json; charset=utf-8", body, false)
            }
            ("tools/call", Some("streamed")) => {
                // A notification first, a comment, then the reply split over
                // two `data` lines, all cut into seven-byte pieces.
                let note = json!({"jsonrpc": "2.0", "method": "notifications/progress",
                                  "params": {"progress": 1}});
                let reply = result(&id, text("streamed: ok"));
                let (head, tail) = reply.split_at(reply.find("\"result\"").expect("result"));
                let events = format!(
                    "event: message\ndata: {note}\n\n: a comment\ndata: {head}\r\ndata: {tail}\n\n"
                );
                let pieces = events.as_bytes().chunks(7).map(<[u8]>::to_vec).collect();
                stream(pieces, true, gone)
            }
            ("tools/call", Some("large")) => {
                let body = result(&id, text(&"x".repeat(over)));
                full(200, "application/json", body, false)
            }
            ("tools/call", Some("large_stream")) => {
                let mut pieces = vec![b"data: ".to_vec()];
                pieces.extend((0..=over / (1 << 20)).map(|_| vec![b'x'; 1 << 20]));
                stream(pieces, false, gone)
            }
            _ => full(404, "", String::new(), false),
        };
        Ok(reply)
    }

    fn connect(url: &str) -> (Connected, Vec<String>) {
        let dir = tempfile::tempdir().expect("dir");
        Connected::now(vec![http_spec("web", url)], dir.path())
    }

    /// **AN HTTP SERVER IS INITIALIZED, LISTED AND CALLED**, its tools named
    /// `mcp__<server>__<tool>` like a child's, the client's header sent as
    /// given on every request.
    #[test]
    fn an_http_server_is_initialized_listed_and_called() {
        let fake = FakeHttp::start();
        let (servers, said) = connect(&fake.url);
        let names: Vec<String> = servers
            .definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            names,
            [
                "mcp__web__echo",
                "mcp__web__streamed",
                "mcp__web__large",
                "mcp__web__large_stream"
            ],
            "{said:?}"
        );
        let out = servers
            .call("mcp__web__echo", &json!({"text": "hi"}), &|| false)
            .expect("called");
        assert_eq!(out.text, "echo: hi");
        let seen = fake.seen();
        let methods: Vec<&str> = seen.iter().map(|s| s.method.as_str()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        assert!(
            seen.iter().all(|s| s.token.as_deref() == Some(TOKEN)),
            "{seen:?}"
        );
    }

    /// **THE SESSION ID GOES BACK.** None on `initialize`; the one the server
    /// named on every request after it (the fake refuses a request without).
    #[test]
    fn the_session_id_goes_back_on_every_request_after_initialize() {
        let fake = FakeHttp::start();
        let (servers, said) = connect(&fake.url);
        assert!(servers.has("mcp__web__echo"), "{said:?}");
        servers
            .call("mcp__web__echo", &json!({"text": "again"}), &|| false)
            .expect("called");
        let seen = fake.seen();
        assert_eq!(seen[0].method, "initialize");
        assert_eq!(seen[0].session, None);
        assert!(seen.len() >= 4, "{seen:?}");
        assert!(
            seen[1..]
                .iter()
                .all(|s| s.session.as_deref() == Some(SESSION)),
            "{seen:?}"
        );
    }

    /// **AN EVENT-STREAM ANSWER IS READ**: a notification and a comment
    /// before the reply, the reply over two `data` lines, all arriving in
    /// small pieces — and the stream, held open after the reply, is let go.
    #[test]
    fn an_event_stream_answer_is_read_and_let_go_after_its_reply() {
        let fake = FakeHttp::start();
        let (servers, said) = connect(&fake.url);
        assert!(servers.has("mcp__web__streamed"), "{said:?}");
        let out = servers
            .call("mcp__web__streamed", &json!({}), &|| false)
            .expect("called");
        assert_eq!(out.text, "streamed: ok");
        let began = Instant::now();
        while !fake.closed.load(Ordering::SeqCst) && began.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            fake.closed.load(Ordering::SeqCst),
            "the held-open stream was never let go"
        );
    }

    /// **AN ANSWER PAST THE BOUND IS REFUSED** — declared or streamed — and
    /// costs that call only: the next one is answered.
    #[test]
    fn an_oversized_answer_is_refused_and_the_link_survives() {
        let fake = FakeHttp::start();
        let (servers, said) = connect(&fake.url);
        assert!(servers.has("mcp__web__large"), "{said:?}");
        for tool in ["mcp__web__large", "mcp__web__large_stream"] {
            let refused = servers
                .call(tool, &json!({}), &|| false)
                .expect_err("past the bound");
            assert!(refused.contains("larger than"), "{tool}: {refused}");
        }
        let out = servers
            .call("mcp__web__echo", &json!({"text": "still"}), &|| false)
            .expect("the link survives");
        assert_eq!(out.text, "echo: still");
    }

    /// **PLAIN HTTP ONLY TO THIS MACHINE.** A plain-http address elsewhere is
    /// refused before anything is sent; https anywhere and plain http to
    /// loopback are accepted; another scheme is refused.
    #[test]
    fn plain_http_to_another_machine_is_refused_before_anything_is_sent() {
        let (servers, said) = connect("http://192.0.2.1:9/mcp");
        assert!(servers.definitions().is_empty());
        assert!(
            said.iter()
                .any(|s| s.contains("`web` was not started") && s.contains("plain http")),
            "{said:?}"
        );
        let bound = Duration::from_secs(1);
        assert!(HttpLink::connect("https://192.0.2.1/mcp", &[], bound, "s").is_ok());
        assert!(HttpLink::connect("http://127.0.0.1:1/mcp", &[], bound, "s").is_ok());
        assert!(HttpLink::connect("http://localhost:1/mcp", &[], bound, "s").is_ok());
        assert!(HttpLink::connect("ftp://127.0.0.1/mcp", &[], bound, "s").is_err());
    }

    /// A header that is not one is refused without its value in the message.
    #[test]
    fn a_bad_header_is_refused_without_saying_its_value() {
        let headers = [("authorization".to_string(), "Bearer se\ncret".to_string())];
        let refused = HttpLink::connect(
            "http://127.0.0.1:1/mcp",
            &headers,
            Duration::from_secs(1),
            "s",
        )
        .err()
        .unwrap_or_default();
        assert!(refused.contains("header number 1"), "{refused}");
        assert!(!refused.contains("cret"), "{refused}");
    }

    #[test]
    fn events_are_parsed_across_pieces() {
        let mut events = Events::default();
        assert!(events.feed(b"data: {\"a\"").is_empty());
        assert!(events.feed(b":1}\r\n").is_empty());
        assert_eq!(events.feed(b"\r\n"), ["{\"a\":1}"]);
        assert!(events.feed(b"event: ping\ndata: x\n\n").is_empty());
        assert_eq!(events.feed(b"data:a\ndata: b\n\n"), ["a\nb"]);
    }
}
