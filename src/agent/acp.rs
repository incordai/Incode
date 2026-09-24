//! Agent Client Protocol (ACP) backend: drives any agent that speaks ACP on its
//! own stdin/stdout.
//!
//! # Where this came from
//!
//! [`crate::agent::registry`] listed `gemini` and `opencode` as
//! `Protocol::Unverified`: their stream formats had never been read here. Both
//! also speak ACP — a published JSON-RPC protocol with a typed schema — so one
//! client covers every agent that speaks it, and the per-agent part shrinks to
//! how it is launched ([`crate::agent::registry::AgentProbe::acp_args`]).
//!
//! # How it is driven
//!
//! - One process per TURN, exactly like the other adapters: started, contained
//!   with [`crate::agent::process_tree`], handshaken, prompted, ended. A session
//!   carries over between turns through `session/resume` (or `session/load`)
//!   when the agent advertises it, and starts fresh when it does not.
//! - The transport is written here: JSON-RPC 2.0, one message per line, on
//!   threads. Every payload is a type from `agent-client-protocol-schema`; the
//!   envelopes are its `JsonRpcMessage`, `Request`, `Response` and
//!   `Notification`. Nothing about the wire shape is copied by hand.
//! - Everything is bounded: the size of one incoming line
//!   ([`MAX_LINE_BYTES`]), the handshake ([`HANDSHAKE_TIMEOUT`]) and the silence
//!   between two messages in a turn ([`IDLE_TIMEOUT`]). The stdout reader runs
//!   on its own thread and the turn waits on it with a timeout, so a silent
//!   agent cannot hold a turn open — which the other adapters, reading stdout
//!   directly, cannot promise.
//! - The cancel token is polled every [`CANCEL_POLL`] while waiting, sends
//!   `session/cancel`, and then the whole process tree is killed.
//!
//! # What the client advertises
//!
//! NOTHING. No `fs` capability — the agent reads and writes with its own
//! tools, inside its own permission layer, and the harness does not become a
//! second file system whose confinement would have to be proven separately. No
//! `terminal` capability either. Any request for a method this client does not
//! provide is answered with JSON-RPC "method not found", never left hanging.
//!
//! # Permission policy
//!
//! `session/request_permission` is answered from the run's
//! [`PermissionMode`] and the operator's [`crate::guards::Guards`], using the
//! same rungs as [`crate::permissions::PermissionPolicy::with_defaults`]:
//!
//! | tool kind | needs | also checked |
//! |---|---|---|
//! | read, search, think | read-only | — |
//! | edit, delete, move | workspace-write | every path inside the workspace or an extra root, none protected |
//! | execute, fetch | danger-full-access | a shell command against the protected paths |
//! | anything else | — | always refused: it cannot be mapped with confidence |
//!
//! An approval is always the one-off option; a standing approval of a write
//! would cover writes nobody sees. A request that cannot be judged is refused.
//!
//! **This is not enforcement, and [`AcpBackend::enforces`] says so.** Measured
//! on opencode 1.18.4: in its ordinary mode it wrote a file WITHOUT asking. So
//! a read-only run also puts the session in the agent's own read-only mode, and
//! refuses the turn when the agent has none; protected paths stay the
//! sentinel's job ([`crate::guards::Sentinel`]).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1 as acp;
use agent_client_protocol_schema::ProtocolVersion;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::{AgentBackend, AgentError, AgentEvent, TurnOutcome};
use crate::agent::claude_code::CANCELLED;
use crate::permissions::PermissionMode;
use crate::runtime::cancel::CancelToken;

/// The longest single message accepted from the agent, in bytes.
///
/// One JSON-RPC message is one line, and the biggest legitimate one is a tool
/// call carrying a file diff: ACP's `Diff` holds the old AND the new text of
/// the whole file, so a 4 MiB source file needs 8 MiB. The longest line
/// measured from a real agent was opencode 1.18.4's list of available
/// commands, 21,366 bytes. A longer line ends the turn: buffering it would let one runaway
/// agent hold unbounded memory in the harness, and cutting it would only
/// corrupt the JSON.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// How long the agent has for the whole handshake: `initialize`, opening the
/// session, and entering read-only mode when that was asked for.
///
/// Measured: opencode 1.18.4 answered `initialize` and `session/new` within a
/// few seconds of a cold start. Starting the MCP servers handed over in
/// `session/new` is part of that answer, so the margin is wide. The case this
/// exists for was also seen: one probe of gemini 0.58.0, on an account it
/// could not serve, sat 120 seconds without answering `initialize` before the
/// probe gave up (a later run answered and refused the session instead).
/// Without a deadline such a turn holds its lane forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the agent may send nothing at all during a turn.
///
/// Reset by every message it sends. A model thinking, or one tool call running
/// a full build or test suite, can be quiet for minutes: ten covers a cold
/// build and test of a crate this size with room left. Longer silence is
/// treated as a wedged agent, reported as a timeout so the work can move to a
/// standby ([`crate::agent::moves_the_work`] reads "timed out").
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often a wait wakes to look at the cancel token.
///
/// The reader thread blocks on the pipe, so the turn itself never does: it
/// waits on a channel with this timeout, and a cancel lands within it even
/// while the agent is silent.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// After `session/cancel`, how long the agent gets to answer `cancelled`
/// before its tree is killed anyway.
///
/// The protocol asks the agent to stop and confirm; an agent that confirms has
/// had the chance to save its session. Two seconds is enough for that and
/// short enough that a person pressing stop sees it stop.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// How this client names itself in `initialize`.
const CLIENT_NAME: &str = "incode";

/// Session mode ids that mean "may read, must not write", as agents spell
/// them. Measured: opencode offers `plan` ("Disallows all edit tools") and
/// gemini documents `--approval-mode plan` as "read-only mode".
const READ_ONLY_MODES: [&str; 3] = ["plan", "read-only", "readonly"];

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Drives an agent over the Agent Client Protocol.
pub struct AcpBackend {
    /// The registry id, e.g. `opencode`.
    id: String,
    binary: String,
    /// What makes the agent speak ACP on stdio, from the registry.
    args: Vec<String>,
    workspace: PathBuf,
    /// The session the next turn continues, when the agent can continue one.
    session_id: Option<String>,
    extra_dirs: Vec<PathBuf>,
    /// The protected paths, checked on every write the agent asks about.
    guards: crate::guards::Guards,
    /// How much the run may do, as the session was started.
    base_mode: PermissionMode,
    /// How much this turn may do: `base_mode`, or read-only for a plan turn.
    mode: PermissionMode,
    /// Every MCP server the harness hands over, as the `mcpServers` JSON the
    /// other backends take. Converted to ACP stdio entries for `session/new`.
    mcp_config: Option<String>,
    /// Who this agent speaks as on the team channel, for the task in hand.
    team_identity: Option<String>,
    /// What the harness knows and the agent does not. ACP has no system prompt
    /// field, so it rides in front of the prompt, as it does for agy.
    system_context: Vec<String>,
    cancel: Option<CancelToken>,
    limits: Limits,
}

impl AcpBackend {
    /// An ACP agent `agent_id`, started as `binary` with `args`.
    #[must_use]
    pub fn new(agent_id: &str, binary: &str, args: &[&str], workspace: PathBuf) -> Self {
        Self {
            id: agent_id.to_string(),
            binary: binary.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            workspace,
            session_id: None,
            extra_dirs: Vec::new(),
            guards: crate::guards::Guards::default(),
            // The session default (`permission_mode_from_env` in the CLI). The
            // caller that knows better says so with `with_permission_mode`.
            base_mode: PermissionMode::WorkspaceWrite,
            mode: PermissionMode::WorkspaceWrite,
            mcp_config: None,
            team_identity: None,
            system_context: Vec::new(),
            cancel: None,
            limits: Limits::DEFAULT,
        }
    }

    /// The session's permission mode, which decides every permission request.
    #[must_use]
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.base_mode = mode;
        self.mode = mode;
        self
    }

    /// The operator's protected paths.
    #[must_use]
    pub fn with_guards(mut self, guards: &crate::guards::Guards) -> Self {
        self.guards = guards.clone();
        self
    }

    /// The MCP servers to hand over, as `{"mcpServers": {...}}` JSON — the
    /// same value [`crate::cli`] builds for the other backends.
    #[must_use]
    pub fn with_mcp_config(mut self, config: String) -> Self {
        self.mcp_config = Some(config);
        self
    }

    /// Carry the harness's context on every turn. Blank blocks are dropped.
    #[must_use]
    pub fn with_system_context(mut self, context: Vec<String>) -> Self {
        self.system_context = context
            .into_iter()
            .filter(|block| !block.trim().is_empty())
            .collect();
        self
    }

    /// The prompt actually sent: the harness's context, then the task.
    ///
    /// Not budgeted like the argv-carried prompts: this one travels over stdin,
    /// where the command-line length limit does not apply.
    fn full_prompt(&self, prompt: &str) -> String {
        if self.system_context.is_empty() {
            prompt.to_string()
        } else {
            format!("{}\n\n{prompt}", self.system_context.join("\n\n"))
        }
    }

    /// The program to start. An agent installed as a script shim is a `.cmd`
    /// file on Windows, which a bare name does not reach.
    fn program(&self) -> PathBuf {
        #[cfg(windows)]
        if let Some(found) =
            crate::verify::resolve_windows_executable(&self.binary, &self.workspace)
        {
            return found;
        }
        PathBuf::from(&self.binary)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.program());
        cmd.args(&self.args).current_dir(&self.workspace);
        // The team server is a child of the agent; see the trait's
        // `set_team_identity` for why the identity travels this way.
        if let Some(role) = &self.team_identity {
            cmd.env(crate::team_mcp::IDENTITY_ENV, role);
        }
        cmd
    }

    /// Everything the turn needs, settled before the agent starts.
    fn setup(&self) -> Result<TurnSetup, AgentError> {
        let cwd = std::path::absolute(&self.workspace).map_err(|e| {
            AgentError::new(format!(
                "the workspace path could not be made absolute: {e}"
            ))
        })?;
        // A root that cannot be made absolute is left out — which makes writes
        // there refused rather than unchecked.
        let extra_dirs: Vec<PathBuf> = self
            .extra_dirs
            .iter()
            .filter_map(|dir| std::path::absolute(dir).ok())
            .collect();
        let mut roots = vec![cwd.clone()];
        roots.extend(extra_dirs.iter().cloned());
        Ok(TurnSetup {
            cwd,
            extra_dirs,
            mcp_servers: mcp_servers(self.mcp_config.as_deref(), self.team_identity.as_deref()),
            resume: self.session_id.clone(),
            policy: Policy {
                mode: self.mode,
                guards: self.guards.clone(),
                roots,
            },
        })
    }
}

