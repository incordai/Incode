//! The MCP servers an ACP client hands a session, as incode's own tools — and
//! the external tools the owner installed ([`crate::ext_tools`]), which are
//! connected the same way in every mode.
//!
//! # Why incode connects to them itself
//!
//! Every tool a model calls runs through the toolbox's one gate: its schema,
//! the operator's guards, the permission mode, the memory engine's pre-tool
//! hook, the person to ask and the cancel token. A server handed to the agent
//! CLI would run outside all of that, so the servers a client names in
//! `session/new` are started HERE, as children of this process, and each of
//! their tools is offered to the model under a prefixed name
//! (`mcp__<server>__<tool>`) and run through [`super::Toolbox::run_call`] like
//! any other.
//!
//! # What such a tool may do
//!
//! Nothing here can tell what a server's tool touches, and what the server
//! says about itself is not evidence. So every one is [`super::Reach::Run`]:
//! refused in a read-only session, asked about in a prompting one, and REFUSED
//! when the memory engine cannot decide (fail closed).
//!
//! # Connected in the background
//!
//! `session/new` does not wait for them. The owner's memory service takes 1 to
//! 15 s to list its tools, and more than 20 s cold (measured), so a session
//! that waited took 42 s to open. [`ClientServers::connect_in_background`]
//! starts every server on a thread of its own and returns at once; each
//! server's tools join the session's toolbox the moment it has listed them,
//! so the next `tools/list` — the bridge asks once per turn, the API route
//! every request — offers them. Until then the server is named as still
//! connecting ([`ClientServers::connecting`]), and what became of it is
//! handed to a callback, a line per server and per tool left out.
//!
//! # Bounds
//!
//! Starting a server and listing its tools: [`START_BOUND`] for each request
//! to a stdio server, [`HTTP_START_BOUND`] to an HTTP one; a server that has
//! not answered by then is dropped, and said so. One call: [`CALL_BOUND`], and
//! abandoned at once when the turn is stopped. The children die with the
//! session's tools — EOF, then their process trees — and a server still
//! connecting when the session ends is dropped the moment it finishes.
//!
//! # Two transports
//!
//! A stdio server is a child this process starts. An HTTP server is an address
//! the client names, spoken to over MCP's Streamable HTTP
//! ([`super::mcp_http`]): https anywhere, plain http only to this machine, no
//! redirect followed, its headers sent as given and written nowhere else.
//! Either is the same [`McpClient`] and the same gate. SSE — the older HTTP
//! transport, a GET stream beside the POSTs — is not spoken.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use serde_json::{json, Value};

use super::image::ToolOutput;
use super::mcp_client::{ChildLink, Link, McpClient};
use super::mcp_http::HttpLink;

/// How long a stdio server has to answer `initialize`, and then `tools/list`.
pub(crate) const START_BOUND: Duration = Duration::from_secs(20);
/// The same for an HTTP server. Longer: the owner's memory service was
/// measured taking 1 to 15 s to list its tools, and more than 20 s cold — and
/// since connecting no longer holds `session/new`, a long bound costs nothing
/// but a thread.
pub(crate) const HTTP_START_BOUND: Duration = Duration::from_secs(60);
/// How long one tool call may take.
pub(crate) const CALL_BOUND: Duration = Duration::from_secs(120);
/// The most servers one session starts.
pub(crate) const MAX_SERVERS: usize = 16;
/// The most tools taken from one server.
const MAX_TOOLS_PER_SERVER: usize = 128;
/// What every such tool's name starts with.
pub(crate) const PREFIX: &str = "mcp__";
/// The longest tool name a model is handed: what the model APIs accept.
const MAX_NAME: usize = 64;

/// A server, as the client named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerSpec {
    pub(crate) name: String,
    pub(crate) transport: Transport,
}

/// How a server is reached.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Transport {
    /// A child this process starts, spoken to on its stdin and stdout.
    Stdio {
        command: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// An MCP endpoint over Streamable HTTP, and the headers to send it —
    /// and, for a server from incode's own configuration, where its OAuth
    /// token is kept ([`crate::mcp_oauth`]).
    Http {
        url: String,
        headers: Vec<(String, String)>,
        oauth: Option<crate::mcp_oauth::Binding>,
    },
}

