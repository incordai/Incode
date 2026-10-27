//! One bridged turn: agy spawned as a model only, running the `incode` custom
//! agent (`agent_file.rs`), its tool calls answered by incode's toolbox over
//! the tool bridge.
//!
//! Wired to: [`super::AgyBackend`]'s `run_turn`, on [`super::AgyTools::Incode`].
//! Tested end to end, on every platform, by `agy/bridged_tests.rs`
//! `a_bridged_turn_runs_its_calls_in_incodes_toolbox_and_a_deny_rule_refuses_one`.

use std::io::BufReader;
use std::path::PathBuf;
use std::process::Command;

use super::agent_file;
use super::{AgyBackend, Reading, HOOKS_BY_ENV, PRINT_TIMEOUT};
use crate::agent::{AgentError, AgentEvent, TurnOutcome};

/// The agent's name in errors, as the person knows it.
const AGENT: &str = "Antigravity";

/// Each incode tool's inputs, as one prompt block: `- Name(required*, other)`.
///
/// Needed because agy does not hand an MCP tool's schema to the model: it
/// writes each to `~/.gemini/antigravity-cli/mcp/<server>/<Tool>.json` and the
/// model is meant to open that with agy's own file tool — which this route
/// does not have. Measured live: the model asked incode's `Read` for that
/// file (refused, outside the workspace) and then called tools with `{}` until
/// a schema refusal named the missing input. Built from the toolbox's own
/// definitions each turn, so it names what this session really offers.
///
/// Called by the backend's `full_prompt` (`agy.rs`) on the bridged route.
pub(super) fn tool_inputs(tools: &crate::toolbox::Toolbox) -> String {
    let mut out = String::from(
        "<incode-tools>\nThe inputs of each incode tool, for `call_mcp_tool` with ServerName \
         `incode`, ToolName as named and Arguments an object of these (`*` is required). \
         Their schema files are outside the workspace and cannot be opened here.\n",
    );
    for def in tools.definitions() {
        let Some(name) = def.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let schema = def.get("input_schema");
        let required: Vec<&str> = schema
            .and_then(|s| s.get("required"))
            .and_then(serde_json::Value::as_array)
            .map(|r| r.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        let mut inputs: Vec<String> = required.iter().map(|r| format!("{r}*")).collect();
        if let Some(properties) = schema
            .and_then(|s| s.get("properties"))
            .and_then(serde_json::Value::as_object)
        {
            inputs.extend(
                properties
                    .keys()
                    .filter(|k| !required.contains(&k.as_str()))
                    .cloned(),
            );
        }
        out.push_str(&format!("- {name}({})\n", inputs.join(", ")));
    }
    out.push_str("</incode-tools>");
    out
}

impl AgyBackend {
    /// The binary agy is told to start as the tool server.
    ///
    /// Called by [`Self::run_bridged`] and by the backend's `mcp_config`.
    pub(super) fn server_exe(&self) -> PathBuf {
        self.tool_server
            .clone()
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("incode"))
    }

    /// Where the agent file goes: a test's folder, else agy's own.
    fn agents_dir(&self) -> Result<PathBuf, AgentError> {
        self.agents_dir
            .clone()
            .or_else(agent_file::agents_dir)
            .ok_or_else(|| {
                AgentError::new(
                    "incode could not find the home folder agy reads its agents from, so it \
                     cannot hand agy its tools; the turn was not started",
                )
            })
    }

    /// The command for one bridged turn: agy running the `incode` agent,
    /// whose only tools are `finish` and incode's tool server. `bridge_env`
    /// is the bridge's address and token, set in agy's environment — which
    /// the server agy starts inherits (measured) — never on its argv.
    ///
    /// `INCORD_HOOKS_BY=incode` is set beside them: agy's own memory hooks
    /// (`~/.gemini/*/hooks.json`) would fire on this run too, and the engine
    /// stands them down when it reads this, because incode's toolbox already
    /// tells it of every call.
    ///
    /// The model, mode, added roots and conversation are handed on as on
    /// agy's own route: with no tools of its own, none of them reaches past
    /// incode's toolbox, which is told the mode and the roots itself.
    ///
    /// Called by [`Self::run_bridged`].
    pub(super) fn bridged_command(&self, prompt: &str, bridge_env: &[(&str, String)]) -> Command {
        let mut cmd = Command::new(&self.binary);
        crate::agent::parent_session::detach_from_parent_session(&mut cmd);
        cmd.current_dir(&self.workspace);
        cmd.arg("--print").arg(prompt);
        cmd.arg("--output-format").arg("stream-json");
        cmd.arg("--print-timeout").arg(PRINT_TIMEOUT);
        cmd.arg("--agent").arg(agent_file::AGENT_NAME);
        // Acceptable ONLY because agy has no tools of its own on this route:
        // the agent gives it `finish` and incode's server, nothing that runs
        // or writes. Without it every headless MCP call is denied unasked
        // (measured); with it, whether a call runs is decided by incode's
        // toolbox — the guards, the engine, the person — for every call.
        cmd.arg("--dangerously-skip-permissions");
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(mode) = &self.mode {
            cmd.arg("--mode").arg(mode);
        }
        for dir in &self.extra_dirs {
            cmd.arg("--add-dir").arg(dir);
        }
        if let Some(id) = &self.conversation_id {
            cmd.arg("--conversation").arg(id);
        }
        for (key, value) in bridge_env {
            cmd.env(key, value);
        }
        cmd.env(HOOKS_BY_ENV, "incode");
        cmd
    }

    /// One bridged turn: the agent file refreshed, the bridge opened, agy
    /// started on `prompt`, every tool call it forwards run by the toolbox,
    /// and its stream read to the end — the Claude route's loop
    /// ([`crate::tool_bridge::drive`]) over agy's lines.
    ///
    /// A turn whose tool server never asked for the tools is refused
    /// ([`crate::tool_bridge::require_tools`]): its model had no tools at all.
    ///
    /// Called by the backend's `run_turn` when the tools are incode's.
    pub(super) fn run_bridged(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(&AgentEvent),
    ) -> Result<TurnOutcome, AgentError> {
        // The channel first: a channel that cannot open starts nothing, and
        // agy is never left to fall back on tools of its own.
        let crate::tool_bridge::Turn {
            exe,
            bridge,
            to,
            from,
        } = crate::tool_bridge::Turn::open(self.cancel.clone())?;
        let exe = self.tool_server.clone().unwrap_or(exe);
        agent_file::ensure(&self.agents_dir()?, &exe).map_err(AgentError::new)?;
        let command = self.bridged_command(prompt, &bridge.env());
        crate::trace!(
            "agent",
            "spawning {} bridged: agent={} model={:?} resume={:?}",
            self.binary,
            agent_file::AGENT_NAME,
            self.model,
            self.conversation_id
        );
        let super::Running {
            mut child,
            tree,
            stdout,
            stderr,
        } = self.spawn(command)?;
        crate::tool_bridge::pump(BufReader::new(stdout), to);

        let Self {
            tools,
            calls,
            conversation_id,
            model,
            cancel,
            ..
        } = self;
        let tools = tools
            .as_mut()
            .ok_or_else(|| AgentError::new("a bridged turn needs incode's toolbox"))?;
        let mut reading = Reading::new(conversation_id, model.clone(), true);
        let driven = crate::tool_bridge::drive(
            &from,
            cancel.as_ref(),
            tools,
            calls,
            on_event,
            &mut |line, tools| {
                let mut events = Vec::new();
                reading.line(&line, &mut |e| events.push(e.clone()));
                for event in &events {
                    if let AgentEvent::SessionStarted { session_id, .. } = event {
                        // The engine files its cards under the conversation.
                        tools.set_session(session_id);
                    }
                }
                Ok(events)
            },
        );
        let listed = bridge.tools_listed();
        // Closed as soon as the loop is done: a call that arrives from here on
        // is refused rather than left waiting on a loop that is gone.
        drop(bridge);
        if let Err(e) = driven {
            // Whatever happened, agy must not outlive the turn — nor whatever
            // it started.
            tree.kill(&mut child);
            return Err(e);
        }
        let outcome = reading.finish(on_event);
        let status = child
            .wait()
            .map_err(|e| AgentError::new(format!("the agent did not exit cleanly: {e}")))?;
        let outcome = outcome.ok_or_else(|| super::no_result_line(status, stderr))?;
        // A failure agy reported is its own reason, and stands: agy refuses
        // some turns before it starts any server — measured, an unknown
        // `--model` is refused at once — and "never started incode's tool
        // server" would hide why.
        if !outcome.is_error {
            crate::tool_bridge::require_tools(listed, AGENT)?;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
#[path = "bridged_tests.rs"]
mod tests;
