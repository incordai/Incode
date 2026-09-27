//! The MCP servers an ACP client hands a session, as incode's own tools.
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
//! # Bounds
//!
//! Starting a server and listing its tools: [`START_BOUND`] for each request,
//! every server on its own thread. One call: [`CALL_BOUND`], and abandoned at
//! once when the turn is stopped. The children die with the session's tools —
//! EOF, then their process trees.
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
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use super::image::ToolOutput;
use super::mcp_client::{ChildLink, Link, McpClient};
use super::mcp_http::HttpLink;

/// How long a server has to answer `initialize`, and then `tools/list`.
pub(crate) const START_BOUND: Duration = Duration::from_secs(20);
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
    /// An MCP endpoint over Streamable HTTP, and the headers to send it.
    Http {
        url: String,
        headers: Vec<(String, String)>,
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
            Self::Http { headers, .. } => f
                .debug_struct("Http")
                .field("headers", &names(headers))
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

/// Every server a session was handed, once connected.
#[derive(Default)]
pub(super) struct ClientServers {
    servers: Vec<Server>,
}

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
    /// Start every server, each on its own thread, and list its tools. What
    /// could not be started, and what was left out, is said in the lines
    /// returned; a server that failed costs its own tools and nothing else.
    pub(super) fn connect(
        specs: Vec<ServerSpec>,
        cwd: &Path,
        filter_env: bool,
    ) -> (Self, Vec<String>) {
        let mut said = Vec::new();
        let mut taken: Vec<String> = Vec::new();
        let mut started = Vec::new();
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
            let cwd = cwd.to_path_buf();
            started.push((
                server.clone(),
                std::thread::spawn(move || start(&spec, &server, &cwd, filter_env)),
            ));
        }
        let mut servers = Vec::new();
        for (server, handle) in started {
            match handle.join() {
                Ok(Ok((client, listed))) => {
                    let (tools, notes) = tools_of(&server, listed);
                    said.extend(notes);
                    said.push(format!(
                        "MCP server `{server}`: {} tool(s), run through incode's own gate",
                        tools.len()
                    ));
                    servers.push(Server {
                        client: Mutex::new(client),
                        tools,
                    });
                }
                Ok(Err(why)) => said.push(format!("MCP server `{server}` was not started: {why}")),
                Err(_) => said.push(format!(
                    "MCP server `{server}` was not started: starting it panicked"
                )),
            }
        }
        (Self { servers }, said)
    }

    fn find(&self, name: &str) -> Option<(&Server, &ClientTool)> {
        self.servers.iter().find_map(|server| {
            server
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .map(|tool| (server, tool))
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

    /// Run one tool on its server, within [`CALL_BOUND`], abandoned when
    /// `stopped` says so.
    pub(super) fn call(
        &self,
        name: &str,
        input: &Value,
        stopped: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, String> {
        let (server, tool) = self
            .find(name)
            .ok_or_else(|| format!("there is no tool called `{name}`"))?;
        let mut client = server
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if client.is_broken() {
            return Err(format!(
                "{name} is not available: its MCP server stopped answering"
            ));
        }
        client.call(&tool.remote, input, CALL_BOUND, stopped)
    }
}

/// Start one server — a child, or a link to an address — and list its tools.
fn start(
    spec: &ServerSpec,
    server: &str,
    cwd: &Path,
    filter_env: bool,
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
        Transport::Http { url, headers } => {
            Box::new(HttpLink::connect(url, headers, CALL_BOUND, &what)?)
        }
    };
    let mut client = McpClient::open(link, &what, START_BOUND)?;
    let listed = client.list_tools(START_BOUND)?;
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
        let description = format!("From the client's MCP server `{server}`. {said}");
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
        let (servers, said) =
            ClientServers::connect(vec![fake_server("fake", &log), missing], dir.path(), false);
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
        let out = servers
            .call("mcp__fake__echo", &json!({"text": "hi"}), &|| false)
            .expect("called");
        assert_eq!(out.text, "echo: hi");
        assert_eq!(calls_in(&log).len(), 1);
        assert!(servers
            .check("mcp__fake__echo", &json!({"text": 5}))
            .is_err());
    }

    /// A spec's `Debug` names its headers and variables, never their values.
    #[test]
    fn a_spec_never_shows_a_value_in_debug() {
        let http = ServerSpec {
            name: "web".to_string(),
            transport: Transport::Http {
                url: "https://example.test/mcp?key=k3y".to_string(),
                headers: vec![("authorization".to_string(), "Bearer s3cret".to_string())],
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