impl std::fmt::Debug for Transport {
    /// Names only: a header's or a variable's value, or the address, can
    /// carry a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |pairs: &[(String, String)]| -> Vec<String> {
            pairs.iter().map(|(name, _)| name.clone()).collect()
        };
        match self {
            Self::Stdio { command, args, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &names(env))
                .finish(),
            Self::Http { headers, oauth, .. } => f
                .debug_struct("Http")
                .field("headers", &names(headers))
                .field("oauth", &oauth.is_some())
                .finish_non_exhaustive(),
        }
    }
}

/// One tool of one server.
struct ClientTool {
    /// The name the model calls it by: `mcp__<server>__<tool>`.
    name: String,
    /// The name the server knows it by.
    remote: String,
    /// As the model is told about it, in the toolbox's spelling.
    definition: Value,
    validator: Result<jsonschema::Validator, String>,
}

/// One running server and its tools.
struct Server {
    client: Mutex<McpClient>,
    tools: Vec<ClientTool>,
}

/// Every server a session was handed: those connected, and those still
/// connecting.
#[derive(Default)]
pub(super) struct ClientServers {
    /// Shared, so a call runs without holding the list: a server that
    /// finishes connecting meanwhile is added at once.
    servers: Vec<Arc<Server>>,
    /// The servers started and not yet connected or dropped, by name.
    connecting: Vec<String>,
    /// Whether this session's servers were started. Once per session.
    opened: bool,
}

/// How long each kind of server has to start: [`START_BOUND`] and
/// [`HTTP_START_BOUND`], except in a test.
#[derive(Debug, Clone, Copy)]
pub(super) struct Bounds {
    pub(super) stdio: Duration,
    pub(super) http: Duration,
}

impl Bounds {
    pub(super) const DEFAULT: Self = Self {
        stdio: START_BOUND,
        http: HTTP_START_BOUND,
    };
}

/// Told what became of each server, a line at a time, from the thread that
/// connected it.
pub(crate) type OnConnected = Arc<dyn Fn(String) + Send + Sync>;