impl AgentBackend for AcpBackend {
    fn id(&self) -> &str {
        &self.id
    }

    /// **Neither half.** Measured: opencode 1.18.4 in its ordinary mode wrote a
    /// file without sending `session/request_permission`, so answering those
    /// requests prevents only what the agent chooses to ask about. The
    /// protected paths fall to the sentinel, and delegation is not something
    /// ACP lets a client refuse.
    fn enforces(&self) -> crate::guards::Enforcement {
        crate::guards::Enforcement::NOTHING
    }

    /// A JSON-RPC error on `session/prompt` is the agent's own machinery
    /// failing — a stop reason is what it reports about the work.
    fn agent_failed(&self, outcome: &TurnOutcome) -> bool {
        outcome.stop_reason == RPC_ERROR || crate::agent::moves_the_work(&outcome.failure())
    }

    fn system_context(&self) -> &[String] {
        &self.system_context
    }

    fn mcp_config(&self) -> Option<String> {
        self.mcp_config.clone()
    }

    fn set_team_identity(&mut self, role: &str) {
        self.team_identity = Some(role.to_string());
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn set_session_id(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    fn clear_session(&mut self) {
        self.session_id = None;
    }

    fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.extra_dirs = dirs;
    }

    /// `plan` means read-only for the turn; `acceptEdits` returns to what the
    /// session was started with, and never less than workspace-write. Anything
    /// else parses as a mode, and a word nobody recognises lands on read-only:
    /// this backend is the one ANSWERING the agent's requests, so it fails
    /// closed rather than letting a typo grant writes.
    fn set_permission_mode(&mut self, mode: &str) {
        self.mode = match mode {
            "plan" | "read-only" => PermissionMode::ReadOnly,
            "acceptEdits" | "accept-edits" | "workspace-write" => {
                if self.base_mode.grants(PermissionMode::WorkspaceWrite) {
                    self.base_mode
                } else {
                    PermissionMode::WorkspaceWrite
                }
            }
            "default" => PermissionMode::Prompt,
            other => PermissionMode::from_str_lossy(other),
        };
    }

    fn set_cancel(&mut self, token: CancelToken) {
        self.cancel = Some(token);
    }

    fn run_turn(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let setup = self.setup()?;
        let sent = self.full_prompt(prompt);

        let mut command = self.command();
        crate::agent::process_tree::prepare(&mut command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::new(format!("could not start `{}`: {e}", self.binary)))?;
        let tree = crate::agent::process_tree::Tree::adopt(&child);
        let stderr = crate::agent::stderr_tail::StderrTail::capture(child.stderr.take());

        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            tree.kill(&mut child);
            let _ = child.wait();
            return Err(AgentError::new("the agent produced no output stream"));
        };
        let incoming = spawn_reader(stdout, self.limits.max_line);
        let mut conn = Connection::new(stdin, incoming, self.limits, self.cancel.clone());

        let result = converse(&mut conn, &setup, &sent, on_event);

        // The process was started for this turn and ends with it, whatever
        // happened — including everything it started, MCP servers and all.
        drop(conn);
        tree.kill(&mut child);
        let _ = child.wait();

        match result {
            Ok(finished) => {
                self.session_id = Some(finished.session_id);
                Ok(finished.outcome)
            }
            Err(e) if e.to_string() == CANCELLED => Err(e),
            Err(e) => {
                let said = stderr.finish();
                Err(if said.is_empty() {
                    e
                } else {
                    AgentError::new(format!("{e}: {said}"))
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// One turn
// ---------------------------------------------------------------------------

/// The stop reason recorded when `session/prompt` itself failed.
const RPC_ERROR: &str = "error";

/// Everything one turn needs to know, gathered before the agent starts.
struct TurnSetup {
    cwd: PathBuf,
    extra_dirs: Vec<PathBuf>,
    mcp_servers: Vec<acp::McpServer>,
    resume: Option<String>,
    policy: Policy,
}

/// A turn that reached its end.
struct Finished {
    session_id: String,
    outcome: TurnOutcome,
}

/// Handshake, then one prompt.
fn converse<W: Write>(
    conn: &mut Connection<W>,
    setup: &TurnSetup,
    prompt: &str,
    on_event: &mut dyn FnMut(&AgentEvent),
) -> Result<Finished, AgentError> {
    let opened = handshake(conn, setup)?;
    on_event(&AgentEvent::SessionStarted {
        session_id: opened.id.to_string(),
        model: opened.model.clone().unwrap_or_default(),
    });
    let outcome = prompt_turn(conn, &setup.policy, &opened.id, prompt, on_event)?;
    Ok(Finished {
        session_id: opened.id.to_string(),
        outcome,
    })
}

/// A session, open and ready for a prompt.
struct Opened {
    id: acp::SessionId,
    /// The model the session reports it will use, when it says.
    model: Option<String>,
}

fn handshake<W: Write>(conn: &mut Connection<W>, setup: &TurnSetup) -> Result<Opened, AgentError> {
    let deadline = Instant::now() + conn.limits.handshake;
    // Default capabilities: no fs, no terminal. See the module note.
    let init = acp::InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(acp::ClientCapabilities::new())
        .client_info(acp::Implementation::new(
            CLIENT_NAME,
            env!("CARGO_PKG_VERSION"),
        ));
    let reply = conn.call(
        acp::AGENT_METHOD_NAMES.initialize,
        init,
        deadline,
        &setup.policy,
    )?;
    let agent: acp::InitializeResponse = decode(reply, acp::AGENT_METHOD_NAMES.initialize)?;
    if agent.protocol_version != ProtocolVersion::V1 {
        return Err(AgentError::new(format!(
            "the agent speaks ACP version {}, and this client speaks only version {}",
            agent.protocol_version,
            ProtocolVersion::V1
        )));
    }
    let caps = agent.agent_capabilities;
    let session = open_session(conn, setup, &caps, deadline)?;
    if !setup.policy.mode.grants(PermissionMode::WorkspaceWrite) {
        enter_read_only(conn, setup, &session, deadline)?;
    }
    Ok(Opened {
        model: current_model(&session.config),
        id: session.id,
    })
}

/// What opening a session reported.
struct Session {
    id: acp::SessionId,
    config: Vec<acp::SessionConfigOption>,
    modes: Option<acp::SessionModeState>,
}

/// Continue the previous session when the agent can, otherwise start one.
fn open_session<W: Write>(
    conn: &mut Connection<W>,
    setup: &TurnSetup,
    caps: &acp::AgentCapabilities,
    deadline: Instant,
) -> Result<Session, AgentError> {
    let names = &acp::AGENT_METHOD_NAMES;
    let extra = if caps.session_capabilities.additional_directories.is_some() {
        setup.extra_dirs.clone()
    } else {
        if !setup.extra_dirs.is_empty() {
            crate::trace!(
                "agent",
                "acp: the agent takes no extra roots; {} directory(ies) not passed",
                setup.extra_dirs.len()
            );
        }
        Vec::new()
    };

    if let Some(previous) = &setup.resume {
        let id = acp::SessionId::new(previous.as_str());
        if caps.session_capabilities.resume.is_some() {
            let request = acp::ResumeSessionRequest::new(id.clone(), setup.cwd.clone())
                .additional_directories(extra.clone())
                .mcp_servers(setup.mcp_servers.clone());
            match conn.call(names.session_resume, request, deadline, &setup.policy)? {
                Ok(value) => {
                    let resumed: acp::ResumeSessionResponse =
                        decode(Ok(value), names.session_resume)?;
                    return Ok(Session {
                        id,
                        config: resumed.config_options.unwrap_or_default(),
                        modes: resumed.modes,
                    });
                }
                Err(error) => crate::trace!(
                    "agent",
                    "acp: could not resume {previous} ({}); starting a new session",
                    describe(&error)
                ),
            }
        } else if caps.load_session {
            // `session/load` replays the history as `session/update`s before
            // it answers. Nothing is emitted before the prompt, so the replay
            // is read and dropped rather than shown as this turn's output.
            let request = acp::LoadSessionRequest::new(id.clone(), setup.cwd.clone())
                .additional_directories(extra.clone())
                .mcp_servers(setup.mcp_servers.clone());
            match conn.call(names.session_load, request, deadline, &setup.policy)? {
                Ok(value) => {
                    let loaded: acp::LoadSessionResponse = decode(Ok(value), names.session_load)?;
                    return Ok(Session {
                        id,
                        config: loaded.config_options.unwrap_or_default(),
                        modes: loaded.modes,
                    });
                }
                Err(error) => crate::trace!(
                    "agent",
                    "acp: could not load {previous} ({}); starting a new session",
                    describe(&error)
                ),
            }
        } else {
            crate::trace!(
                "agent",
                "acp: the agent cannot continue a session; starting a new one"
            );
        }
    }

    let request = acp::NewSessionRequest::new(setup.cwd.clone())
        .additional_directories(extra)
        .mcp_servers(setup.mcp_servers.clone());
    let reply = conn.call(names.session_new, request, deadline, &setup.policy)?;
    let opened: acp::NewSessionResponse = decode(reply, names.session_new)?;
    Ok(Session {
        id: opened.session_id,
        config: opened.config_options.unwrap_or_default(),
        modes: opened.modes,
    })
}

/// How to put a session into its read-only mode.
#[derive(Debug, PartialEq)]
enum ReadOnlySwitch {
    /// It already is.
    Already,
    /// Through a mode-category config option.
    Option {
        config: acp::SessionConfigId,
        value: acp::SessionConfigValueId,
    },
    /// Through the session modes.
    Mode(acp::SessionModeId),
}

fn is_read_only_mode(id: &str) -> bool {
    READ_ONLY_MODES.iter().any(|m| id.eq_ignore_ascii_case(m))
}

/// The read-only mode this session offers, if it offers one.
fn read_only_switch(
    config: &[acp::SessionConfigOption],
    modes: Option<&acp::SessionModeState>,
) -> Option<ReadOnlySwitch> {
    for option in config {
        if option.category != Some(acp::SessionConfigOptionCategory::Mode) {
            continue;
        }
        let acp::SessionConfigKind::Select(select) = &option.kind else {
            continue;
        };
        let values: Vec<&acp::SessionConfigSelectOption> = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(list) => list.iter().collect(),
            acp::SessionConfigSelectOptions::Grouped(groups) => {
                groups.iter().flat_map(|g| g.options.iter()).collect()
            }
            _ => Vec::new(),
        };
        if let Some(found) = values.into_iter().find(|v| is_read_only_mode(&v.value.0)) {
            if select.current_value == found.value {
                return Some(ReadOnlySwitch::Already);
            }
            return Some(ReadOnlySwitch::Option {
                config: option.id.clone(),
                value: found.value.clone(),
            });
        }
    }
    let modes = modes?;
    let found = modes
        .available_modes
        .iter()
        .find(|m| is_read_only_mode(&m.id.0))?;
    if modes.current_mode_id == found.id {
        Some(ReadOnlySwitch::Already)
    } else {
        Some(ReadOnlySwitch::Mode(found.id.clone()))
    }
}

/// Put the session in the agent's own read-only mode, or refuse the turn.
///
/// Refusing is deliberate: an agent in its ordinary mode can write without
/// asking (measured on opencode 1.18.4), so answering its permission requests
/// alone would not keep a read-only run read-only.
fn enter_read_only<W: Write>(
    conn: &mut Connection<W>,
    setup: &TurnSetup,
    session: &Session,
    deadline: Instant,
) -> Result<(), AgentError> {
    let names = &acp::AGENT_METHOD_NAMES;
    match read_only_switch(&session.config, session.modes.as_ref()) {
        Some(ReadOnlySwitch::Already) => Ok(()),
        Some(ReadOnlySwitch::Option { config, value }) => {
            let request = acp::SetSessionConfigOptionRequest::new(
                session.id.clone(),
                config.clone(),
                value.clone(),
            );
            let reply = conn.call(
                names.session_set_config_option,
                request,
                deadline,
                &setup.policy,
            )?;
            let set: acp::SetSessionConfigOptionResponse =
                decode(reply, names.session_set_config_option)?;
            // Believed only if the agent's own account agrees, when it gives one.
            let refused = set.config_options.iter().any(|o| {
                o.id == config
                    && matches!(&o.kind, acp::SessionConfigKind::Select(s) if s.current_value != value)
            });
            if refused {
                return Err(AgentError::new(format!(
                    "the agent did not enter its read-only mode `{value}`; refusing the turn"
                )));
            }
            Ok(())
        }
        Some(ReadOnlySwitch::Mode(mode)) => {
            let request = acp::SetSessionModeRequest::new(session.id.clone(), mode);
            let reply = conn.call(names.session_set_mode, request, deadline, &setup.policy)?;
            reply.map(|_| ()).map_err(|e| {
                AgentError::new(format!(
                    "the agent refused its read-only mode: {}",
                    describe(&e)
                ))
            })
        }
        None => Err(AgentError::new(
            "this run is read-only and the agent offers no read-only mode to put the session \
             in; refusing the turn, because an agent in its ordinary mode can write without \
             asking",
        )),
    }
}

/// The model a session reports it will use, from a model-category option.
fn current_model(config: &[acp::SessionConfigOption]) -> Option<String> {
    config.iter().find_map(|o| {
        if o.category != Some(acp::SessionConfigOptionCategory::Model) {
            return None;
        }
        match &o.kind {
            acp::SessionConfigKind::Select(s) => Some(s.current_value.to_string()),
            _ => None,
        }
    })
}

/// Send the prompt and stream the turn until the agent answers it.
fn prompt_turn<W: Write>(
    conn: &mut Connection<W>,
    policy: &Policy,
    session: &acp::SessionId,
    prompt: &str,
    on_event: &mut dyn FnMut(&AgentEvent),
) -> Result<TurnOutcome, AgentError> {
    let request = acp::PromptRequest::new(
        session.clone(),
        vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
    );
    let prompt_id = conn.request(acp::AGENT_METHOD_NAMES.session_prompt, request)?;
    let mut stream = EventStream::default();
    let idle = conn.limits.idle;
    let silent = format!("the agent sent nothing for {idle:?} (idle deadline)");
    let mut deadline = Instant::now() + idle;
    loop {
        let message = match conn.next(deadline, &silent, true)? {
            Next::Cancelled => {
                stream.flush(on_event);
                return Err(cancel_turn(conn, policy, session, &prompt_id));
            }
            Next::Message(message) => message,
        };
        deadline = Instant::now() + idle;
        match message {
            Message::Response { id, outcome } if id == prompt_id => {
                stream.flush(on_event);
                return outcome_of(outcome, session);
            }
            Message::Response { id, .. } => {
                crate::trace!(
                    "agent",
                    "acp: a reply to request {id} nobody is waiting for"
                );
            }
            Message::Notification(note) => {
                if note.method.as_ref() != acp::CLIENT_METHOD_NAMES.session_update {
                    continue;
                }
                let Some(params) = note.params else { continue };
                match serde_json::from_value::<acp::SessionNotification>(params) {
                    Ok(update) if update.session_id == *session => {
                        stream.apply(update.update, on_event);
                    }
                    Ok(_) => {}
                    Err(e) => crate::trace!("agent", "acp: unreadable session/update: {e}"),
                }
            }
            Message::Request(request) => answer(conn, policy, request, false)?,
        }
    }
}

/// Ask the agent to stop, give it [`CANCEL_GRACE`] to confirm, and report the
/// turn as cancelled. The caller kills the tree either way.
fn cancel_turn<W: Write>(
    conn: &mut Connection<W>,
    policy: &Policy,
    session: &acp::SessionId,
    prompt_id: &acp::RequestId,
) -> AgentError {
    let cancel = acp::CancelNotification::new(session.clone());
    if let Err(e) = conn.notify(acp::AGENT_METHOD_NAMES.session_cancel, cancel) {
        crate::trace!("agent", "acp: session/cancel not delivered: {e}");
        return AgentError::new(CANCELLED);
    }
    let until = Instant::now() + CANCEL_GRACE;
    // Pending permission requests are answered `cancelled`, as the protocol
    // requires, until the agent confirms or the grace runs out.
    while let Ok(Next::Message(message)) = conn.next(until, "the cancel grace", false) {
        match message {
            Message::Response { id, .. } if id == *prompt_id => break,
            Message::Request(request) => {
                if answer(conn, policy, request, true).is_err() {
                    break;
                }
            }
            _ => {}
        }
    }
    AgentError::new(CANCELLED)
}

/// The turn's outcome, from the reply to `session/prompt`.
fn outcome_of(
    reply: Result<Value, acp::Error>,
    session: &acp::SessionId,
) -> Result<TurnOutcome, AgentError> {
    let failed = |reason: &str, detail: String| TurnOutcome {
        session_id: session.to_string(),
        stop_reason: reason.to_string(),
        is_error: true,
        detail: Some(detail),
        served_by: Vec::new(),
    };
    let value = match reply {
        Ok(value) => value,
        Err(error) => return Ok(failed(RPC_ERROR, describe(&error))),
    };
    let response: acp::PromptResponse = serde_json::from_value(value).map_err(|e| {
        AgentError::new(format!(
            "the agent's answer to `session/prompt` could not be read: {e}"
        ))
    })?;
    let (reason, detail) = match response.stop_reason {
        acp::StopReason::EndTurn => {
            return Ok(TurnOutcome {
                session_id: session.to_string(),
                stop_reason: "end_turn".to_string(),
                is_error: false,
                detail: None,
                // Not reported: ACP carries no billing record.
                served_by: Vec::new(),
            });
        }
        acp::StopReason::MaxTokens => ("max_tokens", "the agent reached its token limit"),
        acp::StopReason::MaxTurnRequests => (
            "max_turn_requests",
            "the agent reached its limit on requests in one turn",
        ),
        acp::StopReason::Refusal => ("refusal", "the agent refused to continue"),
        acp::StopReason::Cancelled => ("cancelled", "the agent cancelled the turn itself"),
        _ => (
            "unknown",
            "the agent stopped for a reason this client does not know",
        ),
    };
    Ok(failed(reason, detail.to_string()))
}

/// An agent's JSON-RPC error, as something a person can act on — cleaned of
/// anything shaped like a credential and bounded, like stderr.
fn describe(error: &acp::Error) -> String {
    let said = if error.code == acp::ErrorCode::AuthRequired {
        format!("could not authenticate: {error}")
    } else {
        error.to_string()
    };
    crate::agent::stderr_tail::clean(&said)
}

/// A successful reply, read as `T`; an error reply, as the agent's refusal.
fn decode<T: DeserializeOwned>(
    reply: Result<Value, acp::Error>,
    method: &str,
) -> Result<T, AgentError> {
    let value = reply
        .map_err(|e| AgentError::new(format!("the agent refused `{method}`: {}", describe(&e))))?;
    // `null` and `{}` both say "nothing to add" for a reply whose every field
    // is optional; the schema's structs read only the second.
    let value = if value.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        value
    };
    serde_json::from_value(value).map_err(|e| {
        AgentError::new(format!(
            "the agent's answer to `{method}` could not be read: {e}"
        ))
    })
}

// ---------------------------------------------------------------------------
// Requests from the agent
// ---------------------------------------------------------------------------

/// Answer one request the agent made. Every request gets an answer.
fn answer<W: Write>(
    conn: &mut Connection<W>,
    policy: &Policy,
    request: acp::Request<Value>,
    cancelled: bool,
) -> Result<(), AgentError> {
    if request.method.as_ref() == acp::CLIENT_METHOD_NAMES.session_request_permission {
        let asked = request
            .params
            .map(serde_json::from_value::<acp::RequestPermissionRequest>);
        return match asked {
            Some(Ok(asked)) => {
                let outcome = if cancelled {
                    acp::RequestPermissionOutcome::Cancelled
                } else {
                    decide_permission(&asked, policy)
                };
                conn.respond(request.id, Ok(acp::RequestPermissionResponse::new(outcome)))
            }
            _ => conn.respond::<()>(request.id, Err(acp::Error::invalid_params())),
        };
    }
    crate::trace!(
        "agent",
        "acp: the agent asked for `{}`, which this client does not provide",
        request.method
    );
    conn.respond::<()>(request.id, Err(acp::Error::method_not_found()))
}

/// What the run allows, and where.
struct Policy {
    mode: PermissionMode,
    guards: crate::guards::Guards,
    /// The workspace first, then every extra root. Absolute.
    roots: Vec<PathBuf>,
}

impl Policy {
    /// Why a write to `target` is refused, or `None` when it may go ahead.
    fn refuses(&self, target: &Path) -> Option<String> {
        let workspace = self.roots.first()?;
        let absolute = if target.is_absolute() {
            target.to_path_buf()
        } else {
            workspace.join(target)
        };
        let wanted = comparable(&absolute);
        for root in &self.roots {
            let base = comparable(root);
            let inside = if wanted == base {
                Some("")
            } else {
                wanted
                    .strip_prefix(&base)
                    .and_then(|rest| rest.strip_prefix('/'))
            };
            if let Some(relative) = inside {
                return self.guards.refuse_write(relative);
            }
        }
        Some(format!(
            "`{}` is outside the workspace and every directory this run was given",
            target.display()
        ))
    }
}

/// A path as one comparable string: `.` and `..` resolved without touching
/// the disk, forward slashes, and on Windows no verbatim prefix and no case.
fn comparable(path: &Path) -> String {
    let mut prefix = String::new();
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => prefix = p.as_os_str().to_string_lossy().into_owned(),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
        }
    }
    let prefix = prefix.trim_start_matches(r"\\?\").replace('\\', "/");
    let joined = format!("{prefix}/{}", parts.join("/"));
    if cfg!(windows) {
        joined.to_lowercase()
    } else {
        joined
    }
}

/// How far a tool reaches, which decides what an approval may cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Observe,
    Change,
    Run,
}

/// Whether the tool in a permission request may run, and why not.
fn judge(tool: &acp::ToolCallUpdate, policy: &Policy) -> Result<Reach, String> {
    let Some(kind) = tool.fields.kind else {
        return Err("the request does not say what kind of tool it is".to_string());
    };
    // The same rungs as `PermissionPolicy::with_defaults`: reading needs
    // read-only, writing needs workspace-write, running a command or reaching
    // the network needs full access.
    let (reach, needs) = match kind {
        acp::ToolKind::Read | acp::ToolKind::Search | acp::ToolKind::Think => {
            (Reach::Observe, PermissionMode::ReadOnly)
        }
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => {
            (Reach::Change, PermissionMode::WorkspaceWrite)
        }
        acp::ToolKind::Execute | acp::ToolKind::Fetch => {
            (Reach::Run, PermissionMode::DangerFullAccess)
        }
        other => {
            return Err(format!(
                "a `{other:?}` tool cannot be mapped to a permission with confidence"
            ))
        }
    };
    if !policy.mode.grants(needs) {
        return Err(format!(
            "a `{kind:?}` tool needs {} and this run is {}",
            needs.as_str(),
            policy.mode.as_str()
        ));
    }
    match reach {
        Reach::Change => {
            let targets = targets(tool);
            if targets.is_empty() {
                return Err(
                    "a write that names no path cannot be checked against the protected paths"
                        .to_string(),
                );
            }
            for target in targets {
                if let Some(why) = policy.refuses(&target) {
                    return Err(why);
                }
            }
        }
        Reach::Run => {
            let command = tool
                .fields
                .raw_input
                .as_ref()
                .and_then(|input| input.get("command"))
                .and_then(Value::as_str);
            if let Some(why) = command.and_then(|c| policy.guards.refuse_command(c)) {
                return Err(why);
            }
        }
        Reach::Observe => {}
    }
    Ok(reach)
}

/// Every path a write request names: its locations, and the path fields of its
/// raw input. More paths only makes the check stricter.
fn targets(tool: &acp::ToolCallUpdate) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = tool
        .fields
        .locations
        .iter()
        .flatten()
        .map(|location| location.path.clone())
        .collect();
    if let Some(input) = &tool.fields.raw_input {
        // `filePath` measured on opencode's write tool; the others are the
        // spellings the harness's own tools use.
        for key in ["filePath", "file_path", "path"] {
            if let Some(path) = input.get(key).and_then(Value::as_str) {
                out.push(PathBuf::from(path));
            }
        }
    }
    out
}

/// The option to select for one permission request.
fn decide_permission(
    asked: &acp::RequestPermissionRequest,
    policy: &Policy,
) -> acp::RequestPermissionOutcome {
    use acp::PermissionOptionKind as Kind;
    let pick = |kinds: &[Kind]| {
        kinds.iter().find_map(|kind| {
            asked
                .options
                .iter()
                .find(|option| option.kind == *kind)
                .map(|option| option.option_id.clone())
        })
    };
    let refusal = match judge(&asked.tool_call, policy) {
        Ok(reach) => {
            // A standing approval of a write would cover writes nobody sees,
            // so only a look may be approved for good.
            let allowed: &[Kind] = if reach == Reach::Observe {
                &[Kind::AllowOnce, Kind::AllowAlways]
            } else {
                &[Kind::AllowOnce]
            };
            if let Some(option) = pick(allowed) {
                return acp::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new(option),
                );
            }
            "only a standing approval was offered".to_string()
        }
        Err(why) => why,
    };
    crate::trace!(
        "guards",
        "acp: refused `{}`: {refusal}",
        asked.tool_call.fields.title.as_deref().unwrap_or("a tool")
    );
    match pick(&[Kind::RejectOnce, Kind::RejectAlways]) {
        Some(option) => {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option))
        }
        None => acp::RequestPermissionOutcome::Cancelled,
    }
}

