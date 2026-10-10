//! Running one instruction, without a model credential.
//!
//! The interactive path can be driven by a *model* over an HTTP API
//! ([`crate::agent::api`]), and that needs a credential before it can start at
//! all. An installed agent CLI is a different thing: it
//! owns its own loop and authenticates itself on the operator's subscription,
//! and two harnesses cannot both own the loop.
//!
//! So this is the second door. It takes one instruction, wraps it in everything
//! the harness knows about this project, hands it to the installed agent, and
//! prints what happens. No API key is read, held or needed.
//!
//! # What the instruction carries
//!
//! The same context a session would have assembled: the intake task when the
//! brief is incomplete, the owner's standing rules, the guard categories that
//! will pause the work, and what the recorder and auditor already know. An agent
//! given the bare instruction would rediscover all of it at the operator's
//! expense.
//!
//! # What this does not do
//!
//! It does not run the ledger — that is `/run`, and it demands proof per task.
//! This is one instruction, once, reported honestly: what the agent said and
//! whether it ended cleanly. Nothing here marks anything done.

use crate::outln;
use std::io::Write as _;
use std::path::Path;

use serde_json::{json, Value};

use crate::agent::{AgentBackend, AgentEvent};

/// How `incode -t` reports what happens.
///
/// - `text` (the default): what the agent says and does, printed as it
///   happens, for a person.
/// - `stream-json`: one JSON object per line on stdout as it happens —
///   `init`, `text`, `thinking`, `tool_use`, `tool_result`, `usage`, and
///   last `result` — for CI and scripts.
/// - `json`: only the `result` object, once, at the end.
///
/// In both JSON forms stdout carries the JSON and nothing else: it is taken
/// as a private handle and the process's standard output is pointed at
/// stderr ([`crate::acp_agent::stdio::claim`], as under `--acp`), so every
/// other print — the text a person would read, a child process — lands on
/// stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    StreamJson,
    Json,
}

impl OutputFormat {
    /// The format named on the command line.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "text" => Some(Self::Text),
            "stream-json" => Some(Self::StreamJson),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Where a JSON run's events go, and what its `result` will say.
struct Report {
    /// The real stdout and whether every event is written (`stream-json`) or
    /// only the result (`json`). `None` for text.
    out: Option<(std::fs::File, bool)>,
    session: String,
    agent: Option<String>,
    model: Option<String>,
    agent_session: Option<String>,
    stop_reason: Option<String>,
    served_by: Vec<String>,
    usage: [u32; 4],
}

impl Report {
    fn text() -> Self {
        Self {
            out: None,
            session: String::new(),
            agent: None,
            model: None,
            agent_session: None,
            stop_reason: None,
            served_by: Vec::new(),
            usage: [0; 4],
        }
    }

    /// One line of JSON on the real stdout, when every event is written.
    fn line(&mut self, value: &Value) {
        if let Some((file, true)) = self.out.as_mut() {
            let _ = writeln!(file, "{value}");
            let _ = file.flush();
        }
    }

    /// An agent is about to take the instruction.
    fn init(&mut self, workspace: &Path, agent: &str, model: Option<&str>) {
        let fallback_from = self.agent.replace(agent.to_string());
        self.model = model.map(str::to_string);
        let mut init = json!({
            "type": "init",
            "session_id": self.session,
            "workspace": workspace.to_string_lossy(),
            "agent": agent,
            "model": model,
            "incode_version": env!("CARGO_PKG_VERSION"),
        });
        if let Some(previous) = fallback_from {
            init["fallback_from"] = json!(previous);
        }
        self.line(&init);
    }

