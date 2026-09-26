//! The `Agent` tool: one piece of work handed to a fresh agent.
//!
//! # Where the sub-agent runs
//!
//! On the model endpoint when one is configured (`agent::api`), as it always
//! did. Otherwise on the agent CLI this toolbox's own model runs in, when it
//! runs in one ([`super::Toolbox::run_subagents_on`]): a second, headless run
//! of the same CLI, as a model only, with a toolbox of its own one level
//! deeper — the same bridge, guards, engine, person to ask and stop. Neither:
//! the call is refused and says why.
//!
//! # Seen as it works
//!
//! When the toolbox has a [`crate::watch::Watch`] that follows workers, the
//! sub-agent is announced before it starts, every event of its own
//! conversation is handed to the tap the watch gave out, and how it ended is
//! said once it has — `completed`, `failed`, or `cancelled` when the stop was
//! asked for. Its own tools report to the tap's watch, so its calls, their
//! output and the workers it starts are seen under it.

use std::sync::Arc;

use serde_json::Value;

use super::{Spawned, Toolbox};
use crate::agent::{AgentBackend, AgentEvent, TurnOutcome};
use crate::watch::{WorkerEnd, WorkerSpec};

/// How deep subagents may nest.
///
/// One level of delegation is what the pattern is for; the second is slack. A
/// subagent that can spawn subagents that can spawn subagents is a bill with no
/// ceiling and a tree nobody can read.
pub(super) const MAX_DEPTH: u8 = 2;

/// What a subagent is told about its position.
///
/// Short on purpose. It inherits no conversation, so everything it needs is in
/// the task it was handed — and a long preamble here is paid on every
/// delegation.
const SUBAGENT_SYSTEM: &str = "\
    You are doing ONE piece of work handed to you by another agent, in its \
    workspace, with the same tools. You do not have its conversation and cannot \
    ask it questions. Do the work and report what you found or changed, briefly \
    and concretely — your reply is the only thing that goes back.";

/// The most of the task a worker's announcement carries, in characters.
const MAX_ANNOUNCED_TASK_CHARS: usize = 2_000;

impl Toolbox {
    /// Hand one piece of work to a fresh agent.
    ///
    /// # Why a subagent is worth the trouble
    ///
    /// A long search or a wide read fills the caller's context with material it
    /// will never need again. Handing that out means only the ANSWER comes back,
    /// and the reading is paid for in a conversation that is then thrown away.
    ///
    /// # The role decides the model
    ///
    /// A subagent that greps and one that works out why a bug happens are not
    /// the same job and should not cost the same. The role is looked up in the
    /// operator's own table, so which model serves which kind of work stays
    /// their decision — see [`crate::roles`].
    pub(super) fn run_subagent(&self, input: &Value) -> Result<String, String> {
        if self.depth >= MAX_DEPTH {
            return Err(format!(
                "already {} agents deep, which is the limit. Do this work here \
                 rather than handing it on again.",
                self.depth
            ));
        }
        let task = input
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if task.is_empty() {
            return Err(
                "a subagent needs a task: say what it should do and what to report back"
                    .to_string(),
            );
        }
        // The operator's routing, not ours. An unknown role leaves the model
        // alone rather than guessing at one — the parent's model is a working
        // answer, and a wrong model is a wasted call.
        let role = input
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let (mut child, agent) = match crate::agent::api::Settings::from_env() {
            Some(settings) => (self.api_child(settings, role), "api"),
            None => match &self.cli_agent {
                Some(binary) => (self.cli_child(binary, role), "claude-code"),
                None => {
                    return Err(
                        "no endpoint is configured and this model runs in no agent CLI, so \
                         there is nothing to hand work to"
                            .to_string(),
                    )
                }
            },
        };
        crate::trace!(
            "route",
            "subagent spawning at depth {} on {agent}, model {:?}",
            self.depth.saturating_add(1),
            child.model()
        );
        // The same person to ask and the same stop: a subagent's write is a
        // write in this workspace, and stopping the turn stops it.
        if let Some(prompter) = &self.prompter {
            child.set_prompter(Arc::clone(prompter));
        }
        if let Some(cancel) = &self.cancel {
            child.set_cancel(cancel.clone());
        }
        if !self.session.is_empty() {
            child.set_memory_session(&self.session);
        }
        let mut tap = self.watch.as_ref().and_then(|watch| {
            watch.worker(&WorkerSpec {
                kind: "sub-agent",
                name: if role.is_empty() {
                    "sub-agent".to_string()
                } else {
                    format!("{role} · sub-agent")
                },
                task: crate::text::truncate(task, MAX_ANNOUNCED_TASK_CHARS),
                role: (!role.is_empty()).then(|| role.to_string()),
                agent: Some(agent.to_string()),
                model: child.model().map(str::to_string),
                lane: None,
                workspace: self.workspace.clone(),
            })
        });
        if let Some(tap) = &tap {
            child.set_watch(tap.watch());
        }

        let mut said = String::new();
        let outcome = child.run_turn(task, &mut |event| {
            if let Some(tap) = tap.as_mut() {
                tap.event(event);
            }
            if let AgentEvent::Text(text) = event {
                said.push_str(text);
            }
        });
        if let Some(tap) = tap.as_mut() {
            tap.ended(self.end_of(&outcome, &said));
        }
        let outcome = outcome.map_err(|e| format!("the subagent could not finish: {e}"))?;

        // Recorded whatever it said, INCLUDING a failure: a subagent that ran
        // and got nowhere is still a conversation that was paid for, and the
        // session record showing it is how that becomes visible.
        if let Ok(mut children) = self.children.lock() {
            children.push(Spawned {
                session: outcome.session_id.clone(),
                title: crate::text::truncate(task, 72),
            });
        }
        if outcome.is_error {
            return Err(outcome.failure());
        }
        if said.trim().is_empty() {
            return Err("the subagent finished without reporting anything".to_string());
        }
        Ok(said)
    }

