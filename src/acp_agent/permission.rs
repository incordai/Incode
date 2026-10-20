//! Asking the ACP client — the owner's desktop app, or an editor — before a
//! tool call runs.
//!
//! When the memory engine wants a person to confirm a call, the person using
//! `incode --acp` is at the client. So the client is sent a
//! `session/request_permission`, in the shape it already answers for every ACP
//! agent: the call's id, a title, and two options, allow once and reject once.
//! The engine's own `hook permission-request` runs at the same time, and the
//! first answer wins (see [`crate::hooks::engine::Engine::settle`]).
//!
//! Questions (`AskUserQuestion`) are NOT sent here. The protocol has an
//! elicitation request, but the owner's app answers every agent request other
//! than `session/request_permission` with "method not supported", so a
//! question sent that way could never be answered. It is reported to the model
//! as unanswered instead, with its options.
//!
//! A project's MCP server not yet approved (`.mcp.json`, see
//! [`crate::mcp_config`]) is asked about the same way, once the session is
//! answered: allow once, allow always (remembered as `incode mcp approve`
//! would) or reject ([`ServerApprovals`]). An allowed server then connects in
//! the background, like the client's own.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol_schema::v1 as acp;
use serde_json::Value;

use super::present;
use super::transport::{Outbox, Reply};
use crate::hooks::engine::{Ask, Prompter, Verdict};

/// The option that allows the call.
const ALLOW: &str = "allow";
/// The option that refuses it.
const REJECT: &str = "reject";

/// How often the wait looks at whether the answer came from elsewhere.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// The client behind one ACP session.
pub(crate) struct ClientPrompter {
    outbox: Arc<Outbox>,
    session: acp::SessionId,
}

impl ClientPrompter {
    pub(crate) fn new(outbox: Arc<Outbox>, session: acp::SessionId) -> Self {
        Self { outbox, session }
    }
}