    /// One event of the agent's turn.
    fn event(&mut self, event: &AgentEvent) {
        let line = match event {
            AgentEvent::Text(text) => json!({"type": "text", "text": text}),
            AgentEvent::Thinking(text) => json!({"type": "thinking", "text": text}),
            AgentEvent::ToolUse { id, name, input } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": serde_json::from_str::<Value>(input).unwrap_or_else(|_| json!(input)),
            }),
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
            } => json!({"type": "tool_result", "id": id, "output": output, "is_error": is_error}),
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            } => {
                self.usage = [
                    *input_tokens,
                    *output_tokens,
                    *cache_creation_tokens,
                    *cache_read_tokens,
                ];
                json!({
                    "type": "usage",
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cache_creation_tokens": cache_creation_tokens,
                    "cache_read_tokens": cache_read_tokens,
                })
            }
            AgentEvent::SessionStarted { session_id, .. } => {
                if !session_id.is_empty() {
                    self.agent_session = Some(session_id.clone());
                }
                return;
            }
            // A whole block follows each delta, and the window and quota are
            // the agent's own bookkeeping: not part of the stream.
            AgentEvent::TextDelta(_) | AgentEvent::Quota { .. } | AgentEvent::Context { .. } => {
                return
            }
        };
        self.line(&line);
    }

    /// How the agent's turn ended.
    fn outcome(&mut self, outcome: &crate::agent::TurnOutcome) {
        self.stop_reason = Some(outcome.stop_reason.clone());
        self.served_by.clone_from(&outcome.served_by);
        if !outcome.session_id.is_empty() {
            self.agent_session = Some(outcome.session_id.clone());
        }
    }

    /// The last line: what the run came to, as one object.
    fn finish(&mut self, ran: &Result<String, String>, took: std::time::Duration) {
        let [input, output, cache_creation, cache_read] = self.usage;
        let mut result = json!({
            "type": "result",
            "is_error": ran.is_err(),
            "session_id": self.session,
            "agent": self.agent,
            "model": self.model,
            "agent_session_id": self.agent_session,
            "stop_reason": self.stop_reason,
            "served_by": self.served_by,
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_creation_tokens": cache_creation,
                "cache_read_tokens": cache_read,
            },
            "duration_ms": u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
        });
        match ran {
            Ok(answer) => result["result"] = json!(answer),
            Err(why) => result["error"] = json!(why),
        }
        if let Some((file, _)) = self.out.as_mut() {
            let _ = writeln!(file, "{result}");
            let _ = file.flush();
        }
    }
}

/// [`run_task_with_model`], reported in `format`.
///
/// # Errors
///
/// As [`run_task_with_model`]; and, for a JSON format, when stdout cannot be
/// taken for it. A JSON run's `result` line says the same error.
pub fn run_task_as(
    workspace: &Path,
    instruction: &str,
    model: Option<&str>,
    format: OutputFormat,
) -> Result<String, String> {
    if format == OutputFormat::Text {
        return run_task_with_model(workspace, instruction, model);
    }
    let claimed = crate::acp_agent::stdio::claim()
        .map_err(|e| format!("stdout could not be taken for the JSON output: {e}"))?;
    let mut report = Report::text();
    report.out = Some((claimed.output, format == OutputFormat::StreamJson));
    let began = std::time::Instant::now();
    let ran = run_task_reporting(workspace, instruction, model, &mut report);
    report.finish(&ran, began.elapsed());
    ran
}

/// Run one instruction through an installed agent and report what happened.
///
/// Returns an error when there is no agent to drive, or when the agent could not
/// run at all. An agent that ran and failed is a successful *measurement* of a
/// failure and comes back as a report, because the caller needs to tell the
/// difference between "the work went badly" and "nothing ran".
/// How wide a tool result is drawn on the non-interactive path.
const TASK_PANEL_WIDTH: usize = 100;

pub fn run_task(workspace: &Path, instruction: &str) -> Result<String, String> {
    run_task_with_model(workspace, instruction, None)
}

pub fn run_task_with_model(
    workspace: &Path,
    instruction: &str,
    model: Option<&str>,
) -> Result<String, String> {
    run_task_reporting(workspace, instruction, model, &mut Report::text())
}

