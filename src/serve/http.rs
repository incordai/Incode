//! The local run server: loopback only. Reads are open to admitted clients;
//! the one write ([`command`]) also needs this run's token ([`TOKEN_FILE`]).
//!
//! # Who can reach it
//!
//! This serves what agents are doing to a project on this machine — file paths,
//! task titles, transcript-derived status. It listens on `127.0.0.1` and
//! nowhere else, which keeps other MACHINES out. It does not keep other
//! WEBSITES out: any page the operator opens can send a request to loopback.
//! So every request passes [`Access::admit`] first:
//!
//! - the `Host` header must name this server by a loopback name and its own
//!   port, which defeats DNS rebinding (a hostile domain resolved to
//!   127.0.0.1, whose requests the browser treats as same-origin);
//! - a browser `Origin`, when there is one, must be loopback or listed in
//!   [`ORIGINS_ENV`], and is echoed back exactly — never `*`.
//!
//! A client that sends no `Origin` (curl, a script) is not a browser page and
//! is admitted on the Host check alone.
//!
//! **Do not move this to `0.0.0.0` without adding authentication first.**
//!
//! # Why the stream polls
//!
//! The run writes its transcripts from another process — often several, since
//! an agent CLI is a subprocess of its own. There is no in-process channel to
//! subscribe to, so the honest mechanism is to re-read and send when something
//! changed. [`POLL`] is the tick; a client sees a change within that.
//!
//! The alternative — a filesystem watcher — would add a crate and, measured on
//! this platform, fires several times per write. It would be more code for the
//! same latency.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Bytes, Frame};
use hyper::{Method, Request, Response, StatusCode};

/// How often the stream re-reads the run to see if anything moved.
pub const POLL: std::time::Duration = std::time::Duration::from_millis(700);

/// Where the server writes the address it actually got, inside `.incord/`.
///
/// # There is no default port, on purpose
///
/// A fixed number is a collision waiting for the day another service picks it,
/// and this machine already runs one on the port next door. So by default the
/// OS is asked for a free one and the answer is WRITTEN DOWN here — a client
/// reads the file instead of guessing, and two projects can be watched at once
/// without either being told about the other.
///
/// An operator who names `--port` gets that port or an error. A named port that
/// quietly became a different one would leave an interface talking confidently
/// to a server watching another project.
pub const ADDRESS_FILE: &str = "serve.json";

/// What the server published about itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Address {
    pub port: u16,
    pub url: String,
    pub workspace: String,
    /// The process holding the port, so a file left behind by a killed server
    /// can be told from a live one.
    pub pid: u32,
    pub at: String,
}

/// Record where this server can be reached.
///
/// Failure to write is REPORTED, not fatal: a server that is listening but
/// unadvertised is still usable by anyone told the port, and refusing to serve
/// because a file could not be written would be the worse trade.
fn publish(workspace: &std::path::Path, port: u16) -> std::io::Result<Address> {
    let address = Address {
        port,
        url: format!("http://127.0.0.1:{port}"),
        workspace: workspace.to_string_lossy().to_string(),
        pid: std::process::id(),
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    };
    let dir = workspace.join(crate::ledger::LEDGER_DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(ADDRESS_FILE),
        serde_json::to_string_pretty(&address).unwrap_or_default(),
    )?;
    Ok(address)
}

/// Where a server for this workspace said it was listening, if one did.
///
/// The client's half of [`publish`]. Says nothing about whether that server is
/// still running — `pid` is there so a caller can decide.
#[must_use]
pub fn published(workspace: &std::path::Path) -> Option<Address> {
    let text =
        std::fs::read_to_string(workspace.join(crate::ledger::LEDGER_DIR).join(ADDRESS_FILE))
            .ok()?;
    serde_json::from_str(&text).ok()
}

type Body = BoxBody<Bytes, Infallible>;

/// Extra browser origins allowed to read this server, comma-separated — for an
/// interface that is not served from loopback, such as a desktop shell's own
/// scheme (`tauri://localhost`). Loopback origins need no entry.
pub const ORIGINS_ENV: &str = "INCORD_SERVE_ORIGINS";

/// Who may talk to this server. See the module note.
#[derive(Debug, Clone)]
pub struct Access {
    port: u16,
    extra_origins: Vec<String>,
}

impl Access {
    #[must_use]
    pub fn new(port: u16, extra_origins: Vec<String>) -> Self {
        Access {
            port,
            extra_origins,
        }
    }