// ---------------------------------------------------------------------------
// Streamed updates, in the shape every backend produces
// ---------------------------------------------------------------------------

/// Turns `session/update`s into [`AgentEvent`]s.
#[derive(Default)]
struct EventStream {
    /// The agent message being streamed, emitted whole as `Text` when it ends.
    message: String,
    message_id: Option<acp::MessageId>,
    tools: HashMap<acp::ToolCallId, ToolState>,
    plans: u32,
}

struct ToolState {
    call: acp::ToolCall,
    announced: bool,
    finished: bool,
}

impl EventStream {
    fn apply(&mut self, update: acp::SessionUpdate, on_event: &mut dyn FnMut(&AgentEvent)) {
        match update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                if chunk.message_id.is_some() && chunk.message_id != self.message_id {
                    self.flush(on_event);
                    self.message_id.clone_from(&chunk.message_id);
                }
                if let acp::ContentBlock::Text(text) = chunk.content {
                    on_event(&AgentEvent::TextDelta(text.text.clone()));
                    self.message.push_str(&text.text);
                }
            }
            acp::SessionUpdate::ToolCall(call) => {
                self.flush(on_event);
                let id = call.tool_call_id.clone();
                self.tools.insert(
                    id.clone(),
                    ToolState {
                        call,
                        announced: false,
                        finished: false,
                    },
                );
                self.progress(&id, on_event);
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                self.flush(on_event);
                let id = update.tool_call_id.clone();
                if let Some(state) = self.tools.get_mut(&id) {
                    state.call.update(update.fields);
                } else {
                    match acp::ToolCall::try_from(update) {
                        Ok(call) => {
                            self.tools.insert(
                                id.clone(),
                                ToolState {
                                    call,
                                    announced: false,
                                    finished: false,
                                },
                            );
                        }
                        Err(e) => {
                            crate::trace!("agent", "acp: update for unknown tool call {id}: {e}");
                            return;
                        }
                    }
                }
                self.progress(&id, on_event);
            }
            acp::SessionUpdate::Plan(plan) => {
                self.flush(on_event);
                self.plans += 1;
                // Shown the way a to-do tool call is on the other backends.
                on_event(&AgentEvent::ToolUse {
                    id: format!("plan-{}", self.plans),
                    name: "plan".to_string(),
                    input: render_plan(&plan),
                });
            }
            // Thoughts, command lists, mode and usage changes: not part of the
            // shape the other backends produce.
            _ => {}
        }
    }

    /// Announce a tool call once it runs, and its result once it ends.
    fn progress(&mut self, id: &acp::ToolCallId, on_event: &mut dyn FnMut(&AgentEvent)) {
        let Some(state) = self.tools.get_mut(id) else {
            return;
        };
        let status = state.call.status;
        let ended = matches!(
            status,
            acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
        );
        if (ended || status == acp::ToolCallStatus::InProgress) && !state.announced {
            state.announced = true;
            on_event(&AgentEvent::ToolUse {
                id: id.to_string(),
                name: state
                    .call
                    .name
                    .clone()
                    .unwrap_or_else(|| state.call.title.clone()),
                input: match &state.call.raw_input {
                    Some(input) if input.as_object().is_none_or(|o| !o.is_empty()) => {
                        input.to_string()
                    }
                    _ => state.call.title.clone(),
                },
            });
        }
        if ended && !state.finished {
            state.finished = true;
            on_event(&AgentEvent::ToolResult {
                id: id.to_string(),
                output: tool_output(&state.call),
                is_error: status == acp::ToolCallStatus::Failed,
            });
        }
    }

    /// Emit the message streamed so far as one whole block.
    fn flush(&mut self, on_event: &mut dyn FnMut(&AgentEvent)) {
        let text = std::mem::take(&mut self.message);
        if !text.trim().is_empty() {
            on_event(&AgentEvent::Text(text));
        }
    }
}