/// `text` with everything outside `[A-Za-z0-9_-]` replaced by `_`.
fn safe(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl ClientServers {
    /// Start every server into `shared`, each on its own thread, and return
    /// at once: the lines returned say what was started and what was refused
    /// before starting. Each server's tools join `shared` the moment it has
    /// listed them, and what became of it — its tools, what was left out, or
    /// why it was dropped — is handed to `on_connected`. A server that fails
    /// costs its own tools and nothing else. Once per session: a second call
    /// starts nothing.
    pub(super) fn connect_in_background(
        shared: &Arc<RwLock<Self>>,
        specs: Vec<ServerSpec>,
        cwd: &Path,
        filter_env: bool,
        bounds: Bounds,
        on_connected: &OnConnected,
    ) -> Vec<String> {
        let mut said = Vec::new();
        let mut all = shared.write().unwrap_or_else(PoisonError::into_inner);
        if all.opened {
            return vec!["this session's MCP servers are already connected".to_string()];
        }
        all.opened = true;
        let mut taken: Vec<String> = Vec::new();
        for (index, spec) in specs.into_iter().enumerate() {
            let server = safe(spec.name.trim());
            if index >= MAX_SERVERS {
                said.push(format!(
                    "MCP server `{}` was not started: a session starts at most {MAX_SERVERS}",
                    spec.name
                ));
                continue;
            }
            if server.is_empty() || taken.contains(&server) {
                said.push(format!(
                    "MCP server `{}` was not started: its name is empty or already taken",
                    spec.name
                ));
                continue;
            }
            taken.push(server.clone());
            let bound = match spec.transport {
                Transport::Stdio { .. } => bounds.stdio,
                Transport::Http { .. } => bounds.http,
            };
            said.push(format!(
                "MCP server `{server}`: connecting in the background (up to {}s for each \
                 request); its tools are offered from the first turn after it answers",
                bound.as_secs()
            ));
            all.connecting.push(server.clone());
            let into = Arc::downgrade(shared);
            let cwd = cwd.to_path_buf();
            let on_connected = Arc::clone(on_connected);
            std::thread::spawn(move || {
                let started = start(&spec, &server, &cwd, filter_env, bound);
                // The session may have ended meanwhile: then nothing is
                // added, and a server that did connect is dropped here — EOF,
                // then its tree.
                let Some(shared) = into.upgrade() else {
                    return;
                };
                let lines = shared
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .joined(&server, started);
                drop(shared);
                for line in lines {
                    on_connected(line);
                }
            });
        }
        said
    }

    /// `server`, connected or not: its tools added, and it no longer counted
    /// as connecting. Returns what to say about it.
    fn joined(
        &mut self,
        server: &str,
        started: Result<(McpClient, Vec<Value>), String>,
    ) -> Vec<String> {
        self.connecting.retain(|name| name != server);
        match started {
            Ok((client, listed)) => {
                let (tools, mut said) = tools_of(server, listed);
                said.push(format!(
                    "MCP server `{server}`: {} tool(s), run through incode's own gate",
                    tools.len()
                ));
                self.servers.push(Arc::new(Server {
                    client: Mutex::new(client),
                    tools,
                }));
                said
            }
            Err(why) => vec![format!("MCP server `{server}` was not started: {why}")],
        }
    }

    /// The servers started and still connecting, by name.
    pub(super) fn connecting(&self) -> Vec<String> {
        self.connecting.clone()
    }

    fn find(&self, name: &str) -> Option<(&Server, &ClientTool)> {
        self.servers.iter().find_map(|server| {
            server
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .map(|tool| (&**server, tool))
        })
    }

    /// Whether `name` is one of these tools.
    pub(super) fn has(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// Every tool, as the model is told about them.
    pub(super) fn definitions(&self) -> Vec<Value> {
        self.servers
            .iter()
            .flat_map(|server| server.tools.iter().map(|tool| tool.definition.clone()))
            .collect()
    }

    /// Whether `input` is what the tool's own schema asks for.
    pub(super) fn check(&self, name: &str, input: &Value) -> Result<(), String> {
        match self.find(name) {
            Some((_, tool)) => super::validate::check_with(name, &tool.validator, input),
            None => Err(format!("there is no tool called `{name}`")),
        }
    }

    /// The server that runs `name`, and the name it knows the tool by.
    fn route(&self, name: &str) -> Option<(Arc<Server>, String)> {
        self.servers.iter().find_map(|server| {
            server
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .map(|tool| (Arc::clone(server), tool.remote.clone()))
        })
    }
}

/// Run one tool of `shared`'s servers, within [`CALL_BOUND`], abandoned when
/// `stopped` says so. The list is not held while the call runs, so a server
/// that finishes connecting meanwhile is not kept waiting.
pub(super) fn call(
    shared: &RwLock<ClientServers>,
    name: &str,
    input: &Value,
    stopped: &dyn Fn() -> bool,
) -> Result<ToolOutput, String> {
    let (server, remote) = shared
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .route(name)
        .ok_or_else(|| format!("there is no tool called `{name}`"))?;
    let mut client = server.client.lock().unwrap_or_else(PoisonError::into_inner);
    if client.is_broken() {
        return Err(format!(
            "{name} is not available: its MCP server stopped answering"
        ));
    }
    client.call(&remote, input, CALL_BOUND, stopped)
}

/// Start one server — a child, or a link to an address — and list its tools,
/// giving each request `bound`.
fn start(
    spec: &ServerSpec,
    server: &str,
    cwd: &Path,
    filter_env: bool,
    bound: Duration,
) -> Result<(McpClient, Vec<Value>), String> {
    let what = format!("MCP server `{server}`");
    let link: Box<dyn Link> = match &spec.transport {
        Transport::Stdio { command, args, env } => {
            let mut command = Command::new(command);
            command.args(args).current_dir(cwd);
            if filter_env {
                command.env_clear();
                command.envs(crate::tools::env_filter::filtered_env());
            }
            command.envs(env.iter().map(|(k, v)| (k, v)));
            Box::new(ChildLink::spawn(command, &what)?)
        }
        // Each POST is capped at the longest a call may take; `McpClient`
        // bounds each request more tightly where it must.
        Transport::Http {
            url,
            headers,
            oauth,
        } => {
            let link = HttpLink::connect(url, headers, CALL_BOUND, &what)?;
            match oauth {
                Some(binding) => {
                    Box::new(link.with_auth(crate::mcp_oauth::TokenSource::new(binding, url)))
                }
                None => Box::new(link),
            }
        }
    };
    let mut client = McpClient::open(link, &what, bound)?;
    let listed = client.list_tools(bound)?;
    Ok((client, listed))
}

/// The tools a server listed, as the toolbox offers them, and what was left
/// out and why.
fn tools_of(server: &str, listed: Vec<Value>) -> (Vec<ClientTool>, Vec<String>) {
    let mut tools: Vec<ClientTool> = Vec::new();
    let mut notes = Vec::new();
    for def in listed {
        let Some(remote) = def.get("name").and_then(Value::as_str).map(str::to_string) else {
            notes.push(format!("MCP server `{server}` listed a tool with no name"));
            continue;
        };
        let name = format!("{PREFIX}{server}__{}", safe(&remote));
        if name.len() > MAX_NAME {
            notes.push(format!(
                "`{remote}` from MCP server `{server}` was left out: `{name}` is longer than \
                 {MAX_NAME} characters"
            ));
            continue;
        }
        if tools.iter().any(|t| t.name == name) {
            notes.push(format!(
                "`{remote}` from MCP server `{server}` was left out: another of its tools \
                 has the same name here"
            ));
            continue;
        }
        if tools.len() >= MAX_TOOLS_PER_SERVER {
            notes.push(format!(
                "MCP server `{server}` lists more than {MAX_TOOLS_PER_SERVER} tools; the rest \
                 were left out"
            ));
            break;
        }
        let schema = def
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        let validator = jsonschema::validator_for(&schema).map_err(|e| e.to_string());
        if let Err(why) = &validator {
            notes.push(format!(
                "`{remote}` from MCP server `{server}` was left out: its input schema does not \
                 compile ({why})"
            ));
            continue;
        }
        let said = def
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let description = format!("From the MCP server `{server}`. {said}");
        tools.push(ClientTool {
            definition: json!({
                "name": name,
                "description": description.trim_end(),
                "input_schema": schema,
            }),
            name,
            remote,
            validator,
        });
    }
    (tools, notes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Set only in the child [`fake_server`] starts; its value is the file
    /// every call that reaches the server is written to.
    const CHILD: &str = "INCODE_FAKE_MCP_CHILD";
    /// How long the child waits before answering `tools/list`, in ms.
    const LIST_DELAY: &str = "INCODE_FAKE_MCP_LIST_DELAY_MS";

    /// The child half: this test binary, serving one tool over MCP on its
    /// real stdin and stdout. Returns at once — passing — unless started by
    /// [`fake_server`].
    #[test]
    fn fake_mcp_server_child() {
        let Some(log) = std::env::var_os(CHILD) else {
            return;
        };
        struct Echo(PathBuf);
        impl crate::mcp_stdio::Handler for Echo {
            fn name(&self) -> &str {
                "fake"
            }
            fn tools(&mut self) -> Result<Vec<Value>, String> {
                let delay = std::env::var(LIST_DELAY)
                    .ok()
                    .and_then(|ms| ms.parse().ok())
                    .unwrap_or(0);
                std::thread::sleep(std::time::Duration::from_millis(delay));
                Ok(vec![json!({"name": "echo", "description": "Say it back.",
                           "inputSchema": {"type": "object",
                                           "properties": {"text": {"type": "string"}},
                                           "required": ["text"]}})])
            }
            fn call(&mut self, name: &str, args: &Value) -> Result<String, String> {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.0)
                    .map_err(|e| e.to_string())?;
                writeln!(file, "{name} {args}").map_err(|e| e.to_string())?;
                Ok(format!(
                    "echo: {}",
                    args["text"].as_str().unwrap_or_default()
                ))
            }
        }
        let stdin = std::io::stdin();
        let _ = crate::mcp_stdio::serve(
            &mut Echo(PathBuf::from(log)),
            &mut stdin.lock(),
            &mut std::io::stdout(),
        );
        std::process::exit(0);
    }

    /// A server the client could name: this test binary, running only
    /// [`fake_mcp_server_child`], writing every call it serves to `log`.
    pub(crate) fn fake_server(name: &str, log: &Path) -> ServerSpec {
        ServerSpec {
            name: name.to_string(),
            transport: Transport::Stdio {
                command: std::env::current_exe().unwrap_or_default(),
                args: [
                    "--exact",
                    "toolbox::client_servers::tests::fake_mcp_server_child",
                    "--nocapture",
                    "--test-threads=1",
                    "-q",
                ]
                .map(str::to_string)
                .to_vec(),
                env: vec![(CHILD.to_string(), log.to_string_lossy().into_owned())],
            },
        }
    }

    /// [`fake_server`], answering `tools/list` only after `delay_ms` — a
    /// slow server, or with a long enough delay one that never answers.
    pub(crate) fn slow_fake_server(name: &str, log: &Path, delay_ms: u64) -> ServerSpec {
        let mut spec = fake_server(name, log);
        if let Transport::Stdio { env, .. } = &mut spec.transport {
            env.push((LIST_DELAY.to_string(), delay_ms.to_string()));
        }
        spec
    }

    /// Wait, bounded, until none of `shared`'s servers is still connecting.
    pub(crate) fn wait_connected(shared: &RwLock<ClientServers>, bound: Duration) -> bool {
        let deadline = std::time::Instant::now() + bound;
        while std::time::Instant::now() < deadline {
            if shared
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .connecting
                .is_empty()
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A session's servers, connected and waited for: what a test calls.
    pub(crate) struct Connected(Arc<RwLock<ClientServers>>);

    impl Connected {
        /// Connect `specs` from `cwd` and wait until each has connected or
        /// been dropped; also returns every line said about them.
        pub(crate) fn now(specs: Vec<ServerSpec>, cwd: &Path) -> (Self, Vec<String>) {
            let shared: Arc<RwLock<ClientServers>> = Arc::default();
            let (on, later) = collector();
            let mut said = ClientServers::connect_in_background(
                &shared,
                specs,
                cwd,
                false,
                Bounds::DEFAULT,
                &on,
            );
            assert!(wait_connected(&shared, Duration::from_secs(60)));
            said.extend(later.lock().expect("said").iter().cloned());
            (Self(shared), said)
        }

        pub(crate) fn definitions(&self) -> Vec<Value> {
            self.0.read().expect("servers").definitions()
        }

        pub(crate) fn has(&self, name: &str) -> bool {
            self.0.read().expect("servers").has(name)
        }

        pub(crate) fn call(
            &self,
            name: &str,
            input: &Value,
            stopped: &dyn Fn() -> bool,
        ) -> Result<ToolOutput, String> {
            call(&self.0, name, input, stopped)
        }
    }

    /// Collects what [`ClientServers::connect_in_background`] reports.
    pub(crate) fn collector() -> (OnConnected, Arc<Mutex<Vec<String>>>) {
        let said: Arc<Mutex<Vec<String>>> = Arc::default();
        let into = Arc::clone(&said);
        let on: OnConnected = Arc::new(move |line| {
            into.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line);
        });
        (on, said)
    }

    /// The calls that reached the fake server, one per line.
    pub(crate) fn calls_in(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// **A CLIENT'S SERVER IS STARTED, LISTED AND CALLED**, its tools named
    /// `mcp__<server>__<tool>`; a server that cannot start is said, not fatal.
    #[test]
    fn a_client_server_is_listed_and_called_under_its_prefix() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("calls.log");
        let missing = ServerSpec {
            name: "gone".to_string(),
            transport: Transport::Stdio {
                command: dir.path().join("no-such-program.exe"),
                args: Vec::new(),
                env: Vec::new(),
            },
        };
        let shared: Arc<RwLock<ClientServers>> = Arc::default();
        let (on, said) = collector();
        ClientServers::connect_in_background(
            &shared,
            vec![fake_server("fake", &log), missing],
            dir.path(),
            false,
            Bounds::DEFAULT,
            &on,
        );
        assert!(wait_connected(&shared, Duration::from_secs(60)));
        let said = said.lock().expect("said").clone();
        let servers = shared.read().expect("servers");
        let names: Vec<String> = servers
            .definitions()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(names, ["mcp__fake__echo"], "{said:?}");
        assert!(
            said.iter().any(|s| s.contains("`gone` was not started")),
            "{said:?}"
        );
        assert!(servers
            .check("mcp__fake__echo", &json!({"text": 5}))
            .is_err());
        drop(servers);
        let out = call(&shared, "mcp__fake__echo", &json!({"text": "hi"}), &|| {
            false
        })
        .expect("called");
        assert_eq!(out.text, "echo: hi");
        assert_eq!(calls_in(&log).len(), 1);
    }

    /// **A SLOW SERVER DOES NOT HOLD UP THE SESSION.** Connecting returns
    /// while the server is still listing its tools; it is named as
    /// connecting, and its tool joins once it has answered.
    #[test]
    fn a_slow_server_joins_after_connecting_returned() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("calls.log");
        let shared: Arc<RwLock<ClientServers>> = Arc::default();
        let (on, said) = collector();
        let began = std::time::Instant::now();
        let lines = ClientServers::connect_in_background(
            &shared,
            vec![slow_fake_server("slow", &log, 3000)],
            dir.path(),
            false,
            Bounds::DEFAULT,
            &on,
        );
        let returned = began.elapsed();
        assert!(returned < Duration::from_millis(1500), "{returned:?}");
        assert!(
            lines[0].contains("connecting in the background"),
            "{lines:?}"
        );
        {
            let servers = shared.read().expect("servers");
            assert_eq!(servers.connecting(), ["slow"]);
            assert!(servers.definitions().is_empty(), "offered before it listed");
        }
        assert!(wait_connected(&shared, Duration::from_secs(60)));
        let servers = shared.read().expect("servers");
        assert!(servers.has("mcp__slow__echo"));
        assert!(servers.connecting().is_empty());
        let said = said.lock().expect("said").clone();
        assert!(
            said.iter().any(|s| s.contains("`slow`: 1 tool(s)")),
            "{said:?}"
        );
    }

    /// **A SERVER THAT NEVER ANSWERS IS DROPPED, AND SAID SO**, at its
    /// bound, its child with it; it offers nothing and is no longer counted
    /// as connecting.
    #[test]
    fn a_server_that_never_answers_is_dropped_with_a_message() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("calls.log");
        let shared: Arc<RwLock<ClientServers>> = Arc::default();
        let (on, said) = collector();
        let bound = Duration::from_secs(2);
        let began = std::time::Instant::now();
        ClientServers::connect_in_background(
            &shared,
            vec![slow_fake_server("mute", &log, 600_000)],
            dir.path(),
            false,
            Bounds {
                stdio: bound,
                http: bound,
            },
            &on,
        );
        assert!(wait_connected(&shared, Duration::from_secs(60)));
        let took = began.elapsed();
        assert!(took >= bound, "dropped before its bound: {took:?}");
        assert!(took < Duration::from_secs(30), "{took:?}");
        let servers = shared.read().expect("servers");
        assert!(servers.definitions().is_empty());
        let said = said.lock().expect("said").clone();
        assert!(
            said.iter().any(|s| s.contains("`mute` was not started")),
            "{said:?}"
        );
    }

    /// A spec's `Debug` names its headers and variables, never their values.
    #[test]
    fn a_spec_never_shows_a_value_in_debug() {
        let http = ServerSpec {
            name: "web".to_string(),
            transport: Transport::Http {
                url: "https://example.test/mcp?key=k3y".to_string(),
                headers: vec![("authorization".to_string(), "Bearer s3cret".to_string())],
                oauth: None,
            },
        };
        let shown = format!("{http:?}");
        assert!(shown.contains("authorization"), "{shown}");
        assert!(
            !shown.contains("s3cret") && !shown.contains("k3y"),
            "{shown}"
        );
    }

    #[test]
    fn a_name_is_made_safe_and_bounded() {
        assert_eq!(safe("my server/v2"), "my_server_v2");
        let (tools, notes) = tools_of(
            "s",
            vec![
                json!({"name": "a.b", "inputSchema": {"type": "object"}}),
                json!({"name": "x".repeat(80)}),
                json!({"description": "no name"}),
            ],
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["mcp__s__a_b"]);
        assert_eq!(notes.len(), 2, "{notes:?}");
    }
}
