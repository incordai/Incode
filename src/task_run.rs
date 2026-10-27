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
//! A caller that has already scoped the task says `--spec-gate=off`
//! ([`crate::spec::SpecGate`]): the intake is then neither asked for in the
//! prompt nor required at the write, for this run only, and the result line
//! says `"spec_gate":"off"`.
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

use crate::agent::AgentEvent;

mod drive;
use drive::{drive, Task};

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
    /// Carried into the result so a caller that turned the gate off can
    /// report that it did; a run that relaxed a law must say so where it ends.
    spec_gate: crate::spec::SpecGate,
    /// Files this run created outside the workspace before the agent started,
    /// as full paths. Said in the first `init`, because a stderr line is the
    /// one place a JSON consumer is guaranteed not to look.
    created: Vec<String>,
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
            spec_gate: crate::spec::for_this_run(),
            created: Vec::new(),
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
        match fallback_from {
            Some(previous) => init["fallback_from"] = json!(previous),
            // On the first `init` only, and always present there — empty
            // when nothing was written — so a consumer reads one field
            // rather than testing for it, and a fallback's `init` does not
            // report the same files twice.
            None => init["created"] = json!(self.created),
        }
        self.line(&init);
    }

    /// One event of the agent's turn.
    fn event(&mut self, event: &AgentEvent) {
        let line = match event {
            AgentEvent::Text(text) => json!({"type": "text", "text": text}),
            AgentEvent::Thinking(text) => json!({"type": "thinking", "text": text}),
            AgentEvent::Notice(text) => json!({"type": "notice", "text": text}),
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

    /// What the run's tokens came to, as a `TokenUsage`.
    fn spent(&self) -> crate::runtime::usage::TokenUsage {
        let [input, output, cache_creation, cache_read] = self.usage;
        crate::runtime::usage::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
        }
    }

    /// Estimated dollars, or `None` for a model the price table does not know.
    fn cost_usd(&self) -> Option<f64> {
        self.spent().estimate_cost_usd(self.model.as_deref())
    }

    /// The text run's closing summary, or `None` when nothing was reported.
    fn cost_line(&self) -> Option<String> {
        let spent = self.spent();
        if spent.total_tokens() == 0 {
            return None;
        }
        Some(spent.summary_line(self.model.as_deref()))
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
            // Null for a model the price table does not know: a guessed
            // figure would be read as a bill.
            "cost_usd": self.cost_usd(),
            "duration_ms": u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
            "spec_gate": self.spec_gate.as_str(),
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

/// [`run_task_with_model`], reported in `format`, under `spec_gate`.
///
/// `spec_gate` is fixed for the rest of the process here, before any toolbox
/// is built, so every toolbox the run makes — the lead agent's, a sub-agent's
/// — decides source writes the same way ([`crate::spec::SpecGate`]).
///
/// # Errors
///
/// As [`run_task_with_model`]; and, for a JSON format, when stdout cannot be
/// taken for it. A JSON run's `result` line says the same error.
///
/// `options` are what a caller may also set; [`RunOptions::default`] is none
/// of them.
pub fn run_task_as(
    workspace: &Path,
    instruction: &str,
    model: Option<&str>,
    format: OutputFormat,
    spec_gate: crate::spec::SpecGate,
    options: &RunOptions<'_>,
) -> Result<String, String> {
    let RunOptions {
        command_tools,
        permission_mode,
        created,
    } = *options;
    crate::spec::set_for_this_run(spec_gate);
    if format == OutputFormat::Text {
        let mut report = Report::text();
        let ran = run_task_reporting(
            workspace,
            instruction,
            model,
            command_tools,
            permission_mode,
            &mut report,
        );
        // The run's spend, once, at the end: the per-event lines above it are
        // each one request, and the question afterwards is what it all cost.
        if let Some(line) = report.cost_line() {
            outln!("  · {line}");
        }
        return ran;
    }
    let claimed = crate::acp_agent::stdio::claim()
        .map_err(|e| format!("stdout could not be taken for the JSON output: {e}"))?;
    let mut report = Report::text();
    report.out = Some((claimed.output, format == OutputFormat::StreamJson));
    report.created = created
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let began = std::time::Instant::now();
    let ran = run_task_reporting(
        workspace,
        instruction,
        model,
        command_tools,
        permission_mode,
        &mut report,
    );
    report.finish(&ran, began.elapsed());
    ran
}

/// The parts of a run most callers leave alone.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions<'a> {
    /// A custom command's `allowed-tools`, when not empty: the run is held to
    /// them ([`crate::custom_commands`]).
    pub command_tools: &'a [String],
    /// `-p`, when given: what every candidate agent may do.
    pub permission_mode: Option<crate::permissions::PermissionMode>,
    /// What the process wrote outside the workspace before the run — the
    /// shipped sub-agent definitions on a first run — for the `init` event.
    pub created: &'a [std::path::PathBuf],
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
    run_task_reporting(
        workspace,
        instruction,
        model,
        &[],
        None,
        &mut Report::text(),
    )
}

/// The run itself, telling `report` what happens, with the operator's hooks
/// from `admin.json` and their own plugins.
fn run_task_reporting(
    workspace: &Path,
    instruction: &str,
    model: Option<&str>,
    command_tools: &[String],
    permission_mode: Option<crate::permissions::PermissionMode>,
    report: &mut Report,
) -> Result<String, String> {
    run_task_hooked(
        workspace,
        instruction,
        model,
        command_tools,
        permission_mode,
        report,
        crate::hooks::HookRunner::from_settings(),
    )
}

/// [`run_task_reporting`] with the operator's hooks given, so a test can
/// give its own.
fn run_task_hooked(
    workspace: &Path,
    instruction: &str,
    model: Option<&str>,
    command_tools: &[String],
    permission_mode: Option<crate::permissions::PermissionMode>,
    report: &mut Report,
    operator_hooks: Option<crate::hooks::HookRunner>,
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
    // `session_start` first, as a session's start comes before its first
    // line; what it prints goes to stderr, which a JSON run keeps apart.
    if let Some(hooks) = &operator_hooks {
        announce(
            "session_start",
            &hooks.session_start("startup", &session_id, Some(workspace)),
        );
    }
    // Every way out after the start runs this, a refused line included, so
    // no `session_start` is left without its `session_end`.
    let operator_end = || {
        if let Some(hooks) = &operator_hooks {
            announce(
                "session_end",
                &hooks.session_end("other", &session_id, Some(workspace)),
            );
        }
    };
    let hooked = match &operator_hooks {
        Some(hooks) => match prompt_hook_context(hooks, instruction, &session_id, workspace) {
            Ok(said) => said,
            Err(refused) => {
                operator_end();
                return Err(refused);
            }
        },
        None => Vec::new(),
    };
    // The operator's collector, when telemetry is on (`crate::telemetry`).
    crate::telemetry::session_started("task");
    crate::telemetry::user_prompt(instruction);
    let begun = std::time::Instant::now();
    // The owner's installed external tools, started as MCP servers now, in
    // the background, so they connect while the memory session opens.
    let tools = connect_ext_tools(workspace);
    let _held_to = hold_to_command_tools(&tools, command_tools);
    let mut memory = crate::hooks::memory::MemoryHooks::new(
        crate::hooks::engine::Engine::locate(),
        workspace,
        &session_id,
    );
    let start = memory.start("startup");
    let turn = turn_context(&hooked, memory.prompt(instruction));
    let spec_gate = crate::spec::for_this_run();
    if spec_gate == crate::spec::SpecGate::Off {
        // The text run's record of it, and stderr's under the JSON forms,
        // whose result line carries `spec_gate` as well.
        outln!(
            "  · spec gate: off for this run — the spec set is not required; \
             the guards, the sentinel and verify still hold"
        );
    }
    let prompt = build_prompt(
        workspace,
        instruction,
        start.is_some(),
        turn.as_deref(),
        spec_gate,
    );
    let system = system_context(workspace, start);
    let task = Task {
        workspace,
        instruction,
        model,
        guards: &guards,
        sentinel: &sentinel,
        prompt: &prompt,
        system: &system,
        tools: &tools,
        command_tools,
        permission_mode,
    };
    let ran = drive(&task, &mut memory, report);
    memory.end("other");
    operator_end();
    // The run is one turn. Exported now, before this process ends: a thread
    // of its own would end with it.
    let model = report
        .model
        .clone()
        .or_else(|| report.agent.clone())
        .unwrap_or_default();
    crate::telemetry::turn_finished(&model, &report.spent(), begun.elapsed().as_secs_f64());
    crate::telemetry::export_now();
    ran
}

/// What an operator hook printed, on stderr (which a JSON run keeps apart
/// from its output), named by the event it ran for.
fn announce(event: &str, ran: &crate::hooks::HookRunResult) {
    for said in ran.messages() {
        eprintln!("incode: [{event} hook] {said}");
    }
}

/// The operator's `user_prompt_submit` hooks see the instruction first, as in
/// a session (`cli::plan_turn`): one that refuses it stops the run before the
/// memory engine or an agent hears it (`Err`, the refusal to report). What
/// they print rides the turn as context (`Ok`).
fn prompt_hook_context(
    hooks: &crate::hooks::HookRunner,
    instruction: &str,
    session_id: &str,
    workspace: &Path,
) -> Result<Vec<String>, String> {
    let ran = hooks.user_prompt_submit(instruction, session_id, Some(workspace));
    if ran.is_denied() {
        return Err(format!(
            "not run: the operator's user_prompt_submit hook refused it. {}",
            ran.messages().join("\n")
        ));
    }
    Ok(ran.messages().to_vec())
}

/// A custom command's `allowed-tools` hold the whole run — it is the
/// command's one turn — on every candidate whose tools are incode's. `None`
/// when the run is not a command's; the restriction lasts while it is held.
fn hold_to_command_tools(
    tools: &std::sync::Arc<crate::toolbox::SessionTools>,
    command_tools: &[String],
) -> Option<crate::toolbox::TurnRestriction> {
    (!command_tools.is_empty()).then(|| {
        let (list, warnings) = crate::permissions::rules::Allowlist::parse(
            command_tools,
            crate::admin::home_dir().as_deref(),
        );
        for warning in warnings {
            eprintln!("incode: {warning}");
        }
        tools.restrict_turn(list)
    })
}

/// The turn's context block: what the operator's prompt hooks printed, then
/// the memory engine's block, joined; `None` when there is neither.
fn turn_context(hooked: &[String], engine: Option<String>) -> Option<String> {
    let mut blocks: Vec<String> = hooked
        .iter()
        .map(|said| crate::hooks::prompt_context_block(said))
        .collect();
    blocks.extend(engine);
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

/// What rides the system context rather than the prompt: the project's own
/// instruction files, the harness's standing instructions, the engine's
/// session-start block, and the output style (`crate::styles`) last. The
/// style's notes and warnings go to stderr, which a JSON run keeps apart.
fn system_context(workspace: &Path, start: Option<String>) -> Vec<String> {
    let admin_style =
        match crate::admin::AdminConfig::load(&crate::admin::AdminConfig::default_dir()) {
            Ok(admin) => admin.output_style,
            Err(e) => {
                eprintln!("incode: {e}; no output style is read from it");
                None
            }
        };
    let style = crate::styles::for_session(workspace, admin_style.as_deref());
    for line in style.notes.iter().chain(&style.warnings) {
        eprintln!("incode: {line}");
    }
    crate::instructions::project_block(workspace)
        .into_iter()
        .chain(crate::harness::instructions_block())
        .chain(start)
        .chain(style.block)
        .collect()
}

/// Session tools for one run, with the installed external tools connecting
/// in the background. A configured tool that is not installed is named on
/// stderr, which a JSON run keeps apart from its output.
fn connect_ext_tools(workspace: &Path) -> std::sync::Arc<crate::toolbox::SessionTools> {
    let tools = crate::toolbox::SessionTools::new(true);
    let data_dir = crate::admin::AdminConfig::default_dir();
    let configured = crate::admin::AdminConfig::load(&data_dir)
        .map(|admin| crate::ext_tools::in_effect(&admin))
        .unwrap_or_default();
    let (mut specs, mut said) = crate::ext_tools::session_servers(&configured, &data_dir);
    let (plugin_specs, plugin_said) = crate::plugins::session_servers(workspace);
    specs.extend(plugin_specs);
    said.extend(plugin_said);
    // `.mcp.json` and `~/.incode/mcp.json`; nobody is here to approve a
    // project's server, so only approved ones start.
    let (from_files, about_files) = crate::mcp_config::session_servers(workspace, &data_dir, None);
    specs.extend(from_files);
    said.extend(about_files);
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

/// Where an unattended run is told which of incode's tools the model may use
/// on the API route, as a comma-separated list (`Read,Grep,Glob`). Unset or
/// blank offers them all. A CI job sets it; an agent CLI brings its own tools
/// and is not narrowed by it.
pub const ALLOWED_TOOLS_ENV: &str = "INCODE_ALLOWED_TOOLS";

fn allowed_tools() -> Option<Vec<String>> {
    parse_allowed_tools(&std::env::var(ALLOWED_TOOLS_ENV).unwrap_or_default())
}

fn parse_allowed_tools(raw: &str) -> Option<Vec<String>> {
    let names: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// What a turn's tokens came to, as `158.1k in (140.2k cached) / 2.2k out`.
///
/// The input side counts what the cache wrote and what it served: the agent
/// reports those apart from `input_tokens`, which on a resumed session is a
/// few dozen while the request really carried the whole conversation. Shown
/// without them, the line read `20 in` for a turn the budget charged 158k —
/// the same run, two numbers, and the smaller one on screen.
fn usage_line(input: u32, output: u32, cache_creation: u32, cache_read: u32) -> String {
    let human = |n: u32| crate::cli::activity::human_count(u64::from(n));
    let all_input = u64::from(input) + u64::from(cache_creation) + u64::from(cache_read);
    let cached = if cache_read > 0 {
        format!(" ({} cached)", human(cache_read))
    } else {
        String::new()
    };
    format!(
        "{} in{cached} / {} out",
        crate::cli::activity::human_count(all_input),
        human(output)
    )
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
///
/// With `spec_gate` off the intake is not asked for — not the brief, not the
/// dependencies the PRD never argued for, not the spec set — and the agent is
/// told why instead. Relaxing the refusal at the write while the prompt still
/// asked for five documents would fix nothing: the prompt is what made a
/// one-line fix write them.
#[must_use]
pub fn build_prompt(
    workspace: &Path,
    instruction: &str,
    engine_started: bool,
    engine_turn: Option<&str>,
    spec_gate: crate::spec::SpecGate,
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

    if spec_gate == crate::spec::SpecGate::Off {
        parts.push(crate::spec::GATE_OFF_NOTE.to_string());
    } else {
        // The intake gate. An incomplete brief makes completing it the first
        // job — not a refusal, and not something to discover halfway through.
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

        // The documents the loop reads. Named here rather than left to the
        // model to remember, because a PRD alone produces a project nobody can
        // route work across consistently.
        if let Some(note) = crate::spec::intake_note(workspace) {
            parts.push(note);
        }
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
mod tests;