/// What a finished tool call returned, as text.
fn tool_output(call: &acp::ToolCall) -> String {
    let mut parts: Vec<String> = Vec::new();
    for content in &call.content {
        match content {
            acp::ToolCallContent::Content(block) => {
                if let acp::ContentBlock::Text(text) = &block.content {
                    parts.push(text.text.clone());
                }
            }
            acp::ToolCallContent::Diff(diff) => {
                parts.push(format!("changed {}", diff.path.display()));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        if let Some(raw) = &call.raw_output {
            return raw.to_string();
        }
    }
    parts.join("\n")
}

/// A plan as one line per entry.
fn render_plan(plan: &acp::Plan) -> String {
    plan.entries
        .iter()
        .map(|entry| {
            let status = match entry.status {
                acp::PlanEntryStatus::Pending => "pending",
                acp::PlanEntryStatus::InProgress => "in progress",
                acp::PlanEntryStatus::Completed => "done",
                _ => "unknown",
            };
            format!("[{status}] {}", entry.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The MCP servers the harness hands over, as ACP stdio entries.
///
/// Stdio is the one transport every ACP agent must support, and it is exactly
/// what the other backends are handed: a command and its arguments. The team
/// server also gets the identity in its own environment, in case the agent
/// does not pass its environment on.
fn mcp_servers(config: Option<&str>, team_identity: Option<&str>) -> Vec<acp::McpServer> {
    let Some(config) = config else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(config) else {
        crate::trace!(
            "agent",
            "acp: the MCP configuration is not JSON; none passed"
        );
        return Vec::new();
    };
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, entry)| {
            let command = entry.get("command")?.as_str()?;
            let args: Vec<String> = entry
                .get("args")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|a| a.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut env: Vec<acp::EnvVariable> = entry
                .get("env")
                .and_then(Value::as_object)
                .map(|vars| {
                    vars.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| acp::EnvVariable::new(k, v)))
                        .collect()
                })
                .unwrap_or_default();
            if name == crate::team_mcp::CLIENT_KEY {
                if let Some(role) = team_identity {
                    env.push(acp::EnvVariable::new(crate::team_mcp::IDENTITY_ENV, role));
                }
            }
            Some(acp::McpServer::Stdio(
                acp::McpServerStdio::new(name.clone(), command)
                    .args(args)
                    .env(env),
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Transport: JSON-RPC 2.0, one message per line
// ---------------------------------------------------------------------------

/// The bounds one connection runs under.
#[derive(Debug, Clone, Copy)]
struct Limits {
    handshake: Duration,
    idle: Duration,
    max_line: usize,
}

impl Limits {
    const DEFAULT: Self = Self {
        handshake: HANDSHAKE_TIMEOUT,
        idle: IDLE_TIMEOUT,
        max_line: MAX_LINE_BYTES,
    };
}

/// What the reader thread hands over.
enum Incoming {
    Line(String),
    /// A line passed the cap; the reader stopped.
    TooLong,
    /// The pipe failed; the reader stopped.
    Failed(String),
}

/// Read `stream` line by line on a thread of its own. The channel closes when
/// the stream ends.
fn spawn_reader<R: Read + Send + 'static>(stream: R, max_line: usize) -> Receiver<Incoming> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let sent = match read_bounded_line(&mut reader, max_line) {
                Ok(Some(line)) => tx.send(Incoming::Line(line)),
                Ok(None) => return,
                Err(Bounded::TooLong) => {
                    let _ = tx.send(Incoming::TooLong);
                    return;
                }
                Err(Bounded::Io(e)) => {
                    let _ = tx.send(Incoming::Failed(e.to_string()));
                    return;
                }
            };
            if sent.is_err() {
                return;
            }
        }
    });
    rx
}

enum Bounded {
    TooLong,
    Io(std::io::Error),
}

/// One line, without its terminator, or `None` at the end of the stream.
///
/// Never holds more than `max` bytes of one line: the cap is checked before
/// each chunk is kept, not after the line is whole.
fn read_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<String>, Bounded> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Bounded::Io(e)),
        };
        if available.is_empty() {
            return Ok((!line.is_empty()).then(|| text_of(line)));
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at, true),
            None => (available.len(), false),
        };
        if line.len() + take > max {
            return Err(Bounded::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(if done { take + 1 } else { take });
        if done {
            return Ok(Some(text_of(line)));
        }
    }
}