/// The request the client is sent for one call.
fn request_for(session: &acp::SessionId, ask: &Ask<'_>) -> acp::RequestPermissionRequest {
    let input = ask.tool_input.to_string();
    let id = if ask.call_id.is_empty() {
        "incode-confirm"
    } else {
        ask.call_id
    };
    let title = format!(
        "{} — {}",
        present::title_of(ask.tool_name, &input),
        ask.reason
    );
    let call = acp::ToolCallUpdate::new(
        id.to_string(),
        acp::ToolCallUpdateFields::new()
            .title(title)
            .kind(present::tool_kind(ask.tool_name))
            .status(acp::ToolCallStatus::Pending)
            .raw_input(ask.tool_input.clone()),
    );
    acp::RequestPermissionRequest::new(
        session.clone(),
        call,
        vec![
            acp::PermissionOption::new(ALLOW, "Allow", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new(REJECT, "Reject", acp::PermissionOptionKind::RejectOnce),
        ],
    )
}

/// What the client chose.
enum Chosen {
    /// The option with this id.
    Option(String),
    /// Nothing: it cancelled the question.
    Cancelled,
}

/// What the client's answer says. `None` when it is not an answer at all;
/// `about` names what was asked, for the log.
fn chosen_in(reply: Reply, about: &str) -> Option<Chosen> {
    let result: Value = match reply {
        Ok(result) => result,
        Err(error) => {
            eprintln!("incode acp: the client could not ask about {about}: {error:?}");
            return None;
        }
    };
    let answer = serde_json::from_value::<acp::RequestPermissionResponse>(result).ok()?;
    Some(match answer.outcome {
        acp::RequestPermissionOutcome::Selected(chosen) => {
            Chosen::Option(chosen.option_id.0.to_string())
        }
        _ => Chosen::Cancelled,
    })
}

/// What the client's answer means. `None` when it is not an answer at all.
fn verdict_of(reply: Reply) -> Option<Verdict> {
    // A client that cannot ask leaves it to the owner's app.
    Some(match chosen_in(reply, "a tool call")? {
        Chosen::Option(id) if id == ALLOW => Verdict::Allow,
        Chosen::Option(_) => Verdict::Deny("NOT RUN — refused in the client.".to_string()),
        // The client cancels a pending question when its turn is cancelled.
        Chosen::Cancelled => {
            Verdict::Deny("NOT RUN — the client cancelled the question.".to_string())
        }
    })
}

/// Send `request` and wait for the client's answer, until `settled` says it
/// is no longer wanted. `None` then, or when the client has gone.
fn ask_client(
    outbox: &Outbox,
    request: acp::RequestPermissionRequest,
    settled: &dyn Fn() -> bool,
) -> Option<Reply> {
    let (id, answered) =
        outbox.request(acp::CLIENT_METHOD_NAMES.session_request_permission, request);
    loop {
        match answered.recv_timeout(LOOK_EVERY) {
            Ok(reply) => return Some(reply),
            Err(RecvTimeoutError::Timeout) => {
                if settled() {
                    // The client's card stays until it is answered or the turn
                    // ends; its answer is then dropped.
                    outbox.forget(&id);
                    return None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

impl Prompter for ClientPrompter {
    fn confirm(&self, ask: &Ask<'_>, settled: &dyn Fn() -> bool) -> Option<Verdict> {
        // `settled`: answered in the owner's app, or the turn stopped.
        let reply = ask_client(&self.outbox, request_for(&self.session, ask), settled)?;
        verdict_of(reply)
    }

    fn choose(&self, _questions: &[crate::ask::Question]) -> Option<Vec<crate::ask::Answer>> {
        None
    }
}

// ---------------------------------------------------------------------------
// A project's MCP servers
// ---------------------------------------------------------------------------

/// The option that allows a project's server from now on, as `incode mcp
/// approve` would.
const ALWAYS: &str = "allow-always";

/// How long one project server waits for the client's answer before it is
/// skipped as if the client could not ask.
const SERVER_ASK_BOUND: Duration = Duration::from_secs(10 * 60);

/// What the client said about one project server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerAnswer {
    Once,
    Always,
    Reject,
}

/// The request the client is sent for a project server not yet approved: the
/// same shape as a tool call's, with an allow-always option too.
fn server_request(
    session: &acp::SessionId,
    server: &crate::mcp_config::Configured,
) -> acp::RequestPermissionRequest {
    let call = acp::ToolCallUpdate::new(
        format!("incode-mcp-{}", server.name),
        acp::ToolCallUpdateFields::new()
            .title(format!(
                "Start MCP server `{}` from {} — {}",
                server.name,
                crate::mcp_config::PROJECT_FILE,
                server.entry.summary()
            ))
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::Pending)
            .raw_input(serde_json::json!({
                "server": server.name,
                "file": crate::mcp_config::PROJECT_FILE,
                "starts": server.entry.summary(),
            })),
    );
    acp::RequestPermissionRequest::new(
        session.clone(),
        call,
        vec![
            acp::PermissionOption::new(ALLOW, "Allow once", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new(
                ALWAYS,
                "Always allow",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(REJECT, "Reject", acp::PermissionOptionKind::RejectOnce),
        ],
    )
}

/// What the client's answer about a server means. `None` when it is not an
/// answer at all.
fn server_answer_of(reply: Reply) -> Option<ServerAnswer> {
    Some(match chosen_in(reply, "a project MCP server")? {
        Chosen::Option(id) if id == ALLOW => ServerAnswer::Once,
        Chosen::Option(id) if id == ALWAYS => ServerAnswer::Always,
        Chosen::Option(_) | Chosen::Cancelled => ServerAnswer::Reject,
    })
}

/// Where the answers about a session's project servers go.
pub(crate) struct ServerApprovals {
    pub(crate) outbox: Arc<Outbox>,
    pub(crate) session: acp::SessionId,
    pub(crate) workspace: std::path::PathBuf,
    /// Where an "allow always" is remembered.
    pub(crate) data_dir: std::path::PathBuf,
    /// The session's servers, to start an allowed one into.
    pub(crate) late: crate::toolbox::LateServers,
    /// Told every line about them, as the client's own servers are.
    pub(crate) said: crate::toolbox::OnConnected,
}

impl ServerApprovals {
    /// Ask the client about each of `servers` in turn, on a thread of its
    /// own — the session is already answered — and start each one allowed.
    /// Once the client gives no answer (it cannot ask, the bound passed, it
    /// went), the rest are skipped with the command that approves them, as
    /// when there is nobody to ask.
    pub(crate) fn ask_in_background(self, servers: Vec<crate::mcp_config::Configured>) {
        if servers.is_empty() {
            return;
        }
        std::thread::spawn(move || self.ask_each(servers, SERVER_ASK_BOUND));
    }

    fn ask_each(&self, servers: Vec<crate::mcp_config::Configured>, bound: Duration) {
        let mut answering = true;
        for server in servers {
            if !self.late.is_open() {
                return;
            }
            let answer = if answering {
                let deadline = std::time::Instant::now() + bound;
                let settled = || !self.late.is_open() || std::time::Instant::now() >= deadline;
                ask_client(
                    &self.outbox,
                    server_request(&self.session, &server),
                    &settled,
                )
                .and_then(server_answer_of)
            } else {
                None
            };
            match answer {
                Some(ServerAnswer::Reject) => {
                    (self.said)(crate::mcp_config::refused(&server.name));
                }
                Some(allowed) => {
                    if allowed == ServerAnswer::Always {
                        if let Err(why) =
                            crate::mcp_config::approve(&self.data_dir, &server.name, &server.entry)
                        {
                            (self.said)(crate::mcp_config::approved_once(&server.name, &why));
                        }
                    }
                    self.start(&server);
                }
                None => {
                    answering = false;
                    if !self.late.is_open() {
                        return;
                    }
                    (self.said)(crate::mcp_config::not_approved(&server.name, &server.entry));
                }
            }
        }
    }

    fn start(&self, server: &crate::mcp_config::Configured) {
        match server.spec(&self.data_dir) {
            Ok(spec) => {
                for line in self.late.connect(vec![spec], &self.workspace, &self.said) {
                    (self.said)(line);
                }
            }
            Err(why) => (self.said)(why),
        }
    }
}