/// The run itself, telling `report` what happens.
fn run_task_reporting(
    workspace: &Path,
    instruction: &str,
    model: Option<&str>,
    report: &mut Report,
) -> Result<String, String> {
    if instruction.trim().is_empty() {
        return Err("nothing to do: the instruction is empty".to_string());
    }

    // The guards ride in the prompt below, which is an INSTRUCTION — the agent
    // brings its own file tools and can simply not follow it. This is the rule:
    // the protected files are fingerprinted now and checked when it hands back.
    //
    // A policy that cannot be read stops the run. A malformed guards file
    // silently becoming "no guards" is the worst failure that file has.
    let guards = crate::guards::Guards::load(workspace).map_err(|e| {
        format!("refusing to run: the protected-path policy could not be read ({e}). Running without it would leave the check editable by the thing being judged.")
    })?;
    let sentinel = crate::guards::Sentinel::over(workspace, &guards);
    // A harness that cannot be read stops the run too: the run would be
    // scored as that harness while running on the defaults.
    crate::harness::check().map_err(|e| format!("refusing to run: {e}"))?;

    // The memory engine's session, fired by incode: the model underneath runs
    // with no hooks of its own. One instruction is one session — it starts,
    // takes the person's line, stops, and ends.
    let session_id = uuid::Uuid::new_v4().to_string();
    report.session.clone_from(&session_id);
    // The owner's installed external tools, started as MCP servers now, in
    // the background, so they connect while the memory session opens.
    let tools = connect_ext_tools(workspace);
    let mut memory = crate::hooks::memory::MemoryHooks::new(
        crate::hooks::engine::Engine::locate(),
        workspace,
        &session_id,
    );
    let start = memory.start("startup");
    let turn = memory.prompt(instruction);
    let prompt = build_prompt(workspace, instruction, start.is_some(), turn.as_deref());
    // What rides the system context rather than the prompt: the project's
    // own instruction files, the harness's standing instructions, and the
    // engine's session-start block.
    let system: Vec<String> = crate::instructions::project_block(workspace)
        .into_iter()
        .chain(crate::harness::instructions_block())
        .chain(start)
        .collect();
    let task = Task {
        workspace,
        instruction,
        model,
        guards: &guards,
        sentinel: &sentinel,
        prompt: &prompt,
        system: &system,
        tools: &tools,
    };
    let ran = drive(&task, &mut memory, report);
    memory.end("other");
    ran
}

/// Session tools for one run, with the installed external tools connecting
/// in the background. A configured tool that is not installed is named on
/// stderr, which a JSON run keeps apart from its output.
fn connect_ext_tools(workspace: &Path) -> std::sync::Arc<crate::toolbox::SessionTools> {
    let tools = crate::toolbox::SessionTools::new(true);
    let data_dir = crate::admin::AdminConfig::default_dir();
    let configured = crate::admin::AdminConfig::load(&data_dir)
        .map(|admin| admin.tools)
        .unwrap_or_default();
    let (specs, said) = crate::ext_tools::session_servers(&configured, &data_dir);
    for line in said {
        eprintln!("incode: {line}");
    }
    let on_connected: crate::toolbox::OnConnected =
        std::sync::Arc::new(|line| crate::trace!("tool", "{line}"));
    for line in tools.connect_client_servers(specs, workspace, &on_connected) {
        crate::trace!("tool", "{line}");
    }
    tools
}

/// One instruction, as it is handed to each agent that might run it.
struct Task<'a> {
    workspace: &'a Path,
    instruction: &'a str,
    model: Option<&'a str>,
    guards: &'a crate::guards::Guards,
    sentinel: &'a crate::guards::Sentinel,
    /// What the agent is sent.
    prompt: &'a str,
    /// What rides its system context.
    system: &'a [String],
    /// The run's session tools, with the external tools' servers.
    tools: &'a std::sync::Arc<crate::toolbox::SessionTools>,
}

/// The instruction, through each usable agent in turn until one runs it.
/// One candidate's backend, whose edit session ends when the attempt does.
struct EndsEdits(Box<dyn AgentBackend>);

