//! `incode --acp`: incode as an Agent Client Protocol agent.
//!
//! An ACP client — an editor, or the owner's desktop app — starts
//! `incode --acp` as a child process and drives it with JSON-RPC 2.0, one
//! message per line on stdin and stdout, exactly as it drives any other ACP
//! agent. Here incode IS the agent. The work underneath is still done by the
//! subscription CLIs incode already drives headless, through the SAME session
//! the interactive REPL builds ([`crate::cli::prepare_session`]) and the SAME
//! turn it runs ([`crate::cli::plan_turn`] and
//! [`AgentSession::turn_observed`]). incode never drives another harness over
//! ACP; this module is only ever the agent side of the pipe.
//!
//! # Methods answered
//!
//! - `initialize` — protocol version 1, and the capabilities incode really
//!   has: text and embedded-context prompts, loading and listing sessions,
//!   and stdio MCP servers from the client (no HTTP or SSE ones). No
//!   authentication methods.
//! - `authenticate` — refused as invalid params: the method id must be one
//!   `initialize` advertised, and none is. The subscription CLIs hold their
//!   own logins.
//! - `session/new` — `cwd` must be an absolute path to an existing directory;
//!   it becomes the workspace. The session is built on its own thread, the
//!   client's stdio MCP servers are started as incode's own tools, the answer
//!   carries the modes and the pickers ([`config`]), and the slash commands
//!   follow as `available_commands_update`.
//! - `session/load` — the same, for a session kept in `cwd`: its history is
//!   replayed first ([`history`]).
//! - `session/list` — the sessions kept in `cwd`, or in every workspace an ACP
//!   session was recorded in.
//! - `session/prompt` — one turn; its text, tool calls, tool results, task
//!   list, the diff of each edit that landed and the context window's fill
//!   stream as `session/update`, and the reply carries the stop reason. A
//!   prompt that is a slash command runs that command instead ([`commands`]);
//!   `/run` works the ledger and sends it as the plan ([`run`]).
//! - `session/cancel` — trips the session's cancel token; the pending prompt
//!   answers `cancelled`.
//! - `session/set_mode` — `read-only`, `workspace-write` or
//!   `danger-full-access`, applied from the next turn.
//! - `session/set_config_option` — the agent or the role, applied from the
//!   next turn.
//! - `session/close` — cancels the session's turn and frees its slot.
//!
//! Any other request is answered "method not found"; a notification nothing
//! handles is dropped, as JSON-RPC requires. See `docs/ARCHITECTURE.md`.
//!
//! # Requests sent to the client
//!
//! - `session/request_permission` — when the memory engine says a call on
//!   incode's own tools needs a person ([`permission`]). The client's response
//!   comes back on the reader and is handed to the waiting turn. Also, once a
//!   session is answered, for each project `.mcp.json` server not yet
//!   approved — allow once, allow always or reject — from a thread of its own.
//!
//! # Threads
//!
//! The reader — the calling thread — parses and dispatches and never runs a
//! turn, so a `session/cancel` is read while a prompt is running. Each session
//! lives on a thread of its own, because an [`AgentSession`] is not `Send`: it
//! is built there and never leaves.
//!
//! # stdout
//!
//! Protocol only. [`run_stdio`] moves the process's standard handles before
//! anything else happens — see [`stdio`].

mod commands;
mod config;
mod history;
mod permission;
mod present;
mod run;
// Shared with `incode -t --output-format stream-json|json`, whose stdout is
// its JSON and nothing else, for the same reason.
pub(crate) mod stdio;
mod transport;
mod workers;

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1 as acp;
use agent_client_protocol_schema::ProtocolVersion;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::agent::{AgentBackend, AgentEvent, DetectedAgent};
use crate::cli::agent_turn::{AgentSession, TurnStart};
use crate::config::IncordCodeConfig;
use crate::permissions::PermissionMode;
use crate::runtime::cancel::CancelToken;
use run::LedgerRuns;
use transport::{Incoming, Line, Outbox};

/// Builds the backend for an agent a client picks, the way a session's own
/// was built.
type BuildAgent = Box<dyn Fn(&DetectedAgent) -> Box<dyn AgentBackend>>;

/// Work handed to a session's thread.
///
/// Room for a few: a prompt is admitted only when none is running (the
/// reader refuses a second before it is queued), and an [`Work::Apply`] is a
/// wake-up that is dropped rather than waited for when the queue is full —
/// the next prompt applies the client's choices anyway.
const WORK_QUEUE: usize = 4;

/// How many sessions one connection may hold at once.
///
/// Each session is a thread and, while a turn runs, an agent process tree. An
/// editor opens one per conversation; sixteen is well past what a person
/// drives by hand, and still bounds what a runaway client can make this
/// process start. A `session/new` beyond it is refused, not queued.
const MAX_SESSIONS: usize = 16;

/// At the end of input, how long running turns get to stop before this
/// process returns anyway.
///
/// End of input means the client has gone. Every running turn is cancelled
/// first, and a backend kills its process tree on cancel, which takes well
/// under a second. Two seconds bounds the exit without cutting that short.
///
/// Each session's thread then tells the memory engine the session ended, and
/// the wait allows for that too, up to the engine hook's own bound
/// ([`crate::hooks::engine::SESSION_END_BOUND`]): a process that returned
/// first would kill the hook with it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How often the shutdown looks at the session threads while it waits.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// The modes a client may choose, in the order they are offered.
///
/// incode's own permission modes, one to one. `prompt` and `allow` are
/// policies with no rung on the capability ladder and are not offered.
const MODES: [PermissionMode; 3] = [
    PermissionMode::ReadOnly,
    PermissionMode::WorkspaceWrite,
    PermissionMode::DangerFullAccess,
];

/// Run `incode --acp` on this process's stdin and stdout until the client
/// closes stdin.
///
/// # Errors
///
/// When stdin and stdout cannot be taken for the protocol. Nothing is served
/// then: serving with stdout still shared is the failure this mode must not
/// have.
pub fn run_stdio(config: IncordCodeConfig) -> anyhow::Result<()> {
    let claimed = stdio::claim()
        .map_err(|e| anyhow::anyhow!("could not take stdin/stdout for the protocol: {e}"))?;
    // stderr, which after `claim` is also where stdout goes.
    eprintln!("incode ACP agent ready on stdio");
    serve(
        claimed.input,
        Box::new(claimed.output),
        CliSessions { config },
        transport::MAX_LINE_BYTES,
    );
    Ok(())
}

/// A session, as built for one `session/new` or `session/load`.
pub(crate) struct Opened {
    pub(crate) session: AgentSession,
    /// The permission mode already applied to it.
    pub(crate) mode: PermissionMode,
    /// What the slash commands need beyond the session.
    pub(crate) settings: commands::Settings,
    /// The workspace the session was built on: absolute and canonical, the
    /// spelling its snapshot and tools name files by.
    pub(crate) workspace: PathBuf,
    /// Which models serve which role, as the session was opened with them.
    pub(crate) roles: crate::roles::RoleTable,
    /// Builds the backend for an agent the client picks, the way this
    /// session's own was built.
    pub(crate) build: BuildAgent,
    /// How `/run` works the ledger.
    pub(crate) runs: Box<dyn LedgerRuns>,
}

/// Builds the session a `session/new` or `session/load` asks for.
///
/// Called on the thread that will drive the session, because a session cannot
/// cross threads. `cancel` is the token `session/cancel` trips.
pub(crate) trait SessionFactory: Send + Sync + 'static {
    /// `session_id` is the protocol's id for the session, which is also the id
    /// the memory engine files it under. `loading` is set for a session
    /// opened again (`session/load`).
    fn open(
        &self,
        workspace: &Path,
        cancel: CancelToken,
        session_id: &str,
        loading: bool,
    ) -> Result<Opened, String>;

    /// Where the operator's settings and session records live.
    fn data_dir(&self) -> PathBuf;

    /// The installed external tools, as MCP servers each session starts, and
    /// a line for each configured tool that is not installed. None by default.
    fn tool_servers(&self) -> (Vec<crate::toolbox::ServerSpec>, Vec<String>) {
        (Vec::new(), Vec::new())
    }
}

/// The production factory: the interactive CLI's own session builder.
struct CliSessions {
    config: IncordCodeConfig,
}

impl SessionFactory for CliSessions {
    fn open(
        &self,
        workspace: &Path,
        cancel: CancelToken,
        session_id: &str,
        loading: bool,
    ) -> Result<Opened, String> {
        let mut config = self.config.clone();
        config.workspace = workspace.to_path_buf();
        // Read by `prepare_session` only to open the memory engine's session
        // as `resume`; the model's own conversation is resumed from the
        // session record, in `drive`.
        config.resume_session = loading.then(|| session_id.to_string());
        let prepared = crate::cli::prepare_session(&config, cancel, Instant::now(), session_id)
            .map_err(|e| e.to_string())?;
        let mut session = prepared.session;
        // Kept whether or not an engine reads it: it is what `session/load`
        // replays.
        session.keep_transcript();
        let roles = prepared.admin.resolve_roles().value;
        let build = {
            let workspace = prepared.workspace.clone();
            let system_prompt = prepared.system_prompt.clone();
            let guards = crate::guards::Guards::load(&workspace).map_err(|e| e.to_string())?;
            // As `AgentSession::new` builds the session's first backend.
            Box::new(move |agent: &DetectedAgent| {
                crate::cli::route_backend(
                    &workspace,
                    &agent.id,
                    &agent.command,
                    &guards,
                    &system_prompt,
                    true,
                )
            })
        };
        Ok(Opened {
            session,
            mode: prepared.permission_mode,
            settings: commands::Settings {
                data_dir: prepared.incode_dir.clone(),
                catalogue: prepared.catalogue.clone(),
                lead: prepared.agent,
            },
            workspace: prepared.workspace,
            roles,
            build,
            runs: Box::new(run::CliRuns {
                data_dir: prepared.incode_dir,
                admin: prepared.admin,
                catalogue: prepared.catalogue,
                fleet: prepared.fleet,
                system_prompt: prepared.system_prompt,
                extra_dirs: self.config.additional_dirs.clone(),
                env_filter: self.config.env_filter_enabled,
            }),
        })
    }

    fn data_dir(&self) -> PathBuf {
        crate::admin::AdminConfig::default_dir()
    }

    fn tool_servers(&self) -> (Vec<crate::toolbox::ServerSpec>, Vec<String>) {
        let data_dir = self.data_dir();
        let tools = crate::admin::AdminConfig::load(&data_dir)
            .map(|admin| admin.tools)
            .unwrap_or_default();
        crate::ext_tools::session_servers(&tools, &data_dir)
    }
}

/// Serve one client until its input ends.
pub(crate) fn serve<R: Read, F: SessionFactory>(
    input: R,
    output: Box<dyn Write + Send>,
    factory: F,
    max_line: usize,
) {
    let mut server = Server {
        outbox: Arc::new(Outbox::new(output)),
        factory: Arc::new(factory),
        sessions: HashMap::new(),
        subagents: false,
    };
    let mut reader = BufReader::new(input);
    loop {
        match transport::read_bounded_line(&mut reader, max_line) {
            Ok(None) => break,
            Ok(Some(Line::TooLong { id })) => server.outbox.respond::<Value>(
                id,
                Err(acp::Error::invalid_request().data(Value::String(format!(
                    "a message longer than {max_line} bytes was dropped unread"
                )))),
            ),
            Ok(Some(Line::Text(text))) => {
                if !text.trim().is_empty() {
                    server.handle(&text);
                }
            }
            Err(e) => {
                eprintln!("incode acp: reading from the client failed: {e}");
                break;
            }
        }
    }
    server.shutdown();
}

struct Server<F: SessionFactory> {
    outbox: Arc<Outbox>,
    factory: Arc<F>,
    sessions: HashMap<String, SessionHandle>,
    /// The client advertised the draft sub-agent extension
    /// (`clientCapabilities.subagents`) in `initialize`: workers are shown as
    /// sub-agents ([`workers`]). Without it they are not announced — a client
    /// that does not know the two updates may refuse the message.
    subagents: bool,
}

/// The reader's hold on one session.
struct SessionHandle {
    work: SyncSender<Work>,
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
}

/// One `session/prompt`, handed to the session's thread.
struct Prompt {
    id: acp::RequestId,
    blocks: Vec<acp::ContentBlock>,
}