    /// The bound port, plus whatever [`ORIGINS_ENV`] lists.
    #[must_use]
    pub fn from_env(port: u16) -> Self {
        let extra = std::env::var(ORIGINS_ENV)
            .unwrap_or_default()
            .split(',')
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| !o.is_empty())
            .collect();
        Access::new(port, extra)
    }

    /// Whether a request may be answered, given its `Host` and `Origin`.
    ///
    /// `Ok(Some(origin))` is a browser origin to echo in the reply; `Ok(None)`
    /// is a client that sent none.
    ///
    /// # Errors
    ///
    /// Why the request is refused, in words for the 403.
    pub fn admit(
        &self,
        host: Option<&str>,
        origin: Option<&str>,
    ) -> Result<Option<String>, String> {
        let host = host.ok_or("no Host header")?;
        let expected = [
            format!("127.0.0.1:{}", self.port),
            format!("localhost:{}", self.port),
        ];
        if !expected.iter().any(|e| e.eq_ignore_ascii_case(host)) {
            return Err(format!("this server does not answer to `{host}`"));
        }
        let Some(origin) = origin else {
            return Ok(None);
        };
        if is_loopback_origin(origin) || self.extra_origins.iter().any(|o| o == origin) {
            Ok(Some(origin.to_string()))
        } else {
            Err(format!(
                "origin `{origin}` is not allowed; loopback origins are, and others can be \
                 listed in {ORIGINS_ENV}"
            ))
        }
    }
}

/// `http(s)://localhost` or `http(s)://127.0.0.1`, with an optional port and
/// nothing else — `localhost.evil.example` is not loopback.
fn is_loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let (name, port) = match rest.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (rest, None),
    };
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    port_ok && (name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1")
}

/// Name the admitted origin on a reply, so that browser — and only that one —
/// may read it.
fn with_cors(mut reply: Response<Body>, origin: Option<&str>) -> Response<Body> {
    if let Some(value) = origin.and_then(|o| hyper::header::HeaderValue::from_str(o).ok()) {
        let headers = reply.headers_mut();
        headers.insert(hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        // The reply differs by Origin, so a cache must not hand one site's
        // answer to another.
        headers.insert(
            hyper::header::VARY,
            hyper::header::HeaderValue::from_static("origin"),
        );
    }
    reply
}

/// Where this run's write token is kept, inside `.incord/`.
///
/// Reads need no token. A write — answering an agent — needs it, sent as
/// `Authorization: Bearer <token>`. It keeps out web pages (which also fail the
/// origin check) and other users' programs. It does NOT keep out an agent
/// working in this workspace, which can read `.incord/` like any file; nothing
/// a write offers is more than such an agent can already do to that directory.
pub const TOKEN_FILE: &str = "serve.token";

/// The largest request body a command accepts. An answer is a sentence or a
/// paragraph; anything near this is not one.
const MAX_BODY: usize = 64 * 1024;

/// Make this run's write token and write it where a local interface can read
/// it. A fresh one each time the server starts.
fn issue_token(workspace: &std::path::Path) -> std::io::Result<String> {
    use std::io::Write;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let dir = workspace.join(crate::ledger::LEDGER_DIR);
    std::fs::create_dir_all(&dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(dir.join(TOKEN_FILE))?
        .write_all(token.as_bytes())?;
    Ok(token)
}

/// Whether `header` is exactly `Bearer <token>`, compared in constant time so
/// the reply's timing does not reveal how much of a guess was right.
fn bearer_matches(header: Option<&str>, token: &str) -> bool {
    let Some(given) = header.and_then(|h| h.strip_prefix("Bearer ")) else {
        return false;
    };
    let (given, token) = (given.as_bytes(), token.as_bytes());
    if token.is_empty() || given.len() != token.len() {
        return false;
    }
    given
        .iter()
        .zip(token)
        .fold(0u8, |differ, (a, b)| differ | (a ^ b))
        == 0
}

/// An answer to an agent's question, as the interface sends it.
#[derive(serde::Deserialize)]
struct AnswerRequest {
    question: u64,
    text: String,
}

/// The routes that change something. `None` for a path that is not one, so
/// the caller falls through to [`route`] — which has no write arm at all.
///
/// Split from the socket, like [`route`], so every decision here is testable
/// without binding a port.
pub fn command(
    method: &Method,
    path: &str,
    authorized: bool,
    body: &[u8],
    workspace: &std::path::Path,
) -> Option<Response<Body>> {
    if !(method == Method::POST && path == "/v1/team/answer") {
        return None;
    }
    if !authorized {
        return Some(json(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({ "error": format!(
                "a write needs this run's token: send `Authorization: Bearer <token>` \
                 with the token from .incord/{TOKEN_FILE}"
            ) }),
        ));
    }
    let request: AnswerRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(e) => {
            return Some(json(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": format!(
                    "expected {{\"question\": <number>, \"text\": \"...\"}}: {e}"
                ) }),
            ))
        }
    };
    Some(
        match crate::team::Channel::in_workspace(workspace).answer(
            crate::team::OWNER,
            request.question,
            &request.text,
        ) {
            Ok(sent) => json(
                StatusCode::OK,
                &serde_json::json!({ "answered": request.question, "seq": sent.seq }),
            ),
            Err(why) => json(
                StatusCode::UNPROCESSABLE_ENTITY,
                &serde_json::json!({ "error": why }),
            ),
        },
    )
}

