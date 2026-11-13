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
        // An agent the operator defined, by name: its instructions, role,
        // model and tools. A name that is not defined is refused with the
        // names that are, rather than run as a plain sub-agent.
        let definition = match input
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name) => {
                let loaded = crate::subagents::load(&self.workspace);
                match loaded.find(name) {
                    Some(found) => Some(found.clone()),
                    None => {
                        let known: Vec<&str> =
                            loaded.agents.iter().map(|a| a.name.as_str()).collect();
                        return Err(format!(
                            "there is no agent called `{name}`. Defined here: {}",
                            if known.is_empty() {
                                "none".to_string()
                            } else {
                                known.join(", ")
                            }
                        ));
                    }
                }
            }
            None => None,
        };
        // The operator's routing, not ours. An unknown role leaves the model
        // alone rather than guessing at one — the parent's model is a working
        // answer, and a wrong model is a wasted call. A role in the call wins
        // over the definition's.
        let asked = input
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let role = if asked.is_empty() {
            definition
                .as_ref()
                .and_then(|d| d.role.as_deref())
                .unwrap_or("")
        } else {
            asked
        };
        // What memory holds for the task — past work, the code it is about,
        // the owner's rules — delivered with it, as the lead's turns get theirs
        // (`crate::turn_memory`). Only when this session has the engine.
        let memory = self
            .engine
            .is_some()
            .then(|| crate::turn_memory::for_subagent(task, &self.workspace, &self.session))
            .flatten();
        let spec = Child {
            role,
            definition: definition.as_ref(),
            tools: self.child_tools(definition.as_ref()),
            edits: self.code_edits.as_ref().map(super::CodeEdits::child),
            memory,
        };
        // The parent's own route: a model chosen by name hands work to the
        // same service. No other place configures one.
        let route = self.api_route.clone();
        let (mut child, agent) = match route {
            Some(settings) => (self.api_child(settings, &spec), "api"),
            None => match &self.cli_agent {
                Some(binary) => (self.cli_child(binary, &spec), "claude-code"),
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
        if let Some(owner) = &self.owner {
            child.set_owner(Arc::clone(owner));
        }
        child.share_allowed_writes(self.allowed_writes());
        if let Some(cancel) = &self.cancel {
            child.set_cancel(cancel.clone());
        }
        if !self.session.is_empty() {
            child.set_memory_session(&self.session);
        }
        let mut tap = self.watch.as_ref().and_then(|watch| {
            watch.worker(&WorkerSpec {
                kind: "sub-agent",
                name: match (definition.as_ref(), role.is_empty()) {
                    (Some(def), _) => format!("{} · sub-agent", def.name),
                    (None, true) => "sub-agent".to_string(),
                    (None, false) => format!("{role} · sub-agent"),
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
        let end = self.end_of(&outcome, &said);
        if let Some(tap) = tap.as_mut() {
            tap.ended(end);
        }
        // The operator's `subagent_stop` commands: told how it ended, never
        // asked whether it may. What they print is traced.
        if let Some(hooks) = &self.hooks {
            let named = match (definition.as_ref(), role.is_empty()) {
                (Some(def), _) => def.name.clone(),
                (None, true) => "sub-agent".to_string(),
                (None, false) => role.to_string(),
            };
            let stopped = crate::hooks::SubagentStopped {
                agent: &named,
                task,
                last_message: &said,
                outcome: end.as_str(),
            };
            for told in hooks
                .subagent_stop(&stopped, &self.session, Some(&self.workspace))
                .messages()
            {
                crate::trace!("hooks", "subagent_stop hook: {told}");
            }
        }
        // Its work is over however it ended: what it claimed, read and
        // announced is let go now, not when a lease runs out.
        if let Some(edits) = &spec.edits {
            self.end_code_session(edits);
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

    /// The tools a sub-agent may use: a definition's list, narrowed to what
    /// this agent itself may use, or this agent's own list — so a sub-agent
    /// never gets a tool its parent was not given. `None` is every tool.
    fn child_tools(&self, definition: Option<&crate::subagents::AgentDef>) -> Option<Vec<String>> {
        let named = definition.and_then(|d| d.tools.clone());
        match (named, &self.allowed) {
            (Some(named), Some(mine)) => {
                Some(named.into_iter().filter(|t| mine.contains(t)).collect())
            }
            (Some(named), None) => Some(named),
            (None, mine) => mine.clone(),
        }
    }

    /// The instructions a sub-agent works under: the note on its position,
    /// then its definition's own, when it has one, then what memory holds for
    /// its task.
    fn child_system(spec: &Child<'_>) -> Vec<String> {
        let mut blocks = vec![SUBAGENT_SYSTEM.to_string()];
        if let Some(def) = spec.definition {
            blocks.push(def.prompt.clone());
        }
        blocks.extend(spec.memory.clone());
        blocks
    }

    /// A sub-agent on the model endpoint.
    fn api_child(
        &self,
        mut settings: crate::agent::api::Settings,
        spec: &Child<'_>,
    ) -> Box<dyn AgentBackend> {
        let role = spec.role;
        let inherited = settings.model.clone();
        // The route's own model for delegated work, when it names one — the
        // cheaper model of the same service (a fast GLM under a main GLM, a
        // small Claude under a large one). The operator's role table and the
        // definition still win over it below.
        if !settings.sub_model.is_empty() {
            settings.model.clone_from(&settings.sub_model);
        }
        // A role's candidates, only as this route serves them: a name the
        // service does not have is not sent to it (`Settings::serves`).
        if !role.is_empty() {
            if let Some(chosen) = crate::roles::RoleTable::from_env()
                .candidates_for(role)
                .iter()
                .find_map(|c| settings.serves(c))
            {
                settings.model = chosen;
            }
        }
        // A model the definition names outright wins over the role's, through
        // the route's aliases (`opus` is this service's id for it).
        if let Some(model) = spec.definition.and_then(|d| d.model.as_deref()) {
            settings.model = settings.alias(model);
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
            &Self::child_system(spec).join("\n\n"),
            self.filter_env,
        )
        .at_depth(self.depth + 1)
        .with_engine(self.engine.clone())
        .with_code_edits(spec.edits.clone())
        .with_operator_hooks(self.hooks.clone())
        .with_sandbox(self.sandbox)
        .with_tool_allowlist(spec.tools.clone());
        if let Some(cp) = &self.checkpoints {
            child = child.with_checkpoints(cp.clone());
        }
        Box::new(child)
    }

    /// A sub-agent on the agent CLI `binary`, headless, as a model only: its
    /// tools are a toolbox of its own one level down, under this session's
    /// mode, guards and engine. The role routes its model the way a session's
    /// is routed — the operator's candidates, then the agent's own ladder —
    /// unless the definition names a model outright.
    fn cli_child(&self, binary: &str, spec: &Child<'_>) -> Box<dyn AgentBackend> {
        let role = spec.role;
        let mut child = crate::agent::claude_code::ClaudeCodeBackend::new(self.workspace.clone())
            .with_binary(binary)
            .streaming(true)
            .with_system_context(Self::child_system(spec))
            .with_engine(self.engine.clone())
            .with_code_edits(spec.edits.clone())
            .with_operator_hooks(self.hooks.clone())
            .with_sandbox(self.sandbox)
            .with_tool_allowlist(spec.tools.clone())
            .at_depth(self.depth + 1);
        if let Ok(guards) = &self.guards {
            child = child.with_guards(guards);
        }
        if let Some(mode) = self.mode {
            child.set_permission_mode(mode.agent_mode());
        }
        let mut ladder: Vec<String> = spec
            .definition
            .and_then(|d| d.model.clone())
            .into_iter()
            .collect();
        if !role.is_empty() {
            ladder.extend(
                crate::roles::RoleTable::from_env()
                    .candidates_for(role)
                    .iter()
                    .cloned(),
            );
            ladder.extend(AgentBackend::ladder_for(&child, role));
        }
        if !ladder.is_empty() {
            AgentBackend::set_models(&mut child, &ladder);
        }
        Box::new(child)
    }
}

/// What a sub-agent is started with, beyond the task.
struct Child<'a> {
    /// The role routing its model; empty for none.
    role: &'a str,
    /// The operator's definition it runs, when the call named one.
    definition: Option<&'a crate::subagents::AgentDef>,
    /// The only tools it may use; `None` is every tool.
    tools: Option<Vec<String>>,
    /// Its side of the engine's edit check: a name and a session of its own.
    edits: Option<super::CodeEdits>,
    /// What memory holds for its task ([`crate::turn_memory::for_subagent`]).
    memory: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviewer(tools: Option<&[&str]>) -> crate::subagents::AgentDef {
        crate::subagents::AgentDef {
            name: "reviewer".to_string(),
            description: "reads a change".to_string(),
            role: Some("review".to_string()),
            model: Some("opus".to_string()),
            tools: tools.map(|t| t.iter().map(|s| (*s).to_string()).collect()),
            prompt: "REVIEWER-INSTRUCTIONS: report each problem with its line.".to_string(),
            source: std::path::PathBuf::from("reviewer.md"),
            scope: crate::subagents::Scope::Project,
        }
    }

    /// **A DEFINED AGENT RUNS WITH ITS OWN INSTRUCTIONS, MODEL AND TOOLS.**
    /// Its prompt rides the sub-agent's system context, its model is the one
    /// asked for, and its toolbox offers only the tools it names.
    #[test]
    fn a_defined_agent_gets_its_instructions_model_and_only_its_tools() {
        let ws = tempfile::tempdir().expect("ws");
        let parent = Toolbox::new(ws.path(), true);
        let def = reviewer(Some(&["Read", "Grep"]));
        let spec = Child {
            role: "review",
            definition: Some(&def),
            tools: parent.child_tools(Some(&def)),
            edits: None,
            memory: None,
        };
        let child = parent.cli_child("claude", &spec);
        assert!(
            child
                .system_context()
                .iter()
                .any(|b| b.contains("REVIEWER-INSTRUCTIONS")),
            "{:?}",
            child.system_context()
        );
        assert_eq!(child.model(), Some("opus"));
        let mut offered = child.offered_tools();
        offered.sort();
        assert_eq!(offered, ["Grep", "Read"]);
    }

    /// **A sub-agent starts with what memory holds for its task**, on either
    /// route — its system context carried only its position note and its
    /// definition before, while the lead it works for had memory and code.
    #[test]
    fn a_sub_agent_carries_the_memory_for_its_task() {
        let ws = tempfile::tempdir().expect("ws");
        let parent = Toolbox::new(ws.path(), true);
        let def = reviewer(None);
        let spec = Child {
            role: "",
            definition: Some(&def),
            tools: None,
            edits: None,
            memory: Some(
                "<memory_context>\n## Code (read these)\n### src/a.rs:1-9\n</memory_context>"
                    .to_string(),
            ),
        };
        let child = parent.cli_child("claude", &spec);
        let blocks = child.system_context();
        assert!(
            blocks.iter().any(|b| b.contains("## Code (read these)")),
            "{blocks:?}"
        );
        assert!(
            blocks.iter().any(|b| b.contains("REVIEWER-INSTRUCTIONS")),
            "its definition still rides with it: {blocks:?}"
        );
        let system = Toolbox::child_system(&spec);
        assert_eq!(system.last(), spec.memory.as_ref(), "memory comes last");
    }

    /// **A sub-agent of an API model runs on that model's `sub_model`** — the
    /// same service's cheaper model for delegated work — over the parent's
    /// own route; a definition that names a model still wins.
    #[test]
    fn an_api_sub_agent_runs_on_its_routes_sub_model() {
        let ws = tempfile::tempdir().expect("ws");
        let parent = Toolbox::new(ws.path(), true);
        let settings = crate::agent::api::Settings {
            endpoint: "https://service.invalid/v1/messages".to_string(),
            key: "k".to_string(),
            model: "main-model".to_string(),
            sub_model: "fast-model".to_string(),
            route: "service".to_string(),
            ..Default::default()
        };
        let plain = Child {
            role: "",
            definition: None,
            tools: None,
            edits: None,
            memory: None,
        };
        let child = parent.api_child(settings.clone(), &plain);
        assert_eq!(child.model(), Some("fast-model"));
        assert_eq!(child.id(), "service", "the parent's own route");
        let def = reviewer(None);
        let defined = Child {
            role: "",
            definition: Some(&def),
            tools: None,
            edits: None,
            memory: None,
        };
        assert_eq!(
            parent.api_child(settings.clone(), &defined).model(),
            Some("opus")
        );
        // None named: the child inherits the main model, as before.
        let inherited = crate::agent::api::Settings {
            sub_model: String::new(),
            ..settings
        };
        assert_eq!(
            parent.api_child(inherited, &plain).model(),
            Some("main-model")
        );
    }

    /// A model service on 127.0.0.1 answering each of `times` requests with
    /// `reply` as a whole (not streamed) message: its endpoint.
    fn model_service(reply: &'static str, times: usize) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(times) {
                let Ok(mut stream) = stream else { continue };
                let mut seen = Vec::new();
                let mut buf = [0u8; 8192];
                // Headers, then the body its content-length names.
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&seen).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if seen.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let body = serde_json::json!({
                    "content": [{"type": "text", "text": reply}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 5, "output_tokens": 7}
                })
                .to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://127.0.0.1:{port}/v1/messages")
    }

    /// **WHEN A SUB-AGENT FINISHES, THE OPERATOR'S `subagent_stop` IS TOLD**:
    /// which agent it was, what it was handed, what it reported and how it
    /// ended — run in the workspace, after the real `Agent` tool ran it.
    #[test]
    fn a_finished_sub_agent_runs_the_operators_subagent_stop_hook() {
        let ws = tempfile::tempdir().expect("ws");
        let save_event = if cfg!(windows) {
            "more > subagent-event.json"
        } else {
            "cat > subagent-event.json"
        };
        let mut parent = Toolbox::new(ws.path(), true).with_operator_hooks(Some(
            crate::hooks::HookRunner::new(crate::hooks::HookConfig {
                subagent_stop: vec![save_event.to_string().into()],
                ..crate::hooks::HookConfig::default()
            }),
        ));
        parent.set_api_route(crate::agent::api::Settings {
            endpoint: model_service("the parser is in src/plan.rs", 4),
            key: "k".to_string(),
            model: "m".to_string(),
            route: "local".to_string(),
            ..Default::default()
        });
        let said = parent
            .run_subagent(&serde_json::json!({"task": "find the parser", "role": "explore"}))
            .expect("the sub-agent reported");
        assert_eq!(said, "the parser is in src/plan.rs");

        let event: Value = serde_json::from_str(
            &std::fs::read_to_string(ws.path().join("subagent-event.json"))
                .expect("the subagent_stop hook did not run"),
        )
        .expect("one JSON event");
        assert_eq!(event["hook_event_name"], "SubagentStop");
        assert_eq!(event["agent_type"], "explore");
        assert_eq!(event["task"], "find the parser");
        assert_eq!(
            event["last_assistant_message"],
            "the parser is in src/plan.rs"
        );
        assert_eq!(event["outcome"], "completed");
    }

    /// A sub-agent never gets a tool its parent was not given: a definition's
    /// list is narrowed to the parent's, and with no list it inherits it.
    #[test]
    fn a_sub_agents_tools_are_never_wider_than_its_parents() {
        let ws = tempfile::tempdir().expect("ws");
        let mut parent = Toolbox::new(ws.path(), true);
        parent.restrict_tools(vec!["Read".to_string(), "Agent".to_string()]);
        let def = reviewer(Some(&["Read", "Write"]));
        assert_eq!(
            parent.child_tools(Some(&def)),
            Some(vec!["Read".to_string()])
        );
        assert_eq!(
            parent.child_tools(None),
            Some(vec!["Read".to_string(), "Agent".to_string()])
        );
        let open = Toolbox::new(ws.path(), true);
        assert_eq!(open.child_tools(None), None);
    }

    /// A name that is not defined is refused with the names that are.
    #[test]
    fn an_undefined_agent_is_refused_with_the_defined_names() {
        let ws = tempfile::tempdir().expect("ws");
        let dir = ws.path().join(crate::subagents::PROJECT_DIR);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("scout.md"),
            "---\nname: scout\ndescription: finds things\n---\nFind it.",
        )
        .expect("def");
        let tools = Toolbox::new(ws.path(), true);
        let refused = tools
            .run_subagent(&serde_json::json!({"task": "look", "agent": "nobody"}))
            .expect_err("refused");
        assert!(refused.contains("no agent called `nobody`"), "{refused}");
        assert!(refused.contains("scout"), "{refused}");
        let agent_def = tools
            .definitions()
            .into_iter()
            .find(|d| d["name"] == super::super::AGENT)
            .expect("the Agent tool");
        assert!(
            agent_def["description"]
                .as_str()
                .is_some_and(|d| d.contains("- scout: finds things")),
            "{agent_def}"
        );
    }
}