/// What a session's thread is handed.
enum Work {
    Prompt(Prompt),
    /// The client changed a picker: apply it now, if nothing is running.
    Apply,
}

/// What the reader and a session's thread both touch.
struct Shared {
    /// The token the session was built with — tripped by `session/cancel`.
    cancel: CancelToken,
    state: Mutex<TurnState>,
    /// The pickers, once the session is built: chosen on the reader, applied
    /// on the session's thread. See [`config`].
    choices: Mutex<Option<config::Choices>>,
}

#[derive(Default)]
struct TurnState {
    /// A prompt is running or about to.
    running: bool,
    /// The client cancelled the running prompt.
    cancelled: bool,
    /// The mode the client last chose; applied before the next turn.
    mode: Option<PermissionMode>,
}

impl Shared {
    fn new() -> Self {
        Self {
            cancel: CancelToken::new(),
            state: Mutex::new(TurnState::default()),
            choices: Mutex::new(None),
        }
    }

    fn state(&self) -> MutexGuard<'_, TurnState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn choices(&self) -> MutexGuard<'_, Option<config::Choices>> {
        self.choices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claim the session for a prompt; `false` when one is already running.
    fn begin(&self) -> bool {
        let mut state = self.state();
        if state.running {
            return false;
        }
        state.running = true;
        state.cancelled = false;
        true
    }

    fn finish(&self) {
        let mut state = self.state();
        state.running = false;
        state.cancelled = false;
    }

    /// Cancel the running prompt, if there is one. A cancel with nothing
    /// running is dropped rather than left to stop the next prompt.
    fn cancel_running(&self) {
        let mut state = self.state();
        if state.running {
            state.cancelled = true;
            self.cancel.cancel();
        }
    }

    fn cancelled(&self) -> bool {
        self.state().cancelled
    }

    /// Trip the token again if the prompt was cancelled.
    ///
    /// A turn clears the token when it starts, so that a stale Ctrl-C cannot
    /// cancel it. A `session/cancel` that lands between the last check before
    /// the turn and that reset is cleared with it. Re-tripping on every event
    /// of the turn narrows what that costs to the agent's first line of
    /// output — the backends look at the token per line — and the prompt
    /// still answers `cancelled`, because the answer is decided by
    /// [`Shared::cancelled`], which the reset does not touch.
    fn hold_cancel(&self) {
        if self.cancelled() {
            self.cancel.cancel();
        }
    }
}

impl<F: SessionFactory> Server<F> {
    fn handle(&mut self, line: &str) {
        match transport::classify(line) {
            Err(refused) => {
                let transport::Refusal { id, error } = *refused;
                self.outbox.respond::<Value>(id, Err(error));
            }
            // The answer to a request a session's turn sent the client, and is
            // waiting on. Handed over here, on the reader, because the turn's
            // own thread is the one waiting.
            Ok(Incoming::Response { id, outcome }) => self.outbox.resolve(&id, outcome),
            Ok(Incoming::Notification { method, params }) => self.notification(&method, params),
            Ok(Incoming::Request { id, method, params }) => self.request(id, &method, params),
        }
    }

    fn request(&mut self, id: acp::RequestId, method: &str, params: Option<Value>) {
        let names = &acp::AGENT_METHOD_NAMES;
        if method == names.initialize {
            // Not in the stable schema, so read from the raw capabilities.
            let subagents = params
                .as_ref()
                .and_then(|p| p.get("clientCapabilities"))
                .and_then(|c| c.get("subagents"))
                .is_some_and(|s| !s.is_null() && s != &Value::Bool(false));
            let answer = decode::<acp::InitializeRequest>(params).map(|_| initialize());
            if answer.is_ok() {
                self.subagents = subagents;
            }
            self.outbox.respond(id, answer);
        } else if method == names.authenticate {
            let answer = decode::<acp::AuthenticateRequest>(params).and_then(|request| {
                Err::<acp::AuthenticateResponse, _>(invalid_params(format!(
                    "`{}` is not an authentication method this agent offered; it offers \
                     none, because the agent CLIs underneath hold their own logins",
                    request.method_id
                )))
            });
            self.outbox.respond(id, answer);
        } else if method == names.session_new {
            self.new_session(id, params);
        } else if method == names.session_load {
            self.load_session(id, params);
        } else if method == names.session_list {
            let answer = self.list_sessions(params);
            self.outbox.respond(id, answer);
        } else if method == names.session_prompt {
            self.prompt(id, params);
        } else if method == names.session_set_mode {
            let answer = self.set_mode(params);
            self.outbox.respond(id, answer);
        } else if method == names.session_set_config_option {
            let answer = self.set_config_option(params);
            self.outbox.respond(id, answer);
        } else if method == names.session_close {
            let answer = self.close(params);
            self.outbox.respond(id, answer);
        } else {
            self.outbox
                .respond::<Value>(id, Err(acp::Error::method_not_found()));
        }
    }

    fn notification(&mut self, method: &str, params: Option<Value>) {
        // Every other notification — the protocol-level `$/cancel_request`
        // included — has nothing to act on here: the only long-running thing
        // this agent does is a prompt, and `session/cancel` is how one stops.
        if method != acp::AGENT_METHOD_NAMES.session_cancel {
            return;
        }
        match decode::<acp::CancelNotification>(params) {
            Ok(cancel) => {
                if let Some(handle) = self.sessions.get(&*cancel.session_id.0) {
                    handle.shared.cancel_running();
                }
            }
            Err(e) => eprintln!("incode acp: a session/cancel could not be read: {e}"),
        }
    }

    fn new_session(&mut self, id: acp::RequestId, params: Option<Value>) {
        let request = match decode::<acp::NewSessionRequest>(params) {
            Ok(request) => request,
            Err(e) => return self.outbox.respond::<Value>(id, Err(e)),
        };
        let workspace = match workspace_from(&request.cwd) {
            Ok(workspace) => workspace,
            Err(why) => return self.outbox.respond::<Value>(id, Err(invalid_params(why))),
        };
        if !request.additional_directories.is_empty() {
            // Not advertised, so a client must not send them — and a root
            // outside `cwd` named by the client is exactly what is not taken
            // on trust. Operator-chosen roots come from `--add-dir` instead.
            return self.outbox.respond::<Value>(
                id,
                Err(invalid_params(
                    "additional directories are not offered: this agent works in `cwd` only",
                )),
            );
        }
        let session_id = acp::SessionId::new(uuid::Uuid::new_v4().to_string());
        self.start_session(
            id,
            Start {
                session_id,
                workspace,
                history: None,
                servers: request.mcp_servers,
                subagents: self.subagents,
            },
        );
    }

    /// Open a session kept in `cwd` again: its history replayed, its
    /// model's conversation resumed where the agent can. See [`history`].
    fn load_session(&mut self, id: acp::RequestId, params: Option<Value>) {
        let request = match decode::<acp::LoadSessionRequest>(params) {
            Ok(request) => request,
            Err(e) => return self.outbox.respond::<Value>(id, Err(e)),
        };
        let workspace = match workspace_from(&request.cwd) {
            Ok(workspace) => workspace,
            Err(why) => return self.outbox.respond::<Value>(id, Err(invalid_params(why))),
        };
        if !request.additional_directories.is_empty() {
            return self.outbox.respond::<Value>(
                id,
                Err(invalid_params(
                    "additional directories are not offered: this agent works in `cwd` only",
                )),
            );
        }
        self.prune();
        if self.sessions.contains_key(&*request.session_id.0) {
            return self.outbox.respond::<Value>(
                id,
                Err(acp::Error::invalid_request().data(Value::String(format!(
                    "session `{}` is already open on this connection",
                    request.session_id
                )))),
            );
        }
        let kept = history::transcript_of(&workspace, &request.session_id.0);
        let conversation = match history::read(&kept) {
            Ok(conversation) => conversation,
            Err(_) => {
                return self.outbox.respond::<Value>(
                    id,
                    Err(invalid_params(format!(
                        "no session `{}` is kept in `{}`",
                        request.session_id,
                        workspace.display()
                    ))),
                )
            }
        };
        self.start_session(
            id,
            Start {
                session_id: request.session_id,
                workspace,
                history: Some(conversation),
                servers: request.mcp_servers,
                subagents: self.subagents,
            },
        );
    }

    /// The sessions kept in `cwd`, or — with none named — in every workspace
    /// an ACP session was recorded in.
    fn list_sessions(
        &mut self,
        params: Option<Value>,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        let request = decode::<acp::ListSessionsRequest>(params)?;
        let workspaces: Vec<PathBuf> = match request.cwd {
            Some(cwd) => vec![workspace_from(&cwd).map_err(invalid_params)?],
            None => {
                let mut known: Vec<PathBuf> = Vec::new();
                for record in crate::session::load_all(&self.factory.data_dir()) {
                    let place = PathBuf::from(&record.workspace);
                    if !record.incode_session.is_empty() && !known.contains(&place) {
                        known.push(place);
                    }
                }
                known
            }
        };
        let sessions = workspaces
            .iter()
            .flat_map(|workspace| history::list_in(workspace))
            .collect();
        Ok(acp::ListSessionsResponse::new(sessions))
    }

    /// Sessions whose thread has ended — a build that failed — no longer
    /// count against the limit.
    fn prune(&mut self) {
        self.sessions
            .retain(|_, handle| !handle.thread.is_finished());
    }