fn json(status: StatusCode, value: &impl serde::Serialize) -> Response<Body> {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(text)).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// One server-sent event.
fn sse_frame(event: &str, data: &str) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// Answer one request.
///
/// Split out from the socket so the routing can be tested without binding a
/// port: every decision here is "which path, which method, what status".
pub fn route(method: &Method, path: &str, workspace: &std::path::Path) -> Option<Response<Body>> {
    match (method, path) {
        (&Method::GET, "/v1/run/state") => Some(json(StatusCode::OK, &super::snapshot(workspace))),
        // The FIRST screen: is this folder ready to be worked in, and if not,
        // which documents are still owed. See [`crate::serve::project`].
        (&Method::GET, "/v1/project/gate") => {
            Some(json(StatusCode::OK, &super::project::gate_of(workspace)))
        }
        (&Method::GET, "/v1/tasks") => Some(match super::tasks_of(workspace) {
            Ok(tasks) => json(StatusCode::OK, &serde_json::json!({ "tasks": tasks })),
            Err(why) => json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": why }),
            ),
        }),
        (&Method::GET, "/v1/team") => Some(json(StatusCode::OK, &super::team_of(workspace))),
        // A browser asks before it fetches from another origin.
        (&Method::OPTIONS, _) => Some(
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header("access-control-allow-methods", "GET, POST, OPTIONS")
                .header(
                    "access-control-allow-headers",
                    "authorization, content-type",
                )
                .body(Full::new(Bytes::new()).boxed())
                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed())),
        ),
        // The stream is built by the caller, which owns the ticker.
        (&Method::GET, "/v1/run/stream") => None,
        _ => Some(json(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "no such endpoint" }),
        )),
    }
}

/// The SSE stream: the snapshot now, then again whenever it differs.
///
/// Sends the CURRENT state on connect rather than waiting for a change — a
/// client that connected during a quiet minute would otherwise show an empty
/// board until something happened, which reads as "nothing is running".
fn stream(workspace: PathBuf) -> Response<Body> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(8);

    tokio::spawn(async move {
        let journal = crate::desktop::journal::path_in(&workspace);
        // `None` would mean "send everything in the file", which on connect is
        // a replay of the whole recent history. The FIRST poll therefore takes
        // the current end as the starting cursor and shows nothing older —
        // this stream is what the agent is doing NOW. The file itself is the
        // record for anyone who wants the history.
        let mut seen: Option<u64> = crate::desktop::journal::since(&journal, None)
            .last()
            .map(|entry| entry.seq);

        let mut last: Option<String> = None;
        loop {
            // The desktop channel FIRST, and as its own events: a click is a
            // discrete thing that happened, not a change to a board, and
            // folding it into the run snapshot would lose every action that
            // happened between two polls.
            let fresh = crate::desktop::journal::since(&journal, seen);
            let mut sent_desktop = false;
            for entry in &fresh {
                let data = serde_json::to_string(entry).unwrap_or_default();
                if tx.send(sse_frame("desktop", &data)).await.is_err() {
                    return; // the client went away
                }
                seen = Some(entry.seq);
                sent_desktop = true;
            }

            let snap = super::snapshot(&workspace);
            let body = serde_json::to_string(&snap).unwrap_or_default();
            // Compared WITHOUT its timestamp, which changes every tick and
            // would make every poll look like news.
            let compared =
                serde_json::to_string(&(&snap.workers, &snap.roadmap)).unwrap_or_default();

            if last.as_deref() == Some(compared.as_str()) {
                // A comment line, so a proxy or an idle browser does not decide
                // the connection is dead during a long quiet stretch. Skipped
                // when desktop events just went out: those are traffic, and a
                // keep-alive beside them is noise.
                if !sent_desktop && tx.send(Bytes::from(": tick\n\n")).await.is_err() {
                    return;
                }
            } else {
                last = Some(compared);
                if tx.send(sse_frame("run", &body)).await.is_err() {
                    return;
                }
            }
            tokio::time::sleep(POLL).await;
        }
    });

    let body = EventBody(rx);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(body.boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// A body fed by a channel, one event at a time.
///
/// Implements [`hyper::body::Body`] directly rather than reaching for a stream
/// adapter: the adapters live in `futures-util`, which this crate does not
/// depend on, and two crates to turn a receiver into a stream is a poor trade
/// for the eight lines below.
struct EventBody(tokio::sync::mpsc::Receiver<Bytes>);

impl hyper::body::Body for EventBody {
    type Data = Bytes;
    /// Nothing here can fail: the sender is this process, and a client that
    /// hangs up ends the stream rather than erroring it.
    type Error = Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.get_mut()
            .0
            .poll_recv(cx)
            .map(|next| next.map(|bytes| Ok(Frame::data(bytes))))
    }
}

