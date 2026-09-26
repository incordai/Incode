//! A DevTools protocol client for the headless browser incode starts.
//!
//! Synchronous, over one loopback websocket: a tool call is one thread
//! waiting on one page, and an async runtime would add a scheduler to a
//! conversation that is strictly request, reply, request. Every read has a
//! deadline, so a browser that stops answering ends the call instead of
//! hanging it.
//!
//! The sequence the Screenshot tool drives was measured against the owner's
//! headless browser before it was written here: `/json/version` →
//! `Target.createTarget` → `Target.attachToTarget` (`flatten`) →
//! `Page.enable` → `Emulation.setDeviceMetricsOverride` → `Page.navigate` and
//! its load event → `Page.captureScreenshot` → `Target.closeTarget`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

/// The largest message read from the browser. A full-page screenshot is the
/// largest thing it sends: a PNG inside the image bounds, base64-encoded,
/// with room to be refused by those bounds rather than by the socket.
const MAX_MESSAGE_BYTES: usize = 48 * 1024 * 1024;
/// The most bytes read from `/json/version`.
const MAX_VERSION_BYTES: u64 = 64 * 1024;
/// Events kept while waiting for a reply; older ones are dropped.
const MAX_HELD_EVENTS: usize = 512;

/// `/json/version` from the browser at `port`, or why it did not answer.
///
/// Plain HTTP/1.0 over a loopback socket rather than the shared HTTP client:
/// that client honours proxy settings from the environment, and a request to
/// this machine's own browser must not be sent anywhere else.
pub(super) fn version(port: u16, bound: Duration) -> Result<Value, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, bound)
        .map_err(|e| format!("the browser did not answer on {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(bound))
        .and_then(|()| stream.set_write_timeout(Some(bound)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET /json/version HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .map_err(|e| format!("could not ask the browser for its version: {e}"))?;
    let mut raw = Vec::new();
    stream
        .take(MAX_VERSION_BYTES)
        .read_to_end(&mut raw)
        .map_err(|e| format!("the browser's version did not arrive: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or("the browser's version answer had no body")?;
    serde_json::from_str(body.trim())
        .map_err(|e| format!("the browser's version answer was not JSON: {e}"))
}

/// One DevTools connection.
pub(super) struct Cdp {
    socket: WebSocket<TcpStream>,
    next: u64,
    /// Events that arrived while a reply was awaited, oldest first.
    events: Vec<Value>,
    deadline: Instant,
}

impl Cdp {
    /// Connect to the browser at `port`, for work that must be over by
    /// `deadline`. The address is always loopback, whatever the browser's
    /// version answer names.
    pub(super) fn connect(port: u16, deadline: Instant) -> Result<Self, String> {
        let left = remaining(deadline)?;
        let described = version(port, left)?;
        let named = described
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or("the browser named no DevTools socket")?;
        // The PATH only: the host and port are the ones incode started.
        let path = named
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|at| &rest[at..]))
            .unwrap_or("/devtools/browser");
        let url = format!("ws://127.0.0.1:{port}{path}");
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect_timeout(&addr, remaining(deadline)?)
            .map_err(|e| format!("the DevTools socket did not open: {e}"))?;
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|e| e.to_string())?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let (socket, _) =
            tungstenite::client::client_with_config(url.as_str(), stream, Some(config))
                .map_err(|e| format!("the DevTools handshake failed: {e}"))?;
        Ok(Self {
            socket,
            next: 0,
            events: Vec::new(),
            deadline,
        })
    }

    /// Call `method`, in `session` when given, and return its `result`.
    pub(super) fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::text(message.to_string()))
            .map_err(|e| format!("{method} could not be sent: {e}"))?;
        loop {
            let got = self.next_message(method)?;
            if got.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = got.get("error") {
                    return Err(format!("{method} failed: {error}"));
                }
                return Ok(got.get("result").cloned().unwrap_or(Value::Null));
            }
            self.hold(got);
        }
    }

    /// Wait until an event `wanted` accepts arrives, or `until` passes. An
    /// event that already arrived counts, and is taken.
    pub(super) fn wait_for(
        &mut self,
        what: &str,
        until: Instant,
        wanted: &dyn Fn(&Value) -> bool,
    ) -> Result<Value, String> {
        if let Some(at) = self.events.iter().position(wanted) {
            return Ok(self.events.remove(at));
        }
        let until = until.min(self.deadline);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{what} did not happen in time"));
            }
            self.socket
                .get_mut()
                .set_read_timeout(Some(left))
                .map_err(|e| e.to_string())?;
            let got = match self.read_one() {
                Ok(got) => got,
                Err(Wait::TimedOut) => return Err(format!("{what} did not happen in time")),
                Err(Wait::Failed(why)) => return Err(why),
            };
            if wanted(&got) {
                return Ok(got);
            }
            self.hold(got);
        }
    }

    fn hold(&mut self, event: Value) {
        if self.events.len() >= MAX_HELD_EVENTS {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    /// The next JSON message, within the call's deadline.
    fn next_message(&mut self, method: &str) -> Result<Value, String> {
        let left =
            remaining(self.deadline).map_err(|_| format!("{method} ran past its deadline"))?;
        self.socket
            .get_mut()
            .set_read_timeout(Some(left))
            .map_err(|e| e.to_string())?;
        match self.read_one() {
            Ok(got) => Ok(got),
            Err(Wait::TimedOut) => Err(format!("{method} ran past its deadline")),
            Err(Wait::Failed(why)) => Err(format!("{method}: {why}")),
        }
    }

    fn read_one(&mut self) -> Result<Value, Wait> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    return serde_json::from_str(text.as_str())
                        .map_err(|e| Wait::Failed(format!("the browser sent bad JSON: {e}")));
                }
                Ok(Message::Binary(bytes)) => {
                    return serde_json::from_slice(&bytes)
                        .map_err(|e| Wait::Failed(format!("the browser sent bad JSON: {e}")));
                }
                Ok(Message::Close(_)) => {
                    return Err(Wait::Failed(
                        "the browser closed the connection".to_string(),
                    ))
                }
                // Pings are answered by the library on the next write.
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(Wait::TimedOut)
                }
                Err(e) => return Err(Wait::Failed(format!("the DevTools socket failed: {e}"))),
            }
        }
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

enum Wait {
    TimedOut,
    Failed(String),
}

/// What is left before `deadline`, or an error once it has passed.
pub(super) fn remaining(deadline: Instant) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("the browser ran past its deadline".to_string());
    }
    Ok(left)
}