impl std::ops::Deref for EndsEdits {
    type Target = Box<dyn AgentBackend>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for EndsEdits {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for EndsEdits {
    fn drop(&mut self) {
        self.0.end_code_edits();
    }
}

fn drive(
    task: &Task<'_>,
    memory: &mut crate::hooks::memory::MemoryHooks,
    report: &mut Report,
) -> Result<String, String> {
    let Task {
        workspace,
        instruction,
        model,
        guards,
        sentinel,
        prompt,
        system,
        tools,
    } = *task;
    // WHICH ROUTE, and the CLI requirement belongs INSIDE the choice.
    //
    // An API endpoint needs no installed agent, so asking for a drivable one
    // first would refuse a perfectly good API setup on a machine with nothing
    // installed. The CLI stays the default: an operator who has configured no
    // endpoint sees exactly what they saw before.
    //
    // Both routes enforce the same guards, by different means. The CLI is told
    // them and refuses the write itself; the toolbox behind the API route
    // checks them before acting, because on that route these functions ARE the
    // write and there is no other layer.
    enum Candidate {
        Api(crate::agent::api::Settings),
        Cli(crate::agent::registry::DetectedAgent),
    }

    // One catalogue for both routes: the chosen agent first, then every usable
    // local agent, then a usable API. Setting the API variables no longer
    // replaces the installed agents — it adds one more place to send the work.
    let chain =
        crate::agent::catalogue::Catalogue::gather(&crate::admin::AdminConfig::load_default())
            .usable_chain(crate::agent::registry::preferred_agent().as_deref())?;
    let candidates: Vec<Candidate> = chain
        .into_iter()
        .map(
            |agent| match crate::agent::models::settings_for(&agent.id) {
                Some(settings) => Candidate::Api(settings),
                None => Candidate::Cli(agent),
            },
        )
        .collect();

    let total_candidates = candidates.len();
    let mut failures = Vec::new();

    for (idx, candidate) in candidates.into_iter().enumerate() {
        let agent_id = match &candidate {
            Candidate::Api(settings) => settings.route.clone(),
            Candidate::Cli(agent) => agent.id.clone(),
        };
        let (backend, route): (Box<dyn AgentBackend>, String) = match candidate {
            Candidate::Api(settings) => {
                let named = format!("{} over its API", settings.route);
                (
                    Box::new(
                        crate::agent::api::ApiBackend::new(
                            settings,
                            workspace,
                            &system.join("\n\n"),
                            true,
                        )
                        // Nobody is at an unattended run: a call the engine
                        // wants confirmed is the owner's app's to answer, or
                        // refused.
                        .with_engine(crate::hooks::engine::Engine::locate())
                        .with_code_edits(crate::toolbox::CodeEdits::locate())
                        .with_operator_hooks(crate::hooks::HookRunner::from_settings())
                        .with_sandbox(
                            crate::admin::AdminConfig::load_default()
                                .sandbox_policy(crate::permissions::PermissionMode::WorkspaceWrite),
                        ),
                    ),
                    named,
                )
            }
            Candidate::Cli(agent) => {
                let named = agent.display_name.clone();
                (
                    crate::cli::run_backend(workspace, &agent.id, &agent.command, guards, system),
                    named,
                )
            }
        };
        // However this candidate's attempt ends — an answer, a failure, the
        // next candidate — the engine's edit check hears that its conversation
        // is over: what it claimed, read and announced is let go.
        let mut backend = EndsEdits(backend);
        // Its calls are filed under this run's session, as its stop is.
        backend.set_memory_session(memory.session());
        // The run's one set of session tools, holding the external tools'
        // servers, is every candidate's.
        backend.share_session_tools(std::sync::Arc::clone(tools));

        if let Some(m) = model {
            backend.choose_model(m);
        } else {
            let admin = crate::admin::AdminConfig::load_default();
            let (roles, tiers) = admin.effective_routing();
            let plan_models = roles.get("plan").cloned().unwrap_or_default();
            let (expanded, _) = crate::roles::expand_tiers(&plan_models, &tiers);
            if !expanded.is_empty() {
                backend.set_models(&expanded);
            }
            if backend.model().is_none() {
                let own = backend.ladder_for("implement");
                if !own.is_empty() {
                    backend.set_models(&own);
                }
            }
        }

        outln!("Driving {route} in {}", workspace.display());
        report.init(workspace, &agent_id, backend.model());
        crate::trace!(
            "agent",
            "turn start: route={route} prompt={} chars, guards={} protected",
            prompt.len(),
            guards.effective().len()
        );

        let mut said = Vec::new();
        // Which call each result belongs to; a result carries only an id.
        let mut tool_calls: std::collections::HashMap<String, (String, Option<String>)> =
            std::collections::HashMap::new();

        // What the model said, rebuilt for the memory engine's transcript.
        let mut heard = crate::hooks::memory::Said::default();
        let turn_res = backend.run_turn(prompt, &mut |event: &AgentEvent| {
            heard.hear(event);
            report.event(event);
            show(event, &mut said, &mut tool_calls);
        });

        let outcome = match turn_res {
            Ok(out) => out,
            Err(e) => {
                let msg = format!("the agent could not run: {e}");
                failures.push(format!("{route}: {e}"));
                if idx + 1 < total_candidates {
                    outln!("  · {route} failed ({e}); falling back to next agent...");
                    crate::trace!(
                        "agent",
                        "{route} failed: {e}; falling back to next candidate"
                    );
                    continue;
                }
                return Err(if failures.len() == 1 {
                    msg
                } else {
                    format!("all candidate agents failed:\n{}", failures.join("\n"))
                });
            }
        };

        report.outcome(&outcome);
        crate::trace!(
            "agent",
            "turn end: stop={} error={} said={} chars served_by={:?} session={}",
            outcome.stop_reason,
            outcome.is_error,
            said.iter().map(String::len).sum::<usize>(),
            outcome.served_by,
            outcome.session_id
        );
        if !outcome.session_id.is_empty() {
            let root = crate::admin::AdminConfig::default_dir();
            let mut record = crate::session::Record::new(&outcome.session_id, workspace, &route);
            record.name_from(instruction);
            for (child, title) in backend.children() {
                record.spawned(&child, &title);
            }
            crate::session::save(&root, &record);
        }

        if outcome.is_error {
            let failure_msg = outcome.failure();
            failures.push(format!("{route}: {failure_msg}"));
            if idx + 1 < total_candidates {
                outln!(
                    "  · {route} reported an error ({failure_msg}); falling back to next agent..."
                );
                crate::trace!(
                    "agent",
                    "{route} error: {failure_msg}; falling back to next candidate"
                );
                continue;
            }
            return Err(if failures.len() == 1 {
                failure_msg
            } else {
                format!("all candidate agents failed:\n{}", failures.join("\n"))
            });
        }

        // The turn is ending: the memory engine records it, and may say to
        // carry on — sent to the same agent as its next prompt, bounded as in
        // a session. Before the sentinel, so what a continuation writes is
        // checked too.
        memory.said(crate::hooks::memory::Speaker::Agent, &heard.whole());
        let mut last = heard.last().to_string();
        while let Some(reason) = memory.stop(&last, None) {
            outln!("  · memory engine: carry on — {reason}");
            let mut more = Vec::new();
            let mut heard = crate::hooks::memory::Said::default();
            match backend.run_turn(&reason, &mut |event: &AgentEvent| {
                heard.hear(event);
                report.event(event);
                show(event, &mut more, &mut tool_calls);
            }) {
                Ok(next) if !next.is_error => {
                    report.outcome(&next);
                    memory.said(crate::hooks::memory::Speaker::Agent, &heard.whole());
                    last = heard.last().to_string();
                    said.extend(more);
                }
                Ok(next) => {
                    outln!("  · the continuation failed: {}", next.failure());
                    break;
                }
                Err(e) => {
                    outln!("  · the continuation could not run: {e}");
                    break;
                }
            }
        }

        // Checked before the run is reported as successful. There is no ledger on
        // this path, so the breach IS the outcome: a non-zero exit and the files
        // named, left exactly as the agent left them for a person to look at.
        let breaches = sentinel.breaches(workspace, guards);
        if let Some(refusal) = crate::guards::Sentinel::refusal(&breaches) {
            return Err(refusal);
        }

        let answer = said.join("\n");

        // A DECISION NOBODY MADE MUST NOT VANISH INTO THE OUTPUT.
        //
        // This path is unattended by definition — one instruction, nobody at
        // the prompt — and it never looked at what the agent said for an
        // `<ask>` block. So an agent that reached a fork, asked, and carried
        // on with a guess produced a run that reported plain success, with
        // the question sitting in the middle of the prose as though it were
        // commentary. `ask::unattended_note` was written for exactly this and
        // had no caller.
        //
        // Reported, NOT failed. The agent did the work; what it could not do
        // is get an answer. Failing here would throw away a result that is
        // often fine, and blocking would hang a run nobody is watching —
        // which is the thing this path exists to avoid.
        if let Some(note) = crate::ask::unattended_note(&crate::ask::parse(&answer)) {
            outln!("\n{note}");
            // Returned as well as printed: the caller records this string,
            // and a warning that exists only on a terminal nobody was
            // watching is the same as no warning.
            return Ok(format!("{answer}\n\n{note}"));
        }

        return Ok(answer);
    }

    Err(if failures.is_empty() {
        "no candidate agents available to run".to_string()
    } else {
        format!("all candidate agents failed:\n{}", failures.join("\n"))
    })
}

/// One agent event, printed as it happens; the model's words are kept in
/// `said`, and each call's name in `tool_calls` for the result that follows.
fn show(
    event: &AgentEvent,
    said: &mut Vec<String>,
    tool_calls: &mut std::collections::HashMap<String, (String, Option<String>)>,
) {
    match event {
        AgentEvent::Text(text) => {
            outln!("{text}");
            said.push(text.clone());
        }
        // Printed as it happens rather than summarised at the end: a long
        // task with no output is indistinguishable from a hung one.
        AgentEvent::ToolUse { id, name, input } => {
            tool_calls.insert(id.clone(), (name.clone(), crate::cli::panel::target(input)));
            outln!("  · {name}");
        }
        AgentEvent::SessionStarted { model, .. } => outln!("  · session on {model}"),
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
            ..
        } => {
            outln!("  · {input_tokens} in / {output_tokens} out");
        }
        AgentEvent::Quota { status, .. } => outln!("  · quota: {status}"),
        AgentEvent::ToolResult {
            id,
            output,
            is_error,
        } => {
            let (tool, on) = tool_calls
                .get(id.as_str())
                .cloned()
                .unwrap_or_else(|| ("tool".to_string(), None));
            for line in
                crate::cli::panel::result(&tool, on.as_deref(), output, *is_error, TASK_PANEL_WIDTH)
            {
                outln!("{line}");
            }
        }
        AgentEvent::TextDelta(_) | AgentEvent::Thinking(_) | AgentEvent::Context { .. } => {}
    }
}

/// Wrap the instruction in what the harness knows about this project.
///
/// Assembled in the same order a session would: what must be obeyed, then what
/// is already known, then the instruction itself. The instruction goes LAST
/// because it is what the agent acts on, and context after an instruction reads
/// as an afterthought.
///
/// `engine_started` says the memory engine's session-start block rides the
/// system context: the recorder's handoff and the auditor's findings are in
/// it, so they are not added here too. `engine_turn` is the engine's context
/// for this instruction, which carries the rules; with it, incode's own
/// per-turn rules are not fetched as well.
#[must_use]
pub fn build_prompt(
    workspace: &Path,
    instruction: &str,
    engine_started: bool,
    engine_turn: Option<&str>,
) -> String {
    let admin = crate::admin::AdminConfig::load_default();
    let mut parts = Vec::new();

    if admin.plugins.guards {
        if let Some(block) = crate::guards::context_for_session() {
            parts.push(block);
        }
    }
    match engine_turn {
        Some(block) => parts.push(block.to_string()),
        None => {
            if let Some(block) = crate::rules::context_for_turn(instruction) {
                parts.push(block);
            }
        }
    }

    // The skills index, on this path too. A one-shot task is the same work as a
    // turn in the session and must not know less about the machine it runs on —
    // this pulls guards and rules for exactly that reason, and skills were the
    // one standing source left out.
    if let Some(block) = crate::skills::session_context(&crate::admin::AdminConfig::default_dir()) {
        parts.push(block);
    }

    // The intake gate. An incomplete brief makes completing it the first job —
    // not a refusal, and not something to discover halfway through.
    let gate = crate::brief::check(workspace);
    if let Some(task) = gate.intake_task() {
        parts.push(format!("<intake>\n{task}\n</intake>"));
    }
    if let crate::brief::Gate::Ready(ref brief) = gate {
        if let Some(note) =
            crate::research::intake_note(&crate::research::unvetted(workspace, &brief.prd))
        {
            parts.push(note);
        }
    }

    // The documents the loop reads. Named here rather than left to the model to
    // remember, because a PRD alone produces a project nobody can route work
    // across consistently.
    if let Some(note) = crate::spec::intake_note(workspace) {
        parts.push(note);
    }

    if admin.plugins.auditor && !engine_started {
        if let Some(block) = crate::auditor::context_for_session(workspace) {
            parts.push(block);
        }
    }
    if admin.plugins.recorder && !engine_started {
        if let Some(block) = crate::recorder::context_for_session(workspace) {
            parts.push(block);
        }
    }

    parts.push(format!("Now do this:\n\n{instruction}"));
    parts.join("\n\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// **AN UNANSWERED DECISION MUST NOT READ AS PROSE.**
    ///
    /// `--task` is unattended by definition: one instruction, nobody at the
    /// prompt. It never looked at what the agent said for an `<ask>` block,
    /// so an agent that reached a fork, asked, and carried on with a guess
    /// produced a run reporting plain success — with the question sitting in
    /// the middle of the output as though it were commentary.
    ///
    /// `ask::unattended_note` was written for this and had no caller. This
    /// asserts the pieces the wiring depends on: that the block is recognised
    /// in ordinary agent text, and that the note names the question rather
    /// than merely saying one was asked.
    #[test]
    fn a_question_asked_with_nobody_to_answer_it_is_named() {
        let said = "I started on the endpoint.\n\
                    <ask>\n\
                    Which store should this use?\n\
                    1. Postgres — the document implies joins\n\
                    2. SQLite — one file, nothing to operate\n\
                    </ask>\n\
                    I went with Postgres for now.";

        let questions = crate::ask::parse(said);
        assert_eq!(questions.len(), 1, "the block was not recognised: {said}");

        let note = crate::ask::unattended_note(&questions).expect("a question was asked");
        assert!(
            note.contains("Which store should this use?"),
            "the note must carry the question, not just its existence: {note}"
        );
        assert!(
            note.contains("Postgres") && note.contains("SQLite"),
            "and the options, or the operator cannot answer it: {note}"
        );
    }

    /// And ordinary output stays ordinary. A note on every run would train an
    /// operator to ignore the one that matters.
    #[test]
    fn a_task_that_asked_nothing_gets_no_note() {
        let plain = "I added the endpoint and its test. Both pass.";
        assert!(crate::ask::parse(plain).is_empty());
        assert_eq!(crate::ask::unattended_note(&crate::ask::parse(plain)), None);
    }

    /// **The instruction goes last.** Context after an instruction reads as an
    /// afterthought, and the agent acts on what it read most recently.
    #[test]
    fn the_instruction_is_the_last_thing_the_agent_reads() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Pointed at nothing, so the context blocks are all absent and the test
        // is about ORDER rather than about what any one service returned.
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let dir = tempfile::tempdir().expect("temp dir");
        let prompt = build_prompt(dir.path(), "write the PRD", false, None);
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);

        assert!(prompt.ends_with("write the PRD"), "got: {prompt}");
        assert!(prompt.contains("Now do this:"), "got: {prompt}");
    }

