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

/// What the client's answer means. `None` when it is not an answer at all.
fn verdict_of(reply: Reply) -> Option<Verdict> {
    let result: Value = match reply {
        Ok(result) => result,
        Err(error) => {
            // A client that cannot ask leaves it to the owner's app.
            eprintln!("incode acp: the client could not ask about a tool call: {error:?}");
            return None;
        }
    };
    let answer = serde_json::from_value::<acp::RequestPermissionResponse>(result).ok()?;
    Some(match answer.outcome {
        acp::RequestPermissionOutcome::Selected(chosen) if &*chosen.option_id.0 == ALLOW => {
            Verdict::Allow
        }
        acp::RequestPermissionOutcome::Selected(_) => {
            Verdict::Deny("NOT RUN — refused in the client.".to_string())
        }
        // The client cancels a pending question when its turn is cancelled.
        _ => Verdict::Deny("NOT RUN — the client cancelled the question.".to_string()),
    })
}

impl Prompter for ClientPrompter {
    fn confirm(&self, ask: &Ask<'_>, settled: &dyn Fn() -> bool) -> Option<Verdict> {
        let request = request_for(&self.session, ask);
        let (id, answered) = self
            .outbox
            .request(acp::CLIENT_METHOD_NAMES.session_request_permission, request);
        loop {
            match answered.recv_timeout(LOOK_EVERY) {
                Ok(reply) => return verdict_of(reply),
                Err(RecvTimeoutError::Timeout) => {
                    if settled() {
                        // Answered in the owner's app, or the turn stopped. The
                        // client's card stays until it is answered or the turn
                        // ends; its answer is then dropped.
                        self.outbox.forget(&id);
                        return None;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    fn choose(&self, _questions: &[crate::ask::Question]) -> Option<Vec<crate::ask::Answer>> {
        None
    }
}
