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
use std::path::Path;

use crate::agent::{AgentBackend, AgentEvent};

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

    let prompt = build_prompt(workspace, instruction);
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
    let chain = crate::agent::catalogue::Catalogue::gather(
        &crate::admin::AdminConfig::load_default().api_providers,
    )
    .usable_chain(crate::agent::registry::preferred_agent().as_deref())?;
    let candidates: Vec<Candidate> = chain
        .into_iter()
        .map(|agent| {
            let api = if agent.id == crate::agent::registry::API_ROUTE {
                crate::agent::api::Settings::from_env()
            } else {
                None
            };
            match api {
                Some(settings) => Candidate::Api(settings),
                None => Candidate::Cli(agent),
            }
        })
        .collect();

    let total_candidates = candidates.len();
    let mut failures = Vec::new();

    for (idx, candidate) in candidates.into_iter().enumerate() {
        let (mut backend, route): (Box<dyn AgentBackend>, String) = match candidate {
            Candidate::Api(settings) => (
                Box::new(
                    crate::agent::api::ApiBackend::new(settings, workspace, "", true)
                        // Nobody is at an unattended run: a call the engine
                        // wants confirmed is the owner's app's to answer, or
                        // refused.
                        .with_engine(crate::hooks::engine::Engine::locate()),
                ),
                "a model over the configured API".to_string(),
            ),
            Candidate::Cli(agent) => {
                let named = agent.display_name.clone();
                (
                    crate::cli::run_backend(workspace, &agent.id, &agent.command, &guards, &[]),
                    named,
                )
            }
        };

        if let Some(m) = model {
            backend.set_models(&[m.to_string()]);
        } else {
            let admin = crate::admin::AdminConfig::load_default();
            let plan_models = admin.roles.get("plan").cloned().unwrap_or_default();
            let (expanded, _) = crate::roles::expand_tiers(&plan_models, &admin.tiers);
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

        let turn_res = backend.run_turn(&prompt, &mut |event: &AgentEvent| match event {
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
                for line in crate::cli::panel::result(
                    &tool,
                    on.as_deref(),
                    output,
                    *is_error,
                    TASK_PANEL_WIDTH,
                ) {
                    outln!("{line}");
                }
            }
            AgentEvent::TextDelta(_) | AgentEvent::Context { .. } => {}
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

        // Checked before the run is reported as successful. There is no ledger on
        // this path, so the breach IS the outcome: a non-zero exit and the files
        // named, left exactly as the agent left them for a person to look at.
        let breaches = sentinel.breaches(workspace, &guards);
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

/// Wrap the instruction in what the harness knows about this project.
///
/// Assembled in the same order a session would: what must be obeyed, then what
/// is already known, then the instruction itself. The instruction goes LAST
/// because it is what the agent acts on, and context after an instruction reads
/// as an afterthought.
#[must_use]
pub fn build_prompt(workspace: &Path, instruction: &str) -> String {
    let admin = crate::admin::AdminConfig::load_default();
    let mut parts = Vec::new();

    if admin.plugins.guards {
        if let Some(block) = crate::guards::context_for_session() {
            parts.push(block);
        }
    }
    if let Some(block) = crate::rules::context_for_turn(instruction) {
        parts.push(block);
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

    if admin.plugins.auditor {
        if let Some(block) = crate::auditor::context_for_session(workspace) {
            parts.push(block);
        }
    }
    if admin.plugins.recorder {
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
        let prompt = build_prompt(dir.path(), "write the PRD");
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
        let prompt = build_prompt(dir.path(), "do the thing");
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
}