    /// How a sub-agent's run ended, as its tab shows it.
    fn end_of(
        &self,
        outcome: &Result<TurnOutcome, crate::agent::AgentError>,
        said: &str,
    ) -> WorkerEnd {
        if self.stopped() {
            return WorkerEnd::Cancelled;
        }
        match outcome {
            Ok(done) if !done.is_error && !said.trim().is_empty() => WorkerEnd::Completed,
            _ => WorkerEnd::Failed,
        }
    }

    /// A sub-agent on the model endpoint.
    fn api_child(
        &self,
        mut settings: crate::agent::api::Settings,
        role: &str,
    ) -> Box<dyn AgentBackend> {
        let inherited = settings.model.clone();
        if !role.is_empty() {
            if let Some(chosen) = crate::roles::RoleTable::from_env().model_for(role) {
                settings.model = crate::roles::model_name(chosen).to_string();
            }
        }
        // WHICH WORK WENT WHERE. Routing is invisible from outside: a subagent on
        // the wrong model looks exactly like one on the right model that answered
        // badly. So the choice says itself — including when it did NOT route, and
        // why, which is the case that silently costs the most.
        if role.is_empty() {
            crate::trace!("route", "subagent: no role given, inheriting {inherited}");
        } else if settings.model == inherited {
            crate::trace!(
                "route",
                "subagent: role `{role}` is not configured, inheriting {inherited}"
            );
        } else {
            crate::trace!(
                "route",
                "subagent: role `{role}` -> {} (parent was {inherited})",
                settings.model
            );
        }
        let mut child = crate::agent::api::ApiBackend::new(
            settings,
            &self.workspace,
            SUBAGENT_SYSTEM,
            self.filter_env,
        )
        .at_depth(self.depth + 1)
        .with_engine(self.engine.clone());
        if let Some(cp) = &self.checkpoints {
            child = child.with_checkpoints(cp.clone());
        }
        Box::new(child)
    }

    /// A sub-agent on the agent CLI `binary`, headless, as a model only: its
    /// tools are a toolbox of its own one level down, under this session's
    /// mode, guards and engine. The role routes its model the way a session's
    /// is routed — the operator's candidates, then the agent's own ladder.
    fn cli_child(&self, binary: &str, role: &str) -> Box<dyn AgentBackend> {
        let mut child = crate::agent::claude_code::ClaudeCodeBackend::new(self.workspace.clone())
            .with_binary(binary)
            .streaming(true)
            .with_system_context(vec![SUBAGENT_SYSTEM.to_string()])
            .with_engine(self.engine.clone())
            .at_depth(self.depth + 1);
        if let Ok(guards) = &self.guards {
            child = child.with_guards(guards);
        }
        if let Some(mode) = self.mode {
            child.set_permission_mode(mode.agent_mode());
        }
        if !role.is_empty() {
            let mut ladder = crate::roles::RoleTable::from_env()
                .candidates_for(role)
                .to_vec();
            ladder.extend(AgentBackend::ladder_for(&child, role));
            if !ladder.is_empty() {
                AgentBackend::set_models(&mut child, &ladder);
            }
        }
        Box::new(child)
    }
}