    /// Start a session's thread: it builds the session and answers `id`.
    fn start_session(&mut self, id: acp::RequestId, start: Start) {
        self.prune();
        if self.sessions.len() >= MAX_SESSIONS {
            return self.outbox.respond::<Value>(
                id,
                Err(acp::Error::invalid_request().data(Value::String(format!(
                    "this connection already holds {MAX_SESSIONS} sessions"
                )))),
            );
        }
        let session_id = start.session_id.clone();
        let shared = Arc::new(Shared::new());
        let (work, queue) = mpsc::sync_channel::<Work>(WORK_QUEUE);
        let thread = {
            let factory = Arc::clone(&self.factory);
            let outbox = Arc::clone(&self.outbox);
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                drive(factory.as_ref(), &outbox, id, start, &shared, &queue);
            })
        };
        self.sessions.insert(
            session_id.0.to_string(),
            SessionHandle {
                work,
                shared,
                thread,
            },
        );
    }

    /// Record the client's choice of agent or role, answered at once with
    /// every option as it now stands; the session's thread applies it before
    /// the next turn, or now when it is idle.
    fn set_config_option(
        &mut self,
        params: Option<Value>,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        let request = decode::<acp::SetSessionConfigOptionRequest>(params)?;
        let handle = self.sessions.get(&*request.session_id.0).ok_or_else(|| {
            invalid_params(format!(
                "no session `{}` on this connection",
                request.session_id
            ))
        })?;
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| invalid_params("every option this agent offers is a select"))?;
        let options = {
            let mut choices = handle.shared.choices();
            let choices = choices
                .as_mut()
                .ok_or_else(|| invalid_params("the session is still being built"))?;
            choices
                .choose(&request.config_id.0, &value.0)
                .map_err(invalid_params)?;
            choices.options()
        };
        // A wake-up only: when the queue is full, the next prompt applies it.
        let _ = handle.work.try_send(Work::Apply);
        Ok(acp::SetSessionConfigOptionResponse::new(options))
    }

    fn prompt(&mut self, id: acp::RequestId, params: Option<Value>) {
        let request = match decode::<acp::PromptRequest>(params) {
            Ok(request) => request,
            Err(e) => return self.outbox.respond::<Value>(id, Err(e)),
        };
        let Some(handle) = self.sessions.get(&*request.session_id.0) else {
            return self.outbox.respond::<Value>(
                id,
                Err(invalid_params(format!(
                    "no session `{}` on this connection",
                    request.session_id
                ))),
            );
        };
        if !handle.shared.begin() {
            return self.outbox.respond::<Value>(
                id,
                Err(acp::Error::invalid_request().data(Value::String(
                    "a prompt is already running in this session".to_string(),
                ))),
            );
        }
        let prompt = Prompt {
            id: id.clone(),
            blocks: request.prompt,
        };
        match handle.work.try_send(Work::Prompt(prompt)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                handle.shared.finish();
                self.outbox.respond::<Value>(
                    id,
                    Err(acp::Error::invalid_request().data(Value::String(
                        "the session is still applying earlier changes; send it again".to_string(),
                    ))),
                );
            }
            Err(TrySendError::Disconnected(_)) => {
                handle.shared.finish();
                self.outbox
                    .respond::<Value>(id, Err(internal("the session has ended".to_string())));
            }
        }
    }

    fn set_mode(
        &mut self,
        params: Option<Value>,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        let request = decode::<acp::SetSessionModeRequest>(params)?;
        let handle = self.sessions.get(&*request.session_id.0).ok_or_else(|| {
            invalid_params(format!(
                "no session `{}` on this connection",
                request.session_id
            ))
        })?;
        let mode = MODES
            .into_iter()
            .find(|mode| mode.as_str() == &*request.mode_id.0)
            .ok_or_else(|| {
                invalid_params(format!(
                    "`{}` is not a mode this agent offers: {}",
                    request.mode_id,
                    MODES.map(PermissionMode::as_str).join(", ")
                ))
            })?;
        handle.shared.state().mode = Some(mode);
        Ok(acp::SetSessionModeResponse::new())
    }

    /// End one session: cancel its turn, let its thread go, free its slot.
    ///
    /// The only way a slot comes back on a long-lived connection — without
    /// it, the [`MAX_SESSIONS`]th `session/new` would be refused for good.
    fn close(&mut self, params: Option<Value>) -> Result<acp::CloseSessionResponse, acp::Error> {
        let request = decode::<acp::CloseSessionRequest>(params)?;
        let handle = self
            .sessions
            .remove(&*request.session_id.0)
            .ok_or_else(|| {
                invalid_params(format!(
                    "no session `{}` on this connection",
                    request.session_id
                ))
            })?;
        handle.shared.cancel_running();
        // Dropping the sender ends the thread once its turn, if any, has
        // answered `cancelled`. It is not joined: the reader never waits on a
        // turn.
        drop(handle.work);
        Ok(acp::CloseSessionResponse::new())
    }

    /// The client has gone: stop every turn, and give them a moment to stop.
    fn shutdown(self) {
        // Nothing the client was asked will be answered now; a turn waiting on
        // an answer stops waiting and refuses the call.
        self.outbox.abandon_all();
        let mut threads = Vec::new();
        for (_, handle) in self.sessions {
            handle.shared.cancel_running();
            // Dropping the sender ends the thread's wait for the next prompt.
            drop(handle.work);
            threads.push(handle.thread);
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE + crate::hooks::engine::SESSION_END_BOUND;
        while threads.iter().any(|t| !t.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(SHUTDOWN_POLL);
        }
        for thread in threads {
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

/// What a session's thread starts from.
struct Start {
    session_id: acp::SessionId,
    /// The `cwd` the client named.
    workspace: PathBuf,
    /// The conversation to replay, for `session/load`.
    history: Option<Vec<history::Said>>,
    /// The MCP servers the client named.
    servers: Vec<acp::McpServer>,
    /// The client follows workers as sub-agents.
    subagents: bool,
}

/// The client's MCP servers as incode connects them: stdio and HTTP, not SSE
/// (and `initialize` says so). What is left out is said.
fn server_specs(servers: &[acp::McpServer]) -> (Vec<crate::toolbox::ServerSpec>, Vec<String>) {
    use crate::toolbox::{ServerSpec, Transport};
    let mut specs = Vec::new();
    let mut said = Vec::new();
    for server in servers {
        match server {
            acp::McpServer::Stdio(stdio) => specs.push(ServerSpec {
                name: stdio.name.clone(),
                transport: Transport::Stdio {
                    command: stdio.command.clone(),
                    args: stdio.args.clone(),
                    env: stdio
                        .env
                        .iter()
                        .map(|var| (var.name.clone(), var.value.clone()))
                        .collect(),
                },
            }),
            // The headers go as given; they are never written anywhere else.
            acp::McpServer::Http(http) => specs.push(ServerSpec {
                name: http.name.clone(),
                transport: Transport::Http {
                    url: http.url.clone(),
                    headers: http
                        .headers
                        .iter()
                        .map(|header| (header.name.clone(), header.value.clone()))
                        .collect(),
                },
            }),
            acp::McpServer::Sse(sse) => said.push(format!(
                "MCP server `{}` was not started: SSE servers are not supported \
                 (`mcpCapabilities.sse` is false)",
                sse.name
            )),
            _ => said.push(
                "an MCP server of a kind this agent does not support was not started".to_string(),
            ),
        }
    }
    (specs, said)
}

/// What the client's MCP servers said while it could not be told at once.
#[derive(Default)]
struct Later {
    /// Whether the session has been answered, so the client knows its id.
    open: bool,
    lines: Vec<String>,
}

/// Where what became of each client MCP server goes, once it has connected or
/// been dropped: always the log, and the client as an agent message — at once
/// when the session is answered and no prompt is running, else into `later`,
/// which the session's thread says between turns, so it never lands inside
/// the model's own text or ahead of the session's own answer.
fn connected_later(
    outbox: &Arc<Outbox>,
    session_id: &acp::SessionId,
    shared: &Arc<Shared>,
    later: &Arc<Mutex<Later>>,
) -> crate::toolbox::OnConnected {
    let outbox = Arc::clone(outbox);
    let session_id = session_id.clone();
    let shared = Arc::clone(shared);
    let later = Arc::clone(later);
    Arc::new(move |line: String| {
        eprintln!("incode acp: {line}");
        // Decided and sent under the turn lock and the queue's, so neither a
        // prompt nor the session's thread can move between the look and the
        // message.
        let state = shared.state();
        let mut held = later
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.running || !held.open {
            held.lines.push(line);
        } else {
            outbox.notify(
                acp::CLIENT_METHOD_NAMES.session_update,
                acp::SessionNotification::new(
                    session_id.clone(),
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::from(format!("{line}\n")),
                    )),
                ),
            );
        }
    })
}

/// What the model is told about the client's MCP servers still connecting
/// when a turn starts: their tools are not offered to it yet. `None` when
/// none is.
fn connecting_note(connecting: &[String]) -> Option<String> {
    if connecting.is_empty() {
        return None;
    }
    let names: Vec<String> = connecting.iter().map(|n| format!("`{n}`")).collect();
    Some(format!(
        "The client's MCP server(s) {} are still connecting: their tools (`mcp__<server>__...`) \
         are not offered this turn, and will be from the first turn after they answer. Do not \
         conclude those tools do not exist; say they are still connecting if the task needs them.",
        names.join(", ")
    ))
}

/// One session's thread: build it, answer `session/new` or `session/load`,
/// then run prompts until the reader lets go.
fn drive<F: SessionFactory>(
    factory: &F,
    outbox: &Arc<Outbox>,
    opened_by: acp::RequestId,
    start: Start,
    shared: &Arc<Shared>,
    work: &Receiver<Work>,
) {
    let Start {
        session_id,
        workspace,
        history,
        servers,
        subagents,
    } = start;
    let session_id = &session_id;
    // A panic is caught here and at each prompt below, never let through: the
    // request that was running is owed an answer, and an unwound thread would
    // leave the client waiting for it forever.
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        factory.open(
            &workspace,
            shared.cancel.clone(),
            &session_id.0,
            history.is_some(),
        )
    }))
    .unwrap_or_else(|_| Err("building it panicked".to_string()));
    let Opened {
        mut session,
        mode,
        settings,
        workspace,
        roles,
        build,
        runs,
    } = match opened {
        Ok(opened) => opened,
        Err(why) => {
            return outbox.respond::<Value>(
                opened_by,
                Err(internal(format!("the session could not be built: {why}"))),
            );
        }
    };
    // A mode that is not offered cannot be shown as current. It fails CLOSED,
    // like `PermissionMode::from_str_lossy`.
    let applied = if MODES.contains(&mode) {
        mode
    } else {
        session.set_permission_mode(PermissionMode::ReadOnly.agent_mode());
        PermissionMode::ReadOnly
    };
    // The person using this session is at the client: a tool call that needs
    // confirming is put to them there, as `session/request_permission`.
    session.set_prompter(Arc::new(permission::ClientPrompter::new(
        Arc::clone(outbox),
        session_id.clone(),
    )));
    // What the tools print while they run, what the gate decided, and the
    // workers they start — shown to the client as they happen.
    let watch = workers::SessionWatch::opened(outbox, session_id, subagents);
    session.set_watch(Arc::new(watch.clone()));
    // The client's MCP servers, started here as incode's own tools — every
    // call to them goes through the toolbox's gate. In the background:
    // `session/new` is answered without waiting for them (a slow server once
    // held it for 42 s), and each one's tools are offered from the first turn
    // after it has connected. Bounded; one that does not start costs its own
    // tools and nothing else.
    let (mut specs, mut connected) = server_specs(&servers);
    // The owner's installed external tools join the same way, in the same
    // call: a session's servers are connected once.
    let (tools, about_tools) = factory.tool_servers();
    specs.extend(tools);
    connected.extend(about_tools);
    // `.mcp.json` and `~/.incode/mcp.json`. A project's server nobody has
    // approved yet is asked about once the session is answered, below.
    let (from_files, about_files, unapproved) =
        crate::mcp_config::session_servers_asking_later(&workspace, &factory.data_dir(), None);
    specs.extend(from_files);
    connected.extend(about_files);
    let later: Arc<Mutex<Later>> = Arc::default();
    let on_connected = connected_later(outbox, session_id, shared, &later);
    connected.extend(session.session_tools().connect_client_servers(
        specs,
        &workspace,
        &on_connected,
    ));
    for line in &connected {
        eprintln!("incode acp: {line}");
    }
    let choices = config::opening(&settings, &roles, session.model());
    *shared.choices() = Some(choices.clone());
    let mut held = Held {
        session,
        applied,
        settings,
        workspace,
        roles,
        build,
        runs,
        lead: choices.agent.clone(),
        role: choices.role.clone(),
        shown: choices.options(),
        replay: None,
        watch,
    };
    let say = |update: acp::SessionUpdate| {
        outbox.notify(
            acp::CLIENT_METHOD_NAMES.session_update,
            acp::SessionNotification::new(session_id.clone(), update),
        );
    };
    match &history {
        // The history first, then the answer, as `session/load` requires.
        Some(conversation) => {
            history::resume_or_carry(&mut held, session_id, conversation);
            for update in history::replay(conversation) {
                say(update);
            }
            outbox.respond(
                opened_by,
                Ok(acp::LoadSessionResponse::new()
                    .modes(mode_state(applied))
                    .config_options(held.shown.clone())),
            );
        }
        None => outbox.respond(
            opened_by,
            Ok(acp::NewSessionResponse::new(session_id.clone())
                .modes(mode_state(applied))
                .config_options(held.shown.clone())),
        ),
    }
    // After the answer, so the client knows the session the list belongs to.
    say(acp::SessionUpdate::AvailableCommandsUpdate(
        acp::AvailableCommandsUpdate::new(commands::available()),
    ));
    // After the answer too: the client is asked about the session it knows.
    // Not waited for — each one allowed connects in the background.
    permission::ServerApprovals {
        outbox: Arc::clone(outbox),
        session: session_id.clone(),
        workspace: held.workspace.clone(),
        data_dir: factory.data_dir(),
        late: held.session.session_tools().late_servers(),
        said: on_connected,
    }
    .ask_in_background(unapproved);
    if !connected.is_empty() {
        say(acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::from(format!(
                "{}\n",
                connected.join("\n")
            ))),
        ));
    }

    // What a server that finished connecting before the answer, or during a
    // turn, had to say — said here, between turns, and never inside the
    // model's own text. The first call opens the session to being told at
    // once.
    let say_later = || {
        let mut held = later
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.open = true;
        if !held.lines.is_empty() {
            say(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::from(format!(
                    "{}\n",
                    held.lines.join("\n")
                ))),
            ));
            held.lines.clear();
        }
    };
    say_later();
    while let Ok(next) = work.recv() {
        say_later();
        let prompt = match next {
            Work::Prompt(prompt) => prompt,
            Work::Apply => {
                let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    config::apply(&mut held, shared, outbox, session_id);
                }));
                if applied.is_err() {
                    // As after a panicked turn: the session's state is unknown,
                    // so it ends, and later prompts are refused as ended.
                    eprintln!("incode acp: applying the client's choices panicked");
                    return;
                }
                continue;
            }
        };
        let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_prompt(&mut held, outbox, session_id, shared, &prompt.blocks)
        }));
        let Ok(answer) = answer else {
            shared.finish();
            // The session's state is unknown after a panic, so it is not
            // offered another turn: this prompt is answered, and the thread
            // ends — later prompts to it are refused as a session that ended.
            outbox.respond::<Value>(
                prompt.id,
                Err(internal(
                    "the turn panicked; this session has ended".to_string(),
                )),
            );
            return;
        };
        // Released BEFORE the answer goes out, so a client that sends its next
        // prompt the moment it reads this one's reply is not refused as busy.
        shared.finish();
        outbox.respond(prompt.id, answer);
        say_later();
    }
    // The reader let go: `session/close`, or the client went away. Either way
    // the session is over, and the memory engine is told — on this thread,
    // which owns the session, bounded by the engine runner.
    held.session.end_memory("other");
}