    /// An unreachable body costs the context blocks, never the run — this door
    /// has to work on a machine where the service is not running at all.
    #[test]
    fn the_task_still_runs_when_the_body_is_unreachable() {
        let _guard = crate::brief::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(crate::brief::MEMORY_URL_ENV, "http://127.0.0.1:1");
        let dir = tempfile::tempdir().expect("temp dir");
        let prompt = build_prompt(dir.path(), "do the thing", false, None);
        std::env::remove_var(crate::brief::MEMORY_URL_ENV);

        assert!(prompt.contains("do the thing"));
    }

    /// An empty instruction is refused before an agent is spawned for nothing.
    #[test]
    fn an_empty_instruction_is_refused_without_starting_anything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let refused = run_task(dir.path(), "   ").expect_err("refused");
        assert!(refused.contains("nothing to do"), "got: {refused}");
    }

    /// A turn, as the stream sees it.
    fn a_turn(report: &mut Report) {
        report.session = "sess-json".to_string();
        report.init(Path::new("/ws"), "claude-code", Some("sonnet"));
        for event in [
            AgentEvent::SessionStarted {
                session_id: "agent-conv".to_string(),
                model: "sonnet".to_string(),
            },
            AgentEvent::Thinking("look first".to_string()),
            AgentEvent::TextDelta("Rea".to_string()),
            AgentEvent::ToolUse {
                id: "incode-1".to_string(),
                name: "Read".to_string(),
                input: r#"{"path":"a.txt"}"#.to_string(),
            },
            AgentEvent::ToolResult {
                id: "incode-1".to_string(),
                output: "hello".to_string(),
                is_error: false,
            },
            AgentEvent::Text("It says hello.".to_string()),
            AgentEvent::Usage {
                input_tokens: 10,
                output_tokens: 3,
                cache_creation_tokens: 0,
                cache_read_tokens: 7,
            },
        ] {
            report.event(&event);
        }
        report.outcome(&crate::agent::TurnOutcome {
            session_id: "agent-conv".to_string(),
            stop_reason: "end_turn".to_string(),
            is_error: false,
            detail: None,
            served_by: vec!["claude-sonnet".to_string()],
        });
        report.finish(
            &Ok("It says hello.".to_string()),
            std::time::Duration::from_millis(5),
        );
    }

    fn lines_of(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("written")
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            .collect()
    }

    /// **`stream-json` IS ONE OBJECT PER EVENT, IN ORDER, ENDING WITH THE
    /// RESULT** — the whole blocks, never the deltas beside them.
    #[test]
    fn stream_json_writes_each_event_as_one_json_line() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("out.jsonl");
        let mut report = Report::text();
        report.out = Some((std::fs::File::create(&path).expect("file"), true));
        a_turn(&mut report);
        let lines = lines_of(&path);
        let kinds: Vec<&str> = lines.iter().filter_map(|l| l["type"].as_str()).collect();
        assert_eq!(
            kinds,
            [
                "init",
                "thinking",
                "tool_use",
                "tool_result",
                "text",
                "usage",
                "result"
            ]
        );
        assert_eq!(lines[0]["agent"], "claude-code");
        assert_eq!(lines[0]["session_id"], "sess-json");
        assert_eq!(
            lines[2]["input"]["path"], "a.txt",
            "the input is JSON, not a string"
        );
        let result = &lines[6];
        assert_eq!(result["is_error"], false);
        assert_eq!(result["result"], "It says hello.");
        assert_eq!(result["agent_session_id"], "agent-conv");
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["usage"]["cache_read_tokens"], 7);
    }

    /// `json` is the result alone, once; a failed run's says why.
    #[test]
    fn json_writes_only_the_result() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("out.json");
        let mut report = Report::text();
        report.out = Some((std::fs::File::create(&path).expect("file"), false));
        a_turn(&mut report);
        let lines = lines_of(&path);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["type"], "result");

        let failed = dir.path().join("failed.json");
        let mut report = Report::text();
        report.out = Some((std::fs::File::create(&failed).expect("file"), false));
        report.finish(&Err("no agent".to_string()), std::time::Duration::ZERO);
        let lines = lines_of(&failed);
        assert_eq!(lines[0]["is_error"], true);
        assert_eq!(lines[0]["error"], "no agent");
    }

    /// Set only in the child process the purity test starts.
    const JSON_CHILD: &str = "INCODE_TASK_JSON_CHILD";

    /// The child half: stdout claimed as `run_task_as` claims it, then every
    /// kind of stray print, then a turn reported as `stream-json`.
    #[test]
    fn json_output_child() {
        if std::env::var_os(JSON_CHILD).is_none() {
            return;
        }
        let claimed = crate::acp_agent::stdio::claim().expect("claim stdout");
        println!("stray println");
        crate::outln!("stray outln");
        let _ = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                ["/C", "echo stray child"]
            } else {
                ["-c", "echo stray child"]
            })
            .status();
        let mut report = Report::text();
        report.out = Some((claimed.output, true));
        a_turn(&mut report);
        std::process::exit(0);
    }

    /// **STDOUT CARRIES THE JSON AND NOTHING ELSE.** Every stray print — a
    /// `println!`, the console, a child that inherits stdout — lands on
    /// stderr; stdout is JSON lines only, apart from the test harness's own
    /// banner, printed before the child's code runs.
    #[test]
    fn the_json_output_carries_nothing_else() {
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "task_run::tests::json_output_child",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .env(JSON_CHILD, "1")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut json_lines = 0;
        for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if line == "running 1 test" {
                continue;
            }
            assert!(
                serde_json::from_str::<Value>(line).is_ok(),
                "a stray line reached stdout: {line:?}\nstdout:\n{stdout}"
            );
            json_lines += 1;
        }
        assert_eq!(json_lines, 7, "stdout:\n{stdout}\nstderr:\n{stderr}");
        for stray in ["stray println", "stray outln", "stray child"] {
            assert!(stderr.contains(stray), "{stray} went nowhere: {stderr}");
        }
    }
}
