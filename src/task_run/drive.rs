//! Driving the agents: each usable agent is tried in turn until one runs the
//! instruction.

use crate::outln;
use std::path::Path;

use crate::agent::{AgentBackend, AgentEvent};

use super::{allowed_tools, usage_line, Report, TASK_PANEL_WIDTH};

/// One instruction, as it is handed to each agent that might run it.
pub(super) struct Task<'a> {
    pub(super) workspace: &'a Path,
    pub(super) instruction: &'a str,
    pub(super) model: Option<&'a str>,
    pub(super) guards: &'a crate::guards::Guards,
    pub(super) sentinel: &'a crate::guards::Sentinel,
    /// What the agent is sent.
    pub(super) prompt: &'a str,
    /// What rides its system context.
    pub(super) system: &'a [String],
    /// The run's session tools, with the external tools' servers.
    pub(super) tools: &'a std::sync::Arc<crate::toolbox::SessionTools>,
    /// The custom command's `allowed-tools` the run is held to, if any.
    pub(super) command_tools: &'a [String],
    /// `-p`, when given: applied to every candidate agent.
    pub(super) permission_mode: Option<crate::permissions::PermissionMode>,
}

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
/// One agent that may run the task: an endpoint over its API, or an
/// installed agent CLI.
enum Candidate {
    Api(crate::agent::api::Settings),
    Cli(crate::agent::registry::DetectedAgent),
}

impl Candidate {
    /// One catalogue for both routes: the chosen agent first, then every
    /// usable local agent, then a usable API. Setting the API variables no
    /// longer replaces the installed agents — it adds one more place to send
    /// the work.
    fn chain() -> Result<Vec<Candidate>, String> {
        let chain =
            crate::agent::catalogue::Catalogue::gather(&crate::admin::AdminConfig::load_default())
                .usable_chain(crate::agent::registry::preferred_agent().as_deref())?;
        Ok(chain
            .into_iter()
            .map(
                |agent| match crate::agent::models::settings_for(&agent.id) {
                    Some(settings) => Candidate::Api(settings),
                    None => Candidate::Cli(agent),
                },
            )
            .collect())
    }

    /// The agent as the report names it.
    fn agent_id(&self) -> String {
        match self {
            Candidate::Api(settings) => settings.route.clone(),
            Candidate::Cli(agent) => agent.id.clone(),
        }
    }

    /// Its backend, and its route as the operator is told it.
    fn backend(self, task: &Task<'_>) -> (Box<dyn AgentBackend>, String) {
        match self {
            Candidate::Api(settings) => {
                let named = format!("{} over its API", settings.route);
                let mode = task
                    .permission_mode
                    .unwrap_or(crate::permissions::PermissionMode::WorkspaceWrite);
                let backend = crate::agent::api::ApiBackend::new(
                    settings,
                    task.workspace,
                    &task.system.join("\n\n"),
                    true,
                )
                // Nobody is at an unattended run: a call the engine wants
                // confirmed is the owner's app's to answer, or refused.
                .with_engine(crate::hooks::engine::Engine::locate())
                .with_code_edits(crate::toolbox::CodeEdits::locate())
                .with_operator_hooks(crate::hooks::HookRunner::from_settings())
                .with_sandbox(crate::admin::AdminConfig::load_default().sandbox_policy(mode))
                .with_tool_allowlist(allowed_tools());
                (Box::new(backend), named)
            }
            Candidate::Cli(agent) => {
                let named = agent.display_name.clone();
                let backend = crate::cli::run_backend(
                    task.workspace,
                    &agent.id,
                    &agent.command,
                    task.guards,
                    task.system,
                );
                (backend, named)
            }
        }
    }
}