/// One open session, as its thread holds it.
struct Held {
    session: AgentSession,
    /// The permission mode the session is running under now.
    applied: PermissionMode,
    settings: commands::Settings,
    /// The session's own workspace: absolute and canonical.
    workspace: PathBuf,
    /// The role table the role picker routes by.
    roles: crate::roles::RoleTable,
    /// Builds the backend for an agent the client picks.
    build: BuildAgent,
    /// How `/run` works the ledger.
    runs: Box<dyn LedgerRuns>,
    /// The agent the session's backend is now, by catalogue id.
    lead: String,
    /// The role the session's model was last routed by.
    role: String,
    /// The pickers as the client was last told them.
    shown: Vec<acp::SessionConfigOption>,
    /// A loaded session's conversation, for the next turn, when the model's
    /// own could not be resumed.
    replay: Option<String>,
    /// The session's watch: its turns' steps are shown through it.
    watch: workers::SessionWatch,
}

/// One prompt: the same turn an interactive line runs, streamed as updates —
/// or, when the prompt is a slash command, that command.
fn run_prompt(
    held: &mut Held,
    outbox: &Arc<Outbox>,
    session_id: &acp::SessionId,
    shared: &Shared,
    blocks: &[acp::ContentBlock],
) -> Result<acp::PromptResponse, acp::Error> {
    let workspace = held.workspace.clone();
    let workspace = workspace.as_path();
    let present::Prompted { text, images } =
        present::prompt_parts(blocks, workspace).map_err(invalid_params)?;
    if text.trim().is_empty() {
        return Err(invalid_params("the prompt has no text"));
    }
    // Pictures go to the model with a question; a command runs here and has
    // nowhere to put them.
    if !images.is_empty() && crate::cli::slash_commands::SlashCommand::parse(text.trim()).is_some()
    {
        return Err(invalid_params(
            "pictures go with a prompt to the model, not with a slash command",
        ));
    }

    // The mode the client chose, sent to the thing that does the writing —
    // as `-p` is at startup. Before the turn, because a running agent's mode
    // was fixed when it was started.
    let wanted = shared.state().mode;
    if let Some(wanted) = wanted {
        if wanted != held.applied {
            held.session.set_permission_mode(wanted.agent_mode());
            held.applied = wanted;
        }
    }
    // And the agent and role the client picked, for the same reason.
    config::apply(held, shared, outbox, session_id);

    if shared.cancelled() {
        return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
    }

    // `/run` works the ledger for as long as it takes, streaming, with the
    // ledger as the plan, and its lanes as sub-agents.
    if matches!(
        crate::cli::slash_commands::SlashCommand::parse(text.trim()),
        Some(crate::cli::slash_commands::SlashCommand::Run)
    ) {
        return Ok(acp::PromptResponse::new(run::run(
            held.runs.as_mut(),
            workspace,
            held.applied,
            outbox,
            session_id,
            shared,
            &held.watch,
        )));
    }
    let session = &mut held.session;

    let say = |update: acp::SessionUpdate| {
        outbox.notify(
            acp::CLIENT_METHOD_NAMES.session_update,
            acp::SessionNotification::new(session_id.clone(), update),
        );
    };
    let say_text = |text: &str| {
        if !text.is_empty() {
            say(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::from(text)),
            ));
        }
    };

    // A slash command is run here, as the REPL runs it, and never reaches the
    // agent. It runs to the end: a cancel that lands meanwhile decides the
    // answer, as it does for a turn.
    if let Some(ran) = commands::run(&text, session, workspace, &held.settings) {
        say_text(&ran.said);
        if let Some(mode) = ran.mode {
            // The command switched the mode; the client's picker, and the
            // mode the next prompt asks for, follow it.
            held.applied = mode;
            shared.state().mode = Some(mode);
            say(acp::SessionUpdate::CurrentModeUpdate(
                acp::CurrentModeUpdate::new(mode.as_str()),
            ));
        }
        return Ok(acp::PromptResponse::new(if shared.cancelled() {
            acp::StopReason::Cancelled
        } else {
            acp::StopReason::EndTurn
        }));
    }

    // The pictures go to the agent that leads now, before anything else is
    // done with the prompt: an agent that cannot take them refuses the
    // prompt whole, before the engine or the transcript have heard it.
    let pictures = !images.is_empty();
    if pictures {
        session.attach_images(images).map_err(invalid_params)?;
    }
    let (instruction, mut context) = match crate::cli::plan_turn(session, workspace, &text, &text) {
        crate::cli::TurnPlan::NextQuestion(reply) | crate::cli::TurnPlan::Greeting(reply) => {
            if pictures {
                // No turn runs: the pictures must not ride the next one.
                let _ = session.attach_images(Vec::new());
            }
            say_text(&reply);
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        crate::cli::TurnPlan::Agent {
            instruction,
            context,
        } => (instruction, context),
    };
    // A loaded session whose model could not resume its own conversation is
    // given it here, once, ahead of everything else the turn carries.
    if let Some(earlier) = held.replay.take() {
        context.insert(0, earlier);
    }
    // A turn does not wait for the client's MCP servers still connecting: it
    // runs without their tools, and the model is told so, by name.
    if let Some(note) = connecting_note(&session.session_tools().client_servers_connecting()) {
        context.push(note);
    }

    // Looked at again AFTER the plan, which can wait on the memory service:
    // a cancel that landed during that wait is honoured here, before an agent
    // is started, rather than being cleared when the turn resets its token.
    if shared.cancelled() {
        if pictures {
            let _ = session.attach_images(Vec::new());
        }
        return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
    }

    // Who serves this turn, for every step's `_meta` and the answer's.
    let who = workers::Who {
        role: Some(held.role.clone()),
        agent: Some(held.lead.clone()),
        model: session.model().map(str::to_string),
        lane: None,
    };
    held.watch.serve_as(who.clone());
    let mut presenter = held.watch.presenter(workspace);
    let outcome = session.turn_observed(
        workspace,
        &instruction,
        &context,
        &mut |fragment: &str| {
            shared.hold_cancel();
            say_text(fragment);
        },
        &mut |_status| {},
        Some(&mut |event: &AgentEvent, start: &TurnStart<'_>| {
            shared.hold_cancel();
            for update in presenter.updates(event, &|path| start.before(path)) {
                say(update);
            }
        }),
    );

    // What the session holds for the person once the turn is over — a
    // question the agent asked, a guard the sentinel caught. The REPL prints
    // these after the status line stops; here they follow the reply.
    for notice in session.take_notices() {
        say_text(&format!("\n{notice}\n"));
    }
    // Which conversation the model holds now, so the session can be loaded
    // and resumed; and the pickers, when the turn changed what serves the
    // session (a standby took over) or what model it asks for.
    history::remember(held, session_id, &text);
    config::report(held, shared, outbox, session_id);

    // A cancel decides the answer whatever the turn did with it: ACP requires
    // `cancelled` once the client has asked, even if stopping surfaced as an
    // error underneath.
    if shared.cancelled() {
        return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
    }
    match outcome {
        Ok(outcome) => Ok(
            acp::PromptResponse::new(present::stop_reason(&outcome.stop_reason))
                .meta(workers::prompt_meta(&who)),
        ),
        Err(why) => Err(internal(why)),
    }
}

fn initialize() -> acp::InitializeResponse {
    // One protocol version is spoken. The rule is to answer the client's
    // version when it is supported and the latest supported one otherwise;
    // with a single version both answers are this one.
    acp::InitializeResponse::new(ProtocolVersion::LATEST)
        .agent_capabilities(
            acp::AgentCapabilities::new()
                .load_session(true)
                // Pictures inside the toolbox's image bounds; whether the agent
                // leading the session can take them is decided per prompt.
                .prompt_capabilities(
                    acp::PromptCapabilities::new()
                        .embedded_context(true)
                        .image(true),
                )
                // Stdio servers — every client may send those — and HTTP ones
                // (MCP's Streamable HTTP), each run as incode's own tools. No
                // SSE: the older HTTP transport is not spoken.
                .mcp_capabilities(acp::McpCapabilities::new().http(true).sse(false))
                .session_capabilities(
                    acp::SessionCapabilities::new()
                        .close(acp::SessionCloseCapabilities::new())
                        .list(acp::SessionListCapabilities::new()),
                ),
        )
        .auth_methods(Vec::new())
        .agent_info(acp::Implementation::new("incode", env!("CARGO_PKG_VERSION")).title("incode"))
}

fn mode_state(current: PermissionMode) -> acp::SessionModeState {
    acp::SessionModeState::new(
        current.as_str(),
        MODES
            .into_iter()
            .map(|mode| {
                let (name, description) = match mode {
                    PermissionMode::ReadOnly => {
                        ("Read only", "Proposes changes without making them.")
                    }
                    PermissionMode::DangerFullAccess => (
                        "Full access",
                        "incode's full-access mode. The agent CLI runs as it does for \
                         workspace-write: edits accepted, the protected paths still enforced.",
                    ),
                    _ => (
                        "Workspace write",
                        "Edits files in the workspace, within the guards.",
                    ),
                };
                acp::SessionMode::new(mode.as_str(), name).description(description)
            })
            .collect(),
    )
}

/// The workspace a `session/new` names, when it is one this agent will use.
fn workspace_from(cwd: &Path) -> Result<PathBuf, String> {
    if !cwd.is_absolute() {
        return Err(format!(
            "`cwd` must be an absolute path, got `{}`",
            cwd.display()
        ));
    }
    if !cwd.is_dir() {
        return Err(format!(
            "`cwd` `{}` is not an existing directory",
            cwd.display()
        ));
    }
    Ok(cwd.to_path_buf())
}

fn decode<T: DeserializeOwned>(params: Option<Value>) -> Result<T, acp::Error> {
    serde_json::from_value(params.unwrap_or(Value::Null)).map_err(|e| invalid_params(e.to_string()))
}

fn invalid_params(why: impl Into<String>) -> acp::Error {
    acp::Error::invalid_params().data(Value::String(why.into()))
}

fn internal(why: String) -> acp::Error {
    acp::Error::internal_error().data(Value::String(why))
}

#[cfg(test)]
mod flow_tests;

