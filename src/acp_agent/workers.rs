//! What a session's tools and workers do, as the client is shown it.
//!
//! # A turn's events
//!
//! [`Presenter`] turns incode's event stream into `session/update`s: a call as
//! `tool_call`, its result as `tool_call_update` (a diff for an edit that
//! landed, [`present::Edits`]), an accepted task list as the `plan`, the
//! model's reasoning as `agent_thought_chunk`, the context window as
//! `usage_update`. The session's own turn and every worker's use the same
//! one; only the session's own text arrives another way (the turn's text
//! callback, after the question block is filtered out).
//!
//! # incode's details on every step (`_meta.incode`)
//!
//! Every `tool_call` and `tool_call_update` carries what incode knows about
//! the step and nothing it does not: the role the model was routed by, the
//! agent and the model asked for, the lane; on the result, what the guards or
//! the engine decided before it ran and what the recorder answered a
//! `watchman_lookup`. See [`meta`] and `docs/ARCHITECTURE.md`.
//!
//! # Workers as sub-agents
//!
//! [`SessionWatch`] is the session's [`Watch`]: a running command's output as
//! a `tool_call_update` while it runs, the gate's decisions kept for the
//! result's `_meta`, and — when the client advertised the draft sub-agent
//! extension (`clientCapabilities.subagents`) — each worker announced in the
//! PARENT's session with `subagent_spawned`, its own conversation sent under
//! its own session id, and `subagent_state_update` once it ends. Those two
//! updates are not in the stable schema, so they are written as the JSON the
//! owner's app reads (`agentStore.ts`), inside an ordinary `session/update`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use agent_client_protocol_schema::v1 as acp;
use serde_json::{json, Value};

use super::present;
use super::transport::Outbox;
use crate::agent::AgentEvent;
use crate::cli::agent_turn::Before;
use crate::watch::{Decision, Watch, WorkerEnd, WorkerSpec, WorkerTap};

/// What incode knows about who is doing a step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Who {
    pub(super) role: Option<String>,
    pub(super) agent: Option<String>,
    pub(super) model: Option<String>,
    pub(super) lane: Option<String>,
}

/// What the gate and the recorder said about one call, until its result is
/// shown.
#[derive(Debug, Clone, Default)]
struct CallNotes {
    guard: Option<Decision>,
    /// The recorder's answer: a hit, and how often the output was seen.
    watchman: Option<(bool, Option<u64>)>,
}

type Notes = Arc<Mutex<HashMap<String, CallNotes>>>;

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `_meta` for one step: `{"incode": {…}}` with only the fields known, or
/// `None` when nothing is.
fn meta(who: &Who, notes: Option<&CallNotes>) -> Option<acp::Meta> {
    let mut details = serde_json::Map::new();
    for (key, value) in [
        ("role", &who.role),
        ("agent", &who.agent),
        ("model", &who.model),
        ("lane", &who.lane),
    ] {
        if let Some(value) = value {
            details.insert(key.to_string(), json!(value));
        }
    }
    if let Some(notes) = notes {
        if let Some(decision) = &notes.guard {
            let mut guard = serde_json::Map::new();
            guard.insert("by".to_string(), json!(decision.by.as_str()));
            guard.insert(
                "decision".to_string(),
                json!(if decision.allowed {
                    "allowed"
                } else {
                    "blocked"
                }),
            );
            for (key, value) in [
                ("rule", &decision.rule),
                ("path", &decision.path),
                ("reason", &decision.reason),
            ] {
                if let Some(value) = value {
                    guard.insert(key.to_string(), json!(value));
                }
            }
            details.insert("guard".to_string(), Value::Object(guard));
        }
        if let Some((hit, observations)) = notes.watchman {
            let mut watchman = serde_json::Map::new();
            watchman.insert("hit".to_string(), json!(hit));
            if let Some(seen) = observations {
                watchman.insert("observations".to_string(), json!(seen));
            }
            details.insert("watchman".to_string(), Value::Object(watchman));
        }
    }
    if details.is_empty() {
        return None;
    }
    let mut outer = acp::Meta::new();
    outer.insert("incode".to_string(), Value::Object(details));
    Some(outer)
}

/// `_meta` for a prompt's answer: who served the turn.
pub(super) fn prompt_meta(who: &Who) -> Option<acp::Meta> {
    meta(who, None)
}

/// Whose events a [`Presenter`] shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Voice {
    /// The session's own turn: its text arrives through the turn's text
    /// callback, and the context window is the session's.
    Session,
    /// A worker: its text is shown from its events, and the window it ran in
    /// is not the session's.
    Worker,
}

