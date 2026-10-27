//! One bridged turn: codex spawned as a model only, its tool calls answered
//! by incode's toolbox over the tool bridge.
//!
//! Wired to: [`super::CodexBackend`]'s `run_turn`, on [`super::CodexTools::Incode`].
//! Tested end to end, on every platform, by `agent/codex/tests.rs`
//! `a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one`.

use std::io::BufReader;
use std::process::{Command, Stdio};

use super::wire::Reading;
use super::CodexBackend;
use crate::agent::{AgentError, AgentEvent, TurnOutcome};

impl CodexBackend {
    /// One bridged turn: `command` spawned with the bridge's address and token
    /// in its environment, its tool calls answered by the toolbox, its stream
    /// read to the end — the Claude route's loop ([`crate::tool_bridge::drive`])
    /// over codex's lines.
    ///
    /// Called by the backend's `run_turn` once the turn's channel is open.
    /// `stdin_text` is the prompt when it was too long for the command line:
    /// the command then says `-`, and the text is written to codex's stdin.
    pub(super) fn run_bridged(
        &mut self,
        mut command: Command,
        stdin_text: Option<String>,
        turn: crate::tool_bridge::Turn,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        let crate::tool_bridge::Turn {
            bridge, to, from, ..
        } = turn;
        // How the tool server finds this process: codex's environment, which
        // it passes on to the server by the names `env_vars` lists — never
        // its argv.
        for (key, value) in bridge.env() {
            command.env(key, value);
        }
        crate::agent::process_tree::prepare(&mut command);
        if stdin_text.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AgentError::new(format!("could not start `{}`: {e}", self.binary)))?;
        super::feed_stdin(&mut child, stdin_text);
        let tree = crate::agent::process_tree::Tree::adopt(&child);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::new("the agent produced no output stream"))?;
        let stderr = crate::agent::stderr_tail::StderrTail::capture(child.stderr.take());
        crate::tool_bridge::pump(BufReader::new(stdout), to);

        let Self {
            tools,
            calls,
            thread_id,
            model,
            cancel,
            ..
        } = self;
        let tools = tools
            .as_mut()
            .ok_or_else(|| AgentError::new("a bridged turn needs incode's toolbox"))?;
        let mut reading = Reading::new(thread_id, model.clone(), true);
        let driven = crate::tool_bridge::drive(
            &from,
            cancel.as_ref(),
            tools,
            calls,
            on_event,
            &mut |line, tools| {
                let events = reading.line(&line);
                for event in &events {
                    if let AgentEvent::SessionStarted { session_id, .. } = event {
                        // The engine files its cards under the conversation.
                        tools.set_session(session_id);
                    }
                }
                Ok(events)
            },
        );
        let (outcome, usage) = reading.finish();
        // Closed as soon as the loop is done: a call that arrives from here on
        // is refused rather than left waiting on a loop that is gone.
        drop(bridge);
        if let Err(e) = driven {
            // Whatever happened, codex must not outlive the turn — nor
            // whatever it started.
            tree.kill(&mut child);
            return Err(e);
        }

        on_event(&AgentEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_input_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
        });
        let status = child
            .wait()
            .map_err(|e| AgentError::new(format!("the agent did not exit cleanly: {e}")))?;
        outcome.ok_or_else(|| {
            let stderr_output = stderr.finish();
            let detail = if stderr_output.is_empty() {
                String::new()
            } else {
                format!(": {stderr_output}")
            };
            AgentError::new(format!(
                "the agent produced no result line (exit {}){detail}",
                status.code().unwrap_or(-1)
            ))
        })
    }
}

#[cfg(test)]
#[path = "bridged_tests.rs"]
mod tests;