#[cfg(test)]
mod worker_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentBackend, AgentError, TurnOutcome};
    use serde_json::json;
    use std::io::BufRead;

    /// How long a test waits for any one message before it fails. A hang is a
    /// failure, never a stuck suite.
    const WAIT: Duration = Duration::from_secs(20);

    /// How long the cancel fake waits for a cancel before finishing normally.
    /// Shorter than [`WAIT`], so a cancel that never lands shows up as the
    /// wrong stop reason rather than as a timeout.
    const CANCEL_FAKE_PATIENCE: Duration = Duration::from_secs(5);

    #[derive(Clone, Copy)]
    pub(super) enum Behaviour {
        /// Text, a tool call and its result, and a task list.
        Stream,
        /// Text, then nothing until cancelled.
        WaitForCancel,
        /// [`Behaviour::Stream`], after printing to stdout every way this
        /// crate can.
        Noisy,
        /// Real edits in the workspace, each written before its result is
        /// reported, as an agent CLI does — see [`edit_the_workspace`].
        Edits,
        /// One `Write` through incode's own Toolbox, with the fake engine in
        /// [`Script::engine`] asked first — see [`GUARDED_FILE`].
        Guarded,
        /// One call to the client's MCP server's `echo`, through incode's
        /// own Toolbox — see `crate::toolbox::fake_server`.
        CallMcp,
        /// Text naming the backend and its model — which agent served.
        Named,
        /// Workers, shell output, reasoning and the gate's decisions, through
        /// incode's own Toolbox — see `worker_tests::act`.
        SubAgent,
        SubAgentWaits,
        Shell,
        Thinks,
        Protected,
    }

    /// The file [`Behaviour::Guarded`] writes, when it is allowed to.
    const GUARDED_FILE: &str = "guarded.txt";

    #[derive(Clone)]
    pub(super) struct Script {
        behaviour: Behaviour,
        /// Every agent mode the session set on the backend, in order.
        pub(super) modes: Arc<Mutex<Vec<String>>>,
        /// The fake engine a [`Behaviour::Guarded`] toolbox asks.
        engine: Option<PathBuf>,
        /// Every turn run: `<backend name> <model or ->`, in order.
        pub(super) ran: Arc<Mutex<Vec<String>>>,
        /// Every conversation id a backend was told to continue.
        pub(super) resumed: Arc<Mutex<Vec<String>>>,
        /// Every picture a backend was handed, as its placeholder.
        pub(super) pictures: Arc<Mutex<Vec<String>>>,
        /// The stand-in agent CLI the `Agent` tool's sub-agent runs on.
        pub(super) sub_cli: Option<PathBuf>,
    }

    impl Script {
        pub(super) fn new(behaviour: Behaviour) -> Self {
            Self {
                behaviour,
                modes: Arc::default(),
                engine: None,
                ran: Arc::default(),
                resumed: Arc::default(),
                pictures: Arc::default(),
                sub_cli: None,
            }
        }

        pub(super) fn with_sub_cli(self, cli: PathBuf) -> Self {
            Self {
                sub_cli: Some(cli),
                ..self
            }
        }

        fn guarded(engine: PathBuf) -> Self {
            Self {
                engine: Some(engine),
                ..Self::new(Behaviour::Guarded)
            }
        }
    }

    struct FakeBackend {
        script: Script,
        cancel: Option<CancelToken>,
        workspace: PathBuf,
        /// incode's own tools, for [`Behaviour::Guarded`] and
        /// [`Behaviour::CallMcp`].
        tools: Option<crate::toolbox::Toolbox>,
        /// What this backend calls itself — the agent it stands for.
        name: String,
        /// The model the session asked for.
        model: Option<String>,
        /// The conversation it holds, once it has run a turn.
        conversation: Option<String>,
    }

    impl FakeBackend {
        fn new(script: &Script, workspace: &Path, name: &str) -> Self {
            Self {
                script: script.clone(),
                cancel: None,
                workspace: workspace.to_path_buf(),
                tools: None,
                name: name.to_string(),
                model: None,
                conversation: None,
            }
        }
    }

    fn ended(stop_reason: &str) -> TurnOutcome {
        TurnOutcome {
            session_id: "fake-session".to_string(),
            stop_reason: stop_reason.to_string(),
            is_error: false,
            detail: None,
            served_by: Vec::new(),
        }
    }

    fn stream(on_event: &mut dyn FnMut(&AgentEvent)) -> Result<TurnOutcome, AgentError> {
        on_event(&AgentEvent::TextDelta("Hel".to_string()));
        on_event(&AgentEvent::TextDelta("lo".to_string()));
        on_event(&AgentEvent::ToolUse {
            id: "t1".to_string(),
            name: "Read".to_string(),
            input: r#"{"file_path":"a.txt"}"#.to_string(),
        });
        on_event(&AgentEvent::ToolResult {
            id: "t1".to_string(),
            output: "the contents".to_string(),
            is_error: false,
        });
        on_event(&AgentEvent::ToolUse {
            id: "t2".to_string(),
            name: "TodoWrite".to_string(),
            input: r#"{"todos":[{"content":"read a.txt","status":"completed","priority":"high","activeForm":"reading"}]}"#
                .to_string(),
        });
        on_event(&AgentEvent::ToolResult {
            id: "t2".to_string(),
            output: "Task list saved: 1 task(s)".to_string(),
            is_error: false,
        });
        // Both numbers, as the agent reports them at the end of a turn.
        on_event(&AgentEvent::Usage {
            input_tokens: 2,
            output_tokens: 4,
            cache_creation_tokens: 164,
            cache_read_tokens: 38_663,
        });
        on_event(&AgentEvent::Context {
            used: 38_833,
            size: 1_000_000,
        });
        Ok(ended("end_turn"))
    }

    /// One tool call and its result, with `act` done in between — the order
    /// an agent CLI works in: the call is announced, the tool runs, the
    /// result follows.
    fn tool(
        on_event: &mut dyn FnMut(&AgentEvent),
        id: &str,
        name: &str,
        input: &Value,
        act: impl FnOnce() -> Result<(), String>,
    ) {
        on_event(&AgentEvent::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
        });
        let (output, is_error) = match act() {
            Ok(()) => (format!("{name} done"), false),
            Err(why) => (why, true),
        };
        on_event(&AgentEvent::ToolResult {
            id: id.to_string(),
            output,
            is_error,
        });
    }

    /// The edits [`Behaviour::Edits`] makes. The workspace holds `old.txt`
    /// (`before\n`) when the turn starts; its parent is scratch space the
    /// test owns.
    fn edit_the_workspace(
        workspace: &Path,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let hello = workspace.join("hello.txt");
        let write = |path: PathBuf, text: &'static str| {
            move || std::fs::write(&path, text).map_err(|e| e.to_string())
        };
        // A new file, named absolutely.
        tool(
            on_event,
            "w1",
            "Write",
            &json!({"file_path": hello, "content": "hi\n"}),
            write(hello.clone(), "hi\n"),
        );
        // A splice of it.
        tool(
            on_event,
            "w2",
            "Edit",
            &json!({"file_path": hello, "old_string": "hi", "new_string": "hello"}),
            write(hello.clone(), "hello\n"),
        );
        // A whole write over a file that was there before the turn, named
        // relative to the workspace.
        tool(
            on_event,
            "w3",
            "Write",
            &json!({"file_path": "old.txt", "content": "after\n"}),
            write(workspace.join("old.txt"), "after\n"),
        );
        // An edit that was refused: nothing changed.
        tool(
            on_event,
            "w4",
            "Edit",
            &json!({"file_path": hello, "old_string": "zzz", "new_string": "q"}),
            || Err("old_string not found".to_string()),
        );
        // A splice that landed OUTSIDE the workspace. A splice, because it
        // needs no "before" — so only the workspace check can keep it out.
        let outside = workspace
            .parent()
            .map(|parent| parent.join("outside.txt"))
            .ok_or_else(|| AgentError::new("the test workspace has no parent"))?;
        tool(
            on_event,
            "w5",
            "Edit",
            &json!({"file_path": outside, "old_string": "in", "new_string": "escaped"}),
            write(outside.clone(), "escaped\n"),
        );
        // A whole write to the file spliced above: its before is what the
        // splice left.
        tool(
            on_event,
            "w6",
            "Write",
            &json!({"file_path": hello, "content": "bye\n"}),
            write(hello, "bye\n"),
        );
        Ok(ended("end_turn"))
    }

    impl AgentBackend for FakeBackend {
        fn id(&self) -> &str {
            &self.name
        }

        fn run_turn(
            &mut self,
            prompt: &str,
            on_event: &mut dyn FnMut(&AgentEvent),
        ) -> Result<TurnOutcome, AgentError> {
            self.script
                .ran
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!(
                    "{} {} {}",
                    self.name,
                    self.model.as_deref().unwrap_or("-"),
                    prompt.len()
                ));
            self.conversation = Some(format!("conv-{}", self.name));
            match self.script.behaviour {
                Behaviour::Named => {
                    on_event(&AgentEvent::TextDelta(format!(
                        "served by {} on {}; asked: {prompt}",
                        self.name,
                        self.model.as_deref().unwrap_or("its default")
                    )));
                    Ok(ended("end_turn"))
                }
                Behaviour::SubAgent
                | Behaviour::SubAgentWaits
                | Behaviour::Shell
                | Behaviour::Thinks
                | Behaviour::Protected => {
                    super::worker_tests::act(self.script.behaviour, self.tools.as_ref(), on_event);
                    Ok(ended("end_turn"))
                }
                Behaviour::CallMcp => {
                    let tools = self
                        .tools
                        .as_ref()
                        .ok_or_else(|| AgentError::new("an MCP script needs its toolbox"))?;
                    let input = json!({"text": "from the model"});
                    on_event(&AgentEvent::ToolUse {
                        id: "m1".to_string(),
                        name: "mcp__fake__echo".to_string(),
                        input: input.to_string(),
                    });
                    let (output, is_error) = match tools.run_call("m1", "mcp__fake__echo", &input) {
                        Ok(said) => (said.shown(), false),
                        Err(why) => (why, true),
                    };
                    on_event(&AgentEvent::ToolResult {
                        id: "m1".to_string(),
                        output,
                        is_error,
                    });
                    Ok(ended("end_turn"))
                }
                Behaviour::Stream => stream(on_event),
                Behaviour::WaitForCancel => {
                    on_event(&AgentEvent::TextDelta("working".to_string()));
                    let deadline = Instant::now() + CANCEL_FAKE_PATIENCE;
                    while Instant::now() < deadline {
                        if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                            return Err(AgentError::new(crate::agent::claude_code::CANCELLED));
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(ended("end_turn"))
                }
                Behaviour::Noisy => {
                    println!("STRAY println from the backend");
                    let mut raw = std::io::stdout();
                    let _ =
                        raw.write_all(b"STRAY print from the backend, straight at the handle\n");
                    let _ = raw.flush();
                    crate::cli::render::print_info("STRAY render from the backend");
                    crate::outln!("STRAY outln from the backend");
                    // A child that inherits stdout, as a probe or a hook does.
                    #[cfg(windows)]
                    let child = std::process::Command::new("cmd")
                        .args(["/C", "echo STRAY child process"])
                        .status();
                    #[cfg(not(windows))]
                    let child = std::process::Command::new("sh")
                        .args(["-c", "echo STRAY child process"])
                        .status();
                    assert!(child.is_ok_and(|s| s.success()), "the noisy child must run");
                    stream(on_event)
                }
                Behaviour::Edits => edit_the_workspace(&self.workspace, on_event),
                Behaviour::Guarded => {
                    let tools = self
                        .tools
                        .as_ref()
                        .ok_or_else(|| AgentError::new("a guarded script needs its toolbox"))?;
                    let input = json!({"path": GUARDED_FILE, "content": "written\n"});
                    on_event(&AgentEvent::ToolUse {
                        id: "g1".to_string(),
                        name: crate::toolbox::WRITE.to_string(),
                        input: input.to_string(),
                    });
                    let (output, is_error) =
                        match tools.run_call("g1", crate::toolbox::WRITE, &input) {
                            Ok(said) => (said.shown(), false),
                            Err(why) => (why, true),
                        };
                    on_event(&AgentEvent::ToolResult {
                        id: "g1".to_string(),
                        output,
                        is_error,
                    });
                    Ok(ended("end_turn"))
                }
            }
        }

        fn session_id(&self) -> Option<&str> {
            self.conversation.as_deref()
        }

        fn set_session_id(&mut self, session_id: &str) {
            self.script
                .resumed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(session_id.to_string());
            self.conversation = Some(session_id.to_string());
        }

        fn set_models(&mut self, candidates: &[String]) {
            if let Some(first) = candidates.first() {
                self.model = Some(first.clone());
            }
        }

        fn model(&self) -> Option<&str> {
            self.model.as_deref()
        }

        fn share_session_tools(&mut self, state: Arc<crate::toolbox::SessionTools>) {
            if let Some(tools) = &mut self.tools {
                tools.share_session(state);
            }
        }

        fn set_cancel(&mut self, token: CancelToken) {
            if let Some(tools) = &mut self.tools {
                tools.set_cancel(token.clone());
            }
            self.cancel = Some(token);
        }

        fn set_prompter(&mut self, prompter: Arc<dyn crate::hooks::engine::Prompter>) {
            if let Some(tools) = &mut self.tools {
                tools.set_prompter(prompter);
            }
        }

        fn set_watch(&mut self, watch: Arc<dyn crate::watch::Watch>) {
            if let Some(tools) = &mut self.tools {
                tools.set_watch(watch);
            }
        }

        fn attach_images(&mut self, images: Vec<crate::toolbox::Image>) -> Result<(), String> {
            let mut seen = self
                .script
                .pictures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            seen.extend(images.iter().map(crate::toolbox::Image::placeholder));
            Ok(())
        }

        fn set_permission_mode(&mut self, mode: &str) {
            self.script
                .modes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(mode.to_string());
            // Read back into incode's own mode, as the bridged backend does.
            if let Some(tools) = &mut self.tools {
                tools.set_permission_mode(match mode {
                    "plan" => PermissionMode::ReadOnly,
                    "default" => PermissionMode::Prompt,
                    _ => PermissionMode::WorkspaceWrite,
                });
            }
        }
    }

    /// Builds sessions the way [`CliSessions`] does — an [`AgentSession`] with
    /// the cancel token and the mode applied — around a fake backend, so no
    /// real agent CLI ever runs.
    pub(super) struct FakeSessions {
        pub(super) script: Script,
        pub(super) mode: PermissionMode,
        /// Where `/agent` and `/roles` keep settings: a scratch directory,
        /// never the operator's own.
        pub(super) data_dir: PathBuf,
    }

    /// An agent in the fake catalogue, drivable.
    fn catalogued(id: &str, name: &str) -> DetectedAgent {
        DetectedAgent {
            id: id.to_string(),
            display_name: name.to_string(),
            command: id.to_string(),
            path: Some(format!("/bin/{id}")),
            version: None,
            protocol: crate::agent::Protocol::ClaudeStreamJson,
        }
    }

    /// The fake agent's toolbox: incode's own, over `workspace`, with the
    /// fake engine when the script names one.
    fn fake_tools(script: &Script, workspace: &Path) -> Option<crate::toolbox::Toolbox> {
        use Behaviour as B;
        let needs = script.engine.is_some()
            || matches!(
                script.behaviour,
                B::CallMcp | B::SubAgent | B::SubAgentWaits | B::Shell | B::Protected
            );
        needs.then(|| {
            let engine = script.engine.as_ref().map(|bin| {
                crate::hooks::engine::fake::engine(bin.clone()).capped(Duration::from_secs(30))
            });
            let mut tools = crate::toolbox::Toolbox::new(workspace, true).with_engine(engine);
            tools.set_session("acp-test-session");
            if let Some(cli) = &script.sub_cli {
                tools.run_subagents_on(&cli.to_string_lossy());
            }
            tools
        })
    }

    impl SessionFactory for FakeSessions {
        fn open(
            &self,
            workspace: &Path,
            cancel: CancelToken,
            session_id: &str,
            loading: bool,
        ) -> Result<Opened, String> {
            let mut lead = FakeBackend::new(&self.script, workspace, "claude-code");
            lead.tools = fake_tools(&self.script, workspace);
            let mut session = AgentSession::new(
                workspace.to_path_buf(),
                "claude-code",
                "claude".to_string(),
                &crate::guards::Guards::default(),
                Vec::new(),
            )
            .with_backend(Box::new(lead))
            .with_cancel(cancel);
            session.set_permission_mode(self.mode.agent_mode());
            session.start_memory(
                crate::hooks::memory::MemoryHooks::new(None, workspace, session_id),
                Vec::new(),
                if loading { "resume" } else { "startup" },
            );
            session.keep_transcript();
            let build = {
                let script = self.script.clone();
                let workspace = workspace.to_path_buf();
                Box::new(move |agent: &DetectedAgent| {
                    let mut backend = FakeBackend::new(&script, &workspace, &agent.id);
                    backend.tools = fake_tools(&script, &workspace);
                    Box::new(backend) as Box<dyn AgentBackend>
                })
            };
            Ok(Opened {
                session,
                mode: self.mode,
                settings: commands::Settings {
                    data_dir: self.data_dir.clone(),
                    catalogue: crate::agent::catalogue::Catalogue::of_local(vec![
                        catalogued("claude-code", "Claude Code"),
                        catalogued("codex", "Codex CLI"),
                    ]),
                    lead: catalogued("claude-code", "Claude Code"),
                },
                workspace: workspace.to_path_buf(),
                roles: crate::roles::RoleTable::default().with_role("explore", "small-model"),
                build,
                runs: Box::new(super::flow_tests::FakeRuns),
            })
        }

        fn data_dir(&self) -> PathBuf {
            self.data_dir.clone()
        }
    }

    /// Every line on the protocol channel must be a JSON-RPC 2.0 message: a
    /// response (id with result or error) or a notification or request
    /// (method). Anything else is what a client chokes on.
    fn assert_json_rpc(line: &str) -> Value {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("not JSON on the protocol channel ({e}): {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0", "not JSON-RPC 2.0: {line}");
        let is_response = value.get("id").is_some()
            && (value.get("result").is_some() || value.get("error").is_some());
        assert!(
            is_response || value.get("method").is_some(),
            "neither a response nor a message: {line}"
        );
        value
    }

    /// A client on the other end of a pipe pair, with the agent on a thread.
    pub(super) struct Client {
        to_agent: Option<std::io::PipeWriter>,
        from_agent: Receiver<String>,
        server: JoinHandle<()>,
        next_id: i64,
        /// The sessions' settings directory, removed with the client.
        _data_dir: tempfile::TempDir,
    }

    impl Client {
        pub(super) fn start(script: Script, mode: PermissionMode, max_line: usize) -> Self {
            let (agent_in, to_agent) = std::io::pipe().expect("pipe");
            let (from_agent_raw, agent_out) = std::io::pipe().expect("pipe");
            let data_dir = tempfile::tempdir().expect("data dir");
            let sessions = FakeSessions {
                script,
                mode,
                data_dir: data_dir.path().to_path_buf(),
            };
            let server = std::thread::spawn(move || {
                serve(agent_in, Box::new(agent_out), sessions, max_line);
            });
            let (lines, from_agent) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(from_agent_raw).lines() {
                    let Ok(line) = line else { break };
                    if lines.send(line).is_err() {
                        break;
                    }
                }
            });
            Self {
                to_agent: Some(to_agent),
                from_agent,
                server,
                next_id: 0,
                _data_dir: data_dir,
            }
        }

        pub(super) fn send_raw(&mut self, text: &str) {
            let pipe = self.to_agent.as_mut().expect("still open");
            pipe.write_all(text.as_bytes()).expect("write");
            pipe.write_all(b"\n").expect("write");
            pipe.flush().expect("flush");
        }

        pub(super) fn request(&mut self, method: &str, params: Value) -> i64 {
            self.next_id += 1;
            let id = self.next_id;
            self.send_raw(
                &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                    .to_string(),
            );
            id
        }

        pub(super) fn notify(&mut self, method: &str, params: Value) {
            self.send_raw(
                &json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string(),
            );
        }

        /// The next message, checked to be JSON-RPC.
        pub(super) fn next(&self) -> Value {
            let line = self
                .from_agent
                .recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("the agent said nothing for {WAIT:?}"));
            assert_json_rpc(&line)
        }

        /// Messages up to and including the response to `id`: the
        /// notifications that came first, then the response.
        pub(super) fn until_response(&self, id: i64) -> (Vec<Value>, Value) {
            let mut before = Vec::new();
            loop {
                let message = self.next();
                if message.get("method").is_none() && message["id"] == json!(id) {
                    return (before, message);
                }
                before.push(message);
            }
        }

        pub(super) fn call(&mut self, method: &str, params: Value) -> Value {
            let id = self.request(method, params);
            self.until_response(id).1
        }

        /// The sessions' settings directory: where approvals are kept.
        pub(super) fn data_dir(&self) -> &Path {
            self._data_dir.path()
        }

        pub(super) fn open_session(&mut self, cwd: &Path) -> String {
            let reply = self.call("session/new", json!({"cwd": cwd, "mcpServers": []}));
            reply["result"]["sessionId"]
                .as_str()
                .unwrap_or_else(|| panic!("no session id: {reply}"))
                .to_string()
        }

        /// Close the client's end and wait — bounded — for the agent to stop.
        pub(super) fn close(mut self) {
            drop(self.to_agent.take());
            let deadline = Instant::now() + WAIT;
            while !self.server.is_finished() {
                assert!(
                    Instant::now() < deadline,
                    "the agent did not stop at end of input"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    pub(super) fn updates_of(messages: &[Value]) -> Vec<&Value> {
        messages
            .iter()
            .filter(|m| m["method"] == "session/update")
            .map(|m| &m["params"]["update"])
            .collect()
    }

    pub(super) fn text_of(messages: &[Value]) -> String {
        updates_of(messages)
            .into_iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|u| u["content"]["text"].as_str())
            .collect()
    }

    pub(super) fn client(behaviour: Behaviour) -> Client {
        Client::start(
            Script::new(behaviour),
            PermissionMode::WorkspaceWrite,
            transport::MAX_LINE_BYTES,
        )
    }

    /// **THE HANDSHAKE.** Version 1, and only the capabilities incode really
    /// has: text and embedded context, loading and listing sessions, stdio
    /// and HTTP MCP servers (no SSE), no auth methods.
    #[test]
    fn initialize_negotiates_version_one_and_states_real_capabilities() {
        let mut client = client(Behaviour::Stream);
        let reply = client.call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        let result = &reply["result"];
        assert_eq!(result["protocolVersion"], 1, "{reply}");
        assert_eq!(result["agentCapabilities"]["loadSession"], true, "{reply}");
        assert_eq!(
            result["agentCapabilities"]["mcpCapabilities"]["http"], true,
            "{reply}"
        );
        assert_eq!(
            result["agentCapabilities"]["mcpCapabilities"]["sse"], false,
            "{reply}"
        );
        assert!(
            result["agentCapabilities"]["sessionCapabilities"]["list"].is_object(),
            "{reply}"
        );
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["embeddedContext"], true,
            "{reply}"
        );
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["image"], true,
            "{reply}"
        );
        assert_eq!(result["authMethods"], json!([]), "{reply}");
        assert_eq!(result["agentInfo"]["name"], "incode", "{reply}");

        // A client on a newer protocol is answered with the version spoken.
        let newer = client.call(
            "initialize",
            json!({"protocolVersion": 7, "clientCapabilities": {}}),
        );
        assert_eq!(newer["result"]["protocolVersion"], 1, "{newer}");

        // No method was offered, so none can be chosen.
        let auth = client.call("authenticate", json!({"methodId": "anything"}));
        assert_eq!(auth["error"]["code"], -32602, "{auth}");
        client.close();
    }

    /// **A BAD `cwd` IS REFUSED, NEVER BUILT.** Relative, missing, and a file
    /// that is not a directory: each is invalid params, and no session opens.
    #[test]
    fn session_new_refuses_a_cwd_that_is_not_an_absolute_directory() {
        let mut client = client(Behaviour::Stream);
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("a-file.txt");
        std::fs::write(&file, "x").expect("write");

        for cwd in [
            json!("relative/dir"),
            json!(dir.path().join("does-not-exist")),
            json!(file),
        ] {
            let reply = client.call("session/new", json!({"cwd": cwd, "mcpServers": []}));
            assert_eq!(
                reply["error"]["code"], -32602,
                "cwd {cwd} must be refused: {reply}"
            );
            assert!(reply.get("result").is_none(), "{reply}");
        }
        client.close();
    }

    /// **A PROMPT STREAMS, THEN ENDS WITH `end_turn`.** The text arrives as
    /// message chunks, the tool call and its result as structure, the task
    /// list as a plan — all before the reply that ends the prompt.
    #[test]
    fn a_prompt_streams_its_turn_and_ends_with_end_turn() {
        let mut client = client(Behaviour::Stream);
        let dir = tempfile::tempdir().expect("temp dir");
        let session = client.open_session(dir.path());

        let id = client.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "read a.txt and report"}]}),
        );
        let (streamed, reply) = client.until_response(id);

        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        // The turn ends its text on a line boundary, as it does in a terminal.
        assert_eq!(text_of(&streamed).trim_end(), "Hello", "{streamed:#?}");
        let updates = updates_of(&streamed);
        let call = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call" && u["toolCallId"] == "t1")
            .unwrap_or_else(|| panic!("no tool call: {updates:#?}"));
        assert_eq!(call["kind"], "read", "{call}");
        assert_eq!(call["status"], "in_progress", "{call}");
        let result = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == "t1")
            .unwrap_or_else(|| panic!("no tool result: {updates:#?}"));
        assert_eq!(result["status"], "completed", "{result}");
        let plan = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "plan")
            .unwrap_or_else(|| panic!("no plan: {updates:#?}"));
        assert_eq!(plan["entries"][0]["status"], "completed", "{plan}");
        // The priority the model gave, not a stand-in: the plan is incode's
        // own task list.
        assert_eq!(plan["entries"][0]["priority"], "high", "{plan}");
        client.close();
    }

    /// **CANCEL MID-PROMPT ENDS WITH `cancelled`, AND THE READER NEVER
    /// WAITED ON THE TURN.** While the prompt runs, an unrelated request is
    /// answered — the proof the reader is free — and then the cancel lands
    /// and the prompt answers `cancelled`, well before the fake would have
    /// finished on its own.
    #[test]
    fn cancel_mid_prompt_ends_with_cancelled() {
        let mut client = client(Behaviour::WaitForCancel);
        let dir = tempfile::tempdir().expect("temp dir");
        let session = client.open_session(dir.path());

        let prompt = client.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "do the long thing"}]}),
        );
        // The turn is running once its first words arrive.
        loop {
            let message = client.next();
            if message["params"]["update"]["content"]["text"] == "working" {
                break;
            }
        }
        let busy = client.call("session/fork", json!({}));
        assert_eq!(
            busy["error"]["code"], -32601,
            "answered during the turn: {busy}"
        );

        let started = Instant::now();
        client.notify("session/cancel", json!({"sessionId": session}));
        let (_, reply) = client.until_response(prompt);
        assert_eq!(reply["result"]["stopReason"], "cancelled", "{reply}");
        assert!(
            started.elapsed() < CANCEL_FAKE_PATIENCE,
            "the cancel must stop the turn, not wait it out: {:?}",
            started.elapsed()
        );
        client.close();
    }

    /// **`set_mode` CHANGES THE MODE THE AGENT RUNS UNDER.** The session opens
    /// in `workspace-write`; after `read-only` is chosen, the next turn's
    /// agent is set to its propose-only mode. An unknown mode is refused.
    #[test]
    fn set_mode_changes_the_permission_mode_of_the_next_turn() {
        let script = Script::new(Behaviour::Stream);
        let mut client = Client::start(
            script.clone(),
            PermissionMode::WorkspaceWrite,
            transport::MAX_LINE_BYTES,
        );
        let dir = tempfile::tempdir().expect("temp dir");
        let opened = client.call("session/new", json!({"cwd": dir.path(), "mcpServers": []}));
        let modes = &opened["result"]["modes"];
        assert_eq!(modes["currentModeId"], "workspace-write", "{opened}");
        let offered: Vec<&str> = modes["availableModes"]
            .as_array()
            .expect("modes")
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(
            offered,
            ["read-only", "workspace-write", "danger-full-access"]
        );
        let session = opened["result"]["sessionId"]
            .as_str()
            .expect("id")
            .to_string();

        let bogus = client.call(
            "session/set_mode",
            json!({"sessionId": session, "modeId": "yolo"}),
        );
        assert_eq!(bogus["error"]["code"], -32602, "{bogus}");

        let set = client.call(
            "session/set_mode",
            json!({"sessionId": session, "modeId": "read-only"}),
        );
        assert!(set.get("result").is_some(), "{set}");

        let reply = client.call(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "look around"}]}),
        );
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        let modes = script.modes.lock().expect("modes").clone();
        assert_eq!(
            modes.last().map(String::as_str),
            Some(PermissionMode::ReadOnly.agent_mode()),
            "the turn after set_mode must run read-only: {modes:?}"
        );
        client.close();
    }

    /// **A CLOSED SESSION GIVES ITS SLOT BACK.** Without `session/close` a
    /// long-lived connection would hit [`MAX_SESSIONS`] and be refused for
    /// good; with it, the limit is on sessions open at once.
    #[test]
    fn closing_sessions_frees_their_slots() {
        let mut client = client(Behaviour::Stream);
        let dir = tempfile::tempdir().expect("temp dir");
        for _ in 0..(MAX_SESSIONS + 2) {
            let session = client.open_session(dir.path());
            let closed = client.call("session/close", json!({"sessionId": session}));
            assert!(closed.get("result").is_some(), "{closed}");
            let gone = client.call(
                "session/prompt",
                json!({"sessionId": session, "prompt": [{"type": "text", "text": "hi there"}]}),
            );
            assert_eq!(
                gone["error"]["code"], -32602,
                "a closed session is gone: {gone}"
            );
        }
        client.close();
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let mut client = client(Behaviour::Stream);
        let reply = client.call("session/fork", json!({}));
        assert_eq!(reply["error"]["code"], -32601, "{reply}");
        // A notification nobody handles gets no answer at all — the next
        // thing back is the reply to the request after it.
        client.notify("something/unheard_of", json!({}));
        let after = client.call("initialize", json!({"protocolVersion": 1}));
        assert!(after.get("result").is_some(), "{after}");
        client.close();
    }

    #[test]
    fn malformed_json_is_a_parse_error_and_the_connection_survives() {
        let mut client = client(Behaviour::Stream);
        client.send_raw("{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": ");
        let reply = client.next();
        assert_eq!(reply["error"]["code"], -32700, "{reply}");
        assert_eq!(reply["id"], Value::Null, "{reply}");
        let after = client.call("initialize", json!({"protocolVersion": 1}));
        assert!(after.get("result").is_some(), "{after}");
        client.close();
    }

    /// **AN OVER-LONG LINE IS REFUSED UNREAD, AND THE NEXT ONE IS SERVED.**
    #[test]
    fn an_over_long_line_is_refused_and_the_connection_survives() {
        let mut client = Client::start(
            Script::new(Behaviour::Stream),
            PermissionMode::WorkspaceWrite,
            1024,
        );
        let long = json!({"jsonrpc": "2.0", "id": 99, "method": "initialize",
                          "params": {"protocolVersion": 1, "pad": "x".repeat(5000)}});
        client.send_raw(&long.to_string());
        let refused = client.next();
        assert_eq!(refused["error"]["code"], -32600, "{refused}");
        assert_eq!(
            refused["id"], 99,
            "answered under its own id, or the client waits forever: {refused}"
        );
        let after = client.call("initialize", json!({"protocolVersion": 1}));
        assert!(after.get("result").is_some(), "{after}");
        client.close();
    }

    pub(super) fn prompt_in(client: &mut Client, session: &str, text: &str) -> (Vec<Value>, Value) {
        let id = client.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        );
        client.until_response(id)
    }

    /// **CONTEXT FILL REACHES THE CLIENT AS `usage_update`, WITH NO COST.**
    ///
    /// The shape the desktop app reads: `used` and `size` as numbers. No cost
    /// is invented: the agent's token counts alone never make a
    /// `usage_update`, and nothing here puts a price on a subscription.
    #[test]
    fn the_context_window_reaches_the_client_as_usage_update() {
        let mut client = client(Behaviour::Stream);
        let dir = tempfile::tempdir().expect("temp dir");
        let session = client.open_session(dir.path());
        let (streamed, reply) = prompt_in(&mut client, &session, "read a.txt and report");
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        let usage: Vec<&Value> = updates_of(&streamed)
            .into_iter()
            .filter(|u| u["sessionUpdate"] == "usage_update")
            .collect();
        assert_eq!(usage.len(), 1, "{streamed:#?}");
        assert_eq!(usage[0]["used"], 38_833, "{}", usage[0]);
        assert_eq!(usage[0]["size"], 1_000_000, "{}", usage[0]);
        assert!(usage[0].get("cost").is_none(), "{}", usage[0]);
        client.close();
    }

    /// The `tool_call_update` for one call id.
    fn result_for<'a>(updates: &[&'a Value], id: &str) -> &'a Value {
        updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == id)
            .unwrap_or_else(|| panic!("no result for {id}: {updates:#?}"))
    }

    /// **AN EDIT THAT LANDED IS A DIFF; ANYTHING ELSE IS ITS OUTPUT.**
    ///
    /// Six edits, each written before its result arrives, as an agent CLI
    /// does. The diff names the file absolutely; a new file has no old text; a
    /// splice shows what it took out and put in; a whole write over a file
    /// that existed before the turn shows what the file held then, from the
    /// turn's snapshot; a second write to a spliced file shows what the
    /// splice left. A refused edit and a write outside the workspace are not
    /// diffs.
    #[test]
    fn an_edit_that_landed_is_sent_as_a_diff() {
        let mut client = client(Behaviour::Edits);
        let scratch = tempfile::tempdir().expect("scratch");
        let workspace = scratch.path().join("ws");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("old.txt"), "before\n").expect("old.txt");
        let session = client.open_session(&workspace);

        let (streamed, reply) = prompt_in(&mut client, &session, "make the edits please");
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        let updates = updates_of(&streamed);
        let diff_of = |id: &str| -> Value {
            let update = result_for(&updates, id);
            assert_eq!(update["status"], "completed", "{update}");
            let content = update["content"].as_array().expect("content");
            assert_eq!(content.len(), 1, "{update}");
            assert_eq!(content[0]["type"], "diff", "{update}");
            content[0].clone()
        };
        let hello = workspace.join("hello.txt").to_string_lossy().to_string();

        let created = diff_of("w1");
        assert_eq!(created["path"], hello, "{created}");
        assert!(
            created.get("oldText").is_none_or(Value::is_null),
            "a new file has no old text: {created}"
        );
        assert_eq!(created["newText"], "hi\n", "{created}");

        let spliced = diff_of("w2");
        assert_eq!(spliced["path"], hello, "{spliced}");
        assert_eq!(spliced["oldText"], "hi", "{spliced}");
        assert_eq!(spliced["newText"], "hello", "{spliced}");

        let overwritten = diff_of("w3");
        assert_eq!(
            overwritten["path"],
            workspace.join("old.txt").to_string_lossy().to_string(),
            "a relative name is sent absolute: {overwritten}"
        );
        assert_eq!(
            overwritten["oldText"], "before\n",
            "the before is the turn's snapshot: {overwritten}"
        );
        assert_eq!(overwritten["newText"], "after\n", "{overwritten}");

        let refused = result_for(&updates, "w4");
        assert_eq!(refused["status"], "failed", "{refused}");
        assert_eq!(refused["content"][0]["type"], "content", "{refused}");

        let escaped = result_for(&updates, "w5");
        assert_eq!(escaped["status"], "completed", "{escaped}");
        assert_eq!(
            escaped["content"][0]["type"], "content",
            "a path outside the workspace is never named in a diff: {escaped}"
        );

        let rewritten = diff_of("w6");
        assert_eq!(rewritten["oldText"], "hello\n", "{rewritten}");
        assert_eq!(rewritten["newText"], "bye\n", "{rewritten}");
        client.close();
    }

    pub(super) fn commands_announced(messages: &[Value]) -> Option<Vec<String>> {
        updates_of(messages)
            .into_iter()
            .find(|u| u["sessionUpdate"] == "available_commands_update")
            .map(|u| {
                u["availableCommands"]
                    .as_array()
                    .expect("list")
                    .iter()
                    .filter_map(|c| c["name"].as_str().map(str::to_string))
                    .collect()
            })
    }

    /// **`session/new` IS FOLLOWED BY THE COMMANDS OFFERED**, in the shape the
    /// desktop app reads — `/run` among them.
    #[test]
    fn a_new_session_announces_its_commands() {
        let mut client = client(Behaviour::Stream);
        let dir = tempfile::tempdir().expect("temp dir");
        let session = client.open_session(dir.path());
        let announced = client.next();
        assert_eq!(announced["params"]["sessionId"], session, "{announced}");
        let names = commands_announced(std::slice::from_ref(&announced)).expect("a command list");
        assert_eq!(
            names,
            [
                "undo", "rewind", "plan", "act", "diff", "links", "roadmap", "run", "doctor",
                "memories", "remember", "worktree", "agent", "agents", "roles", "tools"
            ],
            "{announced}"
        );
        client.close();
    }

    /// **A SLASH COMMAND RUNS ITS HANDLER AND NEVER REACHES THE AGENT.**
    ///
    /// What the handler printed is the reply; the fake agent's words and tool
    /// calls never appear. A command that is not offered is not run either.
    #[test]
    fn a_slash_command_runs_here_and_never_reaches_the_agent() {
        let mut client = client(Behaviour::Stream);
        let dir = tempfile::tempdir().expect("temp dir");
        let session = client.open_session(dir.path());

        for (typed, expected) in [
            ("/remember", "Usage: /remember <what to remember>"),
            ("/roles", "Roles (candidates"),
            ("/bg echo hi", "/bg is not a command offered here"),
        ] {
            let (streamed, reply) = prompt_in(&mut client, &session, typed);
            assert_eq!(
                reply["result"]["stopReason"], "end_turn",
                "{typed}: {reply}"
            );
            let said = text_of(&streamed);
            assert!(said.contains(expected), "{typed} said: {said:?}");
            assert!(
                !said.contains("Hello"),
                "{typed} reached the agent: {said:?}"
            );
            assert!(
                !updates_of(&streamed)
                    .iter()
                    .any(|u| u["sessionUpdate"] == "tool_call"),
                "{typed} ran a turn: {streamed:#?}"
            );
        }
        client.close();
    }

    /// **`/plan` AND `/act` SWITCH THE MODE, AND THE CLIENT IS TOLD.** The
    /// next turn runs in the mode the command chose, and the client's mode
    /// picker follows through `current_mode_update`.
    #[test]
    fn plan_and_act_switch_the_mode_and_say_so() {
        let script = Script::new(Behaviour::Stream);
        let mut client = Client::start(
            script.clone(),
            PermissionMode::WorkspaceWrite,
            transport::MAX_LINE_BYTES,
        );
        let dir = tempfile::tempdir().expect("temp dir");
        let session = client.open_session(dir.path());

        let (streamed, _) = prompt_in(&mut client, &session, "/plan");
        let switched = updates_of(&streamed)
            .into_iter()
            .find(|u| u["sessionUpdate"] == "current_mode_update")
            .unwrap_or_else(|| panic!("no mode update: {streamed:#?}"))
            .clone();
        assert_eq!(switched["currentModeId"], "read-only", "{switched}");
        let _ = prompt_in(&mut client, &session, "look around the code");
        assert_eq!(
            script
                .modes
                .lock()
                .expect("modes")
                .last()
                .map(String::as_str),
            Some(PermissionMode::ReadOnly.agent_mode()),
        );

        let (streamed, _) = prompt_in(&mut client, &session, "/act");
        assert!(
            updates_of(&streamed)
                .iter()
                .any(|u| u["sessionUpdate"] == "current_mode_update"
                    && u["currentModeId"] == "workspace-write"),
            "{streamed:#?}"
        );
        client.close();
    }

    /// **`/undo` PUTS THE LAST TURN'S FILES BACK**, through the REPL's own
    /// handler. (A file the turn CREATED stays: the pre-turn snapshot holds
    /// the files that existed, so the REPL's undo leaves it too.)
    #[test]
    fn undo_puts_the_last_turns_files_back() {
        let mut client = client(Behaviour::Edits);
        let scratch = tempfile::tempdir().expect("scratch");
        let workspace = scratch.path().join("ws");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("old.txt"), "before\n").expect("old.txt");
        let session = client.open_session(&workspace);
        let _ = prompt_in(&mut client, &session, "make the edits please");
        assert!(workspace.join("hello.txt").exists());

        let (streamed, reply) = prompt_in(&mut client, &session, "/undo");
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        let said = text_of(&streamed);
        assert!(said.contains("Undid"), "{said:?}");
        assert!(said.contains("restored old.txt"), "{said:?}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("old.txt")).expect("old.txt"),
            "before\n"
        );
        client.close();
    }

    /// A session whose one `Write` the fake engine says needs a person, and
    /// whose owner's app gives no answer — so the client decides it.
    fn guarded_client(engine: &Path) -> Client {
        use crate::hooks::engine::fake::{self, Reply};
        let bin = fake::write(
            engine,
            &[
                (
                    "pre-tool",
                    Reply::exit(0).stdout(
                        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"writing here needs the owner"}}"#,
                    ),
                ),
                ("permission-request", Reply::exit(0)),
            ],
        );
        Client::start(
            Script::guarded(bin),
            PermissionMode::WorkspaceWrite,
            transport::MAX_LINE_BYTES,
        )
    }

    /// Run the guarded prompt, answer the client's permission request with
    /// `option`, and return the request, the updates and the prompt's reply.
    fn answer_the_permission_request(
        client: &mut Client,
        session: &str,
        option: &str,
    ) -> (Value, Vec<Value>, Value) {
        let prompt = client.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "write the file"}]}),
        );
        let asked = loop {
            let message = client.next();
            if message["method"] == "session/request_permission" {
                break message;
            }
        };
        client.send_raw(
            &json!({"jsonrpc": "2.0", "id": asked["id"],
                    "result": {"outcome": {"outcome": "selected", "optionId": option}}})
            .to_string(),
        );
        let (streamed, reply) = client.until_response(prompt);
        (asked, streamed, reply)
    }

    /// **A CALL THE ENGINE HOLDS FOR A PERSON IS PUT TO THE CLIENT — ALLOW.**
    ///
    /// The request is the shape the owner's app answers: the session, the
    /// call by its own id with a title naming the reason, and an allow-once and
    /// a reject-once option. Allowed there, the write happens.
    #[test]
    fn a_held_call_is_asked_of_the_client_and_runs_when_allowed() {
        let engine = tempfile::tempdir().expect("engine");
        let mut client = guarded_client(engine.path());
        let ws = tempfile::tempdir().expect("workspace");
        let session = client.open_session(ws.path());

        let (asked, streamed, reply) =
            answer_the_permission_request(&mut client, &session, "allow");
        let params = &asked["params"];
        assert_eq!(params["sessionId"], session, "{asked}");
        assert_eq!(params["toolCall"]["toolCallId"], "g1", "{asked}");
        assert!(
            params["toolCall"]["title"]
                .as_str()
                .is_some_and(|t| t.contains("writing here needs the owner")),
            "{asked}"
        );
        let kinds: Vec<&str> = params["options"]
            .as_array()
            .expect("options")
            .iter()
            .filter_map(|o| o["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["allow_once", "reject_once"], "{asked}");

        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join(GUARDED_FILE)).expect("written"),
            "written\n"
        );
        let result = result_for(&updates_of(&streamed), "g1").clone();
        assert_eq!(result["status"], "completed", "{result}");
        client.close();
    }

    /// **— AND REJECT.** Refused in the client, nothing is written and the
    /// call is reported failed.
    #[test]
    fn a_held_call_the_client_rejects_is_not_run() {
        let engine = tempfile::tempdir().expect("engine");
        let mut client = guarded_client(engine.path());
        let ws = tempfile::tempdir().expect("workspace");
        let session = client.open_session(ws.path());

        let (_, streamed, reply) = answer_the_permission_request(&mut client, &session, "reject");
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        assert!(
            !ws.path().join(GUARDED_FILE).exists(),
            "a rejected write was made"
        );
        let result = result_for(&updates_of(&streamed), "g1").clone();
        assert_eq!(result["status"], "failed", "{result}");
        assert!(
            result.to_string().contains("refused in the client"),
            "{result}"
        );
        client.close();
    }

    /// Set only in the child process [`the_protocol_channel_carries_nothing_else`]
    /// starts; it names the workspace the child serves.
    const PURITY_CHILD: &str = "INCODE_ACP_PURITY_CHILD";

    /// The child half of the stdout test: this process's REAL stdin and
    /// stdout, claimed exactly as `run_stdio` claims them, serving a backend
    /// that prints to stdout every way this crate can.
    ///
    /// Returns at once — passing — unless started by the test below, which
    /// sets [`PURITY_CHILD`]. It exits the process itself when the client
    /// closes stdin, so the test harness prints nothing after it.
    #[test]
    fn protocol_channel_child() {
        if std::env::var_os(PURITY_CHILD).is_none() {
            return;
        }
        let claimed = stdio::claim().expect("claim stdin/stdout");
        let data_dir = tempfile::tempdir().expect("data dir");
        serve(
            claimed.input,
            Box::new(claimed.output),
            FakeSessions {
                script: Script::new(Behaviour::Noisy),
                mode: PermissionMode::WorkspaceWrite,
                data_dir: data_dir.path().to_path_buf(),
            },
            transport::MAX_LINE_BYTES,
        );
        drop(data_dir);
        std::process::exit(0);
    }

    /// **STDOUT CARRIES THE PROTOCOL AND NOTHING ELSE.**
    ///
    /// A whole handshake and prompt, against a real process's real stdout,
    /// while the backend prints through `println!`, `print!`, the console's
    /// `render` and `outln!`, and a child process that inherits stdout. Every
    /// line on stdout must be a JSON-RPC message, and the stray text must turn
    /// up on stderr — which proves it was printed, and moved, rather than
    /// never printed at all.
    ///
    /// The one exception is the test harness's own banner (`running 1 test`),
    /// printed before the child's code starts; it is matched exactly, so
    /// nothing else can hide behind it.
    #[test]
    fn the_protocol_channel_carries_nothing_else() {
        use std::process::{Command, Stdio};

        let workspace = tempfile::tempdir().expect("workspace");
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "acp_agent::tests::protocol_channel_child",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .env(PURITY_CHILD, workspace.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the child");

        let (stdout_lines, from_child) = mpsc::channel::<String>();
        let child_out = child.stdout.take().expect("stdout");
        std::thread::spawn(move || {
            for line in BufReader::new(child_out).lines() {
                let Ok(line) = line else { break };
                if stdout_lines.send(line).is_err() {
                    break;
                }
            }
        });
        let child_err = child.stderr.take().expect("stderr");
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(child_err).read_to_string(&mut text);
            text
        });
        let mut to_child = child.stdin.take().expect("stdin");

        let mut seen: Vec<String> = Vec::new();
        let mut exchange = |request: Value| -> Value {
            let id = request["id"].clone();
            writeln!(to_child, "{request}").expect("write");
            to_child.flush().expect("flush");
            loop {
                let line = from_child.recv_timeout(WAIT).unwrap_or_else(|_| {
                    panic!("the child said nothing for {WAIT:?}; stdout so far: {seen:#?}")
                });
                seen.push(line.clone());
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if value["id"] == id && value.get("method").is_none() {
                        return value;
                    }
                }
            }
        };

        let init = exchange(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                   "params": {"protocolVersion": 1}}));
        assert!(init.get("result").is_some(), "{init}");
        let opened = exchange(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                                     "params": {"cwd": workspace.path(), "mcpServers": []}}));
        let session = opened["result"]["sessionId"]
            .as_str()
            .expect("session")
            .to_string();
        let prompt = exchange(
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                                     "params": {"sessionId": session,
                                                "prompt": [{"type": "text", "text": "read a.txt"}]}}),
        );
        assert_eq!(prompt["result"]["stopReason"], "end_turn", "{prompt}");
        // A slash command's handler writes for a terminal; here its words
        // must arrive as protocol messages, and nothing else.
        let command = exchange(
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt",
                   "params": {"sessionId": session,
                              "prompt": [{"type": "text", "text": "/links"}]}}),
        );
        assert_eq!(command["result"]["stopReason"], "end_turn", "{command}");
        drop(to_child);

        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("the child did not exit after its input closed");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "child exit: {status}");
        seen.extend(from_child.try_iter());
        let stderr = stderr.join().expect("stderr reader");

        let harness_banner = |line: &str| line.is_empty() || line == "running 1 test";
        let protocol: Vec<&String> = seen.iter().filter(|l| !harness_banner(l)).collect();
        assert!(protocol.len() >= 3, "three replies at least: {seen:#?}");
        for line in &protocol {
            assert_json_rpc(line);
        }
        assert!(
            protocol.iter().any(|l| l.contains("agent_message_chunk")),
            "the turn streamed on stdout: {seen:#?}"
        );
        assert!(
            protocol
                .iter()
                .any(|l| l.contains("agent_message_chunk") && l.contains("source file(s) indexed")),
            "the command's words went out as a message: {seen:#?}"
        );
        for stray in [
            "STRAY println",
            "STRAY print from",
            "STRAY render",
            "STRAY outln",
            "STRAY child process",
        ] {
            assert!(
                stderr.contains(stray),
                "`{stray}` was printed, so it must have gone to stderr:\n{stderr}"
            );
        }
    }
}