/// Turns one conversation's events into `session/update`s.
pub(super) struct Presenter {
    voice: Voice,
    edits: present::Edits,
    plans: present::Plans,
    notes: Notes,
    who: Arc<Mutex<Who>>,
    /// A worker streamed its text: its whole-block copy is not shown again.
    saw_deltas: bool,
}

impl Presenter {
    /// The `session/update`s one event becomes. `before` says what a file
    /// held when the work began.
    pub(super) fn updates(
        &mut self,
        event: &AgentEvent,
        before: &dyn Fn(&Path) -> Before,
    ) -> Vec<acp::SessionUpdate> {
        let who = locked(&self.who).clone();
        match event {
            AgentEvent::ToolUse { id, name, input } => {
                self.edits.called(id, input);
                self.plans.called(id, name, input);
                vec![acp::SessionUpdate::ToolCall(
                    present::tool_call(id, name, input).meta(meta(&who, None)),
                )]
            }
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
            } => {
                let notes = locked(&self.notes).remove(id);
                let update = match self.edits.landed(id, *is_error, before) {
                    Some(diff) => present::tool_diff(id, diff),
                    None => present::tool_result(id, output, *is_error),
                };
                let mut updates = vec![acp::SessionUpdate::ToolCallUpdate(
                    update.meta(meta(&who, notes.as_ref())),
                )];
                if let Some(plan) = self.plans.landed(id, *is_error) {
                    updates.push(acp::SessionUpdate::Plan(plan));
                }
                updates
            }
            AgentEvent::Thinking(text) | AgentEvent::Notice(text) if !text.is_empty() => {
                vec![acp::SessionUpdate::AgentThoughtChunk(
                    acp::ContentChunk::new(acp::ContentBlock::from(text.as_str())),
                )]
            }
            AgentEvent::Context { used, size } if self.voice == Voice::Session => {
                vec![acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(
                    *used, *size,
                ))]
            }
            AgentEvent::TextDelta(text) if self.voice == Voice::Worker => {
                self.saw_deltas = true;
                said(text)
            }
            AgentEvent::Text(text) if self.voice == Voice::Worker && !self.saw_deltas => said(text),
            _ => Vec::new(),
        }
    }
}

fn said(text: &str) -> Vec<acp::SessionUpdate> {
    if text.is_empty() {
        return Vec::new();
    }
    vec![acp::SessionUpdate::AgentMessageChunk(
        acp::ContentChunk::new(acp::ContentBlock::from(text)),
    )]
}

/// A session's watch — its own, or a worker's.
#[derive(Clone)]
pub(super) struct SessionWatch {
    outbox: Arc<Outbox>,
    /// Where this watch's calls are shown: the session, or the worker's own.
    session: acp::SessionId,
    /// The client advertised the sub-agent extension.
    subagents: bool,
    notes: Notes,
    who: Arc<Mutex<Who>>,
    /// Numbers the workers of one client session, nested ones included.
    workers: Arc<AtomicU64>,
    /// The client session every worker id is made under.
    root: acp::SessionId,
}

impl SessionWatch {
    /// The watch of a session the client opened.
    pub(super) fn opened(outbox: &Arc<Outbox>, session: &acp::SessionId, subagents: bool) -> Self {
        Self {
            outbox: Arc::clone(outbox),
            session: session.clone(),
            subagents,
            notes: Arc::default(),
            who: Arc::default(),
            workers: Arc::default(),
            root: session.clone(),
        }
    }

    /// Who serves the session's next turn, for its steps' `_meta`.
    pub(super) fn serve_as(&self, who: Who) {
        *locked(&self.who) = who;
    }

    /// A presenter for the session's own turn, reading this watch's notes.
    pub(super) fn presenter(&self, workspace: &Path) -> Presenter {
        self.presenter_as(Voice::Session, workspace)
    }

    fn presenter_as(&self, voice: Voice, workspace: &Path) -> Presenter {
        Presenter {
            voice,
            edits: present::Edits::new(workspace),
            plans: present::Plans::new(),
            notes: Arc::clone(&self.notes),
            who: Arc::clone(&self.who),
            saw_deltas: false,
        }
    }

    fn say(&self, update: acp::SessionUpdate) {
        self.outbox.notify(
            acp::CLIENT_METHOD_NAMES.session_update,
            acp::SessionNotification::new(self.session.clone(), update),
        );
    }

    /// One of the draft extension's updates, in `session`, as the app reads
    /// it: a `session/update` whose `update` carries its `sessionUpdate`.
    fn extension(&self, session: &acp::SessionId, update: Value) {
        self.outbox.notify(
            acp::CLIENT_METHOD_NAMES.session_update,
            json!({"sessionId": session, "update": update}),
        );
    }