/// Serve until the process ends.
///
/// `port` of `None` asks the OS for a free one — see [`ADDRESS_FILE`]. A named
/// port is bound or the call fails; it is never silently exchanged for another.
///
/// # Errors
///
/// When the port cannot be bound, which for a named port almost always means
/// something else already has it. Reported rather than swallowed: an interface
/// pointed at a server that never started should say so.
pub async fn listen(workspace: PathBuf, port: Option<u16>) -> std::io::Result<()> {
    // Port 0 is the ask-the-OS convention, and the only place that constant
    // means anything — hence no named constant for it.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0)));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // The port ACTUALLY bound, which for `None` is the one the OS just chose.
    let bound = listener.local_addr()?.port();
    match publish(&workspace, bound) {
        Ok(address) => println!("Watching {} on {}", address.workspace, address.url),
        Err(e) => println!(
            "Listening on http://127.0.0.1:{bound}, but could not write the address file: {e}"
        ),
    }
    println!("  GET /v1/run/state     what is happening now");
    println!("  GET /v1/run/stream    the same, whenever it changes");
    println!("  GET /v1/project/gate  whether the project is ready to be worked in");
    println!("  GET /v1/tasks         every task in the ledger");
    println!("  GET /v1/team          what agents said, and open questions");

    // A write with no token to check against must be refused, not waved
    // through — so a token that could not be written means reads only.
    let token: Arc<Option<String>> = Arc::new(match issue_token(&workspace) {
        Ok(token) => {
            println!(
                "  writes need the token in {}",
                workspace
                    .join(crate::ledger::LEDGER_DIR)
                    .join(TOKEN_FILE)
                    .display()
            );
            println!("  POST /v1/team/answer  answer an agent's question as the owner");
            Some(token)
        }
        Err(e) => {
            println!("  could not write the token file ({e}); serving reads only");
            None
        }
    });
    let access = Arc::new(Access::from_env(bound));
    let workspace = Arc::new(workspace);

    loop {
        let (socket, _) = listener.accept().await?;
        let workspace = Arc::clone(&workspace);
        let access = Arc::clone(&access);
        let token = Arc::clone(&token);
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(socket);
            let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let workspace = Arc::clone(&workspace);
                let access = Arc::clone(&access);
                let token = Arc::clone(&token);
                async move {
                    let header = |name| {
                        req.headers()
                            .get(name)
                            .and_then(|v: &hyper::header::HeaderValue| v.to_str().ok())
                    };
                    let origin = match access
                        .admit(header(hyper::header::HOST), header(hyper::header::ORIGIN))
                    {
                        Ok(origin) => origin,
                        Err(why) => {
                            return Ok::<_, Infallible>(json(
                                StatusCode::FORBIDDEN,
                                &serde_json::json!({ "error": why }),
                            ))
                        }
                    };
                    let authorized = (*token)
                        .as_deref()
                        .is_some_and(|t| bearer_matches(header(hyper::header::AUTHORIZATION), t));
                    let path = req.uri().path().to_string();
                    let method = req.method().clone();

                    if method == Method::POST {
                        let body = http_body_util::Limited::new(req.into_body(), MAX_BODY);
                        let bytes = match body.collect().await {
                            Ok(collected) => collected.to_bytes(),
                            Err(e) => {
                                let status = if e.is::<http_body_util::LengthLimitError>() {
                                    StatusCode::PAYLOAD_TOO_LARGE
                                } else {
                                    StatusCode::BAD_REQUEST
                                };
                                return Ok(with_cors(
                                    json(status, &serde_json::json!({ "error": e.to_string() })),
                                    origin.as_deref(),
                                ));
                            }
                        };
                        if let Some(reply) =
                            command(&method, &path, authorized, &bytes, workspace.as_path())
                        {
                            return Ok(with_cors(reply, origin.as_deref()));
                        }
                    }

                    let reply = match route(&method, &path, workspace.as_path()) {
                        Some(reply) => reply,
                        // The one route the router hands back: the stream needs
                        // to own a ticker, which a pure function cannot.
                        None => stream((*workspace).clone()),
                    };
                    Ok(with_cors(reply, origin.as_deref()))
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The state endpoint answers with the run, as JSON.**
    #[test]
    fn the_state_endpoint_returns_the_snapshot() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let reply = route(&Method::GET, "/v1/run/state", dir.path())
            .expect("a GET for state is answered here, not by the stream");

        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(
            reply
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    /// **The stream is NOT answered by the router.**
    ///
    /// `None` is the signal that this route needs a ticker the caller owns.
    /// Answering it here with a plain body would produce a connection that
    /// closes immediately and a client that reconnects forever.
    #[test]
    fn the_stream_route_is_handed_back_to_the_caller() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        assert!(route(&Method::GET, "/v1/run/stream", dir.path()).is_none());
    }

    /// An unknown path is a 404 that says so, not a silent empty 200.
    #[test]
    fn an_unknown_path_is_refused_in_words() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let reply = route(&Method::GET, "/v1/run/anything-else", dir.path()).expect("answered");
        assert_eq!(reply.status(), StatusCode::NOT_FOUND);
    }

    /// **A write is not a route here.**
    ///
    /// `route` is read-only by construction: there is no POST arm to forget to
    /// guard. Writes live in [`command`], which checks the token first. If a
    /// write arm is ever added to `route`, this test fails and whoever adds it
    /// has to decide what it means — which is the point.
    #[test]
    fn nothing_here_accepts_a_write() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            let reply = route(&method, "/v1/run/state", dir.path())
                .expect("every method is answered, not dropped");
            assert_eq!(
                reply.status(),
                StatusCode::NOT_FOUND,
                "{method} must not be a way in"
            );
        }
    }

    /// The browser's pre-flight is answered, or the interface never gets to ask
    /// — and only an admitted origin is named in the answer.
    #[test]
    fn a_cross_origin_preflight_is_allowed() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let reply = route(&Method::OPTIONS, "/v1/run/state", dir.path()).expect("answered");
        assert_eq!(reply.status(), StatusCode::NO_CONTENT);
        let reply = with_cors(reply, Some("http://localhost:5173"));
        assert_eq!(
            reply
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("http://localhost:5173")
        );
    }

    async fn body_text(reply: Response<Body>) -> String {
        let bytes = reply
            .into_body()
            .collect()
            .await
            .map(http_body_util::Collected::to_bytes)
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// **THE FRONT END SEES EVERY TASK, NOT ONLY THE ONES AN AGENT TOUCHED.**
    ///
    /// `/v1/run/state` shows workers — tasks with a transcript. A board needs
    /// the whole ledger: what is queued, blocked, done and on which branch.
    #[tokio::test]
    async fn the_task_list_is_the_whole_ledger() {
        let dir = tempfile::tempdir().expect("a workspace");
        let mut ledger = crate::ledger::Ledger::new();
        ledger.tasks.push(crate::ledger::Task::new(
            "t-queued",
            "r-1",
            "never picked up",
        ));
        ledger.save(dir.path()).expect("save");

        let reply = route(&Method::GET, "/v1/tasks", dir.path()).expect("answered");
        assert_eq!(reply.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_text(reply).await).expect("json");
        assert_eq!(body["tasks"][0]["id"], "t-queued", "{body}");
    }

    /// A ledger that cannot be read is an error the interface can show, not
    /// an empty board that reads as "nothing to do".
    #[tokio::test]
    async fn a_broken_ledger_is_an_error_not_an_empty_board() {
        let dir = tempfile::tempdir().expect("a workspace");
        let path = crate::ledger::Ledger::path_for(dir.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, "{ not json").expect("write");

        let reply = route(&Method::GET, "/v1/tasks", dir.path()).expect("answered");
        assert_eq!(reply.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body_text(reply).await.contains("not a ledger"));
    }

    /// The team channel: what was said, and which questions still wait for an
    /// answer — the list an answer box is built from.
    #[tokio::test]
    async fn the_team_endpoint_lists_messages_and_open_questions() {
        let dir = tempfile::tempdir().expect("a workspace");
        let mut channel = crate::team::Channel::in_workspace(dir.path());
        channel
            .say("review", "all", crate::team::Kind::Note, None, "heads up")
            .expect("say");
        channel
            .say(
                "implement",
                "review",
                crate::team::Kind::Question,
                None,
                "uuid?",
            )
            .expect("ask");

        let reply = route(&Method::GET, "/v1/team", dir.path()).expect("answered");
        assert_eq!(reply.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body_text(reply).await).expect("json");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(2), "{body}");
        assert_eq!(body["open_questions"][0]["text"], "uuid?", "{body}");
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn answer_body(question: u64, text: &str) -> Vec<u8> {
        serde_json::json!({ "question": question, "text": text })
            .to_string()
            .into_bytes()
    }

    fn a_workspace_with_an_open_question() -> (tempfile::TempDir, u64) {
        let dir = tempfile::tempdir().expect("a workspace");
        let asked = crate::team::Channel::in_workspace(dir.path())
            .say(
                "implement",
                "all",
                crate::team::Kind::Question,
                None,
                "uuid or int?",
            )
            .expect("ask");
        (dir, asked.seq)
    }

    /// The token is checked as a whole, never by prefix, and only in the
    /// `Bearer` form a browser `fetch` sends.
    #[test]
    fn only_the_exact_bearer_token_is_accepted() {
        let good = format!("Bearer {TOKEN}");
        assert!(bearer_matches(Some(&good), TOKEN));
        assert!(!bearer_matches(None, TOKEN), "no header");
        assert!(!bearer_matches(Some(TOKEN), TOKEN), "no Bearer scheme");
        assert!(!bearer_matches(Some("Bearer "), TOKEN), "empty token");
        assert!(
            !bearer_matches(Some(&format!("Bearer {}", &TOKEN[..31])), TOKEN),
            "a prefix of the token"
        );
        assert!(
            !bearer_matches(Some(&format!("Bearer {TOKEN}0")), TOKEN),
            "the token plus more"
        );
    }

    /// **A WRITE WITHOUT THE TOKEN IS REFUSED, AND NOTHING IS WRITTEN.**
    #[test]
    fn an_answer_without_the_token_is_refused_and_nothing_is_written() {
        let (dir, asked) = a_workspace_with_an_open_question();
        let reply = command(
            &Method::POST,
            "/v1/team/answer",
            false,
            &answer_body(asked, "uuid"),
            dir.path(),
        )
        .expect("a command route");
        assert_eq!(reply.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            crate::team::Channel::in_workspace(dir.path())
                .open_questions()
                .len(),
            1,
            "an unauthorised request closed the question"
        );
    }

    /// **THE PERSON ANSWERS AN AGENT'S QUESTION FROM THE INTERFACE.**
    #[test]
    fn the_owner_answers_an_open_question() {
        let (dir, asked) = a_workspace_with_an_open_question();
        let reply = command(
            &Method::POST,
            "/v1/team/answer",
            true,
            &answer_body(asked, "uuid"),
            dir.path(),
        )
        .expect("a command route");
        assert_eq!(reply.status(), StatusCode::OK);

        let channel = crate::team::Channel::in_workspace(dir.path());
        assert!(
            channel.open_questions().is_empty(),
            "the question is still open"
        );
        let last = channel.all().pop().expect("the answer");
        assert_eq!(last.from, crate::team::OWNER);
        assert_eq!(last.kind, crate::team::Kind::Answer { to: asked });
    }

    /// An answer to a message that does not exist is refused, and recorded
    /// nowhere — the same rule the agents' own `team_answer` follows.
    #[test]
    fn an_answer_to_no_such_question_is_refused() {
        let (dir, _) = a_workspace_with_an_open_question();
        let before = crate::team::Channel::in_workspace(dir.path()).all().len();
        let reply = command(
            &Method::POST,
            "/v1/team/answer",
            true,
            &answer_body(999, "uuid"),
            dir.path(),
        )
        .expect("a command route");
        assert_eq!(reply.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            crate::team::Channel::in_workspace(dir.path()).all().len(),
            before
        );
    }

    #[test]
    fn a_malformed_answer_is_a_bad_request() {
        let (dir, _) = a_workspace_with_an_open_question();
        let reply = command(
            &Method::POST,
            "/v1/team/answer",
            true,
            b"not json",
            dir.path(),
        )
        .expect("a command route");
        assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
    }

    /// Reads stay reads: a path that is not a command is not answered here.
    #[test]
    fn a_read_path_is_not_a_command() {
        let dir = tempfile::tempdir().expect("a workspace");
        assert!(command(&Method::POST, "/v1/run/state", true, b"{}", dir.path()).is_none());
        assert!(command(&Method::GET, "/v1/team/answer", true, b"", dir.path()).is_none());
    }

    /// The preflight must allow what a write needs, or a browser never sends it.
    #[test]
    fn the_preflight_allows_a_tokened_post() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        let reply = route(&Method::OPTIONS, "/v1/team/answer", dir.path()).expect("answered");
        let header = |name: &str| {
            reply
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase()
        };
        assert!(header("access-control-allow-methods").contains("post"));
        assert!(header("access-control-allow-headers").contains("authorization"));
    }

    const PORT: u16 = 41234;

    fn access() -> Access {
        Access::new(PORT, vec!["tauri://localhost".to_string()])
    }

    fn at_home() -> Option<String> {
        Some(format!("127.0.0.1:{PORT}"))
    }

    /// **NO REPLY IS OPEN TO EVERY ORIGIN.**
    ///
    /// Loopback stops other MACHINES, not other WEBSITES: any page the
    /// operator opens can `fetch` 127.0.0.1, and a wildcard lets its script
    /// read what comes back — file paths, task titles, what the agent typed.
    #[test]
    fn no_route_answers_with_a_wildcard_origin() {
        let dir = tempfile::tempdir().expect("a scratch workspace");
        for (method, path) in [
            (Method::GET, "/v1/run/state"),
            (Method::GET, "/v1/project/gate"),
            (Method::OPTIONS, "/v1/run/state"),
            (Method::GET, "/nowhere"),
        ] {
            let reply = route(&method, path, dir.path()).expect("answered");
            assert!(
                reply.headers().get("access-control-allow-origin").is_none(),
                "{method} {path} named an origin before anyone was admitted"
            );
        }
    }

    /// **A PAGE FROM ANOTHER SITE IS REFUSED.**
    #[test]
    fn a_foreign_origin_is_refused() {
        let refused = access().admit(at_home().as_deref(), Some("https://evil.example"));
        assert!(refused.is_err(), "a foreign site was admitted: {refused:?}");
        let null = access().admit(at_home().as_deref(), Some("null"));
        assert!(null.is_err(), "an opaque origin was admitted: {null:?}");
    }

    /// **A REQUEST ADDRESSED TO ANOTHER NAME IS REFUSED.**
    ///
    /// DNS rebinding: a hostile domain is pointed at 127.0.0.1, so the page's
    /// requests are same-origin in the browser's eyes and no CORS rule is
    /// consulted at all. The Host header still names the hostile domain.
    #[test]
    fn a_rebound_host_is_refused() {
        let host = format!("evil.example:{PORT}");
        assert!(access().admit(Some(&host), None).is_err());
        let wrong_port = format!("127.0.0.1:{}", PORT + 1);
        assert!(access().admit(Some(&wrong_port), None).is_err());
        assert!(access().admit(None, None).is_err(), "no Host at all");
    }

    /// A local dev server, a configured shell, and a plain client all get in —
    /// and a browser origin is echoed exactly, never widened.
    #[test]
    fn local_and_configured_origins_are_admitted_and_echoed() {
        let a = access();
        assert_eq!(
            a.admit(at_home().as_deref(), Some("http://localhost:5173")),
            Ok(Some("http://localhost:5173".to_string()))
        );
        assert_eq!(
            a.admit(at_home().as_deref(), Some("http://127.0.0.1:3000")),
            Ok(Some("http://127.0.0.1:3000".to_string()))
        );
        assert_eq!(
            a.admit(at_home().as_deref(), Some("tauri://localhost")),
            Ok(Some("tauri://localhost".to_string()))
        );
        let named = format!("localhost:{PORT}");
        assert_eq!(
            a.admit(Some(&named), None),
            Ok(None),
            "curl sends no Origin"
        );
    }

    /// A loopback NAME inside a longer host is not loopback.
    #[test]
    fn a_lookalike_loopback_origin_is_refused() {
        for origin in [
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example:80",
            "http://localhost:5173/path",
            "http://localhost:notaport",
        ] {
            assert!(
                access().admit(at_home().as_deref(), Some(origin)).is_err(),
                "{origin} was taken for loopback"
            );
        }
    }

    /// **Two servers on one machine do not fight over a port.**
    ///
    /// The reason there is no default number: this machine already runs a
    /// service on the port next door, and a hardcoded neighbour is a collision
    /// waiting for the day something else picks it. Each server asks for a free
    /// port and writes down the answer, so any number of projects can be
    /// watched at once.
    #[tokio::test]
    async fn two_servers_take_different_ports_and_each_publishes_its_own() {
        let one = tempfile::tempdir().expect("a workspace");
        let two = tempfile::tempdir().expect("another workspace");

        // Bound the same way `listen` does, without spawning the accept loops.
        let a = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free port");
        let b = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("another free port");
        let (pa, pb) = (
            a.local_addr().expect("addr").port(),
            b.local_addr().expect("addr").port(),
        );

        assert_ne!(pa, pb, "the OS must not hand out the same port twice");
        assert_ne!(pa, 0, "a bound port is a real one");

        publish(one.path(), pa).expect("publish the first");
        publish(two.path(), pb).expect("publish the second");

        let found_a = published(one.path()).expect("the first is discoverable");
        let found_b = published(two.path()).expect("the second is discoverable");
        assert_eq!(found_a.port, pa);
        assert_eq!(found_b.port, pb);
        assert_eq!(found_a.url, format!("http://127.0.0.1:{pa}"));
        assert_ne!(
            found_a.workspace, found_b.workspace,
            "each file names the project it is watching"
        );
    }

    /// A workspace nobody is serving is discoverable as exactly that.
    #[test]
    fn an_unserved_workspace_publishes_nothing() {
        let dir = tempfile::tempdir().expect("a workspace");
        assert!(published(dir.path()).is_none());
    }

    /// Read frames off a stream body until one names `event`, or time runs out.
    ///
    /// Returns every frame seen, so a failing test can say what DID arrive
    /// rather than only that the thing it wanted did not.
    async fn frames_until(
        body: &mut Body,
        event: &str,
        patience: std::time::Duration,
    ) -> (Option<String>, Vec<String>) {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + patience;

        while tokio::time::Instant::now() < deadline {
            let Ok(Some(Ok(frame))) = tokio::time::timeout_at(deadline, body.frame()).await else {
                break;
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            let text = String::from_utf8_lossy(&data).to_string();
            if text.starts_with(&format!("event: {event}\n")) {
                return (Some(text.clone()), seen);
            }
            seen.push(text);
        }
        (None, seen)
    }

    /// **THE DESKTOP CHANNEL — `COMPUTER_USE.md` 14c.**
    ///
    /// The desktop MCP server is a SEPARATE PROCESS, so it cannot push into
    /// this stream; it appends to a journal and this poll loop picks it up.
    /// That seam is the whole design, and a test that only checked the journal
    /// would prove the writer works while the reader emits nothing.
    ///
    /// Drop the `since`/`sse_frame("desktop", ..)` block from [`stream`] and
    /// this fails.
    #[tokio::test]
    async fn a_desktop_action_reaches_the_stream_as_its_own_event() {
        let dir = tempfile::tempdir().expect("a workspace");
        let path = crate::desktop::journal::path_in(dir.path());
        let mut journal = crate::desktop::journal::Journal::at(path);

        // Written BEFORE the stream opens, so it belongs to the history the
        // stream must NOT replay.
        journal.record("click", "history", true).expect("write");

        let mut body = stream(dir.path().to_path_buf()).into_body();

        // WAIT FOR THE FIRST FRAME BEFORE WRITING ANYTHING ELSE. The stream's
        // work happens in a spawned task, and `#[tokio::test]` runs a
        // current-thread runtime — so that task does not take its initial
        // cursor until this test awaits. Writing first would put both records
        // in the file before the cursor was taken, and the stream would
        // correctly treat both as history. The first frame is the signal that
        // the task has run.
        let (_, opening) = frames_until(&mut body, "never", POLL * 4).await;
        assert!(!opening.is_empty(), "the stream sent nothing on connect");

        // And this one happens while someone is watching.
        journal
            .record("click_element", "element e16", true)
            .expect("write");

        let (found, others) =
            frames_until(&mut body, "desktop", std::time::Duration::from_secs(10)).await;

        let frame = found.unwrap_or_else(|| {
            panic!("no desktop event arrived; the stream sent: {others:?}");
        });
        assert!(
            frame.contains("element e16"),
            "the wrong action was streamed: {frame}"
        );
        assert!(
            !frame.contains("history"),
            "the stream replayed history to a client that just connected: {frame}"
        );
        assert!(
            frame.ends_with("\n\n"),
            "an SSE frame must end with a blank line or a browser buffers it: {frame:?}"
        );
        assert!(
            others.iter().all(|f| !f.contains("history")),
            "history leaked into an earlier frame: {others:?}"
        );
    }

    /// An event is framed the way the protocol requires — named, and terminated
    /// by a BLANK LINE. Without the blank line a browser buffers forever.
    #[test]
    fn an_event_is_framed_so_a_browser_will_deliver_it() {
        let frame = sse_frame("run", r#"{"a":1}"#);
        let text = String::from_utf8(frame.to_vec()).expect("utf-8");
        assert_eq!(text, "event: run\ndata: {\"a\":1}\n\n");
        assert!(
            text.ends_with("\n\n"),
            "the blank line is what dispatches it"
        );
    }
}