fn text_of(mut bytes: Vec<u8>) -> String {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One message from the agent.
enum Message {
    /// A reply to one of this client's requests.
    Response {
        id: acp::RequestId,
        outcome: Result<Value, acp::Error>,
    },
    /// Something the agent asks of this client.
    Request(acp::Request<Value>),
    Notification(acp::Notification<Value>),
}

/// Classify one line. Presence of `method` and `id` decides the envelope; the
/// envelope itself is the schema's, `jsonrpc: "2.0"` included.
fn parse_message(line: &str) -> Result<Message, String> {
    let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let has = |key: &str| value.get(key).is_some();
    let message = match (has("method"), has("id")) {
        (true, true) => Message::Request(
            serde_json::from_value::<acp::JsonRpcMessage<acp::Request<Value>>>(value)
                .map_err(|e| e.to_string())?
                .into_inner(),
        ),
        (true, false) => Message::Notification(
            serde_json::from_value::<acp::JsonRpcMessage<acp::Notification<Value>>>(value)
                .map_err(|e| e.to_string())?
                .into_inner(),
        ),
        (false, _) => {
            match serde_json::from_value::<acp::JsonRpcMessage<acp::Response<Value>>>(value)
                .map_err(|e| e.to_string())?
                .into_inner()
            {
                acp::Response::Result { id, result } => Message::Response {
                    id,
                    outcome: Ok(result),
                },
                acp::Response::Error { id, error } => Message::Response {
                    id,
                    outcome: Err(error),
                },
            }
        }
    };
    Ok(message)
}

enum Next {
    Message(Message),
    Cancelled,
}

/// One JSON-RPC connection to the agent: this client's writer, and the lines
/// the reader thread hands over.
struct Connection<W: Write> {
    writer: W,
    incoming: Receiver<Incoming>,
    next_id: i64,
    limits: Limits,
    cancel: Option<CancelToken>,
}

impl<W: Write> Connection<W> {
    fn new(
        writer: W,
        incoming: Receiver<Incoming>,
        limits: Limits,
        cancel: Option<CancelToken>,
    ) -> Self {
        Self {
            writer,
            incoming,
            next_id: 0,
            limits,
            cancel,
        }
    }

    fn send<T: Serialize>(&mut self, message: &T) -> Result<(), AgentError> {
        let mut line = serde_json::to_string(message)
            .map_err(|e| AgentError::new(format!("could not encode a message: {e}")))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|e| AgentError::new(format!("the connection to the agent broke: {e}")))
    }

    fn request<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<acp::RequestId, AgentError> {
        self.next_id += 1;
        let id = acp::RequestId::Number(self.next_id);
        self.send(&acp::JsonRpcMessage::wrap(acp::Request {
            id: id.clone(),
            method: method.into(),
            params: Some(params),
        }))?;
        Ok(id)
    }

    fn notify<P: Serialize>(&mut self, method: &str, params: P) -> Result<(), AgentError> {
        self.send(&acp::JsonRpcMessage::wrap(acp::Notification {
            method: method.into(),
            params: Some(params),
        }))
    }

    fn respond<R: Serialize>(
        &mut self,
        id: acp::RequestId,
        result: Result<R, acp::Error>,
    ) -> Result<(), AgentError> {
        self.send(&acp::JsonRpcMessage::wrap(acp::Response::new(id, result)))
    }

    /// The next message, or why there is none by `deadline`.
    ///
    /// Waits on the reader's channel, never on the pipe, so it wakes every
    /// [`CANCEL_POLL`] to look at the cancel token when `watch_cancel` is set.
    fn next(
        &mut self,
        deadline: Instant,
        waiting_for: &str,
        watch_cancel: bool,
    ) -> Result<Next, AgentError> {
        loop {
            if watch_cancel && self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                return Ok(Next::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(AgentError::new(format!("timed out: {waiting_for}")));
            }
            match self
                .incoming
                .recv_timeout((deadline - now).min(CANCEL_POLL))
            {
                Ok(Incoming::Line(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match parse_message(&line) {
                        Ok(message) => return Ok(Next::Message(message)),
                        Err(why) => {
                            crate::trace!(
                                "agent",
                                "acp: skipped a line that is not JSON-RPC: {why}"
                            );
                        }
                    }
                }
                Ok(Incoming::TooLong) => {
                    return Err(AgentError::new(format!(
                        "the agent sent a message longer than {} bytes; the turn was ended \
                         rather than holding it",
                        self.limits.max_line
                    )))
                }
                Ok(Incoming::Failed(why)) => {
                    return Err(AgentError::new(format!(
                        "reading from the agent failed: {why}"
                    )))
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AgentError::new(
                        "the agent closed its connection before answering",
                    ))
                }
            }
        }
    }

    /// Send a request and wait for its reply, answering whatever the agent
    /// asks meanwhile. Notifications before the prompt are not part of the
    /// turn and are dropped.
    fn call<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
        deadline: Instant,
        policy: &Policy,
    ) -> Result<Result<Value, acp::Error>, AgentError> {
        let id = self.request(method, params)?;
        let waiting = format!(
            "the agent did not finish the handshake within {:?} (waiting for `{method}`)",
            self.limits.handshake
        );
        loop {
            match self.next(deadline, &waiting, true)? {
                Next::Cancelled => return Err(AgentError::new(CANCELLED)),
                Next::Message(Message::Response { id: got, outcome }) if got == id => {
                    return Ok(outcome)
                }
                Next::Message(Message::Request(request)) => answer(self, policy, request, false)?,
                Next::Message(_) => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    // -- A fake agent on an in-process pipe pair ---------------------------

    /// One thing the fake agent does after it receives the prompt.
    #[derive(Clone)]
    enum Step {
        /// Write this raw line.
        Say(String),
        /// Send this request and wait for the client's reply.
        Ask(Value),
        /// Answer the prompt with this stop reason.
        Finish(&'static str),
        /// Say nothing more; answer a `session/cancel` with `cancelled`.
        Silent,
    }

    /// What the fake agent saw: every client message, and the replies to its
    /// own requests.
    #[derive(Clone, Default)]
    struct Seen {
        client: Arc<Mutex<Vec<Value>>>,
        replies: Arc<Mutex<Vec<Value>>>,
    }

    impl Seen {
        fn methods(&self) -> Vec<String> {
            self.client
                .lock()
                .expect("lock")
                .iter()
                .filter_map(|m| m.get("method").and_then(Value::as_str).map(str::to_string))
                .collect()
        }
        fn first(&self, method: &str) -> Option<Value> {
            self.client
                .lock()
                .expect("lock")
                .iter()
                .find(|m| m.get("method").and_then(Value::as_str) == Some(method))
                .cloned()
        }
        fn reply(&self, id: i64) -> Option<Value> {
            self.replies
                .lock()
                .expect("lock")
                .iter()
                .find(|m| m.get("id") == Some(&json!(id)))
                .cloned()
        }
    }

    fn update(kind: Value) -> String {
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":kind}})
            .to_string()
    }

    fn chunk(text: &str) -> String {
        update(
            json!({"sessionUpdate":"agent_message_chunk","messageId":"m1",
                      "content":{"type":"text","text":text}}),
        )
    }

    fn permission(id: i64, kind: &str, path: &str) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{
        "sessionId":"s-1",
        "toolCall":{"toolCallId":"t1","title":"write","kind":kind,
                    "locations":[{"path":path}]},
        "options":[
            {"optionId":"yes","name":"Allow","kind":"allow_once"},
            {"optionId":"always","name":"Always","kind":"allow_always"},
            {"optionId":"no","name":"Reject","kind":"reject_once"}
        ]}})
    }

    /// What `session/new` answers: a mode option offering `plan`, like the
    /// one opencode 1.18.4 sent.
    fn session_with_plan_mode() -> Value {
        json!({"sessionId":"s-1","configOptions":[
            {"id":"model","name":"Model","category":"model","type":"select",
             "currentValue":"vendor/model-a","options":[{"value":"vendor/model-a","name":"A"}]},
            {"id":"mode","name":"Session Mode","category":"mode","type":"select",
             "currentValue":"build","options":[
                {"value":"build","name":"build"},{"value":"plan","name":"plan"}]}]})
    }

    fn play(
        input: std::io::PipeReader,
        mut out: std::io::PipeWriter,
        session_new: Value,
        script: Vec<Step>,
        seen: Seen,
    ) {
        let mut lines = BufReader::new(input).lines();
        let mut prompt_id = Value::Null;
        while let Some(Ok(line)) = lines.next() {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            seen.client.lock().expect("lock").push(message.clone());
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let result = |value: Value| json!({"jsonrpc":"2.0","id":id,"result":value});
            let said = match message.get("method").and_then(Value::as_str) {
                Some("initialize") => writeln!(
                    out,
                    "{}",
                    result(json!({"protocolVersion":1,"agentCapabilities":{}}))
                ),
                Some("session/new") => writeln!(out, "{}", result(session_new.clone())),
                Some("session/set_config_option") => {
                    writeln!(out, "{}", result(json!({"configOptions":[]})))
                }
                Some("session/cancel") => {
                    writeln!(
                        out,
                        "{}",
                        json!({"jsonrpc":"2.0","id":prompt_id,
                                               "result":{"stopReason":"cancelled"}})
                    )
                }
                Some("session/prompt") => {
                    prompt_id = id.clone();
                    for step in &script {
                        match step {
                            Step::Say(raw) => {
                                if writeln!(out, "{raw}").is_err() {
                                    return;
                                }
                            }
                            Step::Ask(request) => {
                                if writeln!(out, "{request}").is_err() {
                                    return;
                                }
                                // Wait for the reply to exactly this request.
                                for line in lines.by_ref() {
                                    let Ok(line) = line else { return };
                                    let Ok(got) = serde_json::from_str::<Value>(&line) else {
                                        continue;
                                    };
                                    seen.client.lock().expect("lock").push(got.clone());
                                    if got.get("method").is_none()
                                        && got.get("id") == request.get("id")
                                    {
                                        seen.replies.lock().expect("lock").push(got);
                                        break;
                                    }
                                }
                            }
                            Step::Finish(reason) => {
                                let _ = writeln!(out, "{}", result(json!({"stopReason":reason})));
                            }
                            Step::Silent => break,
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            };
            if said.is_err() {
                return;
            }
        }
    }

    /// A connection whose other end is a scripted fake agent on a thread.
    fn fake_agent(
        limits: Limits,
        session_new: Value,
        script: Vec<Step>,
    ) -> (Connection<std::io::PipeWriter>, Seen) {
        let (client_reads, agent_writes) = std::io::pipe().expect("pipe");
        let (agent_reads, client_writes) = std::io::pipe().expect("pipe");
        let seen = Seen::default();
        let watched = seen.clone();
        std::thread::spawn(move || play(agent_reads, agent_writes, session_new, script, watched));
        let incoming = spawn_reader(client_reads, limits.max_line);
        (Connection::new(client_writes, incoming, limits, None), seen)
    }

    fn setup(mode: PermissionMode, workspace: &Path) -> TurnSetup {
        TurnSetup {
            cwd: workspace.to_path_buf(),
            extra_dirs: Vec::new(),
            mcp_servers: Vec::new(),
            resume: None,
            policy: Policy {
                mode,
                guards: crate::guards::Guards::default(),
                roots: vec![workspace.to_path_buf()],
            },
        }
    }

    fn workspace() -> PathBuf {
        std::env::temp_dir().join("acp-fake-workspace")
    }

    /// Run one turn against a connection, bounded, collecting its events.
    fn drive(
        mut conn: Connection<std::io::PipeWriter>,
        mode: PermissionMode,
    ) -> (Result<Finished, AgentError>, Vec<AgentEvent>) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut events = Vec::new();
            let result = converse(
                &mut conn,
                &setup(mode, &workspace()),
                "say hello",
                &mut |e: &AgentEvent| events.push(e.clone()),
            );
            let _ = tx.send((result, events));
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("the turn hung instead of ending")
    }

    // -- The tests --------------------------------------------------------

    /// **THE HANDSHAKE, AND STREAMED CHUNKS BECOMING OUTPUT.**
    ///
    /// `initialize` advertises no capability, `session/new` carries the
    /// workspace as cwd, and the chunks arrive as deltas and then as one whole
    /// block — the same shape the other backends give the worker, which reads
    /// only `Text`.
    #[test]
    fn a_turn_handshakes_and_streams_chunks_into_text() {
        let (conn, seen) = fake_agent(
            Limits::DEFAULT,
            json!({"sessionId":"s-1"}),
            vec![
                Step::Say(chunk("Hel")),
                Step::Say(chunk("lo")),
                Step::Finish("end_turn"),
            ],
        );
        let (result, events) = drive(conn, PermissionMode::WorkspaceWrite);
        let finished = result.expect("the turn succeeds");

        assert_eq!(finished.session_id, "s-1");
        assert!(!finished.outcome.is_error);
        assert_eq!(finished.outcome.stop_reason, "end_turn");
        assert_eq!(
            seen.methods(),
            ["initialize", "session/new", "session/prompt"],
            "the handshake is initialize, session/new, then the prompt"
        );
        let init = seen.first("initialize").expect("initialize was sent");
        assert_eq!(init["params"]["protocolVersion"], json!(1));
        let caps = &init["params"]["clientCapabilities"];
        assert_eq!(caps["fs"]["readTextFile"], json!(false), "{caps}");
        assert_eq!(caps["fs"]["writeTextFile"], json!(false), "{caps}");
        assert_eq!(caps["terminal"], json!(false), "{caps}");
        let new = seen.first("session/new").expect("session/new was sent");
        assert_eq!(
            new["params"]["cwd"],
            json!(workspace().to_string_lossy()),
            "the workspace is the session's cwd"
        );

        let texts: Vec<&AgentEvent> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Text(_) | AgentEvent::TextDelta(_)))
            .collect();
        assert_eq!(
            texts,
            [
                &AgentEvent::TextDelta("Hel".into()),
                &AgentEvent::TextDelta("lo".into()),
                &AgentEvent::Text("Hello".into()),
            ],
            "every event: {events:?}"
        );
        assert!(matches!(
            events.first(),
            Some(AgentEvent::SessionStarted { session_id, .. }) if session_id == "s-1"
        ));
    }

    /// **A WRITE IS DENIED IN READ-ONLY MODE — AND THE SESSION IS PUT IN THE
    /// AGENT'S OWN READ-ONLY MODE FIRST.**
    ///
    /// The second half exists because answering permission requests is not
    /// enough: measured, an agent in its ordinary mode writes without asking.
    #[test]
    fn a_write_is_refused_in_read_only_mode() {
        let (conn, seen) = fake_agent(
            Limits::DEFAULT,
            session_with_plan_mode(),
            vec![
                Step::Ask(permission(
                    90,
                    "edit",
                    &workspace().join("src/a.rs").to_string_lossy(),
                )),
                Step::Finish("end_turn"),
            ],
        );
        let (result, _) = drive(conn, PermissionMode::ReadOnly);
        result.expect("the turn itself completes");

        let set = seen
            .first("session/set_config_option")
            .expect("read-only mode was never asked for");
        assert_eq!(set["params"]["configId"], json!("mode"));
        assert_eq!(set["params"]["value"], json!("plan"));

        let reply = seen.reply(90).expect("the permission request was answered");
        assert_eq!(
            reply["result"]["outcome"],
            json!({"outcome":"selected","optionId":"no"}),
            "read-only mode approved a write: {reply}"
        );
    }

    /// The same write is approved in workspace-write mode, once and not for
    /// good; a protected path and a path outside the workspace are not.
    #[test]
    fn a_write_is_approved_once_inside_the_workspace_and_never_on_a_guard() {
        let ws = workspace();
        let (conn, seen) = fake_agent(
            Limits::DEFAULT,
            json!({"sessionId":"s-1"}),
            vec![
                Step::Ask(permission(
                    91,
                    "edit",
                    &ws.join("src/a.rs").to_string_lossy(),
                )),
                Step::Ask(permission(
                    92,
                    "edit",
                    &ws.join("tests/t.rs").to_string_lossy(),
                )),
                Step::Ask(permission(
                    93,
                    "edit",
                    &ws.join("../elsewhere/x.rs").to_string_lossy(),
                )),
                Step::Ask(permission(94, "execute", &ws.to_string_lossy())),
                Step::Ask(permission(95, "other", &ws.to_string_lossy())),
                Step::Finish("end_turn"),
            ],
        );
        let (result, _) = drive(conn, PermissionMode::WorkspaceWrite);
        result.expect("the turn completes");

        let chosen =
            |id| seen.reply(id).expect("answered")["result"]["outcome"]["optionId"].clone();
        assert_eq!(chosen(91), json!("yes"), "an ordinary write, approved once");
        assert_eq!(chosen(92), json!("no"), "a protected path is refused");
        assert_eq!(chosen(93), json!("no"), "outside the workspace is refused");
        assert_eq!(chosen(94), json!("no"), "a command needs full access");
        assert_eq!(chosen(95), json!("no"), "an unmappable tool is refused");
        assert!(
            seen.first("session/set_config_option").is_none(),
            "a writing run must not be put in read-only mode"
        );
    }

    /// **AN UNKNOWN METHOD GETS "METHOD NOT FOUND" — NEVER A HANG.**
    ///
    /// No `fs` capability is advertised, so a file read is as unknown as a
    /// made-up method, and both must be answered for the agent to go on.
    #[test]
    fn an_unknown_method_is_answered_with_method_not_found() {
        let (conn, seen) = fake_agent(
            Limits::DEFAULT,
            json!({"sessionId":"s-1"}),
            vec![
                Step::Ask(json!({"jsonrpc":"2.0","id":70,"method":"fs/read_text_file",
                                 "params":{"sessionId":"s-1","path":"/etc/hosts"}})),
                Step::Ask(json!({"jsonrpc":"2.0","id":71,"method":"x/made_up","params":{}})),
                Step::Finish("end_turn"),
            ],
        );
        let (result, _) = drive(conn, PermissionMode::WorkspaceWrite);
        result.expect("the turn completes after both answers");

        for id in [70, 71] {
            let reply = seen.reply(id).expect("the request was answered");
            assert_eq!(
                reply["error"]["code"],
                json!(-32601),
                "not answered with method-not-found: {reply}"
            );
            assert!(reply.get("result").is_none(), "{reply}");
        }
    }

    /// **AN OVER-LONG LINE ENDS THE TURN AS AN ERROR.**
    #[test]
    fn an_over_long_line_is_an_error_not_a_buffer() {
        let limits = Limits {
            max_line: 1024,
            ..Limits::DEFAULT
        };
        let huge = chunk(&"x".repeat(5000));
        let (conn, _) = fake_agent(
            limits,
            json!({"sessionId":"s-1"}),
            vec![Step::Say(huge), Step::Finish("end_turn")],
        );
        let (result, _) = drive(conn, PermissionMode::WorkspaceWrite);
        let error = result.err().expect("an over-long line must fail the turn");
        assert!(
            error.to_string().contains("longer than 1024 bytes"),
            "{error}"
        );
    }

    /// The reader itself never keeps more than the cap of one line.
    #[test]
    fn the_bounded_reader_stops_at_the_cap() {
        let mut stream = std::io::Cursor::new(format!("short\n{}\nafter\n", "y".repeat(100)));
        assert!(matches!(read_bounded_line(&mut stream, 50), Ok(Some(l)) if l == "short"));
        assert!(matches!(
            read_bounded_line(&mut stream, 50),
            Err(Bounded::TooLong)
        ));
    }

    /// **A SILENT AGENT HITS THE IDLE DEADLINE.**
    #[test]
    fn a_silent_agent_hits_the_idle_deadline() {
        let limits = Limits {
            idle: Duration::from_millis(300),
            ..Limits::DEFAULT
        };
        let (conn, _) = fake_agent(limits, json!({"sessionId":"s-1"}), vec![Step::Silent]);
        let started = Instant::now();
        let (result, _) = drive(conn, PermissionMode::WorkspaceWrite);
        let error = result
            .err()
            .expect("a silent agent must not finish the turn");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            crate::agent::moves_the_work(&error.to_string()),
            "a wedged agent's turn should be able to move to a standby: {error}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// **CANCEL SENDS `session/cancel`.** (The tree is killed by `run_turn`;
    /// see `cancel_kills_the_whole_tree`.)
    #[test]
    fn cancel_sends_session_cancel() {
        let token = CancelToken::new();
        let (mut conn, seen) = fake_agent(
            Limits::DEFAULT,
            json!({"sessionId":"s-1"}),
            vec![Step::Silent],
        );
        conn.cancel = Some(token.clone());
        let trip = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            trip.cancel();
        });
        let (result, _) = drive(conn, PermissionMode::WorkspaceWrite);
        let error = result.err().expect("a cancelled turn is not a result");
        assert_eq!(error.to_string(), CANCELLED);
        let cancel = seen
            .first("session/cancel")
            .expect("session/cancel was never sent");
        assert_eq!(cancel["params"]["sessionId"], json!("s-1"));
    }

    /// A handshake that is never answered ends at the handshake deadline.
    #[test]
    fn an_agent_that_never_answers_initialize_times_out() {
        let limits = Limits {
            handshake: Duration::from_millis(300),
            ..Limits::DEFAULT
        };
        let (client_reads, _agent_writes) = std::io::pipe().expect("pipe");
        let (_agent_reads, client_writes) = std::io::pipe().expect("pipe");
        let conn = Connection::new(
            client_writes,
            spawn_reader(client_reads, limits.max_line),
            limits,
            None,
        );
        let (result, _) = drive(conn, PermissionMode::WorkspaceWrite);
        let error = result.err().expect("no answer, no turn");
        assert!(error.to_string().contains("`initialize`"), "{error}");
    }

    /// Tool calls are announced when they run and closed with their result.
    #[test]
    fn tool_calls_become_tool_use_and_tool_result() {
        let mut stream = EventStream::default();
        let mut events = Vec::new();
        let mut on = |e: &AgentEvent| events.push(e.clone());
        let parse = |v: Value| serde_json::from_value::<acp::SessionUpdate>(v).expect("update");
        // The three lines opencode 1.18.4 sent for one write, trimmed.
        stream.apply(
            parse(
                json!({"sessionUpdate":"tool_call","toolCallId":"c1","title":"write",
                         "kind":"edit","status":"pending","locations":[],"rawInput":{}}),
            ),
            &mut on,
        );
        stream.apply(
            parse(json!({"sessionUpdate":"tool_call_update","toolCallId":"c1",
                         "status":"in_progress","rawInput":{"filePath":"/w/hello.txt","content":"hi"}})),
            &mut on,
        );
        stream.apply(
            parse(
                json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed",
                         "content":[{"type":"content","content":{"type":"text",
                                     "text":"Wrote file successfully."}}]}),
            ),
            &mut on,
        );
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            matches!(&events[0], AgentEvent::ToolUse { id, name, input }
                if id == "c1" && name == "write" && input.contains("hello.txt")),
            "{events:?}"
        );
        assert_eq!(
            events[1],
            AgentEvent::ToolResult {
                id: "c1".into(),
                output: "Wrote file successfully.".into(),
                is_error: false
            }
        );
    }

    /// A plan is shown the way a to-do tool call is on the other backends.
    #[test]
    fn a_plan_becomes_one_tool_use() {
        let mut stream = EventStream::default();
        let mut events = Vec::new();
        let plan = serde_json::from_value::<acp::SessionUpdate>(json!({
            "sessionUpdate":"plan","entries":[
                {"content":"read the code","priority":"high","status":"completed"},
                {"content":"fix it","priority":"high","status":"pending"}]}))
        .expect("plan");
        stream.apply(plan, &mut |e: &AgentEvent| events.push(e.clone()));
        assert_eq!(
            events,
            [AgentEvent::ToolUse {
                id: "plan-1".into(),
                name: "plan".into(),
                input: "[done] read the code\n[pending] fix it".into()
            }]
        );
    }

    /// The team server gets the identity in its own environment too.
    #[test]
    fn the_harness_mcp_servers_become_stdio_entries() {
        let config = json!({"mcpServers":{"team":{"command":"/bin/incode",
                                                   "args":["--team-mcp"]}}})
        .to_string();
        let servers = mcp_servers(Some(&config), Some("implement"));
        let wire = serde_json::to_value(&servers).expect("serialise");
        assert_eq!(
            wire,
            json!([{"name":"team","command":"/bin/incode","args":["--team-mcp"],
                    "env":[{"name":crate::team_mcp::IDENTITY_ENV,"value":"implement"}]}])
        );
        assert!(mcp_servers(None, None).is_empty());
    }

    /// A read-only mode is found by id, and "already there" is not a switch.
    #[test]
    fn the_read_only_mode_is_found_where_the_agent_offers_it() {
        let session: acp::NewSessionResponse =
            serde_json::from_value(session_with_plan_mode()).expect("session");
        let config = session.config_options.unwrap_or_default();
        assert_eq!(
            read_only_switch(&config, None),
            Some(ReadOnlySwitch::Option {
                config: acp::SessionConfigId::new("mode"),
                value: acp::SessionConfigValueId::new("plan"),
            })
        );
        assert_eq!(current_model(&config).as_deref(), Some("vendor/model-a"));
        assert_eq!(read_only_switch(&[], None), None);
    }

    /// A read-only run on an agent with no read-only mode is refused.
    #[test]
    fn a_read_only_turn_is_refused_when_the_agent_has_no_read_only_mode() {
        let (conn, seen) = fake_agent(
            Limits::DEFAULT,
            json!({"sessionId":"s-1"}),
            vec![Step::Finish("end_turn")],
        );
        let (result, _) = drive(conn, PermissionMode::ReadOnly);
        let error = result.err().expect("refused");
        assert!(error.to_string().contains("no read-only mode"), "{error}");
        assert!(
            seen.first("session/prompt").is_none(),
            "the prompt must not be sent at all"
        );
    }

    /// `plan` narrows the turn; `acceptEdits` gives back what the session had.
    #[test]
    fn a_plan_turn_is_read_only_and_act_restores_the_session_mode() {
        let mut backend = AcpBackend::new("x", "x", &[], PathBuf::from("."))
            .with_permission_mode(PermissionMode::DangerFullAccess);
        backend.set_permission_mode("plan");
        assert_eq!(backend.mode, PermissionMode::ReadOnly);
        backend.set_permission_mode("acceptEdits");
        assert_eq!(backend.mode, PermissionMode::DangerFullAccess);
        backend.set_permission_mode("no-such-mode");
        assert_eq!(backend.mode, PermissionMode::ReadOnly, "fails closed");
    }

    // -- A real process tree ------------------------------------------------

    /// A fake agent as a script: answers the handshake, starts a grandchild
    /// that ticks into `ticks.txt`, then records every line it is sent.
    fn a_scripted_agent_with_a_ticking_grandchild(dir: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            std::fs::write(
                dir.join("tick.cmd"),
                "@echo off\r\n:loop\r\necho x>>\"%~dp0ticks.txt\"\r\n\
                 ping -n 2 -w 500 127.0.0.1 >nul\r\ngoto loop\r\n",
            )
            .expect("write the grandchild");
            let agent = dir.join("agent.cmd");
            std::fs::write(
                &agent,
                "@echo off\r\n\
                 set /p l1=\r\n\
                 echo {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1}}\r\n\
                 set /p l2=\r\n\
                 echo {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}\r\n\
                 set /p l3=\r\n\
                 start \"\" /B cmd /C \"%~dp0tick.cmd\"\r\n\
                 :loop\r\n\
                 set \"l=\"\r\n\
                 set /p l=\r\n\
                 if defined l echo %l%>>\"%~dp0seen.txt\"\r\n\
                 ping -n 2 -w 500 127.0.0.1 >nul\r\n\
                 goto loop\r\n",
            )
            .expect("write the agent");
            agent
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let agent = dir.join("agent.sh");
            std::fs::write(
                &agent,
                "#!/bin/sh\nd=$(dirname \"$0\")\n\
                 read -r l1\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1}}'\n\
                 read -r l2\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\"}}'\n\
                 read -r l3\n\
                 (while true; do echo x >> \"$d/ticks.txt\"; sleep 0.3; done) &\n\
                 while read -r l; do echo \"$l\" >> \"$d/seen.txt\"; done\n\
                 sleep 60\n",
            )
            .expect("write the agent");
            std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))
                .expect("make it runnable");
            agent
        }
    }

    fn size(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// **CANCEL SENDS `session/cancel` AND KILLS THE WHOLE TREE.**
    ///
    /// Through `run_turn`, on a real process: the agent records what it is
    /// sent, and its grandchild writes to a file while it lives — a file that
    /// stops growing is a grandchild that stopped.
    #[test]
    fn cancel_kills_the_whole_tree() {
        let dir = tempfile::tempdir().expect("scratch");
        let agent = a_scripted_agent_with_a_ticking_grandchild(dir.path());
        let ticks = dir.path().join("ticks.txt");
        let token = CancelToken::new();

        let workspace = dir.path().to_path_buf();
        let binary = agent.to_string_lossy().to_string();
        let turn_token = token.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut backend = AcpBackend::new("fake", &binary, &[], workspace);
            backend.set_cancel(turn_token);
            let result = backend.run_turn("work", &mut |_| {});
            let _ = tx.send(result.map(|o| o.stop_reason));
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        while size(&ticks) < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(size(&ticks) >= 4, "the grandchild never started ticking");

        token.cancel();
        let result = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the cancelled turn did not end");
        assert_eq!(
            result.err().map(|e| e.to_string()).as_deref(),
            Some(CANCELLED)
        );

        let seen = std::fs::read_to_string(dir.path().join("seen.txt")).unwrap_or_default();
        assert!(
            seen.contains("session/cancel"),
            "the agent was sent: {seen:?}"
        );

        // Let a tick already in flight land, then watch for new ones.
        std::thread::sleep(Duration::from_millis(1500));
        let after_kill = size(&ticks);
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(
            size(&ticks),
            after_kill,
            "the grandchild kept running after the cancelled turn"
        );
    }
}