/// A candidate's backend, set up for this run: its calls filed under the
/// run's memory session, the run's session tools shared, `-p` applied, and
/// its models chosen.
fn prepare(
    backend: &mut EndsEdits,
    task: &Task<'_>,
    route: &str,
    memory: &crate::hooks::memory::MemoryHooks,
) {
    // Its calls are filed under this run's session, as its stop is.
    backend.set_memory_session(memory.session());
    // The run's one set of session tools, holding the external tools'
    // servers, is every candidate's.
    backend.share_session_tools(std::sync::Arc::clone(task.tools));
    // `-p`: what the agent may do, decided by the agent's own mode on a
    // CLI route and by the toolbox on the API route.
    if let Some(mode) = task.permission_mode {
        backend.set_permission_mode(mode.agent_mode());
    }
    if !task.command_tools.is_empty() && !backend.runs_tools_here() {
        outln!(
            "  · {}",
            crate::custom_commands::not_enforced_by(route, task.command_tools)
        );
    }
    choose_models(backend, task.model);
}

/// `--model` when given; otherwise the `plan` role's models, its tiers
/// expanded; otherwise the agent's own `implement` ladder.
fn choose_models(backend: &mut EndsEdits, model: Option<&str>) {
    if let Some(m) = model {
        backend.choose_model(m);
        return;
    }
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

/// Why one candidate did not finish the task, worded for each place it is
/// said.
struct Failure {
    /// Kept in the run's list of failures, after the route.
    why: String,
    /// The run's error when this candidate's was the only failure.
    alone: String,
    /// Printed before falling back to the next candidate.
    fallback: String,
    /// Traced before falling back to the next candidate.
    traced: String,
}

impl Failure {
    /// The agent's turn could not run at all.
    fn could_not_run(route: &str, e: &dyn std::fmt::Display) -> Self {
        Self {
            why: e.to_string(),
            alone: format!("the agent could not run: {e}"),
            fallback: format!("{route} failed ({e})"),
            traced: format!("{route} failed: {e}"),
        }
    }

    /// The turn ran and the agent reported an error.
    fn reported(route: &str, failure: String) -> Self {
        Self {
            why: failure.clone(),
            fallback: format!("{route} reported an error ({failure})"),
            traced: format!("{route} error: {failure}"),
            alone: failure,
        }
    }
}

/// The instruction, through each usable agent in turn until one runs it.
///
/// Called by `run_task_hooked`. Each candidate comes from
/// [`Candidate::chain`], is set up by [`prepare`] and run by [`attempt`];
/// [`first_to_finish`] hands the task on when one fails.
pub(super) fn drive(
    task: &Task<'_>,
    memory: &mut crate::hooks::memory::MemoryHooks,
    report: &mut Report,
) -> Result<String, String> {
    first_to_finish(Candidate::chain()?, |candidate| {
        let agent_id = candidate.agent_id();
        let (backend, route) = candidate.backend(task);
        // However this candidate's attempt ends — an answer, a failure, the
        // next candidate — the engine's edit check hears that its conversation
        // is over: what it claimed, read and announced is let go.
        let mut backend = EndsEdits(backend);
        prepare(&mut backend, task, &route, memory);
        let outcome = attempt(task, &mut backend, &route, &agent_id, memory, report);
        (route, outcome)
    })
}

/// Each candidate in turn, `run` answering with its route and how its attempt
/// ended, until one finishes: its result is the run's. A failure is kept
/// and the next candidate tried; when none is left, the run's error is the
/// one failure's own message, or every failure listed.
fn first_to_finish<C>(
    candidates: Vec<C>,
    mut run: impl FnMut(C) -> (String, Result<Result<String, String>, Failure>),
) -> Result<String, String> {
    let total = candidates.len();
    let mut failures = Vec::new();

    for (idx, candidate) in candidates.into_iter().enumerate() {
        let (route, outcome) = run(candidate);
        let failure = match outcome {
            Ok(result) => return result,
            Err(failure) => failure,
        };
        failures.push(format!("{route}: {}", failure.why));
        if idx + 1 == total {
            return Err(if failures.len() == 1 {
                failure.alone
            } else {
                all_failed(&failures)
            });
        }
        outln!("  · {}; falling back to next agent...", failure.fallback);
        crate::trace!(
            "agent",
            "{}; falling back to next candidate",
            failure.traced
        );
    }

    Err(if failures.is_empty() {
        "no candidate agents available to run".to_string()
    } else {
        all_failed(&failures)
    })
}

fn all_failed(failures: &[String]) -> String {
    format!("all candidate agents failed:\n{}", failures.join("\n"))
}

/// One candidate's attempt: its turn, the session record, the memory
/// engine's continuations, then the sentinel and the unattended `<ask>`.
///
/// `Ok` is the run's result, which no other candidate would change: the
/// answer, or the sentinel's refusal. `Err` is a failure the next candidate
/// may recover from.
fn attempt(
    task: &Task<'_>,
    backend: &mut EndsEdits,
    route: &str,
    agent_id: &str,
    memory: &mut crate::hooks::memory::MemoryHooks,
    report: &mut Report,
) -> Result<Result<String, String>, Failure> {
    outln!("Driving {route} in {}", task.workspace.display());
    report.init(task.workspace, agent_id, backend.model());
    crate::trace!(
        "agent",
        "turn start: route={route} prompt={} chars, guards={} protected",
        task.prompt.len(),
        task.guards.effective().len()
    );

    let mut said = Vec::new();
    // Which call each result belongs to; a result carries only an id.
    let mut tool_calls: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();

    // What the model said, rebuilt for the memory engine's transcript.
    let mut heard = crate::hooks::memory::Said::default();
    let outcome = backend
        .run_turn(task.prompt, &mut |event: &AgentEvent| {
            heard.hear(event);
            report.event(event);
            show(event, &mut said, &mut tool_calls);
        })
        .map_err(|e| Failure::could_not_run(route, &e))?;

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
    record_session(&outcome, backend, task, route);

    if outcome.is_error {
        return Err(Failure::reported(route, outcome.failure()));
    }

    // The turn is ending: the memory engine records it, and may say to
    // carry on — sent to the same agent as its next prompt, bounded as in
    // a session. Before the sentinel, so what a continuation writes is
    // checked too. What its tools did, then what it said — the tool calls
    // are what the engine's recorder learns from (`MemoryHooks::tools_ran`).
    memory.tools_ran(&heard);
    memory.said(crate::hooks::memory::Speaker::Agent, &heard.whole());
    carry_on(
        backend,
        memory,
        report,
        heard.last().to_string(),
        &mut said,
        &mut tool_calls,
    );

    // Checked before the run is reported as successful. There is no ledger on
    // this path, so the breach IS the outcome: a non-zero exit and the files
    // named, left exactly as the agent left them for a person to look at.
    let breaches = task.sentinel.breaches(task.workspace, task.guards);
    if let Some(refusal) = crate::guards::Sentinel::refusal(&breaches) {
        return Ok(Err(refusal));
    }

    Ok(Ok(with_unattended_note(said.join("\n"))))
}

/// The agent's session, filed under its id: named after the instruction,
/// with each sub-agent it spawned. Nothing when the agent gave no id.
fn record_session(
    outcome: &crate::agent::TurnOutcome,
    backend: &EndsEdits,
    task: &Task<'_>,
    route: &str,
) {
    if outcome.session_id.is_empty() {
        return;
    }
    let root = crate::admin::AdminConfig::default_dir();
    let mut record = crate::session::Record::new(&outcome.session_id, task.workspace, route);
    record.name_from(task.instruction);
    for (child, title) in backend.children() {
        record.spawned(&child, &title);
    }
    crate::session::save(&root, &record);
}

/// While the memory engine says to carry on, its reason goes to the same
/// agent as the next prompt. What each continuation says joins `said`; one
/// that fails or cannot run ends the loop.
fn carry_on(
    backend: &mut EndsEdits,
    memory: &mut crate::hooks::memory::MemoryHooks,
    report: &mut Report,
    mut last: String,
    said: &mut Vec<String>,
    tool_calls: &mut std::collections::HashMap<String, (String, Option<String>)>,
) {
    while let Some(reason) = memory.stop(&last, None) {
        outln!("  · memory engine: carry on — {reason}");
        let mut more = Vec::new();
        let mut heard = crate::hooks::memory::Said::default();
        let ran = backend.run_turn(&reason, &mut |event: &AgentEvent| {
            heard.hear(event);
            report.event(event);
            show(event, &mut more, tool_calls);
        });
        memory.tools_ran(&heard);
        match ran {
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
}

/// The answer, with a note added when the agent asked a question nobody was
/// here to answer.
///
/// A DECISION NOBODY MADE MUST NOT VANISH INTO THE OUTPUT. This path is
/// unattended by definition — one instruction, nobody at the prompt — and it
/// never looked at what the agent said for an `<ask>` block. So an agent that
/// reached a fork, asked, and carried on with a guess produced a run that
/// reported plain success, with the question sitting in the middle of the
/// prose as though it were commentary.
///
/// Reported, NOT failed. The agent did the work; what it could not do is get
/// an answer. Failing here would throw away a result that is often fine, and
/// blocking would hang a run nobody is watching — which is the thing this
/// path exists to avoid.
fn with_unattended_note(answer: String) -> String {
    match crate::ask::unattended_note(&crate::ask::parse(&answer)) {
        Some(note) => {
            outln!("\n{note}");
            // Returned as well as printed: the caller records this string,
            // and a warning that exists only on a terminal nobody was
            // watching is the same as no warning.
            format!("{answer}\n\n{note}")
        }
        None => answer,
    }
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
            cache_creation_tokens,
            cache_read_tokens,
        } => {
            outln!(
                "  · {}",
                usage_line(
                    *input_tokens,
                    *output_tokens,
                    *cache_creation_tokens,
                    *cache_read_tokens
                )
            );
        }
        AgentEvent::Quota { status, .. } => outln!("  · quota: {status}"),
        AgentEvent::Notice(note) => outln!("  · {note}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A candidate that fails hands the task to the next; the first to
    /// finish decides the run, and the rest are never tried.
    #[test]
    fn the_task_falls_back_until_a_candidate_finishes() {
        let mut tried = Vec::new();
        let got = first_to_finish(vec!["a", "b", "c"], |name| {
            tried.push(name);
            let outcome = match name {
                "a" => Err(Failure::could_not_run(name, &"not installed")),
                "b" => Ok(Ok("done".to_string())),
                _ => unreachable!("tried past the candidate that finished"),
            };
            (name.to_string(), outcome)
        });
        assert_eq!(got, Ok("done".to_string()));
        assert_eq!(tried, ["a", "b"]);
    }

    /// The sentinel's refusal ends the run: it is the result, not a failure
    /// another agent could recover from.
    #[test]
    fn a_refusal_is_the_result_and_no_other_candidate_is_tried() {
        let got = first_to_finish(vec!["a", "b"], |name| match name {
            "a" => (
                name.to_string(),
                Ok(Err("protected files changed".to_string())),
            ),
            _ => unreachable!("tried after a refusal"),
        });
        assert_eq!(got, Err("protected files changed".to_string()));
    }

    /// When every candidate fails, one failure is the error in its own
    /// words; several are listed, each after its route.
    #[test]
    fn when_every_candidate_fails_the_error_says_which_and_why() {
        let could_not = |name: &str| Failure::could_not_run(name, &"not installed");
        let reported = |name: &str| Failure::reported(name, "rate limited".to_string());

        let one = first_to_finish(vec!["a"], |n| (n.to_string(), Err(could_not(n))));
        assert_eq!(
            one,
            Err("the agent could not run: not installed".to_string())
        );
        let one = first_to_finish(vec!["a"], |n| (n.to_string(), Err(reported(n))));
        assert_eq!(one, Err("rate limited".to_string()));

        let both = first_to_finish(vec!["a", "b"], |n| {
            let failure = if n == "a" { could_not(n) } else { reported(n) };
            (n.to_string(), Err(failure))
        });
        assert_eq!(
            both,
            Err("all candidate agents failed:\na: not installed\nb: rate limited".to_string())
        );

        let none = first_to_finish(Vec::<&str>::new(), |_| unreachable!("no candidates"));
        assert_eq!(
            none,
            Err("no candidate agents available to run".to_string())
        );
    }
}