    fn note(&self, call_id: &str, with: impl FnOnce(&mut CallNotes)) {
        with(locked(&self.notes).entry(call_id.to_string()).or_default());
    }
}

impl Watch for SessionWatch {
    fn output(&self, call_id: &str, shown: &str) {
        let who = locked(&self.who).clone();
        self.say(acp::SessionUpdate::ToolCallUpdate(
            acp::ToolCallUpdate::new(
                call_id.to_string(),
                acp::ToolCallUpdateFields::new()
                    .status(acp::ToolCallStatus::InProgress)
                    .content(vec![acp::ToolCallContent::from(shown.to_string())]),
            )
            .meta(meta(&who, None)),
        ));
    }

    fn decided(&self, call_id: &str, decision: &Decision) {
        let decision = decision.clone();
        self.note(call_id, |notes| notes.guard = Some(decision));
    }

    fn recorded(&self, call_id: &str, hit: bool, observations: Option<u64>) {
        self.note(call_id, |notes| notes.watchman = Some((hit, observations)));
    }

    fn worker(&self, spec: &WorkerSpec) -> Option<Box<dyn WorkerTap>> {
        if !self.subagents {
            return None;
        }
        let n = self.workers.fetch_add(1, Ordering::SeqCst) + 1;
        let id = acp::SessionId::new(format!("{}/{}-{n}", self.root, spec.kind));
        self.extension(
            &self.session,
            json!({
                "sessionUpdate": "subagent_spawned",
                "subagentSessionId": id,
                "name": spec.name,
                "task": spec.task,
                "capabilities": {},
            }),
        );
        let watch = Self {
            session: id,
            notes: Arc::default(),
            who: Arc::new(Mutex::new(Who {
                role: spec.role.clone(),
                agent: spec.agent.clone(),
                model: spec.model.clone(),
                lane: spec.lane.clone(),
            })),
            ..self.clone()
        };
        let presenter = watch.presenter_as(Voice::Worker, &spec.workspace);
        Some(Box::new(Tap {
            parent: self.session.clone(),
            watch,
            presenter,
            ended: false,
        }))
    }
}

/// One worker, shown under its own session id until it ends.
struct Tap {
    /// Where it was announced, and where its end is said.
    parent: acp::SessionId,
    watch: SessionWatch,
    presenter: Presenter,
    ended: bool,
}

impl WorkerTap for Tap {
    fn event(&mut self, event: &AgentEvent) {
        // A worker has no snapshot of its own: a whole-file write whose
        // before is not known is shown as its output, never as a new file.
        for update in self.presenter.updates(event, &|_| Before::Unknown) {
            self.watch.say(update);
        }
    }

    fn ended(&mut self, end: WorkerEnd) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        self.watch.extension(
            &self.parent,
            json!({
                "sessionUpdate": "subagent_state_update",
                "subagentSessionId": self.watch.session,
                "state": end.as_str(),
            }),
        );
    }

    fn watch(&self) -> Arc<dyn Watch> {
        Arc::new(self.watch.clone())
    }
}

impl Drop for Tap {
    /// A worker that never said how it ended died on the way: it failed, and
    /// its tab must not stay `running`.
    fn drop(&mut self) {
        self.ended(WorkerEnd::Failed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presenter(voice: Voice, workspace: &Path) -> Presenter {
        Presenter {
            voice,
            edits: present::Edits::new(workspace),
            plans: present::Plans::new(),
            notes: Arc::default(),
            who: Arc::default(),
            saw_deltas: false,
        }
    }

    /// **incode's NOTICE IS A THOUGHT**, for the session's own turn and for
    /// a worker's alike, and never reply text; an empty one is nothing.
    #[test]
    fn a_notice_becomes_a_thought_chunk_for_the_session_and_a_worker() {
        let ws = tempfile::tempdir().expect("ws");
        for voice in [Voice::Session, Voice::Worker] {
            let mut shows = presenter(voice, ws.path());
            let updates = shows.updates(
                &AgentEvent::Notice("[pre_compact hook] noted".to_string()),
                &|_| Before::Unknown,
            );
            let sent: Vec<Value> = updates
                .iter()
                .map(|u| serde_json::to_value(u).expect("json"))
                .collect();
            assert_eq!(sent.len(), 1, "{sent:#?}");
            assert_eq!(sent[0]["sessionUpdate"], "agent_thought_chunk", "{sent:#?}");
            assert_eq!(sent[0]["content"]["text"], "[pre_compact hook] noted");
            assert!(shows
                .updates(&AgentEvent::Notice(String::new()), &|_| Before::Unknown)
                .is_empty());
        }
    }
}
